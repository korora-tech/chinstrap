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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum H264Encoder { Va, X264, OpenH264 }
impl H264Encoder {
    pub const FALLBACK_ORDER: [H264Encoder; 3] = [Self::Va, Self::X264, Self::OpenH264];
    pub fn element_name(self) -> &'static str {
        match self { Self::Va => "vah264enc", Self::X264 => "x264enc", Self::OpenH264 => "openh264enc" }
    }
    pub fn from_element_name(s: &str) -> Option<Self> {
        Self::FALLBACK_ORDER.into_iter().find(|e| e.element_name() == s)
    }
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
    pub h264_encoder: Option<H264Encoder>, // None => video inputs cannot record
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct SegmentSummary {
    pub index: u32,
    pub dir: PathBuf,
    pub status: SegmentStatus,
    pub duration: Option<Duration>,     // stop_clock - B; None while recording
    pub gap_before: Option<Duration>,   // None for first segment of a session
    pub files: Vec<InputFile>,
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
    pub portal_busy: bool,              // picker open
    pub session: Option<SessionSummary>,
    /// Computed by the engine: Idle && !portal_busy && no source Checking && >=1 selected input
    /// is Ready/selectable with its elements present (and h264_encoder / gpu_convert where needed).
    pub can_record: bool,
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
    EndSession,           // Idle only
}

#[derive(Clone, Debug)]
pub enum EngineEvent {
    Snapshot(Box<Snapshot>),             // after EVERY state change; first event is always a Snapshot
    Notice(Notice),
    Level { rms_db: f32, peak_db: f32 }, // mic only, ≤10 Hz, max over channels, while recording
}
