use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(pub u32);   // engine-assigned, stable for process lifetime (keyed by object.serial / node.name)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u32);   // engine-assigned, monotonically increasing, never reused
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CaptureId(pub u32);  // one portal session; may own several SourceIds (shared lifetime)

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputKey { Camera, Microphone, Source(SourceId) }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind { Camera, Microphone }

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    pub id: DeviceId,
    pub kind: DeviceKind,
    pub name: String,                  // gst Device display_name
    pub detail: String,                // "pipewire #61 · 48 kHz · 2 ch" / "pipewire #2148 · 3840×2160 @25 · NV12"
    pub unsupported: Option<String>,   // Some(reason) => listed, not selectable
    /// Cameras: every usable advertised mode, best first; the one with `auto` is the default.
    pub modes: Vec<CameraModeInfo>,
}

/// One camera mode a device advertises (P2-9).
#[derive(Clone, Debug, PartialEq)]
pub struct CameraModeInfo {
    /// Stable key, e.g. "3840×2160 @25 · NV12"; `SelectCameraMode` takes it.
    pub label: String,
    pub width: i32,
    pub height: i32,
    pub fps_n: i32,
    pub fps_d: i32,
    /// "NV12" / "MJPEG" / "DMA-BUF …".
    pub format: String,
    /// The mode chosen automatically.
    pub auto: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind { Monitor, Window, Virtual, TestPattern }

#[derive(Clone, Debug, PartialEq)]
pub enum SourceState { Checking, Ready, Unsupported { reason: String }, Lost { reason: String } }

#[derive(Clone, Debug, PartialEq)]
pub struct SourceInfo {
    pub id: SourceId,
    pub capture: Option<CaptureId>,       // None for TestPattern
    pub kind: SourceKind,
    pub label: String,                    // "Screen 1" / "Window 2" / "Test pattern 3" (number = SourceId)
    pub logical_size: Option<(i32, i32)>, // portal Stream::size(), informational
    pub state: SourceState,
    pub frame_rate: FrameRateMode,
    /// The source's own maximum output rate (portal: the output refresh, from negotiation).
    pub refresh: Option<(i32, i32)>,
    /// Physical size from negotiation, when known.
    pub size: Option<(i32, i32)>,
}

/// A video encoder. (Historically H.264 only, hence `Capabilities::h264_encoder`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoEncoder { Va, VaHevc, X264, OpenH264 }
impl VideoEncoder {
    /// Probe order; the first verified one is the default (`Capabilities::h264_encoder`).
    pub const FALLBACK_ORDER: [VideoEncoder; 4] = [Self::Va, Self::VaHevc, Self::X264, Self::OpenH264];
    pub fn element_name(self) -> &'static str {
        match self { Self::Va => "vah264enc", Self::VaHevc => "vah265enc", Self::X264 => "x264enc", Self::OpenH264 => "openh264enc" }
    }
    pub fn from_element_name(s: &str) -> Option<Self> {
        Self::FALLBACK_ORDER.into_iter().find(|e| e.element_name() == s)
    }
    pub fn codec(self) -> VideoCodec { match self { Self::VaHevc => VideoCodec::Hevc, _ => VideoCodec::H264 } }
    /// Runs on the GPU's video engine (shares its capacity with every other VA encode).
    pub fn is_gpu(self) -> bool { matches!(self, Self::Va | Self::VaHevc) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoCodec { H264, Hevc }
impl VideoCodec {
    /// Manifest name: "h264" | "hevc".
    pub fn name(self) -> &'static str { match self { Self::H264 => "h264", Self::Hevc => "hevc" } }
}

/// Frame rate of a screen/window/test source (SPEC P1-5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FrameRateMode {
    /// Variable frame rate, up to the source's own output refresh (frames only on damage); the
    /// engine may lower the cap to fit the measured encoder capacity (see `CapacityPlan`).
    Native,
    Cfr60,
    Cfr30,
}
#[allow(dead_code)] // `ALL` and `label` are for the UI's frame-rate picker.
impl FrameRateMode {
    pub const ALL: [FrameRateMode; 3] = [Self::Native, Self::Cfr60, Self::Cfr30];
    /// The constant rate, or None for Native.
    pub fn cfr_fps(self) -> Option<i32> { match self { Self::Native => None, Self::Cfr60 => Some(60), Self::Cfr30 => Some(30) } }
    pub fn label(self) -> &'static str { match self { Self::Native => "Native (variable)", Self::Cfr60 => "60 fps", Self::Cfr30 => "30 fps" } }
}

/// How DMA-BUF video is brought into a form the encoder accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuConvert { Va, Gl }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputClass { Any, Video, DmaBufVideo, Mic, PipeWire, MjpegCamera, TestOptional }

/// Single source of truth for element requirements (§5.7). encoders.rs consumes it.
pub const REQUIRED_ELEMENTS: &[(&str, InputClass, &str /* nixpkgs pkg */)] = &[
    ("queue", InputClass::Any, "gstreamer"), ("capsfilter", InputClass::Any, "gstreamer"),
    ("filesink", InputClass::Any, "gstreamer"), ("fakesink", InputClass::Any, "gstreamer"),
    ("matroskamux", InputClass::Any, "gst-plugins-good"),
    ("videoconvert", InputClass::Video, "gst-plugins-base"), ("videoscale", InputClass::Video, "gst-plugins-base"),
    ("videorate", InputClass::Video, "gst-plugins-base"), ("videotestsrc", InputClass::Video, "gst-plugins-base"),
    ("h264parse", InputClass::Video, "gst-plugins-bad"),
    ("audioconvert", InputClass::Mic, "gst-plugins-base"), ("audioresample", InputClass::Mic, "gst-plugins-base"),
    ("opusenc", InputClass::Mic, "gst-plugins-base"), ("level", InputClass::Mic, "gst-plugins-good"),
    ("pipewiresrc", InputClass::PipeWire, "pipewire"),
    ("jpegparse", InputClass::MjpegCamera, "gst-plugins-bad"), ("jpegdec", InputClass::MjpegCamera, "gst-plugins-good"),
    ("timeoverlay", InputClass::TestOptional, "gst-plugins-base"),
    // DmaBufVideo is satisfied by EITHER {vapostproc} (gst-plugins-bad) OR {glupload, glcolorconvert, gldownload}
    // (gst-plugins-base); reported via Capabilities.gpu_convert, not missing_elements.
];

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Capabilities {
    pub h264_encoder: Option<VideoEncoder>, // default video encoder; None => video inputs cannot record
    /// Every verified encoder with its measured 4K throughput (frames/s).
    pub encoders: Vec<(VideoEncoder, f64)>,
    pub gpu_convert: Option<GpuConvert>,   // None => DMA-BUF inputs (all portal sources) cannot record
    pub missing_elements: Vec<String>,
    pub portal: PortalAvailability,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub enum PortalAvailability { #[default] Unknown, Available { version: u32 }, Unavailable { reason: String } }

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    Starting  { segment: u32 },
    Recording { segment: u32, started_at: Instant },  // Instant corresponding to base_time B
    Cutting   { from: u32 },
    Stopping  { segment: u32 },
}
impl Phase { pub fn is_busy(&self) -> bool { !matches!(self, Phase::Idle) } }

#[derive(Clone, Debug, PartialEq)]
pub enum SegmentStatus { Recording, Complete, Incomplete { reason: String } }

#[derive(Clone, Debug, PartialEq)]
pub enum InputStatus { Starting, Recording, Finalizing, Complete, Failed { reason: String } }

#[derive(Clone, Debug, PartialEq)]
pub struct InputFile {
    pub input: InputKey,
    pub label: String,
    pub path: PathBuf,            // absolute
    pub status: InputStatus,
    pub bytes: u64,               // refreshed ~1 Hz while recording, final on finish
    /// Recording, but no frame has arrived yet (UI: "Waiting for first frame").
    pub waiting_for_frame: bool,
    /// Video only: counters and derived quality, refreshed ~1 Hz, final on finish (P2-8).
    pub quality: Option<InputQuality>,
}

/// Per-input frame accounting (P0-1, P2-8).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct InputQuality {
    pub encoder: Option<VideoEncoder>,
    /// Constant rate recorded, or None for variable frame rate (Native).
    pub nominal_fps: Option<f64>,
    /// Native mode: the maximum rate requested from the source.
    pub max_fps: Option<f64>,
    pub frames_in: u64,          // frames delivered by the source and kept
    pub dropped_corrupt: u64,    // truncated/corrupted source frames dropped
    pub dropped_queue: u64,      // dropped by the leaky queue before the encoder (encoder too slow)
    pub duplicated: u64,         // repeated by the rate converter/repeater (gaps), padding excluded
    pub padded: u64,             // repeats that pad a late start to segment time zero
    pub encoded: u64,            // frames written
    pub effective_fps: f64,      // fresh frames per second of recording
    pub repeat_pct: f64,         // duplicated / encoded
    pub bitrate_kbps: u32,
    /// First real frame relative to the segment start (the file itself starts at zero).
    pub start_offset: Option<Duration>,
}

/// The encoder plan for the frozen inputs (P1-5): encoder per input and the load it puts on
/// the GPU's video engine, from the measured throughput.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct CapacityPlan {
    pub inputs: Vec<PlannedInput>,
    /// Worst case (every encode on one video engine), percent of the usable budget.
    pub gpu_load_pct: u32,
    pub cpu_load_pct: u32,
    /// The selection exceeds the measured capacity: frames will be dropped or repeated.
    pub over: bool,
    /// Shown before recording when `over`.
    pub warning: Option<String>,
    /// Automatic choices worth telling the user (codec switches, lowered native caps).
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlannedInput {
    pub input: InputKey,
    pub encoder: VideoEncoder,
    pub width: i32,
    pub height: i32,
    /// Planned frame rate (Native: the cap).
    pub fps: f64,
    pub native: bool,
    pub load_pct: u32,
}

/// Current output-filesystem capacity and the conservative session-rate estimate.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct StorageInfo {
    pub free_bytes: Option<u64>,
    pub estimated_bytes_per_second: u64,
    pub rate_measured: bool,
    pub remaining_seconds: Option<u64>,
    pub stop_reserve_bytes: u64,
    /// A reason Record is refused, or `None` when the current estimate fits.
    pub record_blocked_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryCandidate {
    pub name: String,
    pub dir: PathBuf,
    pub segments: Vec<u32>,
    pub recovering: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SegmentSummary {
    pub index: u32,
    pub dir: PathBuf,
    pub status: SegmentStatus,
    pub duration: Option<Duration>,     // stop_clock - B; None while recording
    pub gap_before: Option<Duration>,   // None for first segment of a session
    pub files: Vec<InputFile>,
    pub transcript: TranscriptStatus,   // tracks the segment's mic file (§12.2)
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    pub name: String,
    pub dir: PathBuf,
    pub manifest: PathBuf,              // <dir>/session.json
    pub segments: Vec<SegmentSummary>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub capabilities: Capabilities,
    pub cameras: Vec<DeviceInfo>,
    pub microphones: Vec<DeviceInfo>,
    pub sources: Vec<SourceInfo>,
    pub selected_camera: Option<DeviceId>,
    pub selected_microphone: Option<DeviceId>,
    pub output_root: PathBuf,
    pub phase: Phase,
    /// Record was accepted, but idle preview pipelines are still stopping.
    pub record_pending: bool,
    pub portal_busy: bool,              // picker open
    pub session: Option<SessionSummary>,
    /// Computed by the engine: Idle && !portal_busy && no source Checking && >=1 selected input
    /// is Ready/selectable with its elements present (and h264_encoder / gpu_convert where needed).
    pub can_record: bool,
    pub transcription: TranscriptionInfo,
    /// Frame-rate mode new window/screen sources get.
    pub frame_rate_default: FrameRateMode,
    /// The selected camera's mode override (a `CameraModeInfo::label`); None = automatic.
    pub selected_camera_mode: Option<String>,
    pub capacity: CapacityPlan,
    /// Free-space estimate for the current selection on the output filesystem.
    pub storage: StorageInfo,
    /// Interrupted sessions from prior launches that are not locked by a live process.
    pub recoveries: Vec<RecoveryCandidate>,
    pub vocabulary: VocabularyInfo,
    /// Live preview state of the selected camera and every source; the frames themselves come
    /// from `EngineHandle::previews()`, never through the Snapshot.
    pub previews: Vec<(InputKey, PreviewState)>,
}

// ---- Live previews -----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum PreviewState { Starting, Live, Unavailable { reason: String } }

/// Where a preview frame came from: an idle preview pipeline, or the recording pipeline's tee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewOrigin { Idle, Recording { segment: u32 } }

/// One downscaled frame (fits in 480×300): tightly packed BGRA, alpha always 255.
#[derive(Clone, Debug)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub origin: PreviewOrigin,
    pub at: Instant,
}

/// Custom vocabulary (P0-2): glossary terms (Whisper prompt) and replacement rules.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct VocabularyInfo {
    pub terms: Vec<String>,
    pub replacements: Vec<ReplacementRule>,
    pub glossary_path: PathBuf,
    pub replacements_path: PathBuf,
    /// Problems reading the files (bad lines, unreadable file).
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplacementRule { pub from: String, pub to: String }

// ---- Transcription (SPEC §12.2) ----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhisperBackend { Vulkan, Cpu }

#[derive(Clone, Debug, PartialEq)]
pub enum ModelState {
    Checking,                                        // startup hash/sidecar check in progress
    Missing  { download_bytes: u64 },                // UI: "Download model (834.5 MiB)"
    Downloading { received: u64, total: u64 },       // total = bytes of the files still missing
    Cancelling,                                      // cancel requested; download thread not yet exited
    Verifying,
    Ready    { id: String, path: PathBuf, vad_download: Option<u64> }, // Some(bytes) => VAD missing, downloadable
    Failed   { reason: String, download_bytes: u64 },// retry = DownloadModel again (resumes .part)
    External { id: String, path: PathBuf },          // CHINSTRAP_WHISPER_MODEL=<path>: no hash check, no download
}
impl ModelState { pub fn usable(&self) -> bool { matches!(self, Self::Ready { .. } | Self::External { .. }) } }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscribeStage { Starting, Decoding, Detecting, Transcribing, Writing }

#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptStatus {
    None,                                            // not requested / no complete mic file
    Queued   { deferred: bool },                     // deferred: CPU backend waiting for recording to stop
    Running  { stage: TranscribeStage, percent: u8, paused: bool },
    Done     { json: PathBuf, srt: PathBuf, vtt: PathBuf, language: String, words: u32, backend: WhisperBackend },
    Failed   { reason: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct OtherSessionJobs { pub queued: u32, pub running_percent: Option<u8> }

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptionInfo {
    pub model: ModelState,
    pub auto: bool,                                  // default true
    pub backend: Option<WhisperBackend>,             // last backend a job actually used
    pub vulkan_broken: bool,                         // latched per §12.3 policy 7 => CPU for rest of process
    pub queued: u32,                                 // all sessions
    pub other_sessions: Option<OtherSessionJobs>,    // jobs belonging to ended sessions; None if none
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity { Info, Warning, Error }

#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub severity: Severity,
    pub input: Option<InputKey>,
    pub message: String,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EngineCommand {
    RefreshDevices,
    SelectCamera(Option<DeviceId>),
    SelectMicrophone(Option<DeviceId>),
    AddPortalSources,     // Idle && !portal_busy only
    AddTestSource,
    RemoveSource(SourceId),
    SetOutputRoot(PathBuf), // Idle && no session only
    Record,
    Stop,
    ToggleRecord,         // Record if Idle, Stop if Recording, else rejected
    Cut,
    AddMarker(String),    // In-memory issue #9 control path; persistence belongs to issue #10
    EndSession,           // Idle only
    RecoverSession { name: String },
    ShowSessionFolder { name: String },
    DismissRecovery { name: String },
    // Transcription (§12.2): contract extension; existing variants unchanged.
    DownloadModel,                 // Missing|Failed, or Ready{vad_download:Some}; downloads only missing files; resumes
    CancelModelDownload,           // Downloading|Verifying only
    SetAutoTranscribe(bool),
    Transcribe { segment: u32 },   // manual + retry; segment's mic Complete, segment not being recorded
    CancelTranscription { segment: u32 },
    CancelOtherSessionTranscriptions,
    // Frame rate, camera mode, vocabulary (P0-2, P1-5, P2-9). Sent by the UI.
    /// Idle only. `source: None` sets the default for window/screen sources and applies it to
    /// every one of them; `Some(id)` sets one source.
    #[allow(dead_code)]
    SetFrameRateMode { source: Option<SourceId>, mode: FrameRateMode },
    /// Idle only. A `CameraModeInfo::label` of the selected camera, or None for automatic.
    #[allow(dead_code)]
    SelectCameraMode(Option<String>),
    #[allow(dead_code)]
    AddVocabularyTerm(String),
    #[allow(dead_code)]
    RemoveVocabularyTerm(String),
    /// Adds a rule, replacing one with the same `from` (case-insensitive).
    #[allow(dead_code)]
    AddReplacement { from: String, to: String },
    #[allow(dead_code)]
    RemoveReplacement { from: String },
    // Transcribe { segment } on a Done segment re-transcribes it with the current vocabulary.
}

#[derive(Clone, Debug)]
pub enum EngineEvent {
    Snapshot(Box<Snapshot>),             // after EVERY state change; first event is always a Snapshot
    Notice(Notice),
    Level { rms_db: f32, peak_db: f32 }, // mic only, ≤10 Hz, max over channels, while recording
}
