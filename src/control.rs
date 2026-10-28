//! Process-wide control boundary for `chinstrap ctl`, D-Bus and portal shortcuts.
//!
//! Architecture: the GUI process acquires the session-bus name *before* starting the engine, so a
//! second launch cannot create a competing capture engine. The service object lives on that bus
//! connection; each D-Bus operation carries a reply slot through the engine mailbox and replies
//! only after the engine accepts or rejects it. Status is read from the engine's published control
//! snapshot. A dedicated portal worker owns the GlobalShortcuts connection and session for the
//! lifetime of the GUI; portal activations use the same acknowledged engine request path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use ashpd::desktop::{CreateSessionOptions, Session};
use ashpd::{AppID, register_host_app_with_connection};
use futures::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use zbus::blocking::{Connection, Proxy};
use zbus::object_server::SignalEmitter;

use crate::studio::{EngineCommand, EngineHandle, Phase, Snapshot};

pub const BUS_NAME: &str = "tech.korora.Chinstrap";
pub const OBJECT_PATH: &str = "/tech/korora/Chinstrap";
pub const INTERFACE: &str = "tech.korora.Chinstrap.Control1";
pub const APP_ID: &str = "tech.korora.Chinstrap";
const PORTAL_UNAVAILABLE: &str =
    "Not available on this desktop. Use `chinstrap ctl` with a compositor keybinding.";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct ControlStatus {
    pub phase: String,
    pub session: Option<String>,
    pub segment: Option<u32>,
    pub elapsed_ms: u64,
    pub markers: u32,
    #[serde(skip)]
    recording_started_at: Option<Instant>,
}

impl Default for ControlStatus {
    fn default() -> Self {
        Self {
            phase: "idle".into(),
            session: None,
            segment: None,
            elapsed_ms: 0,
            markers: 0,
            recording_started_at: None,
        }
    }
}

impl ControlStatus {
    pub(crate) fn from_snapshot(snapshot: &Snapshot, markers: u32) -> Self {
        let session = snapshot.session.as_ref().map(|session| {
            (
                session.name.as_str(),
                session.segments.last().map(|segment| segment.index),
            )
        });
        Self::from_state(&snapshot.phase, snapshot.record_pending, session, markers)
    }

    pub(crate) fn from_state(
        phase: &Phase,
        record_pending: bool,
        session: Option<(&str, Option<u32>)>,
        markers: u32,
    ) -> Self {
        let last_segment = session.and_then(|(_, segment)| segment);
        let (phase, active_segment, recording_started_at) = match phase {
            Phase::Idle if record_pending => {
                let next_segment = last_segment.map_or(1, |segment| segment.saturating_add(1));
                ("starting", Some(next_segment), None)
            }
            Phase::Idle => ("idle", None, None),
            Phase::Starting { segment } => ("starting", Some(*segment), None),
            Phase::Recording {
                segment,
                started_at,
            } => ("recording", Some(*segment), Some(*started_at)),
            Phase::Cutting { from } => ("cutting", Some(*from), None),
            Phase::Stopping { segment } => ("stopping", Some(*segment), None),
        };
        Self {
            phase: phase.into(),
            session: session.map(|(name, _)| name.to_string()),
            segment: active_segment.or(last_segment),
            elapsed_ms: recording_started_at
                .map(|started| {
                    Instant::now()
                        .saturating_duration_since(started)
                        .as_millis() as u64
                })
                .unwrap_or_default(),
            markers,
            recording_started_at,
        }
    }

    pub(crate) fn to_json(&self) -> String {
        let elapsed_ms = self
            .recording_started_at
            .map(|started| {
                Instant::now()
                    .saturating_duration_since(started)
                    .as_millis() as u64
            })
            .unwrap_or(self.elapsed_ms);
        serde_json::to_string(&StatusWire {
            phase: &self.phase,
            session: self.session.as_deref(),
            segment: self.segment,
            elapsed_ms,
            markers: self.markers,
        })
        .expect("control status is serializable")
    }

    fn same_reported_state(&self, other: &Self) -> bool {
        self.phase == other.phase
            && self.session == other.session
            && self.segment == other.segment
            && self.markers == other.markers
    }
}

#[derive(Default)]
pub(crate) struct ControlStatusHub {
    current: ControlStatus,
    subscribers: Vec<std::sync::mpsc::Sender<ControlStatus>>,
}

impl ControlStatusHub {
    pub(crate) fn current(&self) -> ControlStatus {
        self.current.clone()
    }

    pub(crate) fn subscribe(
        &mut self,
    ) -> (ControlStatus, std::sync::mpsc::Receiver<ControlStatus>) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let current = self.current();
        self.subscribers.push(sender);
        (current, receiver)
    }

    pub(crate) fn publish(&mut self, status: ControlStatus) {
        let changed = !status.same_reported_state(&self.current);
        self.current = status.clone();
        if changed {
            self.subscribers
                .retain(|subscriber| subscriber.send(status.clone()).is_ok());
        }
    }
}

#[derive(Serialize)]
struct StatusWire<'a> {
    phase: &'a str,
    session: Option<&'a str>,
    segment: Option<u32>,
    elapsed_ms: u64,
    markers: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PortalStatus {
    Checking,
    Available,
    Configuring,
    Enabled { triggers: Vec<String> },
    Unavailable { reason: String },
    Failed { reason: String },
}

impl PortalStatus {
    pub fn display_text(&self) -> String {
        match self {
            Self::Checking => "Checking global shortcut support…".into(),
            Self::Available => "Enable global hotkeys".into(),
            Self::Configuring => "Waiting for the desktop shortcut dialog…".into(),
            Self::Enabled { triggers } if !triggers.is_empty() => {
                format!("Global hotkeys enabled · {}", triggers.join(" · "))
            }
            Self::Enabled { .. } => "Global hotkeys enabled".into(),
            Self::Unavailable { .. } => portal_unavailable_message().into(),
            Self::Failed { reason } => format!("Global shortcut setup failed: {reason}"),
        }
    }

    fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

pub fn portal_unavailable_message() -> &'static str {
    PORTAL_UNAVAILABLE
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CtlCommand {
    ToggleRecord,
    Record,
    Stop,
    Cut,
    AddMarker(String),
    EndSession,
    Status { json: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CtlError {
    Rejected(String),
    Unavailable(String),
}

impl std::fmt::Display for CtlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(reason) => write!(f, "command rejected: {reason}"),
            Self::Unavailable(reason) => write!(f, "no running Chinstrap ({reason})"),
        }
    }
}

pub fn ctl_exit_code(error: &CtlError) -> i32 {
    match error {
        CtlError::Rejected(_) => 1,
        CtlError::Unavailable(_) => 2,
    }
}

pub fn parse_ctl(args: &[String]) -> Result<CtlCommand, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(
            "usage: chinstrap ctl toggle|record|stop|cut|marker [label]|end|status [--json]".into(),
        );
    };
    match command {
        "toggle" if args.len() == 1 => Ok(CtlCommand::ToggleRecord),
        "record" if args.len() == 1 => Ok(CtlCommand::Record),
        "stop" if args.len() == 1 => Ok(CtlCommand::Stop),
        "cut" if args.len() == 1 => Ok(CtlCommand::Cut),
        "end" if args.len() == 1 => Ok(CtlCommand::EndSession),
        "marker" => Ok(CtlCommand::AddMarker(args[1..].join(" "))),
        "status" if args.len() == 1 => Ok(CtlCommand::Status { json: false }),
        "status" if args.len() == 2 && args[1] == "--json" => Ok(CtlCommand::Status { json: true }),
        "toggle" | "record" | "stop" | "cut" | "end" | "status" => Err(
            "usage: chinstrap ctl toggle|record|stop|cut|marker [label]|end|status [--json]".into(),
        ),
        _ => Err(format!("unknown ctl command: {command}")),
    }
}

pub fn run_ctl(args: &[String]) -> i32 {
    run_ctl_at(BUS_NAME, args)
}

fn run_ctl_at(bus_name: &str, args: &[String]) -> i32 {
    let command = match parse_ctl(args) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("chinstrap ctl: {error}");
            return 2;
        }
    };
    match execute_ctl_at(bus_name, command) {
        Ok(Some(output)) => {
            println!("{output}");
            0
        }
        Ok(None) => 0,
        Err(error) => {
            eprintln!("chinstrap ctl: {error}");
            ctl_exit_code(&error)
        }
    }
}

fn execute_ctl_at(bus_name: &str, command: CtlCommand) -> Result<Option<String>, CtlError> {
    let connection =
        Connection::session().map_err(|error| CtlError::Unavailable(error.to_string()))?;
    let proxy = Proxy::new(&connection, bus_name, OBJECT_PATH, INTERFACE)
        .map_err(|error| CtlError::Unavailable(error.to_string()))?;
    match command {
        CtlCommand::ToggleRecord => call_no_reply(&proxy, "ToggleRecord", &()),
        CtlCommand::Record => call_no_reply(&proxy, "Record", &()),
        CtlCommand::Stop => call_no_reply(&proxy, "Stop", &()),
        CtlCommand::Cut => call_no_reply(&proxy, "Cut", &()),
        CtlCommand::AddMarker(label) => call_no_reply(&proxy, "AddMarker", &(label,)),
        CtlCommand::EndSession => call_no_reply(&proxy, "EndSession", &()),
        CtlCommand::Status { json } => {
            let status: String = proxy.call("Status", &()).map_err(classify_dbus_error)?;
            if json {
                Ok(Some(status))
            } else {
                let wire: StatusWireOwned = serde_json::from_str(&status).map_err(|error| {
                    CtlError::Unavailable(format!("invalid status reply: {error}"))
                })?;
                Ok(Some(format_status(&wire)))
            }
        }
    }
}

fn call_no_reply(
    proxy: &Proxy<'_>,
    method: &str,
    body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
) -> Result<Option<String>, CtlError> {
    proxy
        .call_method(method, body)
        .map(|_| None)
        .map_err(classify_dbus_error)
}

fn classify_dbus_error(error: zbus::Error) -> CtlError {
    match error {
        zbus::Error::MethodError(name, detail, _)
            if name.as_str() == "org.freedesktop.DBus.Error.Failed" =>
        {
            CtlError::Rejected(detail.unwrap_or_else(|| "engine rejected command".into()))
        }
        other => CtlError::Unavailable(other.to_string()),
    }
}

#[derive(Deserialize)]
struct StatusWireOwned {
    phase: String,
    session: Option<String>,
    segment: Option<u32>,
    elapsed_ms: u64,
    markers: u32,
}

fn format_status(status: &StatusWireOwned) -> String {
    format!(
        "phase={} session={} segment={} elapsed_ms={} markers={}",
        status.phase,
        status.session.as_deref().unwrap_or("-"),
        status
            .segment
            .map_or_else(|| "-".into(), |segment| segment.to_string()),
        status.elapsed_ms,
        status.markers
    )
}

pub enum Acquisition {
    Owner(ControlOwner),
    AlreadyRunning,
}

pub struct ControlOwner {
    connection: Connection,
    engine: Arc<RwLock<Option<EngineHandle>>>,
    portal: PortalHandle,
    portal_thread: Option<JoinHandle<()>>,
    raise_rx: async_channel::Receiver<()>,
    stop_status: Arc<AtomicBool>,
    status_thread: Option<JoinHandle<()>>,
}

impl ControlOwner {
    pub fn acquire() -> Result<Acquisition, String> {
        Self::acquire_inner(true)
    }

    pub fn acquire_for_selftest() -> Result<Acquisition, String> {
        Self::acquire_inner(false)
    }

    fn acquire_inner(raise_existing: bool) -> Result<Acquisition, String> {
        let connection = Connection::session()
            .map_err(|error| format!("cannot connect to the session bus: {error}"))?;
        let engine = Arc::new(RwLock::new(None));
        let (raise_tx, raise_rx) = async_channel::unbounded();
        let (portal, portal_rx) = PortalHandle::channel();
        let object = ControlObject {
            engine: engine.clone(),
            raise_tx,
            portal: portal.clone(),
        };
        connection
            .object_server()
            .at(OBJECT_PATH, object)
            .map_err(|error| format!("cannot export the D-Bus control object: {error}"))?;
        if let Err(error) = connection.request_name(BUS_NAME) {
            if matches!(error, zbus::Error::NameTaken) {
                drop(connection);
                if raise_existing {
                    ControlClient::raise_existing().map_err(|raise_error| {
                        format!("Chinstrap is already running, but raising its window failed: {raise_error}")
                    })?;
                }
                return Ok(Acquisition::AlreadyRunning);
            }
            return Err(format!("cannot own D-Bus name {BUS_NAME}: {error}"));
        }

        let portal_state = portal.state.clone();
        let portal_engine = engine.clone();
        let portal_requests = portal.tx.clone();
        let portal_thread = thread::Builder::new()
            .name("chinstrap-global-shortcuts".into())
            .spawn(move || {
                smol::block_on(portal_worker(
                    portal_rx,
                    portal_requests,
                    portal_engine,
                    portal_state,
                ))
            })
            .map_err(|error| format!("cannot start global-shortcuts worker: {error}"))?;

        Ok(Acquisition::Owner(Self {
            connection,
            engine,
            portal,
            portal_thread: Some(portal_thread),
            raise_rx,
            stop_status: Arc::new(AtomicBool::new(false)),
            status_thread: None,
        }))
    }

    pub fn attach_engine(&mut self, engine: EngineHandle) -> Result<(), String> {
        let (initial, updates) = engine.subscribe_control_status();
        *self
            .engine
            .write()
            .map_err(|_| "control engine state lock is poisoned".to_string())? = Some(engine);
        let connection = self.connection.clone();
        let stop = self.stop_status.clone();
        self.status_thread = Some(
            thread::Builder::new()
                .name("chinstrap-dbus-status".into())
                .spawn(move || publish_status_updates(connection, initial, updates, stop))
                .map_err(|error| format!("cannot start D-Bus status publisher: {error}"))?,
        );
        Ok(())
    }

    pub fn handle(&self) -> ControlHandle {
        ControlHandle {
            portal: self.portal.clone(),
            raise_rx: self.raise_rx.clone(),
        }
    }
}

#[cfg(test)]
fn publish_status_changes(connection: Connection, engine: EngineHandle, stop: Arc<AtomicBool>) {
    let (initial, updates) = engine.subscribe_control_status();
    publish_status_updates(connection, initial, updates, stop);
}

fn publish_status_updates(
    connection: Connection,
    initial: ControlStatus,
    updates: std::sync::mpsc::Receiver<ControlStatus>,
    stop: Arc<AtomicBool>,
) {
    forward_status_updates(initial, updates, stop, |json| {
        if let Err(error) =
            connection.emit_signal(None::<&str>, OBJECT_PATH, INTERFACE, "StatusChanged", &json)
        {
            eprintln!("chinstrap: cannot emit StatusChanged: {error}");
        }
    });
}

fn forward_status_updates(
    mut previous: ControlStatus,
    updates: std::sync::mpsc::Receiver<ControlStatus>,
    stop: Arc<AtomicBool>,
    mut emit: impl FnMut(&str),
) {
    while !stop.load(Ordering::Acquire) {
        match updates.recv_timeout(Duration::from_millis(50)) {
            Ok(status) => {
                if !status.same_reported_state(&previous) {
                    let json = status.to_json();
                    emit(&json);
                }
                previous = status;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

impl Drop for ControlOwner {
    fn drop(&mut self) {
        self.stop_status.store(true, Ordering::Release);
        if let Some(thread) = self.status_thread.take() {
            let _ = thread.join();
        }
        self.portal.shutdown();
        if self
            .portal_thread
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
            && let Some(thread) = self.portal_thread.take()
        {
            let _ = thread.join();
        }
    }
}

#[derive(Clone)]
pub struct ControlHandle {
    portal: PortalHandle,
    raise_rx: async_channel::Receiver<()>,
}

impl ControlHandle {
    pub fn portal_status(&self) -> PortalStatus {
        self.portal.status()
    }

    pub async fn enable_global_shortcuts(&self) -> Result<PortalStatus, String> {
        self.portal.enable().await
    }

    pub fn raise_events(&self) -> async_channel::Receiver<()> {
        self.raise_rx.clone()
    }
}

struct ControlObject {
    engine: Arc<RwLock<Option<EngineHandle>>>,
    raise_tx: async_channel::Sender<()>,
    portal: PortalHandle,
}

impl ControlObject {
    fn engine(&self) -> zbus::fdo::Result<EngineHandle> {
        self.engine
            .read()
            .map_err(|_| zbus::fdo::Error::Failed("engine state lock is poisoned".into()))?
            .clone()
            .ok_or_else(|| zbus::fdo::Error::Failed("Chinstrap is still starting".into()))
    }

    fn send(&self, command: EngineCommand) -> zbus::fdo::Result<()> {
        self.engine()?
            .request(command)
            .map_err(zbus::fdo::Error::Failed)
    }
}

#[zbus::interface(name = "tech.korora.Chinstrap.Control1")]
impl ControlObject {
    #[zbus(name = "ToggleRecord")]
    fn toggle_record(&self) -> zbus::fdo::Result<()> {
        self.send(EngineCommand::ToggleRecord)
    }

    #[zbus(name = "Record")]
    fn record(&self) -> zbus::fdo::Result<()> {
        self.send(EngineCommand::Record)
    }

    #[zbus(name = "Stop")]
    fn stop(&self) -> zbus::fdo::Result<()> {
        self.send(EngineCommand::Stop)
    }

    #[zbus(name = "Cut")]
    fn cut(&self) -> zbus::fdo::Result<()> {
        self.send(EngineCommand::Cut)
    }

    #[zbus(name = "AddMarker")]
    fn add_marker(&self, label: String) -> zbus::fdo::Result<()> {
        self.send(EngineCommand::AddMarker(label))
    }

    #[zbus(name = "EndSession")]
    fn end_session(&self) -> zbus::fdo::Result<()> {
        self.send(EngineCommand::EndSession)
    }

    #[zbus(name = "Status")]
    fn status(&self) -> String {
        self.engine
            .read()
            .expect("engine state lock")
            .as_ref()
            .map_or_else(
                || ControlStatus::default().to_json(),
                EngineHandle::control_status_json,
            )
    }

    #[zbus(name = "Raise")]
    fn raise(&self) -> zbus::fdo::Result<()> {
        self.raise_tx
            .send_blocking(())
            .map_err(|_| zbus::fdo::Error::Failed("GUI event loop is unavailable".into()))
    }

    #[zbus(name = "EnableGlobalShortcuts")]
    fn enable_global_shortcuts(&self) -> zbus::fdo::Result<String> {
        let status = smol::block_on(self.portal.enable()).map_err(zbus::fdo::Error::Failed)?;
        Ok(status.display_text())
    }

    #[zbus(signal)]
    async fn status_changed(emitter: &SignalEmitter<'_>, status_json: &str) -> zbus::Result<()>;
}

pub struct ControlClient;

impl ControlClient {
    fn raise_existing() -> Result<(), String> {
        let connection = Connection::session().map_err(|error| error.to_string())?;
        let proxy = Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE)
            .map_err(|error| error.to_string())?;
        proxy
            .call_method("Raise", &())
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone)]
struct PortalHandle {
    tx: async_channel::Sender<PortalRequest>,
    state: Arc<RwLock<PortalStatus>>,
}

enum PortalRequest {
    Enable(futures::channel::oneshot::Sender<Result<PortalStatus, String>>),
    ActivationEnded,
    Shutdown,
}

impl PortalHandle {
    fn channel() -> (Self, async_channel::Receiver<PortalRequest>) {
        let (tx, rx) = async_channel::unbounded();
        (
            Self {
                tx,
                state: Arc::new(RwLock::new(PortalStatus::Checking)),
            },
            rx,
        )
    }

    fn status(&self) -> PortalStatus {
        self.state.read().expect("portal status lock").clone()
    }

    async fn enable(&self) -> Result<PortalStatus, String> {
        let status = self.status();
        if status.is_unavailable() {
            return Err(portal_unavailable_message().into());
        }
        if matches!(status, PortalStatus::Enabled { .. }) {
            return Ok(status);
        }
        let (reply_tx, reply_rx) = futures::channel::oneshot::channel();
        self.tx
            .send(PortalRequest::Enable(reply_tx))
            .await
            .map_err(|_| "global-shortcuts worker is unavailable".to_string())?;
        reply_rx
            .await
            .map_err(|_| "global-shortcuts worker stopped before replying".to_string())?
    }

    fn shutdown(&self) {
        let _ = self.tx.try_send(PortalRequest::Shutdown);
    }
}

async fn portal_worker(
    requests: async_channel::Receiver<PortalRequest>,
    requests_tx: async_channel::Sender<PortalRequest>,
    engine: Arc<RwLock<Option<EngineHandle>>>,
    state: Arc<RwLock<PortalStatus>>,
) {
    let connection = match zbus::Connection::session().await {
        Ok(connection) => connection,
        Err(error) => {
            set_portal_status(
                &state,
                PortalStatus::Unavailable {
                    reason: error.to_string(),
                },
            );
            wait_unavailable(requests).await;
            return;
        }
    };
    let app_id = AppID::try_from(APP_ID).expect("static application ID is valid");
    if let Err(error) = register_host_app_with_connection(connection.clone(), app_id).await {
        set_portal_status(
            &state,
            PortalStatus::Unavailable {
                reason: error.to_string(),
            },
        );
        wait_unavailable(requests).await;
        return;
    }
    let api = match GlobalShortcuts::with_connection(connection.clone()).await {
        Ok(api) if api.version() >= 2 => api,
        Ok(api) => {
            set_portal_status(
                &state,
                PortalStatus::Unavailable {
                    reason: format!("GlobalShortcuts version {} is older than 2", api.version()),
                },
            );
            wait_unavailable(requests).await;
            return;
        }
        Err(error) => {
            set_portal_status(
                &state,
                PortalStatus::Unavailable {
                    reason: error.to_string(),
                },
            );
            wait_unavailable(requests).await;
            return;
        }
    };
    set_portal_status(&state, PortalStatus::Available);
    let mut controller = PortalController {
        api,
        engine,
        state,
        requests_tx,
        session: None,
        activation_task: None,
        activation_stop: None,
    };
    while let Ok(request) = requests.recv().await {
        match request {
            PortalRequest::Enable(reply) => {
                let result = controller.enable().await;
                let _ = reply.send(result);
            }
            PortalRequest::ActivationEnded => controller.activation_stream_ended().await,
            PortalRequest::Shutdown => break,
        }
    }
    controller.stop_activations().await;
    if let Some(session) = controller.session.take() {
        let _ = session.close().await;
    }
}

async fn wait_unavailable(requests: async_channel::Receiver<PortalRequest>) {
    while let Ok(request) = requests.recv().await {
        match request {
            PortalRequest::Enable(reply) => {
                let _ = reply.send(Err(portal_unavailable_message().into()));
            }
            PortalRequest::ActivationEnded => {}
            PortalRequest::Shutdown => break,
        }
    }
}

struct PortalController {
    api: GlobalShortcuts,
    engine: Arc<RwLock<Option<EngineHandle>>>,
    state: Arc<RwLock<PortalStatus>>,
    requests_tx: async_channel::Sender<PortalRequest>,
    session: Option<Session<GlobalShortcuts>>,
    activation_task: Option<smol::Task<()>>,
    activation_stop: Option<async_channel::Sender<()>>,
}

impl PortalController {
    async fn enable(&mut self) -> Result<PortalStatus, String> {
        if let Ok(state) = self.state.read()
            && let PortalStatus::Enabled { .. } = *state
        {
            return Ok(state.clone());
        }
        set_portal_status(&self.state, PortalStatus::Configuring);
        let session = self
            .api
            .create_session(CreateSessionOptions::default())
            .await
            .map_err(|error| self.fail(error.to_string()))?;
        let mut activated = match self.api.receive_activated().await {
            Ok(stream) => stream,
            Err(error) => {
                let _ = session.close().await;
                return Err(self.fail(error.to_string()));
            }
        };
        let shortcuts = [
            NewShortcut::new("toggle-record", "Toggle recording").preferred_trigger("CTRL+SHIFT+R"),
            NewShortcut::new("cut", "Cut recording segment").preferred_trigger("CTRL+SHIFT+C"),
            NewShortcut::new("add-marker", "Add marker").preferred_trigger("CTRL+SHIFT+M"),
            NewShortcut::new("stop", "Stop recording").preferred_trigger("CTRL+SHIFT+S"),
        ];
        let request = match self
            .api
            .bind_shortcuts(&session, &shortcuts, None, BindShortcutsOptions::default())
            .await
        {
            Ok(request) => request,
            Err(error) => {
                let _ = session.close().await;
                return Err(self.fail(error.to_string()));
            }
        };
        let bound = match request.response() {
            Ok(bound) => bound,
            Err(error) => {
                let _ = session.close().await;
                return Err(self.fail(error.to_string()));
            }
        };
        if bound.shortcuts().is_empty() {
            let _ = session.close().await;
            set_portal_status(&self.state, PortalStatus::Available);
            return Err("no shortcuts were bound (the desktop dialog was cancelled)".into());
        }
        let triggers = bound
            .shortcuts()
            .iter()
            .map(|shortcut| format!("{} {}", shortcut.id(), shortcut.trigger_description()))
            .collect::<Vec<_>>();
        let engine = self.engine.clone();
        let state = self.state.clone();
        let requests_tx = self.requests_tx.clone();
        let (stop_tx, stop_rx) = async_channel::bounded(1);
        self.activation_task = Some(smol::spawn(async move {
            let mut stream_ended = false;
            loop {
                futures::select! {
                    event = activated.next().fuse() => {
                        let Some(event) = event else {
                            stream_ended = true;
                            break;
                        };
                        let command = match event.shortcut_id() {
                            "toggle-record" => EngineCommand::ToggleRecord,
                            "cut" => EngineCommand::Cut,
                            "add-marker" => EngineCommand::AddMarker(String::new()),
                            "stop" => EngineCommand::Stop,
                            _ => continue,
                        };
                        let engine = engine
                            .read()
                            .expect("control engine state lock")
                            .clone();
                        if let Some(engine) = engine
                            && let Err(reason) = engine.request(command)
                        {
                            eprintln!("chinstrap: global shortcut rejected: {reason}");
                        }
                    }
                    _ = stop_rx.recv().fuse() => break,
                }
            }
            if stream_ended {
                notify_activation_stream_ended(&requests_tx, &state);
            }
        }));
        self.activation_stop = Some(stop_tx);
        self.session = Some(session);
        let status = PortalStatus::Enabled { triggers };
        set_portal_status(&self.state, status.clone());
        Ok(status)
    }

    async fn activation_stream_ended(&mut self) {
        self.stop_activations().await;
        if let Some(session) =
            take_session_after_activation_disconnect(&mut self.session, &self.state)
            && let Err(error) = session.close().await
        {
            eprintln!("chinstrap: cannot close disconnected GlobalShortcuts session: {error}");
        }
    }

    async fn stop_activations(&mut self) {
        if let Some(stop) = self.activation_stop.take() {
            let _ = stop.send(()).await;
        }
        if let Some(task) = self.activation_task.take() {
            task.await;
        }
    }

    fn fail(&self, reason: String) -> String {
        set_portal_status(
            &self.state,
            PortalStatus::Failed {
                reason: reason.clone(),
            },
        );
        reason
    }
}

fn set_portal_status(state: &RwLock<PortalStatus>, next: PortalStatus) {
    *state.write().expect("portal status lock") = next;
}

fn notify_activation_stream_ended(
    requests_tx: &async_channel::Sender<PortalRequest>,
    state: &RwLock<PortalStatus>,
) {
    // Enqueue cleanup before publishing Available; callers that observe Available can only enqueue
    // Enable after the session-retirement request.
    let mut portal_status = state.write().expect("portal status lock");
    let _ = requests_tx.try_send(PortalRequest::ActivationEnded);
    *portal_status = PortalStatus::Available;
}

fn take_session_after_activation_disconnect<T>(
    session: &mut Option<T>,
    state: &RwLock<PortalStatus>,
) -> Option<T> {
    let session = session.take();
    set_portal_status(state, PortalStatus::Available);
    session
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio::{self, EngineConfig, EngineHandle, FrameRateMode};

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    #[test]
    fn ctl_parses_control_commands_without_starting_a_gui() {
        assert_eq!(parse_ctl(&args(&["toggle"])), Ok(CtlCommand::ToggleRecord));
        assert_eq!(parse_ctl(&args(&["record"])), Ok(CtlCommand::Record));
        assert_eq!(parse_ctl(&args(&["stop"])), Ok(CtlCommand::Stop));
        assert_eq!(parse_ctl(&args(&["cut"])), Ok(CtlCommand::Cut));
        assert_eq!(parse_ctl(&args(&["end"])), Ok(CtlCommand::EndSession));
        assert_eq!(
            parse_ctl(&args(&["marker"])),
            Ok(CtlCommand::AddMarker(String::new()))
        );
        assert_eq!(
            parse_ctl(&args(&["marker", "Chapter 1"])),
            Ok(CtlCommand::AddMarker("Chapter 1".into()))
        );
        assert_eq!(
            parse_ctl(&args(&["status", "--json"])),
            Ok(CtlCommand::Status { json: true })
        );
    }

    #[test]
    fn ctl_classifies_engine_rejection_and_missing_service_exit_codes() {
        assert_eq!(
            ctl_exit_code(&CtlError::Rejected("not recording".into())),
            1
        );
        assert_eq!(
            ctl_exit_code(&CtlError::Unavailable("no session bus".into())),
            2
        );
    }

    #[test]
    fn pending_record_is_reported_as_starting() {
        let snapshot = Snapshot {
            capabilities: Default::default(),
            cameras: vec![],
            microphones: vec![],
            sources: vec![],
            selected_camera: None,
            selected_microphone: None,
            output_root: std::path::PathBuf::new(),
            phase: Phase::Idle,
            portal_busy: false,
            session: None,
            can_record: false,
            transcription: crate::studio::TranscriptionInfo {
                model: crate::studio::ModelState::Checking,
                auto: true,
                backend: None,
                vulkan_broken: false,
                queued: 0,
                other_sessions: None,
            },
            frame_rate_default: FrameRateMode::Native,
            selected_camera_mode: None,
            capacity: Default::default(),
            storage: Default::default(),
            recoveries: vec![],
            vocabulary: Default::default(),
            previews: vec![],
            record_pending: true,
        };

        let status = ControlStatus::from_snapshot(&snapshot, 0);
        assert_eq!(status.phase, "starting");
        assert_eq!(status.segment, Some(1));
    }

    #[test]
    fn status_is_one_line_json_with_the_complete_control_state() {
        let status = ControlStatus {
            phase: "recording".into(),
            session: Some("2026-10-27T12-00-00".into()),
            segment: Some(2),
            elapsed_ms: 1_250,
            markers: 3,
            recording_started_at: None,
        };
        assert_eq!(
            status.to_json(),
            r#"{"phase":"recording","session":"2026-10-27T12-00-00","segment":2,"elapsed_ms":1250,"markers":3}"#
        );
    }

    fn start_fake_engine() -> (
        EngineHandle,
        async_channel::Receiver<crate::studio::EngineEvent>,
        std::path::PathBuf,
    ) {
        let output_root = std::env::temp_dir().join(format!(
            "chinstrap-control-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after UNIX epoch")
                .as_nanos()
        ));
        let config = EngineConfig {
            output_root: output_root.clone(),
            preferred_encoder: None,
            preferred_convert: None,
            enable_portal: false,
            fake: true,
            frame_rate: FrameRateMode::Native,
            test_sources: Vec::new(),
            preview: false,
            selftest_free_bytes: None,
            selftest_min_free_mb: None,
        };
        let (engine, events) = studio::start(config).expect("start fake engine");
        (engine, events, output_root)
    }

    fn wait_for_phase(engine: &EngineHandle, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if engine.control_status().phase == expected {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for phase {expected}");
    }

    #[test]
    fn exported_control_methods_wait_for_engine_results_and_publish_status() {
        let (engine, _events, output_root) = start_fake_engine();
        let (raise_tx, _raise_rx) = async_channel::unbounded();
        let (portal, _portal_rx) = PortalHandle::channel();
        let object = ControlObject {
            engine: Arc::new(RwLock::new(Some(engine.clone()))),
            raise_tx,
            portal,
        };

        let error = object.cut().expect_err("cut is rejected while idle");
        assert!(error.to_string().contains("not recording"));
        object.record().expect("record is accepted");
        wait_for_phase(&engine, "recording");
        object
            .add_marker("opening".into())
            .expect("marker is accepted while recording");
        assert_eq!(engine.control_status().markers, 1);
        let status: serde_json::Value = serde_json::from_str(&object.status()).unwrap();
        assert_eq!(status["phase"], "recording");
        assert_eq!(status["markers"], 1);

        object.cut().expect("cut is accepted while recording");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let status = engine.control_status();
            if status.phase == "recording" && status.segment == Some(2) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(engine.control_status().segment, Some(2));
        object.stop().expect("stop is accepted while recording");
        wait_for_phase(&engine, "idle");
        object.end_session().expect("end closes the session");
        let status: serde_json::Value = serde_json::from_str(&object.status()).unwrap();
        assert_eq!(status["phase"], "idle");
        assert_eq!(status["markers"], 0);

        object.toggle_record().expect("toggle starts a new session");
        wait_for_phase(&engine, "recording");
        object.stop().expect("stop is accepted");
        wait_for_phase(&engine, "idle");
        assert!(engine.shutdown_blocking(Duration::from_secs(2)));
        let _ = std::fs::remove_dir_all(output_root);
    }

    #[test]
    fn ctl_client_uses_dbus_results_status_and_phase_signals() {
        let connection = match Connection::session() {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("skipped D-Bus integration test: no session bus ({error})");
                return;
            }
        };
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after UNIX epoch")
            .as_nanos();
        let bus_name = format!("org.korora.ChinstrapTest.p{}.t{unique}", std::process::id());
        let (engine, _events, output_root) = start_fake_engine();
        let (raise_tx, _raise_rx) = async_channel::unbounded();
        let (portal, _portal_rx) = PortalHandle::channel();
        let object = ControlObject {
            engine: Arc::new(RwLock::new(Some(engine.clone()))),
            raise_tx,
            portal,
        };
        connection
            .object_server()
            .at(OBJECT_PATH, object)
            .expect("export control object");
        match connection.request_name(bus_name.as_str()) {
            Ok(()) => {}
            Err(zbus::Error::NameTaken) => {
                let _ = engine.shutdown_blocking(Duration::from_secs(2));
                eprintln!("skipped D-Bus integration test: unique name is already owned");
                return;
            }
            Err(error) => panic!("request test D-Bus name: {error}"),
        }

        let stop = Arc::new(AtomicBool::new(false));
        let status_thread = {
            let connection = connection.clone();
            let engine = engine.clone();
            let stop = stop.clone();
            thread::spawn(move || publish_status_changes(connection, engine, stop))
        };

        let (signal_tx, signal_rx) = std::sync::mpsc::channel();
        let signal_name = bus_name.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let signal_thread = thread::spawn(move || {
            let connection = Connection::session().expect("connect signal client");
            let proxy = Proxy::new(&connection, signal_name, OBJECT_PATH, INTERFACE)
                .expect("create signal proxy");
            let mut signals = proxy
                .receive_signal("StatusChanged")
                .expect("subscribe to signal");
            ready_tx.send(()).unwrap();
            if let Some(message) = signals.next() {
                let status: String = message.body().deserialize().expect("decode signal status");
                let _ = signal_tx.send(status);
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("signal subscription ready");

        assert_eq!(run_ctl_at(&bus_name, &args(&["cut"])), 1);
        assert_eq!(run_ctl_at(&bus_name, &args(&["record"])), 0);
        let signal: serde_json::Value = serde_json::from_str(
            &signal_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("phase change signal"),
        )
        .expect("valid status signal JSON");
        assert!(signal["phase"] == "starting" || signal["phase"] == "recording");
        wait_for_phase(&engine, "recording");

        assert_eq!(run_ctl_at(&bus_name, &args(&["marker", "opening"])), 0);
        let status = execute_ctl_at(&bus_name, CtlCommand::Status { json: true })
            .expect("status call")
            .expect("JSON status output");
        let status: serde_json::Value = serde_json::from_str(&status).expect("status JSON");
        assert!(status["session"].is_string());
        assert_eq!(status["segment"], 1);
        assert!(status["elapsed_ms"].is_u64());
        assert_eq!(status["markers"], 1);
        assert_eq!(run_ctl_at(&bus_name, &args(&["cut"])), 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if engine.control_status().phase == "recording"
                && engine.control_status().segment == Some(2)
            {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(engine.control_status().segment, Some(2));
        assert_eq!(run_ctl_at(&bus_name, &args(&["stop"])), 0);
        wait_for_phase(&engine, "idle");
        assert_eq!(run_ctl_at(&bus_name, &args(&["end"])), 0);
        assert_eq!(run_ctl_at(&bus_name, &args(&["status", "--json"])), 0);
        let absent_name = format!(
            "org.korora.ChinstrapAbsent.p{}.t{unique}",
            std::process::id()
        );
        assert_eq!(run_ctl_at(&absent_name, &args(&["status"])), 2);

        stop.store(true, Ordering::Release);
        status_thread.join().expect("join status publisher");
        signal_thread.join().expect("join signal listener");
        assert!(engine.shutdown_blocking(Duration::from_secs(2)));
        drop(connection);
        let _ = std::fs::remove_dir_all(output_root);
    }

    #[test]
    fn status_changed_emits_each_rapid_control_state_update() {
        let connection = match Connection::session() {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("skipped D-Bus integration test: no session bus ({error})");
                return;
            }
        };
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after UNIX epoch")
            .as_nanos();
        let bus_name = format!(
            "org.korora.ChinstrapStatusTest.p{}.t{unique}",
            std::process::id()
        );
        let (engine, _events, output_root) = start_fake_engine();
        let (raise_tx, _raise_rx) = async_channel::unbounded();
        let (portal, _portal_rx) = PortalHandle::channel();
        let object = ControlObject {
            engine: Arc::new(RwLock::new(Some(engine.clone()))),
            raise_tx,
            portal,
        };
        connection
            .object_server()
            .at(OBJECT_PATH, object)
            .expect("export control object");
        connection
            .request_name(bus_name.as_str())
            .expect("own unique test D-Bus name");

        let (signal_tx, signal_rx) = std::sync::mpsc::channel();
        let signal_name = bus_name.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let signal_thread = thread::spawn(move || {
            let connection = Connection::session().expect("connect signal client");
            let proxy = Proxy::new(&connection, signal_name, OBJECT_PATH, INTERFACE)
                .expect("create signal proxy");
            let signals = proxy
                .receive_signal("StatusChanged")
                .expect("subscribe to signal");
            ready_tx.send(()).unwrap();
            for message in signals {
                let status: String = message.body().deserialize().expect("decode signal status");
                let terminal: serde_json::Value =
                    serde_json::from_str(&status).expect("valid status signal JSON");
                let done = terminal["phase"] == "idle"
                    && terminal["session"].is_null()
                    && terminal["segment"].is_null()
                    && terminal["markers"] == 0;
                signal_tx.send(status).unwrap();
                if done {
                    break;
                }
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("signal subscription ready");

        let stop = Arc::new(AtomicBool::new(false));
        let status_thread = {
            let connection = connection.clone();
            let engine = engine.clone();
            let stop = stop.clone();
            thread::spawn(move || publish_status_changes(connection, engine, stop))
        };
        engine
            .request(EngineCommand::Record)
            .expect("record accepted");
        wait_for_phase(&engine, "recording");

        let mut statuses = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let status = signal_rx
                .recv_timeout(Duration::from_millis(500))
                .expect("recording phase signal");
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            statuses.push(value.clone());
            if value["phase"] == "recording" {
                break;
            }
        }
        assert!(
            statuses.iter().any(|status| status["phase"] == "recording"),
            "publisher should report the recording phase"
        );

        const RAPID_MARKERS: u32 = 20;
        for index in 1..=RAPID_MARKERS {
            engine
                .request(EngineCommand::AddMarker(format!("marker-{index}")))
                .expect("marker accepted while recording");
        }
        assert_eq!(engine.control_status().markers, RAPID_MARKERS);
        engine.request(EngineCommand::Stop).expect("stop accepted");
        wait_for_phase(&engine, "idle");
        engine
            .request(EngineCommand::EndSession)
            .expect("end session accepted");

        let terminal_deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < terminal_deadline {
            let status = signal_rx
                .recv_timeout(Duration::from_millis(500))
                .expect("terminal status signal");
            let value: serde_json::Value = serde_json::from_str(&status).unwrap();
            let done = value["phase"] == "idle"
                && value["session"].is_null()
                && value["segment"].is_null()
                && value["markers"] == 0;
            statuses.push(value);
            if done {
                break;
            }
        }
        stop.store(true, Ordering::Release);
        status_thread.join().expect("join status publisher");
        signal_thread.join().expect("join signal listener");

        let seen_markers: Vec<u64> = statuses
            .iter()
            .filter_map(|status| status["markers"].as_u64())
            .collect();
        for count in 1..=u64::from(RAPID_MARKERS) {
            assert!(
                seen_markers.contains(&count),
                "StatusChanged omitted marker count {count}; saw {seen_markers:?}"
            );
        }
        assert!(engine.shutdown_blocking(Duration::from_secs(2)));
        drop(connection);
        let _ = std::fs::remove_dir_all(output_root);
    }

    #[test]
    fn status_signal_stream_preserves_every_rapid_phase_transition() {
        let connection = match Connection::session() {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("skipped D-Bus integration test: no session bus ({error})");
                return;
            }
        };
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after UNIX epoch")
            .as_nanos();
        let bus_name = format!(
            "org.korora.ChinstrapPhaseTest.p{}.t{unique}",
            std::process::id()
        );
        connection
            .request_name(bus_name.as_str())
            .expect("own unique test D-Bus name");

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (signals_tx, signals_rx) = std::sync::mpsc::channel::<Vec<String>>();
        let signal_name = bus_name.clone();
        let signal_thread = thread::spawn(move || {
            let connection = Connection::session().expect("connect signal client");
            let proxy = Proxy::new(&connection, signal_name, OBJECT_PATH, INTERFACE)
                .expect("create signal proxy");
            let mut signals = proxy
                .receive_signal("StatusChanged")
                .expect("subscribe to signal");
            ready_tx.send(()).unwrap();
            let mut emitted = Vec::new();
            for _ in 0..7 {
                let message = signals.next().expect("phase StatusChanged signal");
                let json: String = message.body().deserialize().expect("decode signal status");
                emitted.push(json);
            }
            signals_tx.send(emitted).unwrap();
        });
        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("signal subscription ready");

        let mut hub = ControlStatusHub::default();
        let (initial, updates) = hub.subscribe();
        let publisher_connection = connection.clone();
        let publisher = thread::spawn(move || {
            publish_status_updates(
                publisher_connection,
                initial,
                updates,
                Arc::new(AtomicBool::new(false)),
            );
        });
        let started_at = Instant::now();
        let transitions = [
            (Phase::Starting { segment: 1 }, Some(1)),
            (
                Phase::Recording {
                    segment: 1,
                    started_at,
                },
                Some(1),
            ),
            (Phase::Cutting { from: 1 }, Some(1)),
            (Phase::Starting { segment: 2 }, Some(2)),
            (
                Phase::Recording {
                    segment: 2,
                    started_at,
                },
                Some(2),
            ),
            (Phase::Stopping { segment: 2 }, Some(2)),
            (Phase::Idle, Some(2)),
        ];
        for (phase, last_segment) in transitions {
            hub.publish(ControlStatus::from_state(
                &phase,
                false,
                Some(("session", last_segment)),
                0,
            ));
        }
        drop(hub);
        publisher.join().expect("join status publisher");
        let signals = signals_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("collect all phase signals");
        signal_thread.join().expect("join signal listener");

        let emitted: Vec<(String, Option<u64>)> = signals
            .into_iter()
            .map(|json| {
                let status: serde_json::Value = serde_json::from_str(&json).unwrap();
                (
                    status["phase"].as_str().unwrap().to_string(),
                    status["segment"].as_u64(),
                )
            })
            .collect();
        assert_eq!(
            emitted,
            vec![
                ("starting".into(), Some(1)),
                ("recording".into(), Some(1)),
                ("cutting".into(), Some(1)),
                ("starting".into(), Some(2)),
                ("recording".into(), Some(2)),
                ("stopping".into(), Some(2)),
                ("idle".into(), Some(2)),
            ],
            "each phase transition must produce a StatusChanged signal"
        );
    }

    #[test]
    fn portal_worker_reports_the_graceful_unavailable_message() {
        let (portal, requests) = PortalHandle::channel();
        let worker = smol::spawn(wait_unavailable(requests));
        let error = smol::block_on(portal.enable()).expect_err("portal is unavailable");
        assert_eq!(error, portal_unavailable_message());
        portal.shutdown();
        smol::block_on(worker);
    }

    #[test]
    fn unavailable_portal_message_points_to_ctl_and_compositor_keybindings() {
        assert_eq!(
            portal_unavailable_message(),
            "Not available on this desktop. Use `chinstrap ctl` with a compositor keybinding."
        );
        assert_eq!(
            PortalStatus::Unavailable {
                reason: "not present".into()
            }
            .display_text(),
            portal_unavailable_message()
        );
    }

    #[test]
    fn activation_stream_disconnect_releases_session_and_allows_reenable() {
        let (portal, requests) = PortalHandle::channel();
        *portal.state.write().expect("portal status lock") = PortalStatus::Enabled {
            triggers: vec!["toggle-record CTRL+SHIFT+R".into()],
        };
        let mut session = Some("stale portal session");

        notify_activation_stream_ended(&portal.tx, &portal.state);
        assert_eq!(
            portal.status(),
            PortalStatus::Available,
            "disconnect must return global shortcuts to an enableable state"
        );
        let (released, reenabled) = smol::block_on(async {
            assert!(matches!(
                requests.recv().await.expect("session retirement request"),
                PortalRequest::ActivationEnded
            ));
            let released = take_session_after_activation_disconnect(&mut session, &portal.state);
            let enable_portal = portal.clone();
            let enable_task = smol::spawn(async move { enable_portal.enable().await });
            let PortalRequest::Enable(reply) = requests.recv().await.expect("enable request")
            else {
                panic!("enable must be queued after session retirement");
            };
            let enabled = PortalStatus::Enabled {
                triggers: vec!["toggle-record CTRL+SHIFT+R".into()],
            };
            reply
                .send(Ok(enabled.clone()))
                .expect("reply to enable request");
            let reenabled = enable_task.await.expect("shortcut enable result");
            (released, reenabled)
        });

        assert_eq!(released, Some("stale portal session"));
        assert!(
            session.is_none(),
            "disconnected session must not be retained"
        );
        assert_eq!(
            reenabled,
            PortalStatus::Enabled {
                triggers: vec!["toggle-record CTRL+SHIFT+R".into()]
            }
        );
    }
}
