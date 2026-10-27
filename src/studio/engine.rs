//! Engine public surface (SPEC §3.2) and the real engine (§4-§6).
//!
//! The engine thread owns every `gst::Pipeline`, the device monitor, the session/manifest and
//! the state machine. It never blocks on GStreamer (D10): state changes and EOS sends run on
//! short-lived worker threads that post `Msg::Worker`, and every wait is a deadline checked by
//! the 50 ms poll loop. `CHINSTRAP_FAKE_ENGINE=1` runs the fake state machine in `fake.rs`.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

use super::devices::{self, CameraClass, CameraPath, Devices};
use super::encoders::{self, Role};
use super::fake;
use super::plan::{self, Demand, Plan};
use super::portal::{self, PortalCmd, PortalOutcome};
use super::preview::{self, Previews, Tap};
use super::recorder::{self, InputPipeline, Rate, VideoSource};
use super::session::{self, Manifest, ManifestInput, ManifestQuality, ManifestSegment};
use super::stamp::{self, Counters, SizeSlot, UNSET};
use super::storage;
use super::transcribe::control::{JobKey, ModelEvent, SegView, Transcription, TxConfig};
use super::transcribe::vocab;
use super::transcribe::worker::WorkerEvent;
use super::types::*;

#[derive(Clone, Debug)]
pub struct EngineConfig {
    pub output_root: PathBuf, // always absolute (std::path::absolute)
    pub preferred_encoder: Option<VideoEncoder>, // CHINSTRAP_H264_ENCODER
    pub preferred_convert: Option<GpuConvert>, // CHINSTRAP_GPU_CONVERT=va|gl
    pub enable_portal: bool,  // false in --selftest
    pub fake: bool,           // CHINSTRAP_FAKE_ENGINE=1
    /// Frame-rate mode new window/screen sources get (CHINSTRAP_FRAME_RATE=native|60|30).
    pub frame_rate: FrameRateMode,
    /// Selftest only: behaviour of the test pattern with this source id.
    pub test_sources: Vec<(u32, recorder::TestBehaviour)>,
    /// Live previews of the camera and sources (`CHINSTRAP_PREVIEW=0` turns them off).
    pub preview: bool,
    /// Injected filesystem free bytes for `--selftest` scenarios; normal runs leave this unset.
    pub selftest_free_bytes: Option<u64>,
    /// Injected `CHINSTRAP_MIN_FREE_MB` for low-space self-tests only.
    pub selftest_min_free_mb: Option<u64>,
}

/// `CHINSTRAP_FRAME_RATE`: native (default), 60, 30.
pub fn parse_frame_rate(v: Option<&str>) -> FrameRateMode {
    match v.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("60") => FrameRateMode::Cfr60,
        Some("30") => FrameRateMode::Cfr30,
        _ => FrameRateMode::Native,
    }
}

impl EngineConfig {
    /// output_root: CHINSTRAP_OUTPUT_DIR, else gst::glib::user_special_dir(UserDirectory::Videos)/Chinstrap,
    /// else $HOME/Videos/Chinstrap; absolutized.
    pub fn from_env() -> Self {
        let non_empty = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty());
        let root = non_empty("CHINSTRAP_OUTPUT_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                gst::glib::user_special_dir(gst::glib::UserDirectory::Videos)
                    .map(|videos| videos.join("Chinstrap"))
            })
            .unwrap_or_else(|| {
                let home = non_empty("HOME").map(PathBuf::from).unwrap_or_default();
                home.join("Videos").join("Chinstrap")
            });
        let output_root = std::path::absolute(&root).unwrap_or(root);

        let preferred_encoder = std::env::var("CHINSTRAP_H264_ENCODER")
            .ok()
            .and_then(|name| VideoEncoder::from_element_name(name.trim()));
        let preferred_convert = std::env::var("CHINSTRAP_GPU_CONVERT").ok().and_then(|v| {
            match v.trim().to_ascii_lowercase().as_str() {
                "va" => Some(GpuConvert::Va),
                "gl" => Some(GpuConvert::Gl),
                _ => None,
            }
        });
        let fake = std::env::var("CHINSTRAP_FAKE_ENGINE").is_ok_and(|v| v.trim() == "1");
        let frame_rate = parse_frame_rate(std::env::var("CHINSTRAP_FRAME_RATE").ok().as_deref());

        Self {
            output_root,
            preferred_encoder,
            preferred_convert,
            enable_portal: true,
            fake,
            frame_rate,
            test_sources: Vec::new(),
            preview: preview::enabled_from_env(),
            selftest_free_bytes: None,
            selftest_min_free_mb: None,
        }
    }
}

#[derive(Debug)]
pub enum EngineError {
    GstInit(String),
    Spawn(std::io::Error),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GstInit(e) => write!(f, "failed to initialise GStreamer: {e}"),
            Self::Spawn(e) => write!(f, "failed to spawn engine thread: {e}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// Result of one worker-thread `set_state`.
pub(super) struct WorkerResult {
    pub id: u64,
    pub target: gst::State,
    pub result: Result<(), String>,
}

/// Messages to the engine thread (SPEC §3.2).
pub(super) enum Msg {
    Cmd(EngineCommand),
    Portal(PortalOutcome),
    Worker(WorkerResult),
    /// Injectable storage failure at the engine event boundary; used only by the self-test.
    SelftestNoSpace {
        key: InputKey,
    },
    /// Injectable filesystem free-space value; only used by self-tests.
    SelftestFreeBytes {
        bytes: u64,
    },
    /// Transcription worker thread (§12.3).
    Transcribe(WorkerEvent),
    /// Model check/download thread (§12.5).
    Model(ModelEvent),
    RecoveryDone {
        name: String,
        dir: PathBuf,
        result: Result<super::recovery::RecoveryResult, String>,
    },
    Shutdown {
        deadline: Instant,
        /// Receives true when every active segment was finalized before the deadline.
        ack: SyncSender<bool>,
    },
}

struct Inner {
    tx: mpsc::Sender<Msg>,
    previews: Previews,
    done: Mutex<Option<bool>>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Do not rely on tx disconnect: other threads hold Sender clones. Never wait here.
        let (ack, _ack_rx) = mpsc::sync_channel::<bool>(1);
        let _ = self.tx.send(Msg::Shutdown {
            deadline: Instant::now() + Duration::from_secs(5),
            ack,
        });
    }
}

#[derive(Clone)]
pub struct EngineHandle {
    inner: Arc<Inner>,
}

impl EngineHandle {
    /// Never blocks. Logs to stderr if the engine thread is gone.
    pub fn send(&self, cmd: EngineCommand) {
        if let Err(mpsc::SendError(Msg::Cmd(cmd))) = self.inner.tx.send(Msg::Cmd(cmd)) {
            eprintln!("chinstrap: engine thread is gone; dropped command {cmd:?}");
        }
    }

    /// Injects an ENOSPC event at the real engine failure handler, for the self-test only.
    pub(crate) fn inject_storage_failure_for_selftest(&self, key: InputKey) {
        let _ = self.inner.tx.send(Msg::SelftestNoSpace { key });
    }

    /// Replaces the free-space value observed by the storage monitor in self-tests only.
    pub(crate) fn set_free_bytes_for_selftest(&self, bytes: u64) {
        let _ = self.inner.tx.send(Msg::SelftestFreeBytes { bytes });
    }

    /// The newest preview frame of every previewed input (the camera and each source).
    pub fn previews(&self) -> Previews {
        self.inner.previews.clone()
    }

    /// Finalizes any active segment, closes portal sessions, then joins the thread only if it acked.
    /// Idempotent and bounded by `timeout` on every path; returns false on timeout (files may lack
    /// Cues/Duration; manifest may still say "recording"; thread left detached).
    pub fn shutdown_blocking(&self, timeout: Duration) -> bool {
        let mut done = self.inner.done.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(result) = *done {
            return result;
        }
        let (ack, ack_rx) = mpsc::sync_channel(1);
        let deadline = Instant::now() + timeout.saturating_sub(Duration::from_millis(250));
        // Ok(false): the engine acked but ran out of time, so some file may lack its index.
        let acked = match self.inner.tx.send(Msg::Shutdown { deadline, ack }) {
            Ok(()) => ack_rx.recv_timeout(timeout).ok(),
            // The thread exited without acknowledging a shutdown (e.g. it panicked).
            Err(_) => None,
        };
        let result = acked == Some(true);
        let mut join = self.inner.join.lock().unwrap_or_else(|e| e.into_inner());
        // Join only after an ack, or when the thread has already finished (join cannot block).
        if (acked.is_some() || join.as_ref().is_some_and(|h| h.is_finished()))
            && let Some(handle) = join.take()
        {
            let _ = handle.join();
        }
        *done = Some(result);
        result
    }
}

/// Is `msg` one of the criticals PipeWire's GStreamer plugin (1.6.x, gstpipewireformat.c
/// `handle_fraction_prop`) logs for every format whose frame-rate "range" has min == max (169
/// per device scan here, also from a plain `gst-device-monitor-1.0`)? GStreamer then keeps the
/// fixed rate; nothing is lost.
fn is_pipewire_fraction_critical(msg: &str) -> bool {
    msg.contains("gst_value_collect_fraction_range: assertion")
        || msg.contains("range start is not smaller than end for `GstFractionRange'")
}

/// Hides [`is_pipewire_fraction_critical`] messages (one summary line instead) so real errors
/// stay visible; every other GStreamer message goes to GLib's default handler.
fn quiet_pipewire_fraction_criticals() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let seen = AtomicBool::new(false);
        gst::glib::log_set_handler(
            Some("GStreamer"),
            gst::glib::LogLevels::LEVEL_CRITICAL,
            false,
            false,
            move |domain, level, msg| {
                if is_pipewire_fraction_critical(msg) {
                    if !seen.swap(true, Ordering::AcqRel) {
                        eprintln!(
                            "chinstrap: ignoring GStreamer FractionRange criticals from the PipeWire \
                             plugin (a fixed frame rate sent as a range; harmless)"
                        );
                    }
                    return;
                }
                gst::glib::log_default_handler(domain, level, Some(msg));
            },
        );
    });
}

/// Calls gst::init() (idempotent), spawns "chinstrap-engine" (and, if enabled, "chinstrap-portal").
pub fn start(
    config: EngineConfig,
) -> Result<(EngineHandle, async_channel::Receiver<EngineEvent>), EngineError> {
    gst::init().map_err(|e| EngineError::GstInit(e.to_string()))?;
    quiet_pipewire_fraction_criticals();
    let (events_tx, events_rx) = async_channel::unbounded();
    let (tx, rx) = mpsc::channel();
    let previews = Previews::default();
    let hub = previews.clone();
    let join = if config.fake {
        fake::spawn(config, rx, events_tx, hub).map_err(EngineError::Spawn)?
    } else {
        let portal = if config.enable_portal {
            Some(portal::spawn(tx.clone()).map_err(EngineError::Spawn)?)
        } else {
            None
        };
        let engine_tx = tx.clone();
        std::thread::Builder::new()
            .name("chinstrap-engine".into())
            .spawn(move || Engine::new(config, engine_tx, events_tx, portal, hub).run(rx))
            .map_err(EngineError::Spawn)?
    };
    let inner = Arc::new(Inner {
        tx,
        previews,
        done: Mutex::new(None),
        join: Mutex::new(Some(join)),
    });
    Ok((EngineHandle { inner }, events_rx))
}

// ---------------------------------------------------------------------------------------------
// Real engine
// ---------------------------------------------------------------------------------------------

const TICK: Duration = Duration::from_millis(50);
const START_TIMEOUT: Duration = Duration::from_secs(3);
const BASE_MARGIN: Duration = Duration::from_millis(100);
const EARLY_ERROR: Duration = Duration::from_secs(3);
const EOS_GRACE: Duration = Duration::from_secs(1);
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(5);
const NULL_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const SIZE_EVERY: Duration = Duration::from_secs(1);
const MANIFEST_RETRY: Duration = Duration::from_secs(1);
const LEVEL_EVERY: Duration = Duration::from_millis(100);
/// Why a command is refused while a Record waits for the idle previews to stop.
const STARTING: &str = "a recording is starting";
/// A recording mic whose newest muxed buffer is this far behind the clock has stopped (§5.0).
const MIC_STALL: Duration = Duration::from_secs(3);
const NOT_FINALIZED: &str = "not finalized: file may lack index";
/// Native rate assumed for a source whose maximum is unknown.
const DEFAULT_NATIVE_FPS: f64 = 60.0;
/// Re-read the vocabulary files this often (edits made outside the app).
const VOCAB_EVERY: Duration = Duration::from_secs(5);
const NO_FRAMES: &str = "no frames received";
/// A preview that failed is retried after this long.
const PREVIEW_RETRY: Duration = Duration::from_secs(5);
/// An idle preview not Playing by then is given up (and retried).
const PREVIEW_START: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq)]
enum Stage {
    /// Worker → Paused in flight.
    Pausing,
    /// Paused; waiting for the other inputs before B is chosen.
    Paused,
    /// Worker → Playing in flight.
    Starting,
    Running {
        since: Instant,
    },
    /// EOS sent; waiting for it on the bus.
    Draining {
        inject_at: Option<Instant>,
        deadline: Instant,
    },
    /// Worker → Null in flight.
    Nulling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StorageFinalizeAction {
    GracefulEos,
    InjectEos,
    AwaitState,
    AlreadyStopping,
}

fn storage_finalize_action(stage: &Stage) -> StorageFinalizeAction {
    match stage {
        Stage::Running { .. } => StorageFinalizeAction::GracefulEos,
        Stage::Paused => StorageFinalizeAction::InjectEos,
        Stage::Pausing | Stage::Starting => StorageFinalizeAction::AwaitState,
        Stage::Draining { .. } | Stage::Nulling => StorageFinalizeAction::AlreadyStopping,
    }
}

struct CheckpointWriteState {
    last_success: Instant,
    pending: bool,
}

impl CheckpointWriteState {
    fn new(now: Instant) -> Self {
        Self {
            last_success: now,
            pending: false,
        }
    }

    fn request_if_due(&mut self, recording: bool, now: Instant, manifest_dirty: &mut bool) -> bool {
        if session::checkpoint_due(recording, self.last_success, now) {
            self.pending = true;
            *manifest_dirty = true;
            true
        } else {
            false
        }
    }

    fn write_result(&mut self, succeeded: bool, now: Instant, manifest_dirty: &mut bool) {
        if succeeded {
            *manifest_dirty = false;
            if self.pending {
                self.last_success = now;
                self.pending = false;
            }
        } else {
            // Keep the manifest dirty and the checkpoint due until an atomic write succeeds.
            *manifest_dirty = true;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Outcome {
    Complete,
    Failed(String),
    /// Failed before useful output; remove the file only if it contains no data.
    Discard(String),
}

fn storage_stop_outcome(outcome: Option<Outcome>) -> Option<Outcome> {
    match outcome {
        None | Some(Outcome::Discard(_)) => Some(Outcome::Failed(
            "storage stop; partial file preserved".into(),
        )),
        other => other,
    }
}

fn discard_recovery_reason(path: Option<&Path>, encoded: u64, reason: &str) -> Option<String> {
    let bytes = path
        .and_then(|path| std::fs::metadata(path).ok())
        .map_or(0, |metadata| metadata.len());
    (bytes > 0 || encoded > 0).then(|| retained_discard_reason(reason))
}

fn retained_discard_reason(reason: &str) -> String {
    format!("recovery failed: early-failed file retained for recovery: {reason}")
}

fn finish_discard(path: Option<&Path>, encoded: u64, reason: &str) -> String {
    if let Some(recoverable) = discard_recovery_reason(path, encoded, reason) {
        return recoverable;
    }
    if let Some(path) = path {
        let _ = std::fs::remove_file(path);
    }
    reason.to_string()
}

enum PipeKind {
    Input {
        seg: u32,
        key: InputKey,
        rec: InputPipeline,
        resize_at: Option<Instant>,
        /// Keeps recovery locked while this pipeline or one of its worker threads is alive.
        _session_lock: session::SessionLock,
    },
    Probe {
        source: SourceId,
        convert: GpuConvert,
        result: Option<Result<(), String>>,
        /// Negotiated source caps (size, maximum rate).
        caps: recorder::CapsSlot,
    },
    /// Idle live preview of one input; `ident` names what it shows (a changed camera or mode
    /// replaces it). Its failures are silent: it is retried, never reported as an input error.
    Preview { key: InputKey, ident: String },
}

struct Pipe {
    id: u64,
    capture: Option<CaptureId>,
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    kind: PipeKind,
    stage: Stage,
    deadline: Option<Instant>,
    outcome: Option<Outcome>,
    /// Gave up waiting on its Null transition; kept only for the capture fd invariant.
    abandoned: bool,
    last_error: Option<String>,
    /// Target of the worker `set_state` currently running for this pipeline, if any.
    in_flight: Option<gst::State>,
    /// Null was requested while an up-transition was in flight; it is spawned once that
    /// transition's worker reports back, so the two never race for the state lock.
    null_pending: bool,
}

impl Pipe {
    fn input(&self) -> Option<(u32, InputKey)> {
        match &self.kind {
            PipeKind::Input { seg, key, .. } => Some((*seg, *key)),
            PipeKind::Probe { .. } | PipeKind::Preview { .. } => None,
        }
    }

    fn is_preview(&self) -> bool {
        matches!(self.kind, PipeKind::Preview { .. })
    }
}

struct Source {
    info: SourceInfo,
    node: Option<u32>,
    convert: Option<GpuConvert>,
    gl_retry: bool,
}

struct Capture {
    fd: Option<OwnedFd>,
}

struct FileRec {
    file: InputFile,
    meta: ManifestInput,
    first: Option<Arc<std::sync::atomic::AtomicU64>>,
    last: Option<Arc<std::sync::atomic::AtomicU64>>,
    size: Option<SizeSlot>,
    counters: Option<Arc<Counters>>,
}

struct Seg {
    index: u32,
    dir: PathBuf,
    status: SegmentStatus,
    base_ns: Option<u64>,
    stop_ns: Option<u64>,
    gap_ns: Option<u64>,
    started_wall: Option<String>,
    started_at: Option<Instant>,
    timed_out: bool,
    stop_reason: Option<String>,
    files: Vec<FileRec>,
}

struct Session {
    name: String,
    dir: PathBuf,
    created: String,
    _lock: session::SessionLock,
    reserve_path: Option<PathBuf>,
    segs: Vec<Seg>,
}

struct Engine {
    tx: mpsc::Sender<Msg>,
    events: async_channel::Sender<EngineEvent>,
    caps: Capabilities,
    /// Verified encoders, preference order (first = `caps.h264_encoder`); picked per input.
    encoders: Vec<encoders::EncoderCap>,
    /// Encoders whose GPU convert path (`caps.gpu_convert`) passed its probe; DMA-BUF inputs
    /// (portal sources, DMA-BUF cameras) are only given one of these.
    gpu_encoders: Vec<VideoEncoder>,
    /// Test pattern CFR rate (`CHINSTRAP_SCREEN_FPS`, default 30, max 60).
    screen_fps: i32,
    /// Frame-rate mode new window/screen sources get.
    frame_rate_default: FrameRateMode,
    /// Selected camera mode override (a mode label); None = automatic.
    camera_mode: Option<String>,
    vocabulary: VocabularyInfo,
    last_vocab_check: Instant,
    test_sources: Vec<(u32, recorder::TestBehaviour)>,
    devices: Devices,
    selected_camera: Option<DeviceId>,
    selected_microphone: Option<DeviceId>,
    selected_system_audio: Option<DeviceId>,
    /// Only set by the self-test fallback; normal system audio is always a selected PipeWire sink.
    system_audio_test_tone: bool,
    sources: Vec<Source>,
    captures: HashMap<CaptureId, Capture>,
    output_root: PathBuf,
    min_free_mb: u64,
    storage: StorageInfo,
    storage_rate_tracker: storage::StorageRateTracker,
    storage_free_bytes_override: Option<u64>,
    storage_warning_level: u8,
    recoveries: Vec<RecoveryCandidate>,
    phase: Phase,
    portal_busy: bool,
    session: Option<Session>,
    portal: Option<(async_channel::Sender<PortalCmd>, JoinHandle<()>)>,
    clock: gst::Clock,
    pipes: Vec<Pipe>,
    next_pipe: u64,
    next_source: u32,
    next_capture: u32,
    /// Cut in progress: start the next segment once the current one is finalized.
    cut_pending: bool,
    /// stop_clock of the previous segment, for the next segment's gap_before.
    gap_from: Option<u64>,
    finalize_deadline: Option<Instant>,
    shutdown: Option<(Instant, Vec<SyncSender<bool>>)>,
    /// False once a shutdown left some input of the active segment unfinalized.
    shutdown_clean: bool,
    resize_after: Option<Duration>,
    last_sizes: Instant,
    checkpoint: CheckpointWriteState,
    manifest_retry_at: Option<Instant>,
    last_level: Instant,
    last_system_level: Instant,
    dirty: bool,
    manifest_dirty: bool,
    warned: HashSet<String>,
    pending_notices: Vec<Notice>,
    /// Transcription state (§12.3); this thread is its only owner.
    transcription: Transcription,
    /// Live preview frames (shared with the UI) and whether previews are on at all.
    previews: Previews,
    preview_on: bool,
    /// What `Snapshot::previews` reports per input.
    preview_state: HashMap<InputKey, PreviewState>,
    /// Hub generation of each input when its preview (re)started: Live once it moves.
    preview_seen: HashMap<InputKey, u64>,
    /// A failed idle preview is not restarted before this.
    preview_retry: HashMap<InputKey, Instant>,
    /// Record was accepted while previews ran: the segment starts from tick() once every
    /// preview pipeline is Null (break-before-make, D4: one consumer per node).
    record_pending: bool,
}

fn kind_name(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Monitor => "monitor",
        SourceKind::Window => "window",
        SourceKind::Virtual => "virtual",
        SourceKind::TestPattern => "test",
    }
}

fn file_stem(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Monitor | SourceKind::Virtual => "screen",
        SourceKind::Window => "window",
        SourceKind::TestPattern => "test",
    }
}

fn key_string(key: InputKey) -> String {
    match key {
        InputKey::Camera => "camera".into(),
        InputKey::Microphone => "mic".into(),
        InputKey::SystemAudio => "system".into(),
        InputKey::Source(id) => format!("source:{}", id.0),
    }
}

fn status_string(s: &InputStatus) -> &'static str {
    match s {
        InputStatus::Starting => "starting",
        InputStatus::Recording => "recording",
        InputStatus::Finalizing => "finalizing",
        InputStatus::Complete => "complete",
        InputStatus::Failed { .. } => "failed",
    }
}

fn load(a: &Option<Arc<std::sync::atomic::AtomicU64>>) -> Option<u64> {
    a.as_ref()
        .map(|a| a.load(Ordering::Acquire))
        .filter(|v| *v != UNSET)
}

/// Frame accounting of a video input (None for audio), from its counters and file size.
fn quality_of(rec: &InputPipeline, bytes: u64) -> Option<InputQuality> {
    let enc = rec.encoder?;
    let c = &rec.counters;
    let g = Counters::get;
    let level = rec
        .leaky
        .as_ref()
        .map_or(0, |q| q.property::<u32>("current-level-buffers") as u64);
    let dropped_queue = g(&c.queue_in).saturating_sub(g(&c.queue_out) + level);
    // Repeats: videorate's own count; the repeater's output beyond its input; re-injected
    // frames (Native: the last frame at the stop time).
    // videorate resets its own counts when it stops, so its output beyond its input counts too.
    let by_pads = g(&c.rate_out).saturating_sub(g(&c.rate_in));
    let repeated = match (&rec.videorate, rec.rate) {
        (Some(vr), _) => vr.property::<u64>("duplicate").max(by_pads),
        (None, Some(Rate::Cfr(_))) => by_pads,
        _ => 0,
    } + g(&c.reinjected);
    // The first output frame is the first real frame itself (stamped 0), not padding.
    let padded = g(&c.padded).saturating_sub(1).min(repeated);
    let encoded = g(&c.encoded);
    let last = rec.last_pts.load(Ordering::Acquire);
    let secs = if last == UNSET {
        0.0
    } else {
        last as f64 / 1e9
    };
    let fresh = encoded.saturating_sub(repeated);
    let first = g(&c.first_arrival);
    Some(InputQuality {
        encoder: Some(enc),
        nominal_fps: rec
            .rate
            .and_then(Rate::cfr)
            .map(|(n, d)| n as f64 / d.max(1) as f64),
        max_fps: match rec.rate {
            Some(Rate::Native { gop_fps, .. }) => Some(gop_fps as f64),
            _ => None,
        },
        frames_in: g(&c.frames_in),
        dropped_corrupt: g(&c.dropped_corrupt),
        dropped_queue,
        duplicated: repeated - padded,
        padded,
        encoded,
        effective_fps: if secs > 0.0 { fresh as f64 / secs } else { 0.0 },
        repeat_pct: if encoded > 0 {
            (repeated - padded) as f64 * 100.0 / encoded as f64
        } else {
            0.0
        },
        bitrate_kbps: if secs > 0.0 {
            (bytes as f64 * 8.0 / secs / 1000.0).round() as u32
        } else {
            0
        },
        start_offset: (first != UNSET).then(|| Duration::from_nanos(first)),
    })
}

fn retain_session_lock_until_process_exit(lock: session::SessionLock) {
    static LOCKS: OnceLock<Mutex<Vec<session::SessionLock>>> = OnceLock::new();
    LOCKS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(lock);
}

fn spawn_named(name: String, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new().name(name).spawn(f) {
        eprintln!("chinstrap: cannot spawn worker thread: {e}");
    }
}

fn error_text(err: &gst::message::Error) -> String {
    match err.debug() {
        Some(d) => format!("{} ({d})", err.error()),
        None => err.error().to_string(),
    }
}

/// Max over channels of a `level` message field (`rms` / `peak`).
fn level_max(s: &gst::StructureRef, field: &str) -> Option<f32> {
    let v = s.value(field).ok()?;
    let vals: Vec<f64> = if let Ok(arr) = v.get::<glib::ValueArray>() {
        arr.iter().filter_map(|x| x.get::<f64>().ok()).collect()
    } else if let Ok(l) = v.get::<gst::List>() {
        l.iter().filter_map(|x| x.get::<f64>().ok()).collect()
    } else if let Ok(a) = v.get::<gst::Array>() {
        a.iter().filter_map(|x| x.get::<f64>().ok()).collect()
    } else {
        return None;
    };
    vals.into_iter().reduce(f64::max).map(|x| x as f32)
}

fn system_audio_selection_after_refresh(
    selected: Option<DeviceId>,
    available: bool,
    busy: bool,
) -> (Option<DeviceId>, bool) {
    match selected {
        Some(id) if !available => (busy.then_some(id), true),
        _ => (selected, false),
    }
}

impl Engine {
    fn new(
        cfg: EngineConfig,
        tx: mpsc::Sender<Msg>,
        events: async_channel::Sender<EngineEvent>,
        portal: Option<(async_channel::Sender<PortalCmd>, JoinHandle<()>)>,
        previews: Previews,
    ) -> Self {
        let clock = gst::SystemClock::obtain();
        clock.set_property("clock-type", gst::ClockType::Monotonic);
        let mut notices = Vec::new();
        let mut note = |severity, message: String| {
            notices.push(Notice {
                severity,
                input: None,
                message,
                detail: None,
            })
        };

        // §5.7 element checks.
        let missing = encoders::check_elements();
        for (name, class, pkg) in &missing {
            if *class == InputClass::TestOptional {
                // §5.3: skipped when missing; nothing becomes unrecordable.
                note(
                    Severity::Info,
                    format!(
                        "Optional GStreamer element {name} (package {pkg}) is missing; the test pattern has no clock overlay"
                    ),
                );
            } else {
                note(
                    Severity::Error,
                    format!("Missing GStreamer element {name} (package {pkg})"),
                );
            }
        }
        let missing: Vec<String> = missing
            .iter()
            .filter(|(_, class, _)| *class != InputClass::TestOptional)
            .map(|(n, _, _)| n.to_string())
            .collect();
        let jpeg_ok = encoders::class_ok(&missing, InputClass::MjpegCamera);

        // §5.6 encoder selection, §5.2 GPU path probe. The only accepted blocking period.
        let screen_fps = encoders::screen_fps();
        let (encoder_table, enc_notes) = if encoders::class_ok(&missing, InputClass::Video) {
            encoders::select_encoders(cfg.preferred_encoder)
        } else {
            (Vec::new(), Vec::new())
        };
        let h264 = encoder_table.first().map(|c| c.enc);
        for n in enc_notes {
            note(Severity::Info, n);
        }
        if h264.is_none() {
            note(
                Severity::Error,
                "No working video encoder: video inputs cannot record".into(),
            );
        }
        let (gpu, gpu_notes) = encoders::select_convert(cfg.preferred_convert, h264);
        for n in gpu_notes {
            note(Severity::Info, n);
        }
        // The GPU path was probed with the default encoder; probe it with every other encoder an
        // input may be given, so a DMA-BUF input never gets an encoder its path was not checked
        // with.
        let gpu_encoders: Vec<VideoEncoder> = match gpu {
            Some(convert) => encoder_table
                .iter()
                .map(|c| c.enc)
                .filter(|&e| {
                    Some(e) == h264
                        || match encoders::run_probe(
                            &encoders::gpu_probe_launch(convert, e),
                            encoders::PROBE_TIMEOUT,
                        ) {
                            Ok(()) => true,
                            Err(err) => {
                                note(
                                    Severity::Info,
                                    format!(
                                        "GPU convert {convert:?} with {} failed its probe: {err}",
                                        e.element_name()
                                    ),
                                );
                                false
                            }
                        }
                })
                .collect(),
            None => Vec::new(),
        };
        if gpu.is_none() {
            note(
                Severity::Error,
                "Screen/window capture unavailable: neither VA (gst-plugins-bad + a VA driver) nor GL conversion works.".into(),
            );
        }

        let mut devices = Devices::start();
        devices.set_jpeg_ok(jpeg_ok);
        devices.enumerate(gpu.is_some());
        let selected_microphone = devices.microphones.first().map(|m| m.info.id);
        let portal_state = if portal.is_some() {
            PortalAvailability::Unknown
        } else {
            PortalAvailability::Unavailable {
                reason: "portal disabled".into(),
            }
        };
        let now = Instant::now();
        let (wtx, mtx) = (Mutex::new(tx.clone()), Mutex::new(tx.clone()));
        let transcription = Transcription::new(TxConfig::from_env(
            Arc::new(move |e| {
                let _ = wtx
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .send(Msg::Transcribe(e));
            }),
            Arc::new(move |e| {
                let _ = mtx
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .send(Msg::Model(e));
            }),
        ));
        Self {
            output_root: cfg.output_root.clone(),
            tx,
            events,
            caps: Capabilities {
                h264_encoder: h264,
                encoders: encoder_table
                    .iter()
                    .map(|c| (c.enc, c.fps_at(encoders::UHD.0, encoders::UHD.1)))
                    .collect(),
                gpu_convert: gpu,
                missing_elements: missing,
                portal: portal_state,
            },
            encoders: encoder_table,
            gpu_encoders,
            screen_fps,
            frame_rate_default: cfg.frame_rate,
            camera_mode: None,
            vocabulary: vocab::info(),
            last_vocab_check: now,
            test_sources: cfg.test_sources.clone(),
            devices,
            selected_camera: None,
            selected_microphone,
            selected_system_audio: None,
            system_audio_test_tone: false,
            sources: Vec::new(),
            captures: HashMap::new(),
            min_free_mb: cfg.selftest_min_free_mb.unwrap_or_else(|| {
                storage::parse_minimum_free_mb(
                    std::env::var("CHINSTRAP_MIN_FREE_MB").ok().as_deref(),
                )
            }),
            storage: StorageInfo::default(),
            storage_rate_tracker: storage::StorageRateTracker::default(),
            storage_free_bytes_override: cfg.selftest_free_bytes,
            storage_warning_level: 0,
            recoveries: Vec::new(),
            phase: Phase::Idle,
            portal_busy: false,
            session: None,
            portal,
            clock,
            pipes: Vec::new(),
            next_pipe: 1,
            next_source: 1,
            next_capture: 1,
            cut_pending: false,
            gap_from: None,
            finalize_deadline: None,
            shutdown: None,
            shutdown_clean: true,
            resize_after: recorder::test_resize_after(),
            last_sizes: now,
            checkpoint: CheckpointWriteState::new(now),
            manifest_retry_at: None,
            last_level: now,
            last_system_level: now,
            dirty: true,
            manifest_dirty: false,
            warned: HashSet::new(),
            pending_notices: notices,
            transcription,
            previews,
            preview_on: cfg.preview,
            preview_state: HashMap::new(),
            preview_seen: HashMap::new(),
            preview_retry: HashMap::new(),
            record_pending: false,
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        // Discover stale sessions before the first Snapshot so recovery actions are immediately visible.
        match session::scan_interrupted(&self.output_root) {
            Ok(found) => {
                for item in found {
                    let segments = item
                        .segments
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.pending_notices.push(Notice {
                        severity: Severity::Warning,
                        input: None,
                        message: format!(
                            "An earlier recording was interrupted: {}, segment {}. Recover / Show folder / Dismiss.",
                            item.name, segments
                        ),
                        detail: None,
                    });
                    self.recoveries.push(RecoveryCandidate {
                        name: item.name,
                        dir: item.dir,
                        segments: item.segments,
                        recovering: false,
                        error: None,
                    });
                }
            }
            Err(error) => self.pending_notices.push(Notice {
                severity: Severity::Error,
                input: None,
                message: format!("Cannot scan for interrupted recordings: {error}"),
                detail: None,
            }),
        }
        self.refresh_storage();
        self.snapshot();
        for n in std::mem::take(&mut self.pending_notices) {
            let _ = self.events.send_blocking(EngineEvent::Notice(n));
        }
        loop {
            match rx.recv_timeout(TICK) {
                Ok(msg) => self.handle(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    // Cannot happen while we hold `self.tx`; treat as a shutdown anyway.
                    if self.shutdown.is_none() {
                        self.begin_shutdown(Instant::now() + FINALIZE_TIMEOUT, None);
                    }
                }
            }
            while let Ok(msg) = rx.try_recv() {
                self.handle(msg);
            }
            self.tick();
            self.flush();
            if self.should_exit() {
                self.exit();
                return;
            }
        }
    }

    // ---- events ------------------------------------------------------------------------------

    fn notice(&self, severity: Severity, input: Option<InputKey>, message: impl Into<String>) {
        let message = message.into();
        if severity != Severity::Info {
            eprintln!("chinstrap: {severity:?}: {message}");
        }
        let _ = self.events.send_blocking(EngineEvent::Notice(Notice {
            severity,
            input,
            message,
            detail: None,
        }));
    }

    fn notice_detail(
        &self,
        severity: Severity,
        input: Option<InputKey>,
        message: &str,
        detail: &str,
    ) {
        eprintln!("chinstrap: {severity:?}: {message}: {detail}");
        let _ = self.events.send_blocking(EngineEvent::Notice(Notice {
            severity,
            input,
            message: message.into(),
            detail: Some(detail.into()),
        }));
    }

    fn reject(&self, cmd: &EngineCommand, why: &str) {
        self.notice(Severity::Warning, None, format!("Rejected {cmd:?}: {why}"));
    }

    fn snapshot(&mut self) {
        self.dirty = false;
        let snap = Snapshot {
            capabilities: self.caps.clone(),
            cameras: self
                .devices
                .cameras
                .iter()
                .map(|e| e.info.clone())
                .collect(),
            microphones: self
                .devices
                .microphones
                .iter()
                .map(|e| e.info.clone())
                .collect(),
            system_audio_devices: self.devices.system_audio_infos(),
            sources: self.sources.iter().map(|s| s.info.clone()).collect(),
            selected_camera: self.selected_camera,
            selected_microphone: self.selected_microphone,
            selected_system_audio: self.selected_system_audio,
            system_audio_test_tone: self.system_audio_test_tone,
            output_root: self.output_root.clone(),
            phase: self.phase.clone(),
            portal_busy: self.portal_busy,
            session: self.session.as_ref().map(|s| SessionSummary {
                name: s.name.clone(),
                dir: s.dir.clone(),
                manifest: s.dir.join(session::MANIFEST_NAME),
                segments: s
                    .segs
                    .iter()
                    .map(|g| SegmentSummary {
                        index: g.index,
                        dir: g.dir.clone(),
                        status: g.status.clone(),
                        duration: match (g.base_ns, g.stop_ns, &g.status) {
                            (Some(b), Some(s), st) if *st != SegmentStatus::Recording => {
                                Some(Duration::from_nanos(s.saturating_sub(b)))
                            }
                            _ => None,
                        },
                        gap_before: g.gap_ns.map(Duration::from_nanos),
                        files: g.files.iter().map(|f| f.file.clone()).collect(),
                        transcript: self.transcription.status(&JobKey {
                            session_dir: s.dir.clone(),
                            segment: g.index,
                        }),
                    })
                    .collect(),
            }),
            can_record: self.can_record(),
            transcription: self.transcription.info(),
            frame_rate_default: self.frame_rate_default,
            selected_camera_mode: self.camera_mode.clone(),
            capacity: self.capacity(),
            storage: self.storage.clone(),
            recoveries: self.recoveries.clone(),
            vocabulary: self.vocabulary.clone(),
            previews: self
                .preview_keys()
                .into_iter()
                .filter_map(|k| self.preview_state.get(&k).map(|s| (k, s.clone())))
                .collect(),
        };
        let _ = self
            .events
            .send_blocking(EngineEvent::Snapshot(Box::new(snap)));
    }

    fn flush(&mut self) {
        for (severity, message) in self.transcription.take_notices() {
            self.notice(severity, None, message);
        }
        if self.transcription.take_manifest_dirty() {
            self.manifest_dirty = true;
        }
        if self.transcription.take_dirty() {
            self.dirty = true;
        }
        if self.manifest_dirty
            && self
                .manifest_retry_at
                .is_none_or(|retry_at| Instant::now() >= retry_at)
        {
            self.write_manifest();
        }
        if self.dirty {
            self.snapshot();
        }
    }

    fn write_manifest(&mut self) {
        let Some(s) = &self.session else {
            self.manifest_dirty = false;
            self.manifest_retry_at = None;
            return;
        };
        let mut m = Manifest::new(&s.name, s.created.clone());
        for g in &s.segs {
            let (status, reason) = match &g.status {
                SegmentStatus::Recording => ("recording", None),
                SegmentStatus::Complete => ("complete", None),
                SegmentStatus::Incomplete { reason } => ("incomplete", Some(reason.clone())),
            };
            m.segments.push(ManifestSegment {
                index: g.index,
                dir: session::segment_dir_name(g.index),
                status: status.into(),
                reason,
                base_time_ns: g.base_ns,
                stop_clock_ns: g.stop_ns,
                duration_ns: match (g.base_ns, g.stop_ns) {
                    (Some(b), Some(s)) => Some(s.saturating_sub(b)),
                    _ => None,
                },
                gap_before_ns: g.gap_ns,
                started_wall: g.started_wall.clone(),
                inputs: g
                    .files
                    .iter()
                    .map(|f| {
                        let mut mi = f.meta.clone();
                        mi.status = status_string(&f.file.status).into();
                        mi.error = match &f.file.status {
                            InputStatus::Failed { reason } => Some(reason.clone()),
                            _ => None,
                        };
                        // Video: the first real frame's arrival (the file is padded to zero).
                        let arrival = f
                            .counters
                            .as_ref()
                            .filter(|_| f.file.quality.is_some())
                            .map(|c| Counters::get(&c.first_arrival))
                            .filter(|v| *v != UNSET);
                        mi.first_pts_ns = arrival.or(load(&f.first)).or(mi.first_pts_ns);
                        mi.quality = f.file.quality.as_ref().map(|q| ManifestQuality {
                            frames_in: q.frames_in,
                            dropped_corrupt: q.dropped_corrupt,
                            dropped_queue: q.dropped_queue,
                            duplicated: q.duplicated,
                            padded: q.padded,
                            encoded: q.encoded,
                            effective_fps: (q.effective_fps * 100.0).round() / 100.0,
                            repeat_pct: (q.repeat_pct * 100.0).round() / 100.0,
                            bitrate_kbps: q.bitrate_kbps,
                            start_offset_ns: q.start_offset.map(|d| d.as_nanos() as u64),
                        });
                        mi.last_pts_ns = load(&f.last).or(mi.last_pts_ns);
                        if let Some(Some((w, h))) =
                            f.size.as_ref().and_then(|s| s.lock().ok().map(|g| *g))
                        {
                            mi.width = Some(w);
                            mi.height = Some(h);
                        }
                        if f.file.input == InputKey::Microphone {
                            mi.transcript = self.transcription.manifest_entry(&JobKey {
                                session_dir: s.dir.clone(),
                                segment: g.index,
                            });
                        }
                        mi
                    })
                    .collect(),
            });
        }
        let result = session::write_manifest(&s.dir, &m);
        let now = Instant::now();
        self.checkpoint
            .write_result(result.is_ok(), now, &mut self.manifest_dirty);
        self.manifest_retry_at = result.is_err().then_some(now + MANIFEST_RETRY);
        if let Err(e) = result {
            self.notice(
                Severity::Error,
                None,
                format!("Cannot write the session manifest: {e}"),
            );
        }
    }

    // ---- derived state -----------------------------------------------------------------------

    fn video_ok(&self) -> bool {
        self.caps.h264_encoder.is_some()
            && encoders::class_ok(&self.caps.missing_elements, InputClass::Video)
    }

    fn input_recordable(&self, key: InputKey) -> bool {
        let missing = &self.caps.missing_elements;
        match key {
            InputKey::Camera => {
                let Some(e) = self.selected_camera.and_then(|id| self.devices.camera(id)) else {
                    return false;
                };
                e.info.unsupported.is_none()
                    && self.video_ok()
                    && match e.mode(self.camera_mode.as_deref()).map(|m| m.path) {
                        Some(CameraPath::Raw) => true,
                        Some(CameraPath::Jpeg) => {
                            encoders::class_ok(missing, InputClass::MjpegCamera)
                        }
                        Some(CameraPath::DmaBuf) => self.caps.gpu_convert.is_some(),
                        None => false,
                    }
            }
            InputKey::Microphone => {
                self.selected_microphone
                    .and_then(|id| self.devices.microphone(id))
                    .is_some()
                    && encoders::class_ok(missing, InputClass::Mic)
            }
            InputKey::SystemAudio => {
                if !encoders::class_ok(missing, InputClass::Mic) {
                    return false;
                }
                if self.system_audio_test_tone {
                    return encoders::has_element("audiotestsrc");
                }
                match self.selected_system_audio {
                    Some(id) if id == DEFAULT_SYSTEM_AUDIO_ID => {
                        self.devices
                            .system_audio_infos()
                            .iter()
                            .any(|d| d.id == DEFAULT_SYSTEM_AUDIO_ID)
                            && encoders::class_ok(missing, InputClass::PipeWire)
                    }
                    Some(id) => {
                        self.devices
                            .system_audio(id)
                            .is_some_and(|e| e.serial.is_some() && e.info.unsupported.is_none())
                            && encoders::class_ok(missing, InputClass::PipeWire)
                    }
                    None => false,
                }
            }
            InputKey::Source(id) => {
                let Some(s) = self.sources.iter().find(|s| s.info.id == id) else {
                    return false;
                };
                s.info.state == SourceState::Ready
                    && self.video_ok()
                    && (s.info.kind == SourceKind::TestPattern
                        || (encoders::class_ok(missing, InputClass::PipeWire)
                            && encoders::class_ok(missing, InputClass::DmaBufVideo)
                            && s.convert.or(self.caps.gpu_convert).is_some()
                            && s.info.capture.is_some_and(|c| {
                                self.captures.get(&c).is_some_and(|c| c.fd.is_some())
                            })))
            }
        }
    }

    fn frozen_inputs(&self) -> Vec<InputKey> {
        let mut keys = vec![InputKey::Camera, InputKey::Microphone];
        keys.push(InputKey::SystemAudio);
        keys.extend(self.sources.iter().map(|s| InputKey::Source(s.info.id)));
        keys.into_iter()
            .filter(|k| self.input_recordable(*k))
            .collect()
    }

    fn can_record(&self) -> bool {
        self.phase == Phase::Idle
            && !self.record_pending
            && self.shutdown.is_none()
            && !self.portal_busy
            && !self
                .sources
                .iter()
                .any(|s| s.info.state == SourceState::Checking)
            && !self.frozen_inputs().is_empty()
            && self.storage.record_blocked_reason.is_none()
    }

    fn seg_mut(&mut self, index: u32) -> Option<&mut Seg> {
        self.session
            .as_mut()?
            .segs
            .iter_mut()
            .find(|s| s.index == index)
    }

    fn file_mut(&mut self, seg: u32, key: InputKey) -> Option<&mut FileRec> {
        self.seg_mut(seg)?
            .files
            .iter_mut()
            .find(|f| f.file.input == key)
    }

    fn set_file_status(&mut self, seg: u32, key: InputKey, status: InputStatus) {
        if let Some(f) = self.file_mut(seg, key)
            && f.file.status != status
        {
            // A Failed status is final.
            if matches!(f.file.status, InputStatus::Failed { .. })
                && !matches!(status, InputStatus::Failed { .. })
            {
                return;
            }
            f.file.status = status;
            self.dirty = true;
            self.manifest_dirty = true;
        }
    }

    fn current_seg(&self) -> Option<u32> {
        match self.phase {
            Phase::Idle => None,
            Phase::Starting { segment }
            | Phase::Recording { segment, .. }
            | Phase::Stopping { segment } => Some(segment),
            Phase::Cutting { from } => Some(from),
        }
    }

    // ---- message handling --------------------------------------------------------------------

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Cmd(cmd) => self.command(cmd),
            Msg::Portal(outcome) => self.portal_outcome(outcome),
            Msg::Worker(w) => self.worker_result(w),
            Msg::SelftestNoSpace { key } => {
                if self.test_sources.is_empty() {
                    self.notice(
                        Severity::Error,
                        Some(key),
                        "Rejected self-test storage failure outside an injected test engine",
                    );
                } else if let Some(id) = self
                    .pipes
                    .iter()
                    .find(|pipe| pipe.input().is_some_and(|(_, input)| input == key))
                    .map(|pipe| pipe.id)
                {
                    self.disk_full_error(id, "injected ENOSPC test event".into());
                } else {
                    self.notice(
                        Severity::Error,
                        Some(key),
                        "Self-test storage failure could not find the requested input",
                    );
                }
            }
            Msg::SelftestFreeBytes { bytes } => {
                self.storage_free_bytes_override = Some(bytes);
                self.refresh_storage();
            }
            Msg::Transcribe(e) => self.transcription.on_worker(e),
            Msg::Model(e) => {
                let segs = self.seg_views();
                self.transcription.on_model(e, &segs);
            }
            Msg::RecoveryDone { name, dir, result } => self.recovery_done(name, dir, result),
            Msg::Shutdown { deadline, ack } => self.begin_shutdown(deadline, Some(ack)),
        }
    }

    fn command(&mut self, cmd: EngineCommand) {
        if self.shutdown.is_some() {
            self.reject(&cmd, "the engine is shutting down");
            return;
        }
        let busy = self.phase.is_busy() || self.record_pending;
        match &cmd {
            EngineCommand::RefreshDevices => {
                self.refresh_devices();
                self.dirty = true;
            }
            EngineCommand::SelectCamera(_)
            | EngineCommand::SelectMicrophone(_)
            | EngineCommand::SelectSystemAudio(_)
            | EngineCommand::SelectSystemAudioTestTone
            | EngineCommand::AddPortalSources
            | EngineCommand::AddTestSource
            | EngineCommand::RemoveSource(_)
            | EngineCommand::SetOutputRoot(_)
            | EngineCommand::EndSession
            | EngineCommand::RecoverSession { .. }
            | EngineCommand::SetFrameRateMode { .. }
            | EngineCommand::SelectCameraMode(_)
                if busy =>
            {
                let why = if self.record_pending {
                    STARTING
                } else {
                    "not allowed while recording"
                };
                self.reject(&cmd, why);
            }
            EngineCommand::SetFrameRateMode { source, mode } => match source {
                None => {
                    self.frame_rate_default = *mode;
                    for s in &mut self.sources {
                        if s.info.kind != SourceKind::TestPattern {
                            s.info.frame_rate = *mode;
                        }
                    }
                    self.dirty = true;
                }
                Some(id) => match self.sources.iter_mut().find(|s| s.info.id == *id) {
                    Some(s) => {
                        s.info.frame_rate = *mode;
                        self.dirty = true;
                    }
                    None => self.reject(&cmd, "unknown source"),
                },
            },
            EngineCommand::SelectCameraMode(label) => {
                let known = match (
                    label,
                    self.selected_camera.and_then(|id| self.devices.camera(id)),
                ) {
                    (None, _) => true,
                    (Some(l), Some(cam)) => cam.modes.iter().any(|m| &m.label() == l),
                    (Some(_), None) => false,
                };
                if known {
                    self.camera_mode = label.clone();
                    self.dirty = true;
                } else {
                    self.reject(&cmd, "the selected camera has no such mode");
                }
            }
            EngineCommand::AddVocabularyTerm(_)
            | EngineCommand::RemoveVocabularyTerm(_)
            | EngineCommand::AddReplacement { .. }
            | EngineCommand::RemoveReplacement { .. } => {
                if let Err(e) = vocab::command(&cmd) {
                    self.reject(&cmd, &e);
                }
                self.vocabulary = vocab::info();
                self.dirty = true;
            }
            EngineCommand::SelectCamera(id) => match id {
                None => {
                    self.selected_camera = None;
                    self.camera_mode = None;
                    self.dirty = true;
                }
                Some(id) => match self.devices.camera(*id) {
                    Some(c) if c.info.unsupported.is_none() => {
                        if self.selected_camera != Some(*id) {
                            self.camera_mode = None;
                        }
                        self.selected_camera = Some(*id);
                        self.dirty = true;
                    }
                    Some(_) => self.reject(&cmd, "camera is not usable"),
                    None => self.reject(&cmd, "unknown camera"),
                },
            },
            EngineCommand::SelectMicrophone(id) => match id {
                None => {
                    self.selected_microphone = None;
                    self.dirty = true;
                }
                Some(id) if self.devices.microphone(*id).is_some() => {
                    self.selected_microphone = Some(*id);
                    self.dirty = true;
                }
                Some(_) => self.reject(&cmd, "unknown microphone"),
            },
            EngineCommand::SelectSystemAudio(id) => match id {
                None => {
                    self.selected_system_audio = None;
                    self.system_audio_test_tone = false;
                    self.dirty = true;
                }
                Some(id)
                    if *id == DEFAULT_SYSTEM_AUDIO_ID
                        && self
                            .devices
                            .system_audio_infos()
                            .iter()
                            .any(|d| d.id == DEFAULT_SYSTEM_AUDIO_ID) =>
                {
                    self.selected_system_audio = Some(*id);
                    self.system_audio_test_tone = false;
                    self.dirty = true;
                }
                Some(id)
                    if self
                        .devices
                        .system_audio(*id)
                        .is_some_and(|e| e.info.unsupported.is_none()) =>
                {
                    self.selected_system_audio = Some(*id);
                    self.system_audio_test_tone = false;
                    self.dirty = true;
                }
                Some(_) => self.reject(&cmd, "unknown or unsupported system-audio output"),
            },
            EngineCommand::SelectSystemAudioTestTone => {
                self.selected_system_audio = None;
                self.system_audio_test_tone = true;
                self.dirty = true;
            }
            EngineCommand::AddPortalSources => {
                if self.portal_busy {
                    self.reject(&cmd, "the picker is already open");
                } else if !matches!(self.caps.portal, PortalAvailability::Available { .. }) {
                    self.reject(&cmd, "the screencast portal is unavailable");
                } else if let Some((ptx, _)) = &self.portal {
                    let capture = CaptureId(self.next_capture);
                    self.next_capture += 1;
                    if ptx.try_send(PortalCmd::Pick { capture }).is_ok() {
                        self.captures.insert(capture, Capture { fd: None });
                        self.portal_busy = true;
                        self.dirty = true;
                    } else {
                        self.reject(&cmd, "the portal thread is gone");
                    }
                }
            }
            EngineCommand::AddTestSource => {
                let id = SourceId(self.next_source);
                self.next_source += 1;
                let state = if self.video_ok() {
                    SourceState::Ready
                } else {
                    SourceState::Unsupported {
                        reason: "no working video encoder".into(),
                    }
                };
                self.sources.push(Source {
                    info: SourceInfo {
                        id,
                        capture: None,
                        kind: SourceKind::TestPattern,
                        label: format!("Test pattern {}", id.0),
                        logical_size: Some((1280, 720)),
                        state,
                        // The test pattern is CFR at CHINSTRAP_SCREEN_FPS until set otherwise.
                        frame_rate: if self.screen_fps == 60 {
                            FrameRateMode::Cfr60
                        } else {
                            FrameRateMode::Cfr30
                        },
                        refresh: Some((self.test_behaviour(id).refresh_fps(), 1)),
                        size: Some(
                            self.test_behaviour(id)
                                .size
                                .unwrap_or_else(recorder::test_size),
                        ),
                    },
                    node: None,
                    convert: self.caps.gpu_convert,
                    gl_retry: false,
                });
                self.dirty = true;
            }
            EngineCommand::RemoveSource(id) => {
                let Some(pos) = self.sources.iter().position(|s| s.info.id == *id) else {
                    self.reject(&cmd, "unknown source");
                    return;
                };
                self.sources.remove(pos);
                // Any probe still running for it is stopped; its result is ignored.
                let ids: Vec<u64> = self
                    .pipes
                    .iter()
                    .filter(|p| matches!(&p.kind, PipeKind::Probe { source, .. } if source == id))
                    .map(|p| p.id)
                    .collect();
                for pid in ids {
                    self.request_null(pid, None);
                }
                self.dirty = true;
            }
            EngineCommand::SetOutputRoot(root) => {
                if self.session.is_some() {
                    self.reject(&cmd, "a session is open");
                } else {
                    self.output_root = std::path::absolute(root).unwrap_or_else(|_| root.clone());
                    self.dirty = true;
                }
            }
            EngineCommand::EndSession => {
                if self.session.is_none() {
                    self.reject(&cmd, "no open session");
                } else {
                    // Changes handled earlier in this batch (a transcription Done, cancel or
                    // failure) are only marked dirty; write them while the session is open.
                    if self.transcription.take_manifest_dirty() {
                        self.manifest_dirty = true;
                    }
                    if self.manifest_dirty {
                        self.write_manifest();
                    }
                    self.release_session_reserve(Severity::Warning);
                    self.session = None;
                    // Its transcription jobs keep running (§12.3 policy 11).
                    self.transcription.set_session(None);
                    self.dirty = true;
                }
            }
            EngineCommand::RecoverSession { name } => self.start_recovery(name),
            EngineCommand::ShowSessionFolder { name } => self.show_session_folder(name),
            EngineCommand::DismissRecovery { name } => self.dismiss_recovery(name),
            EngineCommand::Record => self.record(&cmd),
            EngineCommand::Stop => self.stop(&cmd),
            EngineCommand::ToggleRecord => match self.phase {
                Phase::Idle if self.record_pending => self.cancel_pending_record(),
                Phase::Idle => self.record(&cmd),
                Phase::Recording { .. } => self.stop(&cmd),
                _ => self.reject(&cmd, "the engine is busy"),
            },
            EngineCommand::Cut => self.cut(&cmd),
            EngineCommand::DownloadModel
            | EngineCommand::CancelModelDownload
            | EngineCommand::SetAutoTranscribe(_)
            | EngineCommand::Transcribe { .. }
            | EngineCommand::CancelTranscription { .. }
            | EngineCommand::CancelOtherSessionTranscriptions => {
                let segs = self.seg_views();
                if let Err(why) = self.transcription.command(&cmd, &segs) {
                    self.reject(&cmd, why.as_str());
                }
            }
        }
    }

    fn start_recovery(&mut self, name: &str) {
        let Some(candidate) = self
            .recoveries
            .iter()
            .find(|item| item.name == name)
            .cloned()
        else {
            self.notice(
                Severity::Warning,
                None,
                "This interrupted session is no longer available",
            );
            return;
        };
        if candidate.recovering {
            self.notice(
                Severity::Info,
                None,
                "Recovery is already running for this session",
            );
            return;
        }
        if let Some(item) = self.recoveries.iter_mut().find(|item| item.name == name) {
            item.recovering = true;
            item.error = None;
        }
        self.dirty = true;
        let tx = self.tx.clone();
        let worker_name = candidate.name.clone();
        let worker_dir = candidate.dir.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("chinstrap-recovery-{}", candidate.name))
            .spawn(move || {
                let result = super::recovery::recover_session(&worker_dir);
                let _ = tx.send(Msg::RecoveryDone {
                    name: worker_name,
                    dir: worker_dir,
                    result,
                });
            });
        if let Err(error) = spawn {
            if let Some(item) = self.recoveries.iter_mut().find(|item| item.name == name) {
                item.recovering = false;
                item.error = Some(error.to_string());
            }
            self.notice(
                Severity::Error,
                None,
                format!("Cannot start recovery for {name}: {error}"),
            );
            self.dirty = true;
        }
    }

    fn show_session_folder(&self, name: &str) {
        let Some(candidate) = self.recoveries.iter().find(|item| item.name == name) else {
            self.notice(
                Severity::Warning,
                None,
                "This interrupted session is no longer available",
            );
            return;
        };
        if let Err(error) = std::process::Command::new("xdg-open")
            .arg(&candidate.dir)
            .spawn()
        {
            self.notice(
                Severity::Error,
                None,
                format!("Cannot open the interrupted session folder: {error}"),
            );
        }
    }

    fn dismiss_recovery(&mut self, name: &str) {
        let before = self.recoveries.len();
        self.recoveries.retain(|item| item.name != name);
        if self.recoveries.len() != before {
            self.dirty = true;
        }
    }

    fn recovery_done(
        &mut self,
        name: String,
        dir: PathBuf,
        result: Result<super::recovery::RecoveryResult, String>,
    ) {
        match result {
            Ok(result) => {
                if result.failed_files == 0 {
                    self.recoveries.retain(|item| item.name != name);
                } else if let Some(item) = self.recoveries.iter_mut().find(|item| item.name == name)
                {
                    item.recovering = false;
                    item.error = Some(
                        "Some files could not be recovered; original footage was preserved. Retry or show the folder.".into(),
                    );
                }
                for (segment, mic) in &result.mic_files {
                    self.transcription.segment_completed(
                        JobKey {
                            session_dir: dir.clone(),
                            segment: *segment,
                        },
                        Some(mic.clone()),
                    );
                }
                let severity = if result.failed_files == 0 {
                    Severity::Info
                } else {
                    Severity::Warning
                };
                let message = if result.failed_files == 0 {
                    format!(
                        "Recovery of {name} finished: {} file(s) recovered",
                        result.recovered_files
                    )
                } else {
                    format!(
                        "Recovery of {name} partially finished: {} file(s) recovered, {} file(s) left untouched; retry remains available",
                        result.recovered_files, result.failed_files
                    )
                };
                self.notice(severity, None, message);
            }
            Err(error) => {
                if let Some(item) = self.recoveries.iter_mut().find(|item| item.name == name) {
                    item.recovering = false;
                    item.error = Some(error.clone());
                }
                self.notice(
                    Severity::Error,
                    None,
                    format!("Recovery of {name} failed: {error}"),
                );
            }
        }
        self.dirty = true;
    }

    /// The current session's segments as the transcription state needs them.
    fn seg_views(&self) -> Vec<SegView> {
        let Some(s) = &self.session else {
            return Vec::new();
        };
        s.segs
            .iter()
            .map(|g| SegView {
                index: g.index,
                mic: g
                    .files
                    .iter()
                    .find(|f| {
                        f.file.input == InputKey::Microphone
                            && f.file.status == InputStatus::Complete
                    })
                    .map(|f| f.file.path.clone()),
                recording: g.status == SegmentStatus::Recording,
            })
            .collect()
    }

    fn refresh_devices(&mut self) {
        let gpu = self.caps.gpu_convert.is_some();
        self.devices.enumerate(gpu);
        self.validate_selection();
    }

    fn system_audio_available(&self, id: DeviceId) -> bool {
        if id == DEFAULT_SYSTEM_AUDIO_ID {
            self.devices
                .system_audio_infos()
                .iter()
                .any(|device| device.id == DEFAULT_SYSTEM_AUDIO_ID)
        } else {
            self.devices.system_audio(id).is_some()
        }
    }

    fn validate_selection(&mut self) {
        let busy = self.phase.is_busy() || self.record_pending;
        if let Some(id) = self.selected_system_audio {
            let available = self.system_audio_available(id);
            let (selection, disconnected) =
                system_audio_selection_after_refresh(Some(id), available, busy);
            self.selected_system_audio = selection;
            let warning_key = format!("system-audio-disconnected-{}", id.0);
            if available {
                self.warned.remove(&warning_key);
            } else if disconnected && self.warned.insert(warning_key) {
                self.notice(
                    Severity::Warning,
                    Some(InputKey::SystemAudio),
                    "System audio output disconnected",
                );
            }
        }
        if busy {
            return;
        }
        if let Some(id) = self.selected_camera
            && self
                .devices
                .camera(id)
                .is_none_or(|c| c.info.unsupported.is_some())
        {
            self.selected_camera = None;
            self.notice(
                Severity::Warning,
                Some(InputKey::Camera),
                "Camera disconnected",
            );
        }
        if let Some(id) = self.selected_microphone
            && self.devices.microphone(id).is_none()
        {
            self.selected_microphone = None;
            self.notice(
                Severity::Warning,
                Some(InputKey::Microphone),
                "Microphone disconnected",
            );
        }
        self.dirty = true;
    }

    // ---- recording control -------------------------------------------------------------------

    fn record(&mut self, cmd: &EngineCommand) {
        self.refresh_storage();
        if let Some(why) = self.storage.record_blocked_reason.clone() {
            self.reject(cmd, &why);
            return;
        }
        if !self.can_record() {
            let why = if self.record_pending {
                STARTING
            } else if self.phase.is_busy() {
                "the engine is busy"
            } else if self.portal_busy {
                "the picker is open"
            } else {
                "nothing recordable is selected, or a source is still being checked"
            };
            self.reject(cmd, why);
            return;
        }
        // Break-before-make (D4): every idle preview lets go of its node first; tick() starts
        // the segment once they are all Null.
        if self.pipes.iter().any(|p| p.is_preview() && !p.abandoned) {
            self.record_pending = true;
            self.stop_previews();
            self.dirty = true;
            return;
        }
        self.begin_recording();
    }

    /// Record, once nothing else uses the inputs: opens the session if needed and starts a
    /// segment with the frozen input set.
    fn begin_recording(&mut self) {
        self.refresh_storage();
        if let Some(why) = self.storage.record_blocked_reason.clone() {
            self.record_pending = false;
            self.notice(
                Severity::Error,
                None,
                format!("Recording did not start: {why}"),
            );
            self.dirty = true;
            return;
        }
        if self.session.is_none() {
            match session::create_session_dir(&self.output_root, &session::session_name_now()) {
                Ok((name, dir)) => {
                    let lock = match session::SessionLock::acquire(&dir) {
                        Ok(lock) => lock,
                        Err(e) => {
                            let _ = std::fs::remove_dir(&dir);
                            self.notice(
                                Severity::Error,
                                None,
                                format!("Cannot lock the new recording session: {e}"),
                            );
                            return;
                        }
                    };
                    let reserve = match storage::create_emergency_reserve(
                        &dir,
                        storage::EMERGENCY_RESERVE_BYTES,
                    ) {
                        Ok(path) => path,
                        Err(e) => {
                            drop(lock);
                            let _ = std::fs::remove_dir_all(&dir);
                            self.notice(
                                Severity::Error,
                                None,
                                format!("Cannot reserve emergency stop space: {e}"),
                            );
                            return;
                        }
                    };
                    self.transcription.set_session(Some(dir.clone()));
                    self.session = Some(Session {
                        name,
                        dir,
                        created: session::wall_now(),
                        _lock: lock,
                        reserve_path: Some(reserve),
                        segs: Vec::new(),
                    });
                    self.storage_warning_level = 0;
                    self.write_manifest();
                }
                Err(e) => {
                    self.notice(
                        Severity::Error,
                        None,
                        format!(
                            "Cannot create a session in {}: {e}",
                            self.output_root.display()
                        ),
                    );
                    return;
                }
            }
        }
        if let Some(session) = self.session.as_mut()
            && session.reserve_path.is_none()
        {
            match storage::create_emergency_reserve(&session.dir, storage::EMERGENCY_RESERVE_BYTES)
            {
                Ok(path) => session.reserve_path = Some(path),
                Err(e) => {
                    self.notice(
                        Severity::Error,
                        None,
                        format!("Cannot reserve emergency stop space: {e}"),
                    );
                    return;
                }
            }
        }
        let (inputs, stuck): (Vec<_>, Vec<_>) = self
            .frozen_inputs()
            .into_iter()
            .partition(|k| !self.preview_stuck(*k));
        for key in stuck {
            self.notice(
                Severity::Warning,
                Some(key),
                format!("{key:?}: its preview did not stop, so it may still hold the device; recording without it"),
            );
        }
        if inputs.is_empty() {
            self.notice(
                Severity::Error,
                None,
                "Recording did not start: every input is still held by a preview that did not stop",
            );
            return;
        }
        if self.selected_camera.is_some() && !inputs.contains(&InputKey::Camera) {
            self.notice(
                Severity::Warning,
                Some(InputKey::Camera),
                "The selected camera cannot record; recording without it",
            );
        }
        if self.selected_microphone.is_some() && !inputs.contains(&InputKey::Microphone) {
            self.notice(
                Severity::Warning,
                Some(InputKey::Microphone),
                "The selected microphone cannot record; recording without it",
            );
        }
        if self.selected_system_audio.is_some() && !inputs.contains(&InputKey::SystemAudio) {
            self.notice(
                Severity::Warning,
                Some(InputKey::SystemAudio),
                "The selected system audio cannot record; recording without it",
            );
        }
        self.start_segment(inputs, None);
    }

    fn stop(&mut self, cmd: &EngineCommand) {
        if self.record_pending {
            self.cancel_pending_record();
            return;
        }
        let Phase::Recording { segment, .. } = self.phase else {
            self.reject(cmd, "not recording");
            return;
        };
        self.finalize_segment(segment);
        self.phase = Phase::Stopping { segment };
        self.dirty = true;
    }

    fn cut(&mut self, cmd: &EngineCommand) {
        let Phase::Recording { segment, .. } = self.phase else {
            let why = if self.record_pending {
                STARTING
            } else {
                "not recording"
            };
            self.reject(cmd, why);
            return;
        };
        self.finalize_segment(segment);
        self.cut_pending = true;
        self.phase = Phase::Cutting { from: segment };
        self.dirty = true;
    }

    fn deadline_cap(&self, d: Instant) -> Instant {
        match &self.shutdown {
            Some((sd, _)) => d.min(*sd),
            None => d,
        }
    }

    /// Builds the pipelines of a new segment and starts them (§5.1). Returns false if nothing
    /// could be started (the engine is then Idle).
    fn start_segment(&mut self, inputs: Vec<InputKey>, gap_from: Option<u64>) {
        let Some(sess) = &self.session else {
            return;
        };
        let index = sess.segs.len() as u32 + 1;
        let session_lock = sess._lock.clone();
        let dir = match session::create_segment_dir(&sess.dir, index) {
            Ok(d) => d,
            Err(e) => {
                self.notice(
                    Severity::Error,
                    None,
                    format!("Cannot create segment directory: {e}"),
                );
                self.phase = Phase::Idle;
                self.dirty = true;
                return;
            }
        };
        let enc = self.caps.h264_encoder;
        // One plan for the whole segment: encoders and rates fit the measured capacity together.
        let plan = self.plan_for(&inputs);
        for n in &plan.notes {
            if self.warned.insert(n.clone()) {
                self.notice(Severity::Info, None, n.clone());
            }
        }
        if let Some(w) = plan.warning() {
            self.notice(Severity::Warning, None, w);
        }
        let mut files = Vec::new();
        let mut started = Vec::new();
        for key in inputs {
            let (label, kind, file_name, capture, device) = match key {
                InputKey::Camera => {
                    let name = self
                        .selected_camera
                        .and_then(|id| self.devices.camera(id))
                        .map(|e| e.info.name.clone());
                    (
                        "Camera".to_string(),
                        "camera",
                        "camera.mkv".to_string(),
                        None,
                        name,
                    )
                }
                InputKey::Microphone => {
                    let name = self
                        .selected_microphone
                        .and_then(|id| self.devices.microphone(id))
                        .map(|e| e.info.name.clone());
                    (
                        "Microphone".to_string(),
                        "microphone",
                        "mic.mkv".to_string(),
                        None,
                        name,
                    )
                }
                InputKey::SystemAudio => {
                    let info = self.selected_system_audio.and_then(|id| {
                        self.devices
                            .system_audio_infos()
                            .into_iter()
                            .find(|d| d.id == id)
                    });
                    let label = info
                        .as_ref()
                        .map(|d| format!("System audio · {}", d.name))
                        .unwrap_or_else(|| "System audio · self-test tone".into());
                    (
                        label,
                        "system-audio",
                        "system.mkv".to_string(),
                        None,
                        info.map(|d| d.name).or_else(|| {
                            self.system_audio_test_tone
                                .then(|| "synthetic 440 Hz test tone".into())
                        }),
                    )
                }
                InputKey::Source(id) => {
                    let s = self.sources.iter().find(|s| s.info.id == id);
                    let kind = s.map_or(SourceKind::Monitor, |s| s.info.kind);
                    (
                        s.map_or_else(|| format!("Source {}", id.0), |s| s.info.label.clone()),
                        kind_name(kind),
                        format!("{}-{}.mkv", file_stem(kind), id.0),
                        s.and_then(|s| s.info.capture),
                        None,
                    )
                }
            };
            let path = dir.join(&file_name);
            let codec = if matches!(key, InputKey::Microphone | InputKey::SystemAudio) {
                "opus"
            } else {
                plan.get(key).map_or("h264", |a| a.enc.codec().name())
            };
            let mut rec = FileRec {
                file: InputFile {
                    input: key,
                    label: label.clone(),
                    path: path.clone(),
                    status: InputStatus::Starting,
                    bytes: 0,
                    waiting_for_frame: false,
                    quality: None,
                },
                meta: ManifestInput {
                    key: key_string(key),
                    kind: kind.into(),
                    label,
                    file: file_name,
                    codec: codec.into(),
                    encoder: (codec != "opus")
                        .then(|| enc.map(|e| e.element_name().to_string()))
                        .flatten(),
                    convert: None,
                    capture: capture.map(|c| c.0),
                    width: None,
                    height: None,
                    framerate: None,
                    max_framerate: None,
                    first_pts_ns: None,
                    last_pts_ns: None,
                    status: "starting".into(),
                    error: None,
                    device,
                    transcript: None,
                    quality: None,
                },
                first: None,
                last: None,
                size: None,
                counters: None,
            };
            // Break-before-make (D4): no pipeline of this capture may still be out of Null.
            let blocked = capture.is_some_and(|c| self.pipes.iter().any(|p| p.capture == Some(c)));
            let built = if blocked {
                Err("a previous pipeline for this capture did not stop".to_string())
            } else {
                self.build_input(key, index, &path, &plan)
            };
            match built {
                Ok(ip) => {
                    rec.meta.convert = encoders::convert_name(ip.convert);
                    if let Some(e) = ip.encoder {
                        rec.meta.encoder = Some(e.element_name().to_string());
                        rec.meta.codec = e.codec().name().into();
                    }
                    match ip.rate {
                        Some(Rate::Cfr((n, d))) => rec.meta.framerate = Some(format!("{n}/{d}")),
                        Some(Rate::Native { cap, .. }) => {
                            rec.meta.framerate = Some("variable".into());
                            rec.meta.max_framerate = cap.map(|c| format!("{c}/1"));
                        }
                        None => {}
                    }
                    if ip.preview.is_some() {
                        self.preview_restarted(key);
                    }
                    rec.counters = Some(ip.counters.clone());
                    rec.first = Some(ip.first_pts.clone());
                    rec.last = Some(ip.last_pts.clone());
                    rec.size = Some(ip.size.clone());
                    if let Some(w) = &ip.warning
                        && self.warned.insert(w.clone())
                    {
                        self.notice(Severity::Warning, Some(key), w.clone());
                    }
                    started.push((key, capture, ip));
                }
                Err(reason) => {
                    self.notice_detail(Severity::Error, Some(key), "Input cannot record", &reason);
                    rec.file.status = InputStatus::Failed { reason };
                }
            }
            files.push(rec);
        }
        let seg = Seg {
            index,
            dir,
            status: SegmentStatus::Recording,
            base_ns: None,
            stop_ns: None,
            gap_ns: None,
            started_wall: None,
            started_at: None,
            timed_out: false,
            stop_reason: None,
            files,
        };
        if let Some(s) = self.session.as_mut() {
            s.segs.push(seg);
        }
        self.phase = Phase::Starting { segment: index };
        self.dirty = true;
        self.manifest_dirty = true;
        let pending_gap = gap_from;
        for (key, capture, ip) in started {
            let id = self.next_pipe;
            self.next_pipe += 1;
            let pipeline = ip.pipeline.clone();
            let bus = ip.bus.clone();
            self.pipes.push(Pipe {
                id,
                capture,
                pipeline: pipeline.clone(),
                bus,
                kind: PipeKind::Input {
                    seg: index,
                    key,
                    rec: ip,
                    resize_at: None,
                    _session_lock: session_lock.clone(),
                },
                stage: Stage::Pausing,
                deadline: Some(self.deadline_cap(Instant::now() + START_TIMEOUT)),
                outcome: None,
                abandoned: false,
                last_error: None,
                in_flight: None,
                null_pending: false,
            });
            self.spawn_state(id, pipeline, gst::State::Paused);
        }
        self.gap_from = pending_gap;
    }

    /// What each video input among `inputs` needs (the capacity plan's input).
    fn demands(&self, inputs: &[InputKey]) -> Vec<Demand> {
        let mut out = Vec::new();
        for &key in inputs {
            match key {
                InputKey::Microphone | InputKey::SystemAudio => {}
                InputKey::Camera => {
                    let Some(e) = self.selected_camera.and_then(|id| self.devices.camera(id))
                    else {
                        continue;
                    };
                    let Some(mode) = e.mode(self.camera_mode.as_deref()) else {
                        continue;
                    };
                    let (n, d) = mode.output_fps();
                    out.push(Demand {
                        key,
                        label: "Camera".into(),
                        size: mode.output_size(),
                        fps: n as f64 / d.max(1) as f64,
                        native: false,
                        gpu_path: mode.path == CameraPath::DmaBuf,
                        camera: true,
                    });
                }
                InputKey::Source(id) => {
                    let Some(s) = self.sources.iter().find(|s| s.info.id == id) else {
                        continue;
                    };
                    let size = match (s.info.kind, s.info.size) {
                        (SourceKind::TestPattern, _) => {
                            let (w, h) = self
                                .test_behaviour(id)
                                .size
                                .unwrap_or_else(recorder::test_size);
                            stamp::lock_size(w, h)
                        }
                        (_, Some((w, h))) => stamp::lock_size(w, h),
                        // The physical size is only known from negotiation: plan for UHD.
                        _ => encoders::UHD,
                    };
                    let (fps, native) = match s.info.frame_rate.cfr_fps() {
                        Some(f) => (f as f64, false),
                        None => (
                            s.info
                                .refresh
                                .map(|(n, d)| n as f64 / d.max(1) as f64)
                                .unwrap_or(DEFAULT_NATIVE_FPS),
                            true,
                        ),
                    };
                    out.push(Demand {
                        key,
                        label: s.info.label.clone(),
                        size,
                        fps,
                        native,
                        gpu_path: s.convert.or(self.caps.gpu_convert).is_some(),
                        camera: false,
                    });
                }
            }
        }
        out
    }

    fn test_behaviour(&self, id: SourceId) -> recorder::TestBehaviour {
        self.test_sources
            .iter()
            .find(|(i, _)| *i == id.0)
            .map(|(_, b)| *b)
            .unwrap_or_default()
    }

    fn plan_for(&self, inputs: &[InputKey]) -> Plan {
        plan::plan(&self.demands(inputs), &self.encoders, &self.gpu_encoders)
    }

    /// The plan for what Record would start now (Snapshot::capacity).
    fn capacity(&self) -> CapacityPlan {
        let p = self.plan_for(&self.frozen_inputs());
        CapacityPlan {
            inputs: p
                .inputs
                .iter()
                .map(|a| {
                    let size = self
                        .demands(&[a.key])
                        .first()
                        .map_or(encoders::UHD, |d| d.size);
                    PlannedInput {
                        input: a.key,
                        encoder: a.enc,
                        width: size.0.min(a.max.0),
                        height: size.1.min(a.max.1),
                        fps: a.fps,
                        native: a.native,
                        load_pct: (a.load * 100.0).round() as u32,
                    }
                })
                .collect(),
            gpu_load_pct: (p.gpu_load / plan::GPU_BUDGET * 100.0).round() as u32,
            cpu_load_pct: (p.cpu_load / plan::CPU_BUDGET * 100.0).round() as u32,
            over: p.over,
            warning: p.warning(),
            notes: p.notes.clone(),
        }
    }

    fn measured_storage_rate(&mut self) -> Option<u64> {
        let segment = match &self.phase {
            Phase::Recording { segment, .. } => *segment,
            _ => return None,
        };
        let bytes = self
            .session
            .as_ref()?
            .segs
            .iter()
            .find(|seg| seg.index == segment)?
            .files
            .iter()
            .map(|file| {
                std::fs::metadata(&file.file.path)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0)
            })
            .fold(0u64, u64::saturating_add);
        self.storage_rate_tracker
            .observe(segment, bytes, Instant::now())
            .filter(|rate| *rate > 0)
    }

    /// Refreshes the conservative plan rate and available space for the current selection.
    fn refresh_storage(&mut self) {
        let inputs = self.frozen_inputs();
        let demands = self.demands(&inputs);
        let plan = self.plan_for(&inputs);
        let mut video_kbps = Vec::new();
        for assigned in &plan.inputs {
            let Some(demand) = demands.iter().find(|d| d.key == assigned.key) else {
                continue;
            };
            let role = if demand.camera {
                Role::Camera
            } else {
                Role::Screen
            };
            let width = demand.size.0.min(assigned.max.0);
            let height = demand.size.1.min(assigned.max.1);
            let fps = assigned.fps.ceil().clamp(1.0, i32::MAX as f64) as i32;
            video_kbps.push(encoders::bitrate_kbps(
                role,
                assigned.enc.codec(),
                width,
                height,
                (fps, 1),
            ));
        }
        let planned_rate = storage::estimate_bytes_per_second(
            &video_kbps,
            inputs.contains(&InputKey::Microphone),
            inputs.contains(&InputKey::SystemAudio),
        );
        let measured_rate = self.measured_storage_rate();
        let rate = measured_rate.unwrap_or(planned_rate);
        let free = match self.storage_free_bytes_override {
            Some(bytes) => Ok(bytes),
            None => storage::available_bytes(&self.output_root),
        };
        let free_bytes = free.as_ref().ok().copied();
        let reserve = storage::stop_reserve_bytes(rate, self.min_free_mb);
        let required_before_start = reserve.saturating_add(storage::EMERGENCY_RESERVE_BYTES);
        let record_blocked_reason = if inputs.is_empty() {
            None
        } else {
            match &free {
                Err(e) => Some(format!("Cannot check output free space: {e}")),
                Ok(bytes) if *bytes < required_before_start => Some(format!(
                    "Not enough free space to start safely ({} bytes available; {} bytes including the emergency stop reserve required)",
                    bytes, required_before_start
                )),
                Ok(_) => None,
            }
        };
        let next = StorageInfo {
            free_bytes,
            estimated_bytes_per_second: rate,
            rate_measured: measured_rate.is_some(),
            remaining_seconds: free_bytes
                .and_then(|bytes| storage::remaining_seconds(bytes.saturating_sub(reserve), rate)),
            stop_reserve_bytes: reserve,
            record_blocked_reason,
        };
        if self.storage != next {
            self.storage = next;
            self.dirty = true;
        }
    }

    /// Monitors free space once per second and sends one warning at each threshold per session.
    fn monitor_storage(&mut self) {
        if !matches!(self.phase, Phase::Recording { .. }) {
            return;
        }
        let Some(free_bytes) = self.storage.free_bytes else {
            self.stop_for_storage("stopped: could not verify free space");
            return;
        };
        let level = self
            .storage
            .remaining_seconds
            .map(storage::warning_level)
            .unwrap_or(0);
        if level > self.storage_warning_level {
            self.storage_warning_level = level;
            let message = if level >= 2 {
                "Less than 3 minutes remain before the storage stop reserve"
            } else {
                "Less than 10 minutes remain before the storage stop reserve"
            };
            self.notice(Severity::Warning, None, message);
        }
        if storage::stop_threshold_reached(free_bytes, self.storage.stop_reserve_bytes) {
            self.stop_for_storage("stopped: disk almost full");
        }
    }

    fn release_session_reserve(&mut self, severity: Severity) {
        let Some(path) = self
            .session
            .as_ref()
            .and_then(|session| session.reserve_path.clone())
        else {
            return;
        };
        match storage::release_emergency_reserve(&path) {
            Ok(()) => {
                if let Some(session) = self.session.as_mut()
                    && session.reserve_path.as_deref() == Some(path.as_path())
                {
                    session.reserve_path = None;
                }
            }
            Err(error) => self.notice(
                severity,
                None,
                format!("Could not release emergency stop space: {error}"),
            ),
        }
    }

    /// Use the ordinary shared-deadline EOS path for every input, retaining incomplete status.
    fn stop_for_storage(&mut self, reason: &str) {
        let Some(segment) = self.current_seg() else {
            return;
        };
        if let Some(g) = self.seg_mut(segment) {
            g.stop_reason = Some(reason.to_string());
        }
        self.release_session_reserve(Severity::Error);
        self.cut_pending = false;
        self.record_pending = false;
        self.finalize_segment(segment);
        self.phase = Phase::Stopping { segment };
        self.notice(Severity::Warning, None, reason);
        self.dirty = true;
        self.manifest_dirty = true;
    }

    /// Encoder and timing for a planned video input; `limit` caps the size.
    fn video_target(
        &self,
        plan: &Plan,
        key: InputKey,
        cfr: (i32, i32),
        limit: (i32, i32),
    ) -> Result<(VideoEncoder, recorder::VideoTarget), String> {
        let a = plan
            .get(key)
            .ok_or("no video encoder can record this input")?;
        let demand = self.demands(&[key]).into_iter().next();
        let rate = if a.native {
            let lowered = demand.is_some_and(|d| a.fps < d.fps - 0.5);
            // Unknown refresh: the plan assumed DEFAULT_NATIVE_FPS, so ask the source for no
            // more than that (it could otherwise send up to its real refresh, beyond the plan).
            let unknown = match key {
                InputKey::Source(id) => self
                    .sources
                    .iter()
                    .find(|s| s.info.id == id)
                    .is_some_and(|s| s.info.refresh.is_none()),
                _ => false,
            };
            Rate::Native {
                cap: (lowered || unknown).then(|| a.fps.round() as i32),
                gop_fps: a.fps.ceil() as i32,
            }
        } else {
            Rate::Cfr(cfr)
        };
        Ok((
            a.enc,
            recorder::VideoTarget {
                rate,
                max: (limit.0.min(a.max.0), limit.1.min(a.max.1)),
            },
        ))
    }

    fn build_input(
        &self,
        key: InputKey,
        seg: u32,
        path: &Path,
        plan: &Plan,
    ) -> Result<InputPipeline, String> {
        let name = format!("seg{seg:03}-{}", key_string(key).replace(':', "-"));
        let tap = self.preview_on.then(|| {
            Tap::new(
                &self.previews,
                key,
                PreviewOrigin::Recording { segment: seg },
            )
        });
        match key {
            InputKey::Microphone => {
                let e = self
                    .selected_microphone
                    .and_then(|id| self.devices.microphone(id))
                    .ok_or("the microphone is gone")?;
                recorder::build_mic(&name, e, &self.clock, path)
            }
            InputKey::SystemAudio => {
                if self.system_audio_test_tone {
                    return recorder::build_system_audio_test_tone(&name, &self.clock, path);
                }
                let id = self
                    .selected_system_audio
                    .ok_or("system audio is not selected")?;
                let serial = if id == DEFAULT_SYSTEM_AUDIO_ID {
                    None
                } else {
                    Some(
                        self.devices
                            .system_audio(id)
                            .and_then(|e| e.serial.as_deref())
                            .ok_or("the selected output is gone")?,
                    )
                };
                recorder::build_system_audio(&name, serial, &self.clock, path)
            }
            InputKey::Camera => {
                let e = self
                    .selected_camera
                    .and_then(|id| self.devices.camera(id))
                    .ok_or("the camera is gone")?;
                if let Some(CameraClass::Unsupported(r)) = &e.class {
                    return Err(r.clone());
                }
                let mode = e
                    .mode(self.camera_mode.as_deref())
                    .ok_or("camera is not usable")?
                    .clone();
                // Native size and rate (≤60 fps); downscaled only above 4096×2160.
                let (enc, target) = self.video_target(
                    plan,
                    key,
                    mode.output_fps(),
                    (devices::CAMERA_MAX_W, devices::CAMERA_MAX_H),
                )?;
                recorder::build_video(
                    &name,
                    VideoSource::Camera { entry: e, mode },
                    self.caps.gpu_convert,
                    enc,
                    Role::Camera,
                    target,
                    &self.clock,
                    path,
                    tap,
                )
            }
            InputKey::Source(id) => {
                let s = self
                    .sources
                    .iter()
                    .find(|s| s.info.id == id)
                    .ok_or("the source is gone")?;
                let convert = s.convert.or(self.caps.gpu_convert);
                let source = match s.info.kind {
                    SourceKind::TestPattern => VideoSource::Test(self.test_behaviour(id)),
                    _ => {
                        let fd = s
                            .info
                            .capture
                            .and_then(|c| self.captures.get(&c))
                            .and_then(|c| c.fd.as_ref())
                            .ok_or("the screen share is gone")?
                            .as_raw_fd();
                        VideoSource::Portal {
                            fd,
                            node: s.node.ok_or("no PipeWire node")?,
                        }
                    }
                };
                let cfr = (s.info.frame_rate.cfr_fps().unwrap_or(self.screen_fps), 1);
                let (enc, target) =
                    self.video_target(plan, key, cfr, (stamp::MAX_W, stamp::MAX_H))?;
                recorder::build_video(
                    &name,
                    source,
                    convert,
                    enc,
                    Role::Screen,
                    target,
                    &self.clock,
                    path,
                    tap,
                )
            }
        }
    }

    /// Stop step 1-2 (§6): stop_clock, then finalize every live input with one shared deadline.
    fn finalize_segment(&mut self, segment: u32) {
        let stop = self.clock.time().nseconds();
        if let Some(seg) = self.seg_mut(segment)
            && seg.stop_ns.is_none()
        {
            seg.stop_ns = Some(stop);
        }
        let deadline = self.deadline_cap(Instant::now() + FINALIZE_TIMEOUT);
        self.finalize_deadline = Some(deadline);
        let preserve_partial_files = self
            .session
            .as_ref()
            .and_then(|session| session.segs.iter().find(|seg| seg.index == segment))
            .is_some_and(|seg| seg.stop_reason.is_some());
        let ids: Vec<(u64, Stage)> = self
            .pipes
            .iter()
            .filter(|p| !p.abandoned && p.input().is_some_and(|(s, _)| s == segment))
            .map(|p| (p.id, p.stage.clone()))
            .collect();
        for (id, stage) in ids {
            if preserve_partial_files {
                match storage_finalize_action(&stage) {
                    StorageFinalizeAction::GracefulEos => self.finalize_input(id, true),
                    StorageFinalizeAction::InjectEos => {
                        self.preserve_storage_input(id);
                        self.finalize_input(id, false);
                    }
                    StorageFinalizeAction::AwaitState => self.preserve_storage_input(id),
                    StorageFinalizeAction::AlreadyStopping => self.preserve_storage_input(id),
                }
            } else {
                match stage {
                    Stage::Running { .. } => self.finalize_input(id, true),
                    Stage::Pausing | Stage::Paused | Stage::Starting => self.request_null(
                        id,
                        Some(Outcome::Discard("stopped before recording started".into())),
                    ),
                    Stage::Draining { .. } | Stage::Nulling => {}
                }
            }
        }
        self.manifest_dirty = true;
        self.dirty = true;
    }

    fn preserve_storage_input(&mut self, id: u64) {
        if let Some(pipe) = self.pipes.iter_mut().find(|pipe| pipe.id == id) {
            pipe.outcome = storage_stop_outcome(pipe.outcome.take());
        }
    }

    fn storage_stop_pending_for(&self, id: u64) -> bool {
        let Some(pipe) = self.pipes.iter().find(|pipe| pipe.id == id) else {
            return false;
        };
        matches!(self.phase, Phase::Stopping { segment } if pipe.input().is_some_and(|(s, _)| s == segment))
            && matches!(pipe.outcome, Some(Outcome::Failed(_)))
    }

    /// Finalizes one input (§6): healthy → pipeline EOS then inject after 1 s; broken → inject
    /// EOS at the first element after the source right away.
    fn finalize_input(&mut self, id: u64, healthy: bool) {
        let deadline = self.deadline_cap(
            self.finalize_deadline
                .unwrap_or_else(|| Instant::now() + FINALIZE_TIMEOUT),
        );
        let stop_ns = self
            .pipes
            .iter()
            .find(|p| p.id == id)
            .and_then(Pipe::input)
            .and_then(|(seg, _)| {
                self.session
                    .as_ref()?
                    .segs
                    .iter()
                    .find(|g| g.index == seg)?
                    .stop_ns
            });
        let Some(p) = self.pipes.iter_mut().find(|p| p.id == id) else {
            return;
        };
        let PipeKind::Input {
            seg,
            key,
            rec,
            _session_lock,
            ..
        } = &p.kind
        else {
            return;
        };
        let (seg, key, session_lock) = (*seg, *key, _session_lock.clone());
        match p.stage {
            Stage::Running { .. } => {}
            Stage::Paused if !healthy => {}
            Stage::Draining {
                inject_at: Some(_),
                deadline: d,
            } if !healthy => {
                // Already draining via the pipeline; escalate to injection now.
                inject_eos(rec.eos_target.clone(), id, session_lock.clone());
                p.stage = Stage::Draining {
                    inject_at: None,
                    deadline: d,
                };
                return;
            }
            _ => return,
        }
        if healthy {
            let pipeline = p.pipeline.clone();
            // Variable rate: the last frame again at the stop time, so the file lasts the whole
            // segment even when the source sent nothing since (a static window).
            let base = rec.base.load(Ordering::Acquire);
            let tail = rec.tail.clone().zip(stop_ns.filter(|_| base != UNSET));
            let worker_lock = session_lock.clone();
            spawn_named(format!("chinstrap-gst-eos-{id}"), move || {
                let _session_lock = worker_lock;
                if let Some((t, stop)) = tail {
                    t.push_at(stop.saturating_sub(base));
                }
                pipeline.send_event(gst::event::Eos::new());
            });
            p.stage = Stage::Draining {
                inject_at: Some(Instant::now() + EOS_GRACE),
                deadline,
            };
        } else {
            inject_eos(rec.eos_target.clone(), id, session_lock.clone());
            p.stage = Stage::Draining {
                inject_at: None,
                deadline,
            };
        }
        p.deadline = None;
        self.set_file_status(seg, key, InputStatus::Finalizing);
    }

    /// Requests Null on a worker. `outcome` (if any) is applied when Null is reached.
    fn request_null(&mut self, id: u64, outcome: Option<Outcome>) {
        let deadline = self.deadline_cap(Instant::now() + NULL_TIMEOUT);
        let Some(p) = self.pipes.iter_mut().find(|p| p.id == id) else {
            return;
        };
        if outcome.is_some() && p.outcome.is_none() {
            p.outcome = outcome;
        }
        if p.stage == Stage::Nulling {
            return;
        }
        p.stage = Stage::Nulling;
        p.deadline = Some(deadline);
        if p.in_flight.is_some_and(|s| s != gst::State::Null) {
            // Never race an in-flight Paused/Playing worker (§4.1: closed only once Null).
            p.null_pending = true;
            return;
        }
        let pipeline = p.pipeline.clone();
        self.spawn_state(id, pipeline, gst::State::Null);
    }

    fn spawn_state(&mut self, id: u64, pipeline: gst::Pipeline, target: gst::State) {
        if let Some(p) = self.pipes.iter_mut().find(|p| p.id == id) {
            p.in_flight = Some(target);
        }
        let tx = self.tx.clone();
        let session_lock = self
            .pipes
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| match &p.kind {
                PipeKind::Input { _session_lock, .. } => Some(_session_lock.clone()),
                PipeKind::Probe { .. } | PipeKind::Preview { .. } => None,
            });
        spawn_named(format!("chinstrap-gst-worker-{id}"), move || {
            let _session_lock = session_lock;
            let result = match pipeline.set_state(target) {
                Ok(gst::StateChangeSuccess::Async) if target == gst::State::Null => {
                    Err("GStreamer did not confirm the asynchronous NULL transition".to_string())
                }
                Ok(_) => Ok(()),
                Err(error) => Err(error.to_string()),
            };
            drop(pipeline);
            let _ = tx.send(Msg::Worker(WorkerResult { id, target, result }));
        });
    }

    /// Disk-full is a segment-wide failure: preserve the affected file, set a terminal failure
    /// outcome before the shared EOS path, and never route it through early-error discard.
    fn disk_full_error(&mut self, id: u64, detail: String) {
        let Some((segment, key)) = self.pipes.iter().find(|p| p.id == id).and_then(Pipe::input)
        else {
            return;
        };
        let reason = format!("disk is full: {detail}");
        if let Some(pipe) = self.pipes.iter_mut().find(|p| p.id == id)
            && pipe.outcome.is_none()
        {
            pipe.outcome = Some(Outcome::Failed(reason.clone()));
        }
        self.set_file_status(segment, key, InputStatus::Failed { reason });
        self.notice(
            Severity::Error,
            Some(key),
            "The disk is full; finalising all recording inputs together",
        );
        self.stop_for_storage("stopped: disk almost full");
    }

    /// Marks an input failed. Before recording (or within the first 3 s with nothing muxed):
    /// Null + delete. Otherwise: finalize by EOS injection, keep the file (§5.1).
    fn fail_input(&mut self, id: u64, reason: String) {
        let Some(p) = self.pipes.iter().find(|p| p.id == id) else {
            return;
        };
        let Some((seg, key)) = p.input() else {
            return;
        };
        let early = match (&p.stage, &p.kind) {
            (Stage::Pausing | Stage::Paused | Stage::Starting, _) => true,
            (Stage::Running { since }, PipeKind::Input { rec, .. }) => {
                since.elapsed() < EARLY_ERROR && rec.first_pts.load(Ordering::Acquire) == UNSET
            }
            _ => false,
        };
        let stage = p.stage.clone();
        let already = matches!(
            self.session
                .as_ref()
                .and_then(|s| s.segs.iter().find(|g| g.index == seg))
                .and_then(|g| g.files.iter().find(|f| f.file.input == key))
                .map(|f| &f.file.status),
            Some(InputStatus::Failed { .. })
        );
        if !already {
            self.notice_detail(Severity::Error, Some(key), "Input failed", &reason);
        }
        self.set_file_status(
            seg,
            key,
            InputStatus::Failed {
                reason: reason.clone(),
            },
        );
        match stage {
            Stage::Nulling => {}
            _ if early => self.request_null(id, Some(Outcome::Discard(reason))),
            Stage::Running { .. } | Stage::Draining { .. } => {
                if let Some(p) = self.pipes.iter_mut().find(|p| p.id == id) {
                    p.outcome = Some(Outcome::Failed(reason));
                }
                self.finalize_input(id, false);
            }
            _ => self.request_null(id, Some(Outcome::Failed(reason))),
        }
    }

    fn worker_result(&mut self, w: WorkerResult) {
        if let Some(p) = self.pipes.iter_mut().find(|p| p.id == w.id)
            && p.in_flight == Some(w.target)
        {
            p.in_flight = None;
        }
        if w.target == gst::State::Null
            && let Err(error) = &w.result
        {
            let input = self
                .pipes
                .iter()
                .find(|p| p.id == w.id)
                .and_then(Pipe::input);
            if let Some((seg, key)) = input {
                let reason = format!("{NOT_FINALIZED}: could not confirm NULL state: {error}");
                if let Some(p) = self.pipes.iter_mut().find(|p| p.id == w.id) {
                    p.abandoned = true;
                    p.deadline = None;
                    p.null_pending = false;
                    p.last_error = Some(error.clone());
                    p.outcome = Some(Outcome::Failed(reason.clone()));
                }
                self.set_file_status(
                    seg,
                    key,
                    InputStatus::Failed {
                        reason: reason.clone(),
                    },
                );
                self.notice(Severity::Error, Some(key), format!("{key:?}: {reason}"));
                if self.shutdown.is_some() {
                    self.shutdown_clean = false;
                }
            } else {
                self.remove_pipe(w.id);
            }
            return;
        }
        // A Discard result can unlink its file. Poll peer buses and the filesystem before that
        // cleanup, otherwise a pending ENOSPC or reserve threshold can lose the race to removal.
        let may_discard = w.target == gst::State::Null
            && self
                .pipes
                .iter()
                .any(|p| p.id == w.id && matches!(p.outcome.as_ref(), Some(Outcome::Discard(_))));
        if may_discard {
            let ids: Vec<u64> = self.pipes.iter().map(|p| p.id).collect();
            for id in ids {
                self.drain_bus(id);
            }
            self.refresh_storage();
            self.monitor_storage();
        } else {
            // Pick up an error posted during this pipeline's transition, for a better reason.
            self.drain_bus(w.id);
        }
        // A Null deferred behind this transition runs now.
        if w.target != gst::State::Null
            && let Some(p) = self.pipes.iter_mut().find(|p| p.id == w.id)
            && p.null_pending
            && p.in_flight.is_none()
        {
            p.null_pending = false;
            let pipeline = p.pipeline.clone();
            self.spawn_state(w.id, pipeline, gst::State::Null);
            return;
        }
        let Some(p) = self.pipes.iter().find(|p| p.id == w.id) else {
            return;
        };
        let reason = |p: &Pipe, e: &str| p.last_error.clone().unwrap_or_else(|| e.to_string());
        match (w.target, &p.stage) {
            (gst::State::Null, _) => {
                if p.stage == Stage::Nulling || p.abandoned {
                    self.remove_pipe(w.id);
                }
            }
            (gst::State::Paused, Stage::Pausing) => match &w.result {
                Ok(()) => {
                    if let Some(p) = self.pipes.iter_mut().find(|p| p.id == w.id) {
                        p.stage = Stage::Paused;
                        p.deadline = None;
                    }
                    if self.storage_stop_pending_for(w.id) {
                        self.finalize_input(w.id, false);
                    }
                }
                Err(e) => {
                    let r = reason(p, e);
                    self.fail_input(w.id, r);
                }
            },
            (gst::State::Playing, Stage::Starting) => {
                let is_probe = matches!(p.kind, PipeKind::Probe { .. });
                let is_preview = p.is_preview();
                match &w.result {
                    Ok(()) => {
                        let input = p.input();
                        if let Some(p) = self.pipes.iter_mut().find(|p| p.id == w.id) {
                            p.stage = Stage::Running {
                                since: Instant::now(),
                            };
                            if !is_probe || is_preview {
                                p.deadline = None;
                            }
                            if let PipeKind::Input {
                                seg: 1,
                                rec,
                                resize_at,
                                ..
                            } = &mut p.kind
                                && rec.resize_filter.is_some()
                            {
                                *resize_at = self.resize_after.map(|d| Instant::now() + d);
                            }
                        }
                        if let Some((seg, key)) = input {
                            self.set_file_status(seg, key, InputStatus::Recording);
                            if let Phase::Starting { segment } = self.phase
                                && segment == seg
                            {
                                let started_at = self
                                    .session
                                    .as_ref()
                                    .and_then(|s| s.segs.iter().find(|g| g.index == seg))
                                    .and_then(|g| g.started_at)
                                    .unwrap_or_else(Instant::now);
                                self.phase = Phase::Recording {
                                    segment,
                                    started_at,
                                };
                                self.dirty = true;
                            }
                        }
                        if self.storage_stop_pending_for(w.id) {
                            self.finalize_input(w.id, false);
                        }
                    }
                    Err(e) => {
                        let r = reason(p, e);
                        if is_probe {
                            self.probe_done(w.id, Err(r));
                        } else if is_preview {
                            self.preview_failed(w.id, r);
                        } else {
                            self.fail_input(w.id, r);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn remove_pipe(&mut self, id: u64) {
        let Some(idx) = self.pipes.iter().position(|p| p.id == id) else {
            return;
        };
        let p = self.pipes.remove(idx);
        let abandoned = p.abandoned;
        let outcome = p.outcome.clone();
        match p.kind {
            PipeKind::Input { seg, key, rec, .. } => {
                let path = self
                    .session
                    .as_ref()
                    .and_then(|s| s.segs.iter().find(|g| g.index == seg))
                    .and_then(|g| g.files.iter().find(|f| f.file.input == key))
                    .map(|f| f.file.path.clone());
                let bytes = path
                    .as_ref()
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map_or(0, |m| m.len());
                let quality = quality_of(&rec, bytes);
                let encoded = Counters::get(&rec.counters.encoded);
                drop(rec);
                drop(p.pipeline);
                if abandoned {
                    return;
                }
                if let Some(f) = self.file_mut(seg, key) {
                    f.file.quality = quality;
                    f.file.waiting_for_frame = false;
                }
                // An input that never produced a frame is a failure, never a complete file.
                let outcome = match outcome {
                    Some(Outcome::Complete) if encoded == 0 => {
                        self.notice(Severity::Error, Some(key), format!("{key:?}: {NO_FRAMES}"));
                        Some(Outcome::Discard(NO_FRAMES.into()))
                    }
                    o => o,
                };
                match outcome {
                    Some(Outcome::Complete) => {
                        self.set_file_status(seg, key, InputStatus::Complete)
                    }
                    Some(Outcome::Failed(r)) => {
                        self.set_file_status(seg, key, InputStatus::Failed { reason: r })
                    }
                    Some(Outcome::Discard(r)) => {
                        let reason = finish_discard(path.as_deref(), encoded, &r);
                        self.set_file_status(seg, key, InputStatus::Failed { reason })
                    }
                    None => self.set_file_status(
                        seg,
                        key,
                        InputStatus::Failed {
                            reason: "stopped".into(),
                        },
                    ),
                }
                if let Some(path) = path
                    && let Some(f) = self.file_mut(seg, key)
                {
                    f.file.bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                }
                self.dirty = true;
                self.manifest_dirty = true;
            }
            PipeKind::Probe {
                source,
                convert,
                result,
                caps,
            } => {
                drop(p.pipeline);
                if abandoned {
                    return;
                }
                let caps = caps.lock().ok().and_then(|c| c.clone());
                self.apply_probe(source, convert, result, caps);
            }
            PipeKind::Preview { .. } => drop(p.pipeline),
        }
    }

    // ---- bus ---------------------------------------------------------------------------------

    fn drain_bus(&mut self, id: u64) {
        let Some(p) = self.pipes.iter().find(|p| p.id == id) else {
            return;
        };
        let bus = p.bus.clone();
        while let Some(msg) = bus.pop() {
            self.bus_message(id, &msg);
        }
    }

    fn bus_message(&mut self, id: u64, msg: &gst::Message) {
        let Some(p) = self.pipes.iter_mut().find(|p| p.id == id) else {
            return;
        };
        let is_probe = matches!(p.kind, PipeKind::Probe { .. });
        let is_preview = p.is_preview();
        let input_key = p.input().map(|(_, key)| key);
        match msg.view() {
            gst::MessageView::Eos(_) => {
                if is_preview {
                    self.preview_failed(id, "the preview ended".into());
                } else if is_probe {
                    self.probe_done(id, Ok(()));
                } else if matches!(p.stage, Stage::Draining { .. }) {
                    let outcome = p.outcome.clone().unwrap_or(Outcome::Complete);
                    self.request_null(id, Some(outcome));
                } else if matches!(p.stage, Stage::Running { .. }) {
                    // The source ended by itself: the file is finalized, the input is done.
                    let reason = "source ended".to_string();
                    if let Some((seg, key)) = p.input() {
                        self.notice(Severity::Warning, Some(key), "Input ended unexpectedly");
                        self.set_file_status(
                            seg,
                            key,
                            InputStatus::Failed {
                                reason: reason.clone(),
                            },
                        );
                    }
                    self.request_null(id, Some(Outcome::Failed(reason)));
                }
            }
            gst::MessageView::Error(e) => {
                let out_of_space = e.error().matches(gst::ResourceError::NoSpaceLeft);
                let text = error_text(e);
                eprintln!(
                    "chinstrap: gst error in {} from {}: {text}",
                    p.pipeline.name(),
                    e.src()
                        .map(|s| s.path_string().to_string())
                        .unwrap_or_default()
                );
                if p.last_error.is_none() {
                    p.last_error = Some(text.clone());
                }
                if is_preview {
                    self.preview_failed(id, text);
                } else if is_probe {
                    self.probe_done(id, Err(text));
                } else if out_of_space {
                    self.disk_full_error(id, text);
                } else {
                    match p.stage {
                        Stage::Nulling => {}
                        Stage::Draining { .. } => {
                            if p.outcome.is_none() {
                                p.outcome = Some(Outcome::Failed(text.clone()));
                            }
                            self.finalize_input(id, false);
                        }
                        _ => self.fail_input(id, text),
                    }
                }
            }
            gst::MessageView::Warning(w) => {
                eprintln!(
                    "chinstrap: gst warning from {}: {} ({:?})",
                    p.pipeline.name(),
                    w.error(),
                    w.debug()
                );
            }
            gst::MessageView::ClockLost(_) => {
                // §5.0: logged and ignored; never cycle PAUSED→PLAYING (would change B).
                eprintln!("chinstrap: clock lost in {} (ignored)", p.pipeline.name());
            }
            gst::MessageView::Element(el) => {
                if let Some(s) = el.structure()
                    && s.name() == "level"
                    && matches!(self.phase, Phase::Recording { .. })
                    && let (Some(rms), Some(peak)) = (level_max(s, "rms"), level_max(s, "peak"))
                {
                    match input_key {
                        Some(InputKey::Microphone) if self.last_level.elapsed() >= LEVEL_EVERY => {
                            self.last_level = Instant::now();
                            let _ = self.events.send_blocking(EngineEvent::Level {
                                rms_db: rms,
                                peak_db: peak,
                            });
                        }
                        Some(InputKey::SystemAudio)
                            if self.last_system_level.elapsed() >= LEVEL_EVERY =>
                        {
                            self.last_system_level = Instant::now();
                            let _ = self.events.send_blocking(EngineEvent::SystemAudioLevel {
                                rms_db: rms,
                                peak_db: peak,
                            });
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    // ---- portal ------------------------------------------------------------------------------

    fn portal_outcome(&mut self, outcome: PortalOutcome) {
        match outcome {
            PortalOutcome::Availability(a) => {
                self.caps.portal = a;
                self.dirty = true;
            }
            PortalOutcome::Opened {
                capture,
                streams,
                fd,
            } => {
                self.portal_busy = false;
                self.dirty = true;
                if self.shutdown.is_some() || !self.captures.contains_key(&capture) {
                    drop(fd);
                    self.release_capture(capture);
                    return;
                }
                if let Some(c) = self.captures.get_mut(&capture) {
                    c.fd = Some(fd);
                }
                let mut ids = Vec::new();
                for (node, kind, size) in streams {
                    let id = SourceId(self.next_source);
                    self.next_source += 1;
                    let kind = match kind {
                        Some(ashpd::desktop::screencast::SourceType::Window) => SourceKind::Window,
                        Some(ashpd::desktop::screencast::SourceType::Virtual) => {
                            SourceKind::Virtual
                        }
                        _ => SourceKind::Monitor,
                    };
                    let label = match kind {
                        SourceKind::Window => format!("Window {}", id.0),
                        _ => format!("Screen {}", id.0),
                    };
                    let state = if !self.video_ok() {
                        SourceState::Unsupported {
                            reason: "no working video encoder".into(),
                        }
                    } else if self.caps.gpu_convert.is_none() {
                        SourceState::Unsupported {
                            reason: "no GPU conversion path (VA or GL)".into(),
                        }
                    } else {
                        SourceState::Checking
                    };
                    let checking = state == SourceState::Checking;
                    self.sources.push(Source {
                        info: SourceInfo {
                            id,
                            capture: Some(capture),
                            kind,
                            label,
                            logical_size: size,
                            state,
                            frame_rate: self.frame_rate_default,
                            refresh: None,
                            size: None,
                        },
                        node: Some(node),
                        convert: None,
                        gl_retry: false,
                    });
                    if checking {
                        ids.push(id);
                    }
                }
                if let Some(convert) = self.caps.gpu_convert {
                    for id in ids {
                        self.start_probe(id, convert);
                    }
                }
            }
            PortalOutcome::Failed {
                capture,
                reason,
                cancelled,
            } => {
                self.portal_busy = false;
                self.captures.remove(&capture);
                self.dirty = true;
                if cancelled {
                    self.notice(Severity::Info, None, "Picker cancelled");
                } else {
                    self.notice_detail(Severity::Error, None, "Screen picker failed", &reason);
                }
            }
            PortalOutcome::Closed { capture } => self.capture_closed(capture),
        }
    }

    fn capture_closed(&mut self, capture: CaptureId) {
        let mut lost = Vec::new();
        for s in self
            .sources
            .iter_mut()
            .filter(|s| s.info.capture == Some(capture))
        {
            s.info.state = SourceState::Lost {
                reason: "Share ended".into(),
            };
            lost.push(s.info.id);
        }
        self.dirty = true;
        if lost.is_empty() {
            return;
        }
        self.notice(
            Severity::Warning,
            Some(InputKey::Source(lost[0])),
            format!("Screen share ended ({} source(s) lost)", lost.len()),
        );
        let ids: Vec<u64> = self
            .pipes
            .iter()
            .filter(|p| !p.abandoned)
            .filter_map(|p| match p.input() {
                Some((_, InputKey::Source(sid))) if lost.contains(&sid) => Some(p.id),
                _ => None,
            })
            .collect();
        for id in ids {
            self.fail_input(id, "Source lost".into());
        }
    }

    fn release_capture(&mut self, capture: CaptureId) {
        if let Some(c) = self.captures.remove(&capture) {
            drop(c.fd);
        }
        if let Some((ptx, _)) = &self.portal {
            let _ = ptx.try_send(PortalCmd::Release { capture });
        }
    }

    /// Closes fds of captures with no remaining sources and no pipeline out of Null (§4.1).
    fn release_idle_captures(&mut self) {
        let idle: Vec<CaptureId> = self
            .captures
            .iter()
            .filter(|(id, c)| {
                c.fd.is_some()
                    && !self.sources.iter().any(|s| s.info.capture == Some(**id))
                    && !self.pipes.iter().any(|p| p.capture == Some(**id))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in idle {
            self.release_capture(id);
        }
    }

    fn start_probe(&mut self, source: SourceId, convert: GpuConvert) {
        let Some(s) = self.sources.iter().find(|s| s.info.id == source) else {
            return;
        };
        let Some(capture) = s.info.capture else {
            return;
        };
        let Some(fd) = self
            .captures
            .get(&capture)
            .and_then(|c| c.fd.as_ref())
            .map(|f| f.as_raw_fd())
        else {
            return;
        };
        let Some(node) = s.node else {
            return;
        };
        let name = format!("probe-{}-{convert:?}", source.0);
        match recorder::build_probe(&name, fd, node, convert, self.caps.h264_encoder) {
            Ok((pipeline, bus, caps)) => {
                let id = self.next_pipe;
                self.next_pipe += 1;
                self.pipes.push(Pipe {
                    id,
                    capture: Some(capture),
                    pipeline: pipeline.clone(),
                    bus,
                    kind: PipeKind::Probe {
                        source,
                        convert,
                        result: None,
                        caps,
                    },
                    stage: Stage::Starting,
                    deadline: Some(Instant::now() + PROBE_TIMEOUT),
                    outcome: None,
                    abandoned: false,
                    last_error: None,
                    in_flight: None,
                    null_pending: false,
                });
                self.spawn_state(id, pipeline, gst::State::Playing);
            }
            Err(e) => self.apply_probe(source, convert, Some(Err(e)), None),
        }
    }

    fn probe_done(&mut self, id: u64, result: Result<(), String>) {
        let Some(p) = self.pipes.iter_mut().find(|p| p.id == id) else {
            return;
        };
        if let PipeKind::Probe { result: slot, .. } = &mut p.kind
            && slot.is_none()
        {
            *slot = Some(result);
        }
        self.request_null(id, None);
    }

    /// Applies a negotiation probe result once its pipeline is Null and dropped.
    fn apply_probe(
        &mut self,
        source: SourceId,
        convert: GpuConvert,
        result: Option<Result<(), String>>,
        caps: Option<gst::Caps>,
    ) {
        let gl_ok = encoders::convert_elements(GpuConvert::Gl)
            .iter()
            .all(|e| encoders::has_element(e));
        let Some(s) = self.sources.iter_mut().find(|s| s.info.id == source) else {
            return;
        };
        if s.info.state != SourceState::Checking {
            return;
        }
        match result.unwrap_or_else(|| Err("probe stopped".into())) {
            Ok(()) => {
                s.info.state = SourceState::Ready;
                s.convert = Some(convert);
                if let Some(caps) = &caps {
                    let facts = recorder::source_facts(caps);
                    s.info.size = facts.size.or(s.info.size);
                    s.info.refresh = facts.max_rate.or(s.info.refresh);
                    eprintln!(
                        "chinstrap: {}: source {:?} at up to {:?} fps",
                        s.info.label, s.info.size, s.info.refresh
                    );
                }
            }
            Err(e) => {
                // Any VA failure is retried with GL: a DMA-BUF modifier mismatch surfaces first
                // as "no more input formats", before the not-negotiated flow error (§5.2).
                if convert == GpuConvert::Va && gl_ok && !s.gl_retry {
                    // Retried by tick() once no pipeline of this capture is out of Null.
                    s.gl_retry = true;
                } else {
                    s.info.state = SourceState::Unsupported { reason: e };
                }
            }
        }
        self.dirty = true;
    }

    fn start_gl_retries(&mut self) {
        let ready: Vec<SourceId> = self
            .sources
            .iter()
            .filter(|s| {
                s.gl_retry
                    && s.convert.is_none()
                    && s.info.state == SourceState::Checking
                    && !self.pipes.iter().any(|p| p.capture == s.info.capture)
            })
            .map(|s| s.info.id)
            .collect();
        for id in ready {
            if let Some(s) = self.sources.iter_mut().find(|s| s.info.id == id) {
                s.convert = Some(GpuConvert::Gl);
            }
            self.start_probe(id, GpuConvert::Gl);
            // `convert` is set again from the probe result.
            if let Some(s) = self.sources.iter_mut().find(|s| s.info.id == id) {
                s.convert = None;
            }
        }
    }

    // ---- previews ----------------------------------------------------------------------------

    /// Inputs with a preview: the selected camera, then every source.
    fn preview_keys(&self) -> Vec<InputKey> {
        self.selected_camera
            .map(|_| InputKey::Camera)
            .into_iter()
            .chain(self.sources.iter().map(|s| InputKey::Source(s.info.id)))
            .collect()
    }

    /// Idle previews to run now, each with what it shows. Every source of a capture that is
    /// still being checked waits: its probe is the node's consumer until then.
    fn preview_wanted(&self) -> Vec<(InputKey, String)> {
        let mut out = Vec::new();
        if self.input_recordable(InputKey::Camera) {
            out.push((
                InputKey::Camera,
                format!("{:?}/{:?}", self.selected_camera, self.camera_mode),
            ));
        }
        for s in &self.sources {
            let checking = s.info.capture.is_some()
                && self.sources.iter().any(|o| {
                    o.info.capture == s.info.capture && o.info.state == SourceState::Checking
                });
            if !checking && self.input_recordable(InputKey::Source(s.info.id)) {
                out.push((InputKey::Source(s.info.id), format!("{:?}", s.convert)));
            }
        }
        out
    }

    fn set_preview_state(&mut self, key: InputKey, state: PreviewState) {
        if self.preview_state.get(&key) != Some(&state) {
            self.preview_state.insert(key, state);
            self.dirty = true;
        }
    }

    /// A preview of `key` (idle pipeline or recording branch) started; Live with its next frame.
    fn preview_restarted(&mut self, key: InputKey) {
        self.preview_seen.insert(key, self.previews.generation(key));
        self.set_preview_state(key, PreviewState::Starting);
    }

    fn stop_previews(&mut self) {
        let ids: Vec<u64> = self
            .pipes
            .iter()
            .filter(|p| p.is_preview() && !p.abandoned)
            .map(|p| p.id)
            .collect();
        for id in ids {
            self.request_null(id, None);
        }
    }

    /// An idle preview failed: stopped and retried after [`PREVIEW_RETRY`]. Never a notice and
    /// never an input failure.
    fn preview_failed(&mut self, id: u64, reason: String) {
        let key = match self.pipes.iter().find(|p| p.id == id) {
            Some(p) if p.stage == Stage::Nulling => return,
            Some(Pipe {
                kind: PipeKind::Preview { key, .. },
                ..
            }) => *key,
            _ => return,
        };
        eprintln!("chinstrap: preview of {key:?} stopped: {reason}");
        self.preview_retry
            .insert(key, Instant::now() + PREVIEW_RETRY);
        self.set_preview_state(key, PreviewState::Unavailable { reason });
        self.request_null(id, None);
    }

    fn start_preview(&mut self, key: InputKey, ident: String) {
        let tap = Tap::new(&self.previews, key, PreviewOrigin::Idle);
        let name = format!("preview-{}", key_string(key).replace(':', "-"));
        let mut capture = None;
        let built = match key {
            InputKey::Microphone | InputKey::SystemAudio => return,
            InputKey::Camera => match self.selected_camera.and_then(|id| self.devices.camera(id)) {
                Some(entry) => match entry.mode(self.camera_mode.as_deref()) {
                    Some(mode) => recorder::build_preview(
                        &name,
                        VideoSource::Camera {
                            entry,
                            mode: mode.clone(),
                        },
                        self.caps.gpu_convert,
                        &tap,
                    ),
                    None => Err("camera is not usable".into()),
                },
                None => Err("the camera is gone".into()),
            },
            InputKey::Source(id) => {
                let Some(s) = self.sources.iter().find(|s| s.info.id == id) else {
                    return;
                };
                let convert = s.convert.or(self.caps.gpu_convert);
                match s.info.kind {
                    SourceKind::TestPattern => recorder::build_preview(
                        &name,
                        VideoSource::Test(self.test_behaviour(id)),
                        convert,
                        &tap,
                    ),
                    _ => {
                        capture = s.info.capture;
                        let fd = s
                            .info
                            .capture
                            .and_then(|c| self.captures.get(&c))
                            .and_then(|c| c.fd.as_ref())
                            .map(|f| f.as_raw_fd());
                        match (fd, s.node) {
                            (Some(fd), Some(node)) => recorder::build_preview(
                                &name,
                                VideoSource::Portal { fd, node },
                                convert,
                                &tap,
                            ),
                            _ => Err("the screen share is gone".into()),
                        }
                    }
                }
            }
        };
        match built {
            Ok((pipeline, bus)) => {
                let id = self.next_pipe;
                self.next_pipe += 1;
                self.pipes.push(Pipe {
                    id,
                    capture,
                    pipeline: pipeline.clone(),
                    bus,
                    kind: PipeKind::Preview { key, ident },
                    stage: Stage::Starting,
                    deadline: Some(Instant::now() + PREVIEW_START),
                    outcome: None,
                    abandoned: false,
                    last_error: None,
                    in_flight: None,
                    null_pending: false,
                });
                self.spawn_state(id, pipeline, gst::State::Playing);
                self.preview_restarted(key);
            }
            Err(reason) => {
                self.preview_retry
                    .insert(key, Instant::now() + PREVIEW_RETRY);
                self.set_preview_state(key, PreviewState::Unavailable { reason });
            }
        }
    }

    /// Stop (or the shortcut again) before a pending Record started: nothing is recorded and the
    /// idle previews come back.
    fn cancel_pending_record(&mut self) {
        self.record_pending = false;
        self.dirty = true;
    }

    /// A preview of `key` gave up on reaching Null: its pipeline may still hold the node or
    /// device until it does.
    fn preview_stuck(&self, key: InputKey) -> bool {
        self.pipes.iter().any(|p| {
            p.abandoned && matches!(&p.kind, PipeKind::Preview { key: k, .. } if *k == key)
        })
    }

    /// Record accepted while previews ran: starts the segment once every preview is Null.
    fn start_pending_record(&mut self) {
        if !self.record_pending || self.pipes.iter().any(|p| p.is_preview() && !p.abandoned) {
            return;
        }
        self.record_pending = false;
        self.dirty = true;
        if self.can_record() {
            self.begin_recording();
        } else {
            self.notice(
                Severity::Warning,
                None,
                "Recording did not start: nothing recordable is selected any more",
            );
        }
    }

    /// Runs exactly the idle previews that are wanted (none while busy) and keeps
    /// `Snapshot::previews` current.
    fn reconcile_previews(&mut self) {
        let keys = self.preview_keys();
        self.previews.retain(&keys);
        self.preview_state.retain(|k, _| keys.contains(k));
        if !self.preview_on {
            for key in keys {
                self.set_preview_state(
                    key,
                    PreviewState::Unavailable {
                        reason: "previews are off (CHINSTRAP_PREVIEW=0)".into(),
                    },
                );
            }
            return;
        }
        let idle = self.phase == Phase::Idle && !self.record_pending && self.shutdown.is_none();
        let wanted = if idle {
            self.preview_wanted()
        } else {
            Vec::new()
        };
        let stale: Vec<u64> = self
            .pipes
            .iter()
            .filter(|p| !p.abandoned && p.stage != Stage::Nulling)
            .filter(|p| match &p.kind {
                PipeKind::Preview { key, ident } => {
                    !wanted.iter().any(|(k, i)| k == key && i == ident)
                }
                _ => false,
            })
            .map(|p| p.id)
            .collect();
        for id in stale {
            self.request_null(id, None);
        }
        let now = Instant::now();
        for (key, ident) in &wanted {
            // One consumer per node: a replaced preview is started once the old one is gone.
            let present = self
                .pipes
                .iter()
                .any(|p| matches!(&p.kind, PipeKind::Preview { key: k, .. } if k == key));
            if !present && self.preview_retry.get(key).is_none_or(|t| now >= *t) {
                self.start_preview(*key, ident.clone());
            }
        }
        // A recording branch that failed (the recording goes on).
        let killed: Vec<InputKey> = self
            .pipes
            .iter()
            .filter_map(|p| match &p.kind {
                PipeKind::Input { key, rec, .. }
                    if rec
                        .preview
                        .as_ref()
                        .is_some_and(|k| k.load(Ordering::Acquire)) =>
                {
                    Some(*key)
                }
                _ => None,
            })
            .collect();
        for key in killed {
            self.set_preview_state(
                key,
                PreviewState::Unavailable {
                    reason: "the preview stopped; recording continues".into(),
                },
            );
        }
        for key in keys {
            match self.preview_state.get(&key) {
                // No new consumer while the old one may still hold the node.
                _ if idle && self.preview_stuck(key) => {
                    self.set_preview_state(
                        key,
                        PreviewState::Unavailable {
                            reason: "the previous preview did not stop".into(),
                        },
                    );
                }
                Some(PreviewState::Starting)
                    if self.previews.generation(key)
                        > self.preview_seen.get(&key).copied().unwrap_or(0) =>
                {
                    self.set_preview_state(key, PreviewState::Live);
                }
                // Idle and not previewable: say why.
                _ if idle
                    && !wanted.iter().any(|(k, _)| *k == key)
                    && self.preview_retry.get(&key).is_none_or(|t| now >= *t) =>
                {
                    let state = self.preview_blocked(key);
                    self.set_preview_state(key, state);
                }
                _ => {}
            }
        }
    }

    /// Why an idle input has no preview.
    fn preview_blocked(&self, key: InputKey) -> PreviewState {
        let source = match key {
            InputKey::Source(id) => self.sources.iter().find(|s| s.info.id == id),
            _ => None,
        };
        match source.map(|s| &s.info.state) {
            Some(SourceState::Checking | SourceState::Ready) => PreviewState::Starting,
            Some(SourceState::Unsupported { reason } | SourceState::Lost { reason }) => {
                PreviewState::Unavailable {
                    reason: reason.clone(),
                }
            }
            None => PreviewState::Unavailable {
                reason: "this input cannot be previewed".into(),
            },
        }
    }

    // ---- periodic work -----------------------------------------------------------------------

    fn tick(&mut self) {
        // Device hotplug.
        if self.devices.poll(self.caps.gpu_convert.is_some()) {
            self.validate_selection();
        }
        // Buses.
        let ids: Vec<u64> = self.pipes.iter().map(|p| p.id).collect();
        for id in ids {
            self.drain_bus(id);
        }
        self.check_deadlines();
        self.drive_phase();
        self.start_pending_record();
        self.reconcile_previews();
        self.start_gl_retries();
        self.release_idle_captures();
        self.periodic();
        self.transcription.tick(self.phase.is_busy());
    }

    fn check_deadlines(&mut self) {
        let now = Instant::now();
        let mut actions: Vec<(u64, Stage, bool)> = Vec::new();
        for p in &self.pipes {
            if p.abandoned {
                continue;
            }
            let probe = matches!(p.kind, PipeKind::Probe { .. } | PipeKind::Preview { .. });
            match &p.stage {
                Stage::Draining {
                    inject_at,
                    deadline,
                } => {
                    if now >= *deadline || inject_at.is_some_and(|t| now >= t) {
                        actions.push((p.id, p.stage.clone(), probe));
                    }
                }
                _ => {
                    if p.deadline.is_some_and(|d| now >= d) {
                        actions.push((p.id, p.stage.clone(), probe));
                    }
                }
            }
        }
        for (id, stage, probe) in actions {
            match stage {
                Stage::Nulling => {
                    // Give up on this pipeline (D10): apply its outcome, leave the worker detached.
                    let input = self
                        .pipes
                        .iter()
                        .find(|p| p.id == id)
                        .and_then(|p| p.input());
                    let outcome = self
                        .pipes
                        .iter()
                        .find(|p| p.id == id)
                        .and_then(|p| p.outcome.clone());
                    if let Some(p) = self.pipes.iter_mut().find(|p| p.id == id) {
                        p.abandoned = true;
                        eprintln!(
                            "chinstrap: {} did not reach Null in time; abandoned",
                            p.pipeline.name()
                        );
                    }
                    if let Some((seg, key)) = input {
                        if self.shutdown.is_some() {
                            self.shutdown_clean = false;
                        }
                        let status = match outcome {
                            Some(Outcome::Complete) => InputStatus::Complete,
                            Some(Outcome::Failed(reason)) => InputStatus::Failed { reason },
                            Some(Outcome::Discard(reason)) => InputStatus::Failed {
                                reason: retained_discard_reason(&reason),
                            },
                            None => InputStatus::Failed {
                                reason: "did not stop".into(),
                            },
                        };
                        self.set_file_status(seg, key, status);
                    } else if let Some(p) = self.pipes.iter_mut().find(|p| p.id == id)
                        && let PipeKind::Probe {
                            source,
                            convert,
                            result,
                            caps,
                        } = &mut p.kind
                    {
                        let (s, c, r) = (*source, *convert, result.take());
                        let caps = caps.lock().ok().and_then(|c| c.clone());
                        self.apply_probe(s, c, r, caps);
                    }
                }
                Stage::Draining { deadline, .. } if now >= deadline => {
                    let input = self
                        .pipes
                        .iter()
                        .find(|p| p.id == id)
                        .and_then(|p| p.input());
                    if let Some((seg, key)) = input {
                        self.notice(
                            Severity::Error,
                            Some(key),
                            format!("{key:?}: {NOT_FINALIZED}"),
                        );
                        if let Some(g) = self.seg_mut(seg) {
                            g.timed_out = true;
                        }
                        if self.shutdown.is_some() {
                            self.shutdown_clean = false;
                        }
                    }
                    self.request_null(id, Some(Outcome::Failed(NOT_FINALIZED.into())));
                    // to_null keeps an earlier outcome; a timeout always wins.
                    if let Some(p) = self.pipes.iter_mut().find(|p| p.id == id) {
                        p.outcome = Some(Outcome::Failed(NOT_FINALIZED.into()));
                    }
                }
                Stage::Draining { deadline, .. } => {
                    // Healthy EOS did not arrive within 1 s: inject as for a broken input.
                    if let Some(p) = self.pipes.iter_mut().find(|p| p.id == id)
                        && let PipeKind::Input {
                            rec, _session_lock, ..
                        } = &p.kind
                    {
                        inject_eos(rec.eos_target.clone(), id, _session_lock.clone());
                        p.stage = Stage::Draining {
                            inject_at: None,
                            deadline,
                        };
                    }
                }
                _ if probe => {
                    if self.pipes.iter().any(|p| p.id == id && p.is_preview()) {
                        self.preview_failed(id, "did not start".into());
                    } else {
                        self.probe_done(id, Err("no frames within 3 s".into()));
                    }
                }
                Stage::Pausing | Stage::Starting => {
                    self.fail_input(id, "did not start within 3 s".into())
                }
                _ => {}
            }
        }
    }

    /// The §5.1 start sequence and the §6 Stop/Cut completion, driven by the poll loop.
    fn drive_phase(&mut self) {
        let Some(seg) = self.current_seg() else {
            return;
        };
        let live = |e: &Self| -> Vec<(u64, Stage)> {
            e.pipes
                .iter()
                .filter(|p| !p.abandoned && p.input().is_some_and(|(s, _)| s == seg))
                .map(|p| (p.id, p.stage.clone()))
                .collect()
        };
        match self.phase.clone() {
            Phase::Starting { segment } => {
                let pipes = live(self);
                let base_set = self
                    .session
                    .as_ref()
                    .and_then(|s| s.segs.iter().find(|g| g.index == segment))
                    .is_some_and(|g| g.base_ns.is_some());
                if !base_set {
                    if pipes.iter().any(|(_, s)| *s == Stage::Pausing) {
                        return;
                    }
                    let paused: Vec<u64> = pipes
                        .iter()
                        .filter(|(_, s)| *s == Stage::Paused)
                        .map(|(id, _)| *id)
                        .collect();
                    if paused.is_empty() {
                        if pipes.is_empty() {
                            self.complete_segment(segment);
                        }
                        return;
                    }
                    let b = self.clock.time().nseconds() + BASE_MARGIN.as_nanos() as u64;
                    let started_at = Instant::now() + BASE_MARGIN;
                    let gap = self.gap_from.take().map(|s| b.saturating_sub(s));
                    if let Some(g) = self.seg_mut(segment) {
                        g.base_ns = Some(b);
                        g.started_at = Some(started_at);
                        g.started_wall = Some(session::wall_now());
                        g.gap_ns = gap;
                    }
                    let deadline = self.deadline_cap(Instant::now() + START_TIMEOUT);
                    for id in paused {
                        if let Some(p) = self.pipes.iter_mut().find(|p| p.id == id)
                            && let PipeKind::Input { rec, .. } = &p.kind
                        {
                            rec.base.store(b, Ordering::Release);
                            p.pipeline.set_base_time(gst::ClockTime::from_nseconds(b));
                            p.stage = Stage::Starting;
                            p.deadline = Some(deadline);
                            let pipeline = p.pipeline.clone();
                            self.spawn_state(id, pipeline, gst::State::Playing);
                        }
                    }
                    self.manifest_dirty = true;
                    self.dirty = true;
                } else if pipes.is_empty() {
                    // Nothing reached Playing.
                    self.complete_segment(segment);
                }
            }
            Phase::Recording { segment, .. } => {
                // If every input has failed, the segment ends (Incomplete) and the engine idles.
                let all_failed = self
                    .session
                    .as_ref()
                    .and_then(|s| s.segs.iter().find(|g| g.index == segment))
                    .is_some_and(|g| {
                        g.files
                            .iter()
                            .all(|f| matches!(f.file.status, InputStatus::Failed { .. }))
                    });
                if all_failed {
                    self.finalize_segment(segment);
                    self.phase = Phase::Stopping { segment };
                    self.dirty = true;
                }
            }
            Phase::Stopping { segment } | Phase::Cutting { from: segment } => {
                if live(self).is_empty() {
                    self.complete_segment(segment);
                }
            }
            Phase::Idle => {}
        }
    }

    /// Stop step 3 / Cut step 3-4 (§6).
    fn complete_segment(&mut self, segment: u32) {
        let stop_now = self.clock.time().nseconds();
        let mut survivors = Vec::new();
        // Inputs that only sent no frame (a static window: niri renders on damage, and each
        // segment opens a new stream) stay selected for the next Cut segment.
        let mut keep = Vec::new();
        let mut stop_ns = None;
        if let Some(g) = self.seg_mut(segment) {
            if g.stop_ns.is_none() && g.base_ns.is_some() {
                g.stop_ns = Some(stop_now);
            }
            stop_ns = g.stop_ns;
            if g.stop_reason.is_some() {
                for f in &mut g.files {
                    if f.file.path.exists()
                        && let Err(error) = super::recovery::verify_matroska_index(&f.file.path)
                    {
                        f.file.status = InputStatus::Failed {
                            reason: format!(
                                "recovery failed: storage stop left an unindexed file: {error}"
                            ),
                        };
                    }
                }
            }
            for f in &mut g.files {
                match f.file.status {
                    InputStatus::Complete => {
                        survivors.push(f.file.input);
                        keep.push(f.file.input);
                    }
                    InputStatus::Failed { ref reason } if reason == NO_FRAMES => {
                        keep.push(f.file.input)
                    }
                    InputStatus::Failed { .. } => {}
                    _ => {
                        f.file.status = InputStatus::Failed {
                            reason: "aborted".into(),
                        }
                    }
                }
            }
            g.status = if let Some(reason) = g.stop_reason.clone() {
                SegmentStatus::Incomplete { reason }
            } else if survivors.is_empty() {
                SegmentStatus::Incomplete {
                    reason: "every input failed".into(),
                }
            } else if g.timed_out {
                SegmentStatus::Incomplete {
                    reason: NOT_FINALIZED.into(),
                }
            } else {
                SegmentStatus::Complete
            };
        }
        // §12.3 policy 1: auto-transcribe the finished mic file.
        if let Some(sess) = &self.session {
            let key = JobKey {
                session_dir: sess.dir.clone(),
                segment,
            };
            let mic = self
                .seg_views()
                .into_iter()
                .find(|v| v.index == segment)
                .and_then(|v| v.mic);
            self.transcription.segment_completed(key, mic);
        }
        self.manifest_dirty = true;
        self.dirty = true;
        self.finalize_deadline = None;
        let cut = std::mem::take(&mut self.cut_pending);
        // Frozen input set minus Failed or Lost (§6 Cut step 1); "no frames received" is not
        // a failure of the source itself, so it is kept (in the original order).
        let next: Vec<InputKey> = keep
            .into_iter()
            .filter(|k| match k {
                InputKey::Source(id) => self
                    .sources
                    .iter()
                    .any(|s| s.info.id == *id && s.info.state == SourceState::Ready),
                _ => true,
            })
            .collect();
        let continue_cut = cut && self.shutdown.is_none() && !next.is_empty();
        if !continue_cut {
            self.release_session_reserve(Severity::Warning);
        }
        self.phase = Phase::Idle;
        if cut && self.shutdown.is_none() {
            if next.is_empty() {
                self.notice(Severity::Warning, None, "Cut: no inputs left to record");
            } else {
                self.write_manifest();
                self.start_segment(next, stop_ns);
            }
        }
    }

    fn periodic(&mut self) {
        let now = Instant::now();
        // Selftest resize of the test pattern (§5.3).
        for p in &mut self.pipes {
            if let PipeKind::Input { rec, resize_at, .. } = &mut p.kind
                && resize_at.is_some_and(|t| now >= t)
                && matches!(p.stage, Stage::Running { .. })
            {
                *resize_at = None;
                if let Some(f) = &rec.resize_filter {
                    recorder::renegotiate_test(f);
                    eprintln!(
                        "chinstrap: {}: test pattern renegotiated to {} (selftest resize)",
                        p.pipeline.name(),
                        recorder::TEST_RESIZE_CAPS
                    );
                }
            }
        }
        if now.duration_since(self.last_vocab_check) >= VOCAB_EVERY {
            self.last_vocab_check = now;
            let v = vocab::info();
            if v != self.vocabulary {
                self.vocabulary = v;
                self.dirty = true;
            }
        }
        if now.duration_since(self.last_sizes) < SIZE_EVERY {
            return;
        }
        self.last_sizes = now;
        self.refresh_storage();
        self.monitor_storage();
        self.checkpoint.request_if_due(
            matches!(self.phase, Phase::Recording { .. }),
            now,
            &mut self.manifest_dirty,
        );
        self.check_mic_stall();
        self.refresh_quality();
        let Some(seg) = self.current_seg() else {
            return;
        };
        let mut changed = false;
        if let Some(g) = self.seg_mut(seg) {
            for f in &mut g.files {
                if matches!(
                    f.file.status,
                    InputStatus::Recording | InputStatus::Finalizing
                ) {
                    let bytes = std::fs::metadata(&f.file.path)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    if bytes != f.file.bytes {
                        f.file.bytes = bytes;
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.dirty = true;
        }
    }

    /// Live quality and "waiting for first frame" of the current segment's inputs (~1 Hz).
    fn refresh_quality(&mut self) {
        let Some(seg) = self.current_seg() else {
            return;
        };
        let bytes_of = |key: InputKey| {
            self.session
                .as_ref()
                .and_then(|s| s.segs.iter().find(|g| g.index == seg))
                .and_then(|g| g.files.iter().find(|f| f.file.input == key))
                .map_or(0, |f| f.file.bytes)
        };
        let mut updates = Vec::new();
        for p in &self.pipes {
            if let PipeKind::Input {
                seg: s, key, rec, ..
            } = &p.kind
                && *s == seg
                && !p.abandoned
                && matches!(p.stage, Stage::Running { .. } | Stage::Draining { .. })
            {
                let waiting = match rec.encoder {
                    Some(_) => Counters::get(&rec.counters.frames_in) == 0,
                    None => rec.first_pts.load(Ordering::Acquire) == UNSET,
                };
                updates.push((*key, waiting, quality_of(rec, bytes_of(*key))));
            }
        }
        for (key, waiting, q) in updates {
            if let Some(f) = self.file_mut(seg, key)
                && (f.file.quality != q || f.file.waiting_for_frame != waiting)
            {
                f.file.quality = q;
                f.file.waiting_for_frame = waiting;
                self.dirty = true;
            }
        }
    }

    /// §5.0: a mic that stops delivering data (e.g. its node suspended after CLOCK_LOST and never
    /// resumed) is finalized as Failed rather than silently recorded short.
    fn check_mic_stall(&mut self) {
        if !matches!(self.phase, Phase::Recording { .. }) {
            return;
        }
        let now_ns = self.clock.time().nseconds();
        let stalled: Vec<u64> = self
            .pipes
            .iter()
            .filter(|p| !p.abandoned)
            .filter_map(|p| match (&p.kind, &p.stage) {
                (
                    PipeKind::Input {
                        key: InputKey::Microphone,
                        rec,
                        ..
                    },
                    Stage::Running { since },
                ) if since.elapsed() >= MIC_STALL => {
                    let base = rec.base.load(Ordering::Acquire);
                    if base == UNSET {
                        return None;
                    }
                    let running = now_ns.saturating_sub(base);
                    let last = rec.last_pts.load(Ordering::Acquire);
                    let last = if last == UNSET { 0 } else { last };
                    (running.saturating_sub(last) >= MIC_STALL.as_nanos() as u64).then_some(p.id)
                }
                _ => None,
            })
            .collect();
        for id in stalled {
            self.fail_input(id, "microphone stopped delivering data".into());
        }
    }

    // ---- shutdown ----------------------------------------------------------------------------

    fn begin_shutdown(&mut self, deadline: Instant, ack: Option<SyncSender<bool>>) {
        let first = self.shutdown.is_none();
        match &mut self.shutdown {
            Some((d, acks)) => {
                *d = (*d).min(deadline);
                acks.extend(ack);
            }
            None => self.shutdown = Some((deadline, ack.into_iter().collect())),
        }
        if !first {
            return;
        }
        // Never waits on transcription or a download (§12.3 policy 9).
        self.transcription.shutdown();
        if let Some(seg) = self.current_seg()
            && self
                .session
                .as_ref()
                .and_then(|s| s.segs.iter().find(|g| g.index == seg))
                .is_some_and(|g| g.timed_out)
        {
            self.shutdown_clean = false;
        }
        // Stop probes and previews; their results no longer matter.
        self.record_pending = false;
        let probes: Vec<u64> = self
            .pipes
            .iter()
            .filter(|p| matches!(p.kind, PipeKind::Probe { .. } | PipeKind::Preview { .. }))
            .map(|p| p.id)
            .collect();
        for id in probes {
            self.request_null(id, None);
        }
        self.cut_pending = false;
        match self.phase.clone() {
            Phase::Idle => {}
            Phase::Recording { segment, .. } => {
                self.finalize_segment(segment);
                self.phase = Phase::Stopping { segment };
            }
            Phase::Starting { segment } => {
                // Abandon what has not started; finalize what has.
                self.finalize_segment(segment);
                self.phase = Phase::Stopping { segment };
            }
            Phase::Cutting { .. } | Phase::Stopping { .. } => {
                // Shorten every wait to the shutdown deadline.
                if let Some(d) = self.finalize_deadline {
                    self.finalize_deadline = Some(self.deadline_cap(d));
                }
                let cap = self.shutdown.as_ref().map(|(d, _)| *d);
                for p in &mut self.pipes {
                    if let (Stage::Draining { deadline, .. }, Some(cap)) = (&mut p.stage, cap) {
                        *deadline = (*deadline).min(cap);
                    }
                    if let (Some(d), Some(cap)) = (&mut p.deadline, cap) {
                        *d = (*d).min(cap);
                    }
                }
            }
        }
        self.dirty = true;
    }

    fn should_exit(&self) -> bool {
        let Some((deadline, _)) = &self.shutdown else {
            return false;
        };
        let finished = self.phase == Phase::Idle;
        finished || Instant::now() >= *deadline
    }

    fn exit(mut self) {
        let (deadline, acks) = self.shutdown.take().unwrap_or((Instant::now(), Vec::new()));
        if let Some(seg) = self.current_seg() {
            // Out of time: whatever is still live is not finalized.
            let ids: Vec<u64> = self
                .pipes
                .iter()
                .filter(|p| p.input().is_some_and(|(s, _)| s == seg) && !p.abandoned)
                .map(|p| p.id)
                .collect();
            for id in ids {
                let Some(p) = self.pipes.iter().find(|p| p.id == id) else {
                    continue;
                };
                let input = p.input();
                // A pipe already Nulling with a recorded outcome reached EOS (or failed) before
                // the deadline: its file is as finalized as it will get, so keep that outcome.
                let settled = match (&p.stage, &p.outcome) {
                    (Stage::Nulling, Some(Outcome::Complete)) => Some(InputStatus::Complete),
                    (Stage::Nulling, Some(Outcome::Failed(reason))) => Some(InputStatus::Failed {
                        reason: reason.clone(),
                    }),
                    (Stage::Nulling, Some(Outcome::Discard(reason))) => Some(InputStatus::Failed {
                        reason: retained_discard_reason(reason),
                    }),
                    _ => None,
                };
                if let Some((s, k)) = input {
                    match settled {
                        Some(status) => self.set_file_status(s, k, status),
                        None => {
                            self.set_file_status(
                                s,
                                k,
                                InputStatus::Failed {
                                    reason: NOT_FINALIZED.into(),
                                },
                            );
                            if let Some(g) = self.seg_mut(s) {
                                g.timed_out = true;
                            }
                            self.shutdown_clean = false;
                        }
                    }
                }
                self.request_null(id, None);
            }
            self.complete_segment(seg);
        }
        // Portal: Shutdown, drop the sender, never wait past the deadline.
        if let Some((ptx, join)) = self.portal.take() {
            let _ = ptx.try_send(PortalCmd::Shutdown);
            drop(ptx);
            while !join.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if join.is_finished() {
                let _ = join.join();
            }
        }
        self.devices.stop();
        self.transcription.shutdown();
        if self.transcription.take_manifest_dirty() {
            self.manifest_dirty = true;
        }
        // fds still used by pipelines that never reached Null stay open (fd invariant).
        for (id, c) in self.captures.drain() {
            if self.pipes.iter().any(|p| p.capture == Some(id)) {
                std::mem::forget(c.fd);
            }
        }
        // If shutdown is leaving any input pipeline behind, its detached worker or GStreamer
        // streaming thread may still touch the file after this engine is dropped. Keep recovery
        // excluded until process exit, even if those workers outlive the engine thread.
        let live_session_locks: Vec<_> = self
            .pipes
            .iter()
            .filter_map(|p| match &p.kind {
                PipeKind::Input { _session_lock, .. } => Some(_session_lock.clone()),
                PipeKind::Probe { .. } | PipeKind::Preview { .. } => None,
            })
            .collect();
        for lock in live_session_locks {
            retain_session_lock_until_process_exit(lock);
        }
        self.phase = Phase::Idle;
        self.release_session_reserve(Severity::Warning);
        self.write_manifest();
        self.snapshot();
        let clean = self.shutdown_clean;
        for ack in acks {
            let _ = ack.send(clean);
        }
    }
}

/// Sends EOS into the sink pad of the first element after the source, on a worker thread
/// (the pad's stream lock may be held by a streaming thread).
fn inject_eos(pad: gst::Pad, id: u64, session_lock: session::SessionLock) {
    spawn_named(format!("chinstrap-gst-inject-{id}"), move || {
        let _session_lock = session_lock;
        pad.send_event(gst::event::Eos::new());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_stop_does_not_discard_open_starting_peers() {
        assert_eq!(
            storage_finalize_action(&Stage::Pausing),
            StorageFinalizeAction::AwaitState
        );
        assert_eq!(
            storage_finalize_action(&Stage::Paused),
            StorageFinalizeAction::InjectEos
        );
        assert_eq!(
            storage_finalize_action(&Stage::Starting),
            StorageFinalizeAction::AwaitState
        );
        assert_eq!(
            storage_finalize_action(&Stage::Running {
                since: Instant::now()
            }),
            StorageFinalizeAction::GracefulEos
        );
        assert_eq!(
            storage_finalize_action(&Stage::Nulling),
            StorageFinalizeAction::AlreadyStopping
        );
    }

    #[test]
    fn storage_stop_overrides_pending_discard_to_preserve_footage() {
        assert_eq!(
            storage_stop_outcome(Some(Outcome::Discard("early failure".into()))),
            Some(Outcome::Failed(
                "storage stop; partial file preserved".into()
            ))
        );
        assert_eq!(
            storage_stop_outcome(Some(Outcome::Complete)),
            Some(Outcome::Complete)
        );
    }

    #[test]
    fn early_discard_preserves_nonempty_output_as_recovery_candidate() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("chinstrap-early-discard-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("partial.mkv");
        std::fs::write(&path, b"already-written media").unwrap();

        let reason = finish_discard(Some(&path), 0, "early source failure");

        assert_eq!(
            reason,
            "recovery failed: early-failed file retained for recovery: early source failure"
        );
        assert!(path.exists(), "written output must not be unlinked");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn early_discard_removes_empty_output_without_changing_failure_reason() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("chinstrap-empty-discard-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.mkv");
        std::fs::write(&path, b"").unwrap();

        let reason = finish_discard(Some(&path), 0, "early source failure");

        assert_eq!(reason, "early source failure");
        assert!(
            !path.exists(),
            "empty output should retain early-discard cleanup"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn early_discard_preserves_frames_even_when_file_size_is_stale() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("chinstrap-stale-size-discard-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("encoded.mkv");
        std::fs::write(&path, b"").unwrap();

        let reason = finish_discard(Some(&path), 1, "early source failure");

        assert_eq!(
            reason,
            "recovery failed: early-failed file retained for recovery: early source failure"
        );
        assert!(
            path.exists(),
            "encoded-frame evidence should prevent unlinking"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn retained_early_discard_error_is_retryable_after_null_timeout() {
        assert_eq!(
            retained_discard_reason("did not reach Null"),
            "recovery failed: early-failed file retained for recovery: did not reach Null"
        );
    }

    fn test_input_pipe(
        id: u64,
        key: InputKey,
        outcome: Option<Outcome>,
        session_lock: session::SessionLock,
    ) -> Pipe {
        let pipeline = gst::Pipeline::with_name(&format!("storage-test-{id}"));
        let sink = gst::ElementFactory::make("fakesink")
            .name(format!("storage-test-sink-{id}"))
            .build()
            .unwrap();
        pipeline.add(&sink).unwrap();
        let bus = pipeline.bus().unwrap();
        let rec = InputPipeline {
            pipeline: pipeline.clone(),
            bus: bus.clone(),
            eos_target: sink.static_pad("sink").unwrap(),
            base: Arc::new(std::sync::atomic::AtomicU64::new(UNSET)),
            first_pts: Arc::new(std::sync::atomic::AtomicU64::new(UNSET)),
            last_pts: Arc::new(std::sync::atomic::AtomicU64::new(UNSET)),
            size: Arc::new(std::sync::Mutex::new(None)),
            resize_filter: None,
            convert: None,
            encoder: None,
            rate: None,
            warning: None,
            counters: Arc::new(Counters::default()),
            videorate: None,
            leaky: None,
            tail: None,
            preview: None,
        };
        Pipe {
            id,
            capture: None,
            pipeline,
            bus,
            kind: PipeKind::Input {
                seg: 1,
                key,
                rec,
                resize_at: None,
                _session_lock: session_lock,
            },
            stage: Stage::Nulling,
            deadline: None,
            outcome,
            abandoned: false,
            last_error: None,
            in_flight: Some(gst::State::Null),
            null_pending: false,
        }
    }

    fn storage_test_engine(root: &Path, storage_error: bool) -> (Engine, PathBuf) {
        gst::init().unwrap();
        let session_dir = root.join("session");
        let segment_dir = session_dir.join("segment-001");
        std::fs::create_dir_all(&segment_dir).unwrap();
        let session_lock = session::SessionLock::acquire(&session_dir).unwrap();
        let camera_path = segment_dir.join("camera.mkv");
        let mic_path = segment_dir.join("mic.mkv");
        std::fs::write(&camera_path, b"").unwrap();
        std::fs::write(&mic_path, b"").unwrap();

        let make_file = |key: InputKey, path: PathBuf| {
            let (kind, codec) = match key {
                InputKey::Camera => ("camera", "h264"),
                InputKey::Microphone => ("microphone", "opus"),
                InputKey::SystemAudio => ("system audio", "opus"),
                InputKey::Source(_) => ("test", "h264"),
            };
            let label = format!("{kind} test");
            let error = "early failure";
            let meta: ManifestInput = serde_json::from_value(serde_json::json!({
                "key": key_string(key),
                "kind": kind,
                "label": label,
                "file": path.file_name().unwrap().to_string_lossy(),
                "codec": codec,
                "status": "failed",
                "error": error,
            }))
            .unwrap();
            FileRec {
                file: InputFile {
                    input: key,
                    label,
                    path,
                    status: InputStatus::Failed {
                        reason: error.into(),
                    },
                    bytes: 0,
                    waiting_for_frame: false,
                    quality: None,
                },
                meta,
                first: None,
                last: None,
                size: None,
                counters: None,
            }
        };
        let now = Instant::now();
        let session = Session {
            name: "storage-test".into(),
            dir: session_dir.clone(),
            created: "test".into(),
            _lock: session_lock.clone(),
            reserve_path: None,
            segs: vec![Seg {
                index: 1,
                dir: segment_dir,
                status: SegmentStatus::Recording,
                base_ns: Some(0),
                stop_ns: None,
                gap_ns: None,
                started_wall: Some("test".into()),
                started_at: Some(now),
                timed_out: false,
                stop_reason: None,
                files: vec![
                    make_file(InputKey::Camera, camera_path.clone()),
                    make_file(InputKey::Microphone, mic_path),
                ],
            }],
        };
        let camera = test_input_pipe(
            1,
            InputKey::Camera,
            Some(Outcome::Discard("early failure".into())),
            session_lock.clone(),
        );
        let microphone = test_input_pipe(2, InputKey::Microphone, None, session_lock);
        if storage_error {
            let error =
                gst::message::Error::new(gst::ResourceError::NoSpaceLeft, "injected ENOSPC");
            let _ = microphone.bus.post(error);
        }

        let (tx, _rx) = mpsc::channel();
        let (events, _event_rx) = async_channel::unbounded();
        let now = Instant::now();
        let tx_config = TxConfig {
            models_dir: root.join("models"),
            resolved: super::super::transcribe::model::Resolved::External {
                id: "missing-test-model".into(),
                path: root.join("missing-test-model.bin"),
            },
            resolve_warning: None,
            force_cpu: true,
            language: None,
            gpu_device: 0,
            make_command: Arc::new(|| Ok(std::process::Command::new("true"))),
            worker_sink: Arc::new(|_| {}),
            model_sink: Arc::new(|_| {}),
            dl_opts: super::super::transcribe::model::DlOpts::default(),
            url_for: Arc::new(|_| String::new()),
        };
        let devices = Devices::start();
        let engine = Engine {
            tx,
            events,
            caps: Capabilities::default(),
            encoders: Vec::new(),
            gpu_encoders: Vec::new(),
            screen_fps: 30,
            frame_rate_default: FrameRateMode::Native,
            camera_mode: None,
            vocabulary: VocabularyInfo::default(),
            last_vocab_check: now,
            test_sources: Vec::new(),
            devices,
            selected_camera: None,
            selected_microphone: None,
            selected_system_audio: None,
            system_audio_test_tone: false,
            sources: Vec::new(),
            captures: HashMap::new(),
            output_root: root.to_path_buf(),
            min_free_mb: 0,
            storage: StorageInfo::default(),
            storage_rate_tracker: storage::StorageRateTracker::default(),
            storage_free_bytes_override: None,
            storage_warning_level: 0,
            recoveries: Vec::new(),
            phase: Phase::Recording {
                segment: 1,
                started_at: now,
            },
            portal_busy: false,
            session: Some(session),
            portal: None,
            clock: gst::SystemClock::obtain(),
            pipes: vec![camera, microphone],
            next_pipe: 3,
            next_source: 1,
            next_capture: 1,
            cut_pending: false,
            gap_from: None,
            finalize_deadline: None,
            shutdown: None,
            shutdown_clean: true,
            resize_after: None,
            last_sizes: now,
            checkpoint: CheckpointWriteState::new(now),
            manifest_retry_at: None,
            last_level: now,
            last_system_level: now,
            dirty: false,
            manifest_dirty: false,
            warned: HashSet::new(),
            pending_notices: Vec::new(),
            transcription: Transcription::new(tx_config),
            previews: Previews::default(),
            preview_on: false,
            preview_state: HashMap::new(),
            preview_seen: HashMap::new(),
            preview_retry: HashMap::new(),
            record_pending: false,
        };
        (engine, camera_path)
    }

    fn storage_test_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "chinstrap-storage-order-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn selected_unrecordable_system_audio_is_reported() {
        let root = storage_test_root("system-audio-unrecordable-warning");
        let (mut engine, _) = storage_test_engine(&root, false);
        let (events, event_rx) = async_channel::unbounded();
        engine.events = events;
        engine.pipes.clear();
        engine.phase = Phase::Idle;
        engine.selected_system_audio = Some(DeviceId(u32::MAX));
        engine.caps.h264_encoder = Some(VideoEncoder::X264);
        engine.sources.push(Source {
            info: SourceInfo {
                id: SourceId(999),
                capture: None,
                kind: SourceKind::TestPattern,
                label: "Test pattern 999".into(),
                logical_size: Some((1280, 720)),
                state: SourceState::Ready,
                frame_rate: FrameRateMode::Cfr30,
                refresh: Some((30, 1)),
                size: Some((1280, 720)),
            },
            node: None,
            convert: None,
            gl_retry: false,
        });
        engine.session.as_mut().unwrap().reserve_path = Some(root.join("stop-reserve"));
        let blocker = root.join("session").join("segment-002");
        std::fs::write(
            &blocker,
            b"force segment creation to stop before pipelines start",
        )
        .unwrap();

        engine.begin_recording();

        let mut warning_found = false;
        while let Ok(event) = event_rx.try_recv() {
            if let EngineEvent::Notice(notice) = event
                && notice.message == "The selected system audio cannot record; recording without it"
            {
                warning_found = true;
            }
        }
        assert!(
            warning_found,
            "selected but unrecordable system audio must not be silently omitted"
        );
        engine.devices.stop();
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn null_timeout_marks_shutdown_unclean() {
        let root = storage_test_root("null-timeout-shutdown-ack");
        let (mut engine, _) = storage_test_engine(&root, false);
        engine.pipes.retain(|p| p.id == 1);
        let now = Instant::now();
        let pipe = engine.pipes.iter_mut().find(|p| p.id == 1).unwrap();
        pipe.stage = Stage::Nulling;
        pipe.deadline = Some(now - Duration::from_secs(1));
        pipe.outcome = Some(Outcome::Discard("stopped before recording started".into()));
        engine.shutdown = Some((now + Duration::from_secs(1), Vec::new()));
        engine.shutdown_clean = true;

        engine.check_deadlines();

        assert!(engine.pipes.iter().find(|p| p.id == 1).unwrap().abandoned);
        assert!(
            !engine.shutdown_clean,
            "an unresolved NULL transition must not be acknowledged as clean"
        );
        engine.devices.stop();
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_null_transition_keeps_recovery_locked_after_session_owner_drops() {
        let root = storage_test_root("null-failure-lock");
        let session_dir = root.join("session");
        let (mut engine, path) = storage_test_engine(&root, false);
        engine.pipes.retain(|p| p.id == 1);

        engine.worker_result(WorkerResult {
            id: 1,
            target: gst::State::Null,
            result: Err("injected NULL transition failure".into()),
        });
        let status_is_retryable = matches!(
            engine.session.as_ref().unwrap().segs[0].files[0].file.status,
            InputStatus::Failed { ref reason } if reason.starts_with(NOT_FINALIZED)
        );
        engine.write_manifest();
        engine.session.take();
        let file_is_preserved = path.exists();
        let lock_is_held = session::SessionLock::is_locked(&session_dir).unwrap();
        let recovery_is_blocked = session::scan_interrupted(&root).unwrap().is_empty();

        engine.devices.stop();
        drop(engine);
        let recovery_is_discoverable_after_release = session::scan_interrupted(&root)
            .unwrap()
            .iter()
            .any(|candidate| candidate.dir == session_dir);
        std::fs::remove_dir_all(root).unwrap();

        assert!(
            file_is_preserved,
            "failed NULL must never run early-discard unlink"
        );
        assert!(
            status_is_retryable,
            "unfinished output must remain retryable"
        );
        assert!(
            lock_is_held,
            "the input pipe must retain its session-lock lease"
        );
        assert!(
            recovery_is_blocked,
            "recovery must wait while a writer may remain"
        );
        assert!(
            recovery_is_discoverable_after_release,
            "recovery should discover the partial file after the worker lease is released"
        );
    }

    #[test]
    fn pending_storage_bus_error_precedes_discard_worker_unlink() {
        let root = storage_test_root("enospc");
        let (mut engine, path) = storage_test_engine(&root, true);

        engine.worker_result(WorkerResult {
            id: 1,
            target: gst::State::Null,
            result: Ok(()),
        });

        assert!(path.exists(), "pending ENOSPC must preserve the peer file");
        assert!(matches!(engine.phase, Phase::Stopping { segment: 1 }));
        assert!(matches!(
            engine.session.as_ref().unwrap().segs[0].files[0].file.status,
            InputStatus::Failed { ref reason }
                if reason == "storage stop; partial file preserved"
        ));
        engine.devices.stop();
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn early_discard_still_unlinks_empty_file_without_storage_stop() {
        let root = storage_test_root("early-discard");
        let (mut engine, path) = storage_test_engine(&root, false);

        engine.worker_result(WorkerResult {
            id: 1,
            target: gst::State::Null,
            result: Ok(()),
        });

        assert!(
            !path.exists(),
            "normal early-error discard should still unlink empty output"
        );
        assert!(matches!(
            engine.session.as_ref().unwrap().segs[0].files[0].file.status,
            InputStatus::Failed { ref reason } if reason == "early failure"
        ));
        engine.devices.stop();
        drop(engine);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_manifest_checkpoint_keeps_dirty_and_retries_from_last_success() {
        let started = Instant::now();
        let mut state = CheckpointWriteState::new(started);
        let mut manifest_dirty = false;
        let first_attempt = started + Duration::from_secs(5);

        assert!(state.request_if_due(true, first_attempt, &mut manifest_dirty));
        assert!(manifest_dirty);
        state.write_result(false, first_attempt, &mut manifest_dirty);
        assert!(manifest_dirty, "failed write must remain dirty for retry");
        assert_eq!(state.last_success, started);
        assert!(state.pending);

        let retry = first_attempt + Duration::from_secs(1);
        assert!(state.request_if_due(true, retry, &mut manifest_dirty));
        state.write_result(true, retry, &mut manifest_dirty);
        assert!(!manifest_dirty);
        assert_eq!(state.last_success, retry);
        assert!(!state.pending);
        assert!(!state.request_if_due(true, retry + Duration::from_secs(4), &mut manifest_dirty));
        assert!(state.request_if_due(true, retry + Duration::from_secs(5), &mut manifest_dirty));
    }

    #[test]
    fn disconnected_system_output_is_retained_while_busy_and_cleared_when_idle() {
        let id = DeviceId(42);
        assert_eq!(
            system_audio_selection_after_refresh(Some(id), false, true),
            (Some(id), true)
        );
        assert_eq!(
            system_audio_selection_after_refresh(Some(id), false, false),
            (None, true)
        );
    }

    #[test]
    fn connected_or_unselected_system_audio_does_not_report_disconnect() {
        let id = DeviceId(42);
        assert_eq!(
            system_audio_selection_after_refresh(Some(id), true, true),
            (Some(id), false)
        );
        assert_eq!(
            system_audio_selection_after_refresh(None, false, false),
            (None, false)
        );
    }
}
