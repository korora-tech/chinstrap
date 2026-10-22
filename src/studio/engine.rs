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
use std::sync::{Arc, Mutex};
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
use super::recorder::{self, InputPipeline, Rate, VideoSource};
use super::session::{self, Manifest, ManifestInput, ManifestQuality, ManifestSegment};
use super::stamp::{self, Counters, SizeSlot, UNSET};
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
    /// Transcription worker thread (§12.3).
    Transcribe(WorkerEvent),
    /// Model check/download thread (§12.5).
    Model(ModelEvent),
    Shutdown {
        deadline: Instant,
        /// Receives true when every active segment was finalized before the deadline.
        ack: SyncSender<bool>,
    },
}

struct Inner {
    tx: mpsc::Sender<Msg>,
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
    let join = if config.fake {
        fake::spawn(config, rx, events_tx).map_err(EngineError::Spawn)?
    } else {
        let portal = if config.enable_portal {
            Some(portal::spawn(tx.clone()).map_err(EngineError::Spawn)?)
        } else {
            None
        };
        let engine_tx = tx.clone();
        std::thread::Builder::new()
            .name("chinstrap-engine".into())
            .spawn(move || Engine::new(config, engine_tx, events_tx, portal).run(rx))
            .map_err(EngineError::Spawn)?
    };
    let inner = Arc::new(Inner {
        tx,
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
const LEVEL_EVERY: Duration = Duration::from_millis(100);
/// A recording mic whose newest muxed buffer is this far behind the clock has stopped (§5.0).
const MIC_STALL: Duration = Duration::from_secs(3);
const NOT_FINALIZED: &str = "not finalized: file may lack index";
/// Native rate assumed for a source whose maximum is unknown.
const DEFAULT_NATIVE_FPS: f64 = 60.0;
/// Re-read the vocabulary files this often (edits made outside the app).
const VOCAB_EVERY: Duration = Duration::from_secs(5);
const NO_FRAMES: &str = "no frames received";

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

#[derive(Clone, Debug, PartialEq)]
enum Outcome {
    Complete,
    Failed(String),
    /// Failed, and the partial file is deleted.
    Discard(String),
}

enum PipeKind {
    Input {
        seg: u32,
        key: InputKey,
        rec: InputPipeline,
        resize_at: Option<Instant>,
    },
    Probe {
        source: SourceId,
        convert: GpuConvert,
        result: Option<Result<(), String>>,
        /// Negotiated source caps (size, maximum rate).
        caps: recorder::CapsSlot,
    },
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
            PipeKind::Probe { .. } => None,
        }
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
    files: Vec<FileRec>,
}

struct Session {
    name: String,
    dir: PathBuf,
    created: String,
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
    sources: Vec<Source>,
    captures: HashMap<CaptureId, Capture>,
    output_root: PathBuf,
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
    last_level: Instant,
    dirty: bool,
    manifest_dirty: bool,
    warned: HashSet<String>,
    pending_notices: Vec<Notice>,
    /// Transcription state (§12.3); this thread is its only owner.
    transcription: Transcription,
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

impl Engine {
    fn new(
        cfg: EngineConfig,
        tx: mpsc::Sender<Msg>,
        events: async_channel::Sender<EngineEvent>,
        portal: Option<(async_channel::Sender<PortalCmd>, JoinHandle<()>)>,
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
            sources: Vec::new(),
            captures: HashMap::new(),
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
            last_level: now,
            dirty: true,
            manifest_dirty: false,
            warned: HashSet::new(),
            pending_notices: notices,
            transcription,
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        // First event is always a Snapshot; startup notices follow it.
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
            sources: self.sources.iter().map(|s| s.info.clone()).collect(),
            selected_camera: self.selected_camera,
            selected_microphone: self.selected_microphone,
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
            vocabulary: self.vocabulary.clone(),
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
        if self.manifest_dirty {
            self.write_manifest();
        }
        if self.dirty {
            self.snapshot();
        }
    }

    fn write_manifest(&mut self) {
        self.manifest_dirty = false;
        let Some(s) = &self.session else {
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
        if let Err(e) = session::write_manifest(&s.dir, &m) {
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
        keys.extend(self.sources.iter().map(|s| InputKey::Source(s.info.id)));
        keys.into_iter()
            .filter(|k| self.input_recordable(*k))
            .collect()
    }

    fn can_record(&self) -> bool {
        self.phase == Phase::Idle
            && self.shutdown.is_none()
            && !self.portal_busy
            && !self
                .sources
                .iter()
                .any(|s| s.info.state == SourceState::Checking)
            && !self.frozen_inputs().is_empty()
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
            Msg::Transcribe(e) => self.transcription.on_worker(e),
            Msg::Model(e) => {
                let segs = self.seg_views();
                self.transcription.on_model(e, &segs);
            }
            Msg::Shutdown { deadline, ack } => self.begin_shutdown(deadline, Some(ack)),
        }
    }

    fn command(&mut self, cmd: EngineCommand) {
        if self.shutdown.is_some() {
            self.reject(&cmd, "the engine is shutting down");
            return;
        }
        let busy = self.phase.is_busy();
        match &cmd {
            EngineCommand::RefreshDevices => {
                self.refresh_devices();
                self.dirty = true;
            }
            EngineCommand::SelectCamera(_)
            | EngineCommand::SelectMicrophone(_)
            | EngineCommand::AddPortalSources
            | EngineCommand::AddTestSource
            | EngineCommand::RemoveSource(_)
            | EngineCommand::SetOutputRoot(_)
            | EngineCommand::EndSession
            | EngineCommand::SetFrameRateMode { .. }
            | EngineCommand::SelectCameraMode(_)
                if busy =>
            {
                self.reject(&cmd, "not allowed while recording");
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
                        refresh: Some((recorder::TEST_NATIVE_FPS, 1)),
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
                    self.session = None;
                    // Its transcription jobs keep running (§12.3 policy 11).
                    self.transcription.set_session(None);
                    self.dirty = true;
                }
            }
            EngineCommand::Record => self.record(&cmd),
            EngineCommand::Stop => self.stop(&cmd),
            EngineCommand::ToggleRecord => match self.phase {
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

    fn validate_selection(&mut self) {
        if self.phase.is_busy() {
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
        if !self.can_record() {
            let why = if self.phase.is_busy() {
                "the engine is busy"
            } else if self.portal_busy {
                "the picker is open"
            } else {
                "nothing recordable is selected, or a source is still being checked"
            };
            self.reject(cmd, why);
            return;
        }
        if self.session.is_none() {
            match session::create_session_dir(&self.output_root, &session::session_name_now()) {
                Ok((name, dir)) => {
                    self.transcription.set_session(Some(dir.clone()));
                    self.session = Some(Session {
                        name,
                        dir,
                        created: session::wall_now(),
                        segs: Vec::new(),
                    });
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
        let inputs = self.frozen_inputs();
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
        self.start_segment(inputs, None);
    }

    fn stop(&mut self, cmd: &EngineCommand) {
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
            self.reject(cmd, "not recording");
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
            let codec = if key == InputKey::Microphone {
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
                InputKey::Microphone => {}
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
            Rate::Native {
                cap: lowered.then(|| a.fps.round() as i32),
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
        match key {
            InputKey::Microphone => {
                let e = self
                    .selected_microphone
                    .and_then(|id| self.devices.microphone(id))
                    .ok_or("the microphone is gone")?;
                recorder::build_mic(&name, e, &self.clock, path)
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
        let ids: Vec<(u64, Stage)> = self
            .pipes
            .iter()
            .filter(|p| !p.abandoned && p.input().is_some_and(|(s, _)| s == segment))
            .map(|p| (p.id, p.stage.clone()))
            .collect();
        for (id, stage) in ids {
            match stage {
                Stage::Running { .. } => self.finalize_input(id, true),
                Stage::Pausing | Stage::Paused | Stage::Starting => self.request_null(
                    id,
                    Some(Outcome::Discard("stopped before recording started".into())),
                ),
                Stage::Draining { .. } | Stage::Nulling => {}
            }
        }
        self.manifest_dirty = true;
        self.dirty = true;
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
        let PipeKind::Input { seg, key, rec, .. } = &p.kind else {
            return;
        };
        let (seg, key) = (*seg, *key);
        match p.stage {
            Stage::Running { .. } => {}
            Stage::Draining {
                inject_at: Some(_),
                deadline: d,
            } if !healthy => {
                // Already draining via the pipeline; escalate to injection now.
                inject_eos(rec.eos_target.clone(), id);
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
            spawn_named(format!("chinstrap-gst-eos-{id}"), move || {
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
            inject_eos(rec.eos_target.clone(), id);
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
        spawn_named(format!("chinstrap-gst-worker-{id}"), move || {
            let result = pipeline
                .set_state(target)
                .map(|_| ())
                .map_err(|e| e.to_string());
            drop(pipeline);
            let _ = tx.send(Msg::Worker(WorkerResult { id, target, result }));
        });
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
        // Pick up an error message posted during the transition, for a better reason.
        self.drain_bus(w.id);
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
                }
                Err(e) => {
                    let r = reason(p, e);
                    self.fail_input(w.id, r);
                }
            },
            (gst::State::Playing, Stage::Starting) => {
                let is_probe = matches!(p.kind, PipeKind::Probe { .. });
                match &w.result {
                    Ok(()) => {
                        let input = p.input();
                        if let Some(p) = self.pipes.iter_mut().find(|p| p.id == w.id) {
                            p.stage = Stage::Running {
                                since: Instant::now(),
                            };
                            if !is_probe {
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
                    }
                    Err(e) => {
                        let r = reason(p, e);
                        if is_probe {
                            self.probe_done(w.id, Err(r));
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
                        if let Some(path) = path.as_ref() {
                            let _ = std::fs::remove_file(path);
                        }
                        self.set_file_status(seg, key, InputStatus::Failed { reason: r })
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
        match msg.view() {
            gst::MessageView::Eos(_) => {
                if is_probe {
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
                if is_probe {
                    self.probe_done(id, Err(text));
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
                    && self.last_level.elapsed() >= LEVEL_EVERY
                    && matches!(self.phase, Phase::Recording { .. })
                    && let (Some(rms), Some(peak)) = (level_max(s, "rms"), level_max(s, "peak"))
                {
                    self.last_level = Instant::now();
                    let _ = self.events.send_blocking(EngineEvent::Level {
                        rms_db: rms,
                        peak_db: peak,
                    });
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
            let probe = matches!(p.kind, PipeKind::Probe { .. });
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
                        let status = match outcome {
                            Some(Outcome::Complete) => InputStatus::Complete,
                            Some(Outcome::Failed(r)) | Some(Outcome::Discard(r)) => {
                                InputStatus::Failed { reason: r }
                            }
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
                        && let PipeKind::Input { rec, .. } = &p.kind
                    {
                        inject_eos(rec.eos_target.clone(), id);
                        p.stage = Stage::Draining {
                            inject_at: None,
                            deadline,
                        };
                    }
                }
                _ if probe => self.probe_done(id, Err("no frames within 3 s".into())),
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
        let mut stop_ns = None;
        if let Some(g) = self.seg_mut(segment) {
            if g.stop_ns.is_none() && g.base_ns.is_some() {
                g.stop_ns = Some(stop_now);
            }
            stop_ns = g.stop_ns;
            for f in &mut g.files {
                match f.file.status {
                    InputStatus::Complete => survivors.push(f.file.input),
                    InputStatus::Failed { .. } => {}
                    _ => {
                        f.file.status = InputStatus::Failed {
                            reason: "aborted".into(),
                        }
                    }
                }
            }
            g.status = if survivors.is_empty() {
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
        // Frozen input set minus Failed or Lost (§6 Cut step 1).
        let next: Vec<InputKey> = survivors
            .into_iter()
            .filter(|k| match k {
                InputKey::Source(id) => self
                    .sources
                    .iter()
                    .any(|s| s.info.id == *id && s.info.state == SourceState::Ready),
                _ => true,
            })
            .collect();
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
        // Stop probes; their results no longer matter.
        let probes: Vec<u64> = self
            .pipes
            .iter()
            .filter(|p| matches!(p.kind, PipeKind::Probe { .. }))
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
                    (Stage::Nulling, Some(Outcome::Failed(r) | Outcome::Discard(r))) => {
                        Some(InputStatus::Failed { reason: r.clone() })
                    }
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
        self.phase = Phase::Idle;
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
fn inject_eos(pad: gst::Pad, id: u64) {
    spawn_named(format!("chinstrap-gst-inject-{id}"), move || {
        pad.send_event(gst::event::Eos::new());
    });
}
