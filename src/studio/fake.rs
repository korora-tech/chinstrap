//! Fake engine (SPEC §11 step 1), used while the real backend is built and for UI work
//! (`CHINSTRAP_FAKE_ENGINE=1`). It touches no devices, portals or files: every path it
//! reports is synthetic. It follows the same command rules as the real engine (§3.1):
//! out-of-phase commands are rejected with a Warning notice, never queued.
//!
//! Transcription is simulated too (§12): the model starts Missing, `DownloadModel` "downloads"
//! it in about 4 s, and each finished segment is "transcribed" in about 2 s.
//!
//! Frame rates, the capacity plan, camera modes, quality counters and the vocabulary follow the
//! real contract: the plan is the real planner over a fake encoder table (this host's measured
//! rates), windows wait ~2 s for their first frame, quality numbers tick while recording, and
//! the vocabulary is kept in memory (no files are written).
//!
//! Previews are synthetic: every ~66 ms each previewed input gets a 480×270 frame in its own
//! hue with a moving bar, framed in red while it is being recorded.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gstreamer as gst;

use super::encoders::EncoderCap;
use super::engine::{EngineConfig, Msg};
use super::plan::{self, Demand};
use super::preview::{self, Previews};
use super::transcribe::vocab;
use super::types::*;

const TICK: Duration = Duration::from_millis(50);
const STEP: Duration = Duration::from_millis(300);
const CHECK_DELAY: Duration = Duration::from_secs(1);
const PICKER_DELAY: Duration = Duration::from_secs(2);
const CLOSE_AFTER: Duration = Duration::from_secs(10);
const LEVEL_EVERY: Duration = Duration::from_millis(100);
const BYTES_EVERY: Duration = Duration::from_secs(1);
const FAKE_MODEL_BYTES: u64 = 874_188_075;
const FAKE_VAD_BYTES: u64 = 885_098;
const TX_STEP: Duration = Duration::from_millis(200);
/// Windows deliver their first frame this long after recording starts.
const FIRST_FRAME_AFTER: Duration = Duration::from_secs(2);
/// The fake encoder table: this host's measured 4K throughput.
const FAKE_ENCODERS: [(VideoEncoder, f64); 3] = [
    (VideoEncoder::Va, 98.0),
    (VideoEncoder::VaHevc, 136.0),
    (VideoEncoder::X264, 118.0),
];

pub(super) fn spawn(
    config: EngineConfig,
    rx: Receiver<Msg>,
    events: async_channel::Sender<EngineEvent>,
    previews: Previews,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("chinstrap-engine".into())
        .spawn(move || FakeEngine::new(config, events, previews).run(rx))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timer {
    SourceReady(SourceId),
    PickerDone,
    Started(u32),
    CutFinalized(u32),
    Stopped(u32),
    CaptureClosed(CaptureId),
}

struct FakeEngine {
    events: async_channel::Sender<EngineEvent>,
    caps: Capabilities,
    cameras: Vec<DeviceInfo>,
    microphones: Vec<DeviceInfo>,
    sources: Vec<SourceInfo>,
    selected_camera: Option<DeviceId>,
    selected_microphone: Option<DeviceId>,
    output_root: PathBuf,
    phase: Phase,
    portal_busy: bool,
    session: Option<SessionSummary>,
    next_source: u32,
    next_capture: u32,
    timers: Vec<(Instant, Timer)>,
    /// Captures whose "Closed" timer has been armed (first time they are recorded).
    close_armed: Vec<CaptureId>,
    /// Frozen input set of the current segment.
    inputs: Vec<InputKey>,
    stop_clock: Option<Instant>,
    /// (segment, Instant it reached Recording).
    segment_started: Option<(u32, Instant)>,
    last_level: Instant,
    last_bytes: Instant,
    level_phase: f32,
    model: ModelState,
    auto: bool,
    cancel_at: Option<Instant>,
    last_tx_step: Instant,
    frame_rate_default: FrameRateMode,
    camera_mode: Option<String>,
    vocabulary: VocabularyInfo,
    previews: Previews,
    preview_on: bool,
    last_preview: Instant,
    /// Clock of the synthetic preview animation.
    born: Instant,
    /// The preview states last sent (a change sends a Snapshot).
    preview_sent: Vec<(InputKey, PreviewState)>,
}

fn fake_mode(w: i32, h: i32, fps: i32, format: &str, auto: bool) -> CameraModeInfo {
    CameraModeInfo {
        label: format!("{w}×{h} @{fps} · {format}"),
        width: w,
        height: h,
        fps_n: fps,
        fps_d: 1,
        format: format.into(),
        auto,
    }
}

impl FakeEngine {
    fn new(
        config: EngineConfig,
        events: async_channel::Sender<EngineEvent>,
        previews: Previews,
    ) -> Self {
        let portal = if config.enable_portal {
            PortalAvailability::Available { version: 5 }
        } else {
            PortalAvailability::Unavailable {
                reason: "portal disabled".into(),
            }
        };
        let caps = Capabilities {
            h264_encoder: Some(config.preferred_encoder.unwrap_or(VideoEncoder::Va)),
            encoders: FAKE_ENCODERS.to_vec(),
            gpu_convert: Some(config.preferred_convert.unwrap_or(GpuConvert::Va)),
            missing_elements: Vec::new(),
            portal,
        };
        let cameras = vec![
            DeviceInfo {
                id: DeviceId(1),
                kind: DeviceKind::Camera,
                name: "Fake HD Webcam".into(),
                detail: "fake #80 · 1920×1080 @30 · MJPEG".into(),
                unsupported: None,
                modes: vec![
                    fake_mode(1920, 1080, 30, "MJPEG", true),
                    fake_mode(3840, 2160, 25, "NV12", false),
                    fake_mode(1280, 720, 60, "MJPEG", false),
                    fake_mode(1920, 1080, 5, "YUY2", false),
                ],
            },
            DeviceInfo {
                id: DeviceId(2),
                kind: DeviceKind::Camera,
                name: "Fake Loopback Dummy".into(),
                detail: "fake #81 · 3840×2160 @25 · DMA-BUF NV12".into(),
                unsupported: Some("no usable format: video/x-raw(memory:DMABuf)".into()),
                modes: Vec::new(),
            },
        ];
        let microphones = vec![
            DeviceInfo {
                id: DeviceId(3),
                kind: DeviceKind::Microphone,
                name: "Fake USB Microphone".into(),
                detail: "fake #61 · 48 kHz · 2 ch".into(),
                unsupported: None,
                modes: Vec::new(),
            },
            DeviceInfo {
                id: DeviceId(4),
                kind: DeviceKind::Microphone,
                name: "Fake Built-in Audio".into(),
                detail: "fake #62 · 44.1 kHz · 1 ch".into(),
                unsupported: None,
                modes: Vec::new(),
            },
        ];
        let now = Instant::now();
        Self {
            events,
            caps,
            selected_microphone: microphones.first().map(|m| m.id),
            cameras,
            microphones,
            sources: Vec::new(),
            selected_camera: None,
            output_root: config.output_root,
            phase: Phase::Idle,
            portal_busy: false,
            session: None,
            next_source: 1,
            next_capture: 1,
            timers: Vec::new(),
            close_armed: Vec::new(),
            inputs: Vec::new(),
            stop_clock: None,
            segment_started: None,
            last_level: now,
            last_bytes: now,
            level_phase: 0.0,
            model: ModelState::Missing {
                download_bytes: FAKE_MODEL_BYTES + FAKE_VAD_BYTES,
            },
            auto: true,
            cancel_at: None,
            last_tx_step: now,
            frame_rate_default: config.frame_rate,
            camera_mode: None,
            vocabulary: VocabularyInfo {
                terms: vec!["Rawkode".into(), "Kubernetes".into()],
                replacements: vec![ReplacementRule {
                    from: "raw code".into(),
                    to: "Rawkode".into(),
                }],
                glossary_path: PathBuf::from("/fake/chinstrap/glossary.txt"),
                replacements_path: PathBuf::from("/fake/chinstrap/replacements.txt"),
                warnings: Vec::new(),
            },
            previews,
            preview_on: config.preview,
            last_preview: now,
            born: now,
            preview_sent: Vec::new(),
        }
    }

    /// The selected camera's mode (override or automatic).
    fn camera_mode_info(&self) -> Option<&CameraModeInfo> {
        let cam = self
            .selected_camera
            .and_then(|id| self.cameras.iter().find(|c| c.id == id))?;
        match &self.camera_mode {
            Some(l) => cam.modes.iter().find(|m| &m.label == l),
            None => cam.modes.iter().find(|m| m.auto),
        }
    }

    /// The real planner over the fake encoder table.
    fn capacity(&self) -> CapacityPlan {
        let table: Vec<EncoderCap> = FAKE_ENCODERS
            .iter()
            .map(|&(enc, fps)| EncoderCap {
                enc,
                max: (4096, 2160),
                fps_at_max: fps,
            })
            .collect();
        let mut demands = Vec::new();
        for key in self.frozen_inputs() {
            match key {
                InputKey::Microphone => {}
                InputKey::Camera => {
                    if let Some(m) = self.camera_mode_info() {
                        demands.push(Demand {
                            key,
                            label: "Camera".into(),
                            size: (m.width, m.height),
                            fps: m.fps_n as f64 / m.fps_d.max(1) as f64,
                            native: false,
                            gpu_path: false,
                            camera: true,
                        });
                    }
                }
                InputKey::Source(id) => {
                    if let Some(s) = self.sources.iter().find(|s| s.id == id) {
                        let (fps, native) = match s.frame_rate.cfr_fps() {
                            Some(f) => (f as f64, false),
                            None => (s.refresh.map_or(60.0, |(n, d)| n as f64 / d as f64), true),
                        };
                        demands.push(Demand {
                            key,
                            label: s.label.clone(),
                            size: s.size.unwrap_or((3840, 2160)),
                            fps,
                            native,
                            gpu_path: true,
                            camera: false,
                        });
                    }
                }
            }
        }
        let gpu_ok: Vec<VideoEncoder> = FAKE_ENCODERS.iter().map(|e| e.0).collect();
        let p = plan::plan(&demands, &table, &gpu_ok);
        CapacityPlan {
            inputs: p
                .inputs
                .iter()
                .map(|a| {
                    let size = demands
                        .iter()
                        .find(|d| d.key == a.key)
                        .map_or((3840, 2160), |d| d.size);
                    PlannedInput {
                        input: a.key,
                        encoder: a.enc,
                        width: size.0,
                        height: size.1,
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
            notes: p.notes,
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        self.snapshot();
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Msg::Cmd(cmd)) => self.command(cmd),
                Ok(Msg::Shutdown { deadline: _, ack }) => {
                    self.shutdown();
                    let _ = ack.send(true);
                    return;
                }
                #[allow(unreachable_patterns)]
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.shutdown();
                    return;
                }
            }
            self.poll();
        }
    }

    // ---- events ---------------------------------------------------------------------------

    fn snapshot(&mut self) {
        self.preview_sent = self.preview_states();
        let snap = Snapshot {
            capabilities: self.caps.clone(),
            cameras: self.cameras.clone(),
            microphones: self.microphones.clone(),
            sources: self.sources.clone(),
            selected_camera: self.selected_camera,
            selected_microphone: self.selected_microphone,
            output_root: self.output_root.clone(),
            phase: self.phase.clone(),
            portal_busy: self.portal_busy,
            session: self.session.clone(),
            can_record: self.can_record(),
            transcription: TranscriptionInfo {
                model: self.model.clone(),
                auto: self.auto,
                backend: self.session.as_ref().and_then(|s| {
                    s.segments
                        .iter()
                        .any(|g| matches!(g.transcript, TranscriptStatus::Done { .. }))
                        .then_some(WhisperBackend::Vulkan)
                }),
                vulkan_broken: false,
                queued: self.session.as_ref().map_or(0, |s| {
                    s.segments
                        .iter()
                        .filter(|g| matches!(g.transcript, TranscriptStatus::Queued { .. }))
                        .count() as u32
                }),
                other_sessions: None,
            },
            frame_rate_default: self.frame_rate_default,
            selected_camera_mode: self.camera_mode.clone(),
            capacity: self.capacity(),
            storage: StorageInfo::default(),
            recoveries: Vec::new(),
            vocabulary: self.vocabulary.clone(),
            previews: self.preview_sent.clone(),
        };
        let _ = self
            .events
            .send_blocking(EngineEvent::Snapshot(Box::new(snap)));
    }

    fn notice(&self, severity: Severity, input: Option<InputKey>, message: impl Into<String>) {
        let _ = self.events.send_blocking(EngineEvent::Notice(Notice {
            severity,
            input,
            message: message.into(),
            detail: Some("fake engine".into()),
        }));
    }

    fn reject(&self, cmd: &EngineCommand, why: &str) {
        self.notice(Severity::Warning, None, format!("Rejected {cmd:?}: {why}"));
    }

    // ---- derived state ---------------------------------------------------------------------

    fn camera_ok(&self) -> bool {
        self.selected_camera
            .and_then(|id| self.cameras.iter().find(|c| c.id == id))
            .is_some_and(|c| c.unsupported.is_none())
    }

    fn frozen_inputs(&self) -> Vec<InputKey> {
        let mut inputs = Vec::new();
        if self.camera_ok() {
            inputs.push(InputKey::Camera);
        }
        if self.selected_microphone.is_some() {
            inputs.push(InputKey::Microphone);
        }
        inputs.extend(
            self.sources
                .iter()
                .filter(|s| s.state == SourceState::Ready)
                .map(|s| InputKey::Source(s.id)),
        );
        inputs
    }

    fn can_record(&self) -> bool {
        self.phase == Phase::Idle
            && !self.portal_busy
            && !self
                .sources
                .iter()
                .any(|s| s.state == SourceState::Checking)
            && !self.frozen_inputs().is_empty()
    }

    fn after(&mut self, delay: Duration, timer: Timer) {
        self.timers.push((Instant::now() + delay, timer));
    }

    // ---- previews -------------------------------------------------------------------------

    /// Inputs with a preview tile: the selected camera, then every source.
    fn preview_keys(&self) -> Vec<InputKey> {
        self.selected_camera
            .map(|_| InputKey::Camera)
            .into_iter()
            .chain(self.sources.iter().map(|s| InputKey::Source(s.id)))
            .collect()
    }

    /// Inputs that get frames now: idle, every usable one; busy, the recorded ones.
    fn previewed(&self) -> Vec<InputKey> {
        let ready = |k: &InputKey| match k {
            InputKey::Camera => self.camera_ok(),
            InputKey::Microphone => false,
            InputKey::Source(id) => self
                .sources
                .iter()
                .any(|s| s.id == *id && s.state == SourceState::Ready),
        };
        if !self.preview_on {
            return Vec::new();
        }
        match self.current_segment() {
            None => self.preview_keys().into_iter().filter(ready).collect(),
            Some(_) => self.inputs.iter().copied().filter(ready).collect(),
        }
    }

    fn current_segment(&self) -> Option<u32> {
        match self.phase {
            Phase::Idle => None,
            Phase::Starting { segment }
            | Phase::Recording { segment, .. }
            | Phase::Stopping { segment } => Some(segment),
            Phase::Cutting { from } => Some(from),
        }
    }

    fn preview_states(&self) -> Vec<(InputKey, PreviewState)> {
        let previewed = self.previewed();
        self.preview_keys()
            .into_iter()
            .map(|k| {
                let state = if !self.preview_on {
                    PreviewState::Unavailable {
                        reason: "previews are off (CHINSTRAP_PREVIEW=0)".into(),
                    }
                } else if previewed.contains(&k) && self.previews.generation(k) > 0 {
                    PreviewState::Live
                } else {
                    match self.sources.iter().find(|s| InputKey::Source(s.id) == k) {
                        Some(SourceInfo {
                            state:
                                SourceState::Unsupported { reason } | SourceState::Lost { reason },
                            ..
                        }) => PreviewState::Unavailable {
                            reason: reason.clone(),
                        },
                        _ => PreviewState::Starting,
                    }
                };
                (k, state)
            })
            .collect()
    }

    fn publish_previews(&mut self) {
        let keys = self.preview_keys();
        self.previews.retain(&keys);
        let origin = match self.phase {
            Phase::Recording { segment, .. } => PreviewOrigin::Recording { segment },
            _ => PreviewOrigin::Idle,
        };
        // Starting/Stopping/Cutting: the recording pipelines are not producing yet / anymore.
        if self.current_segment().is_none() || origin != PreviewOrigin::Idle {
            let t = self.born.elapsed().as_secs_f64();
            for key in self.previewed() {
                self.previews.publish(key, fake_frame(key, origin, t));
            }
        }
        if self.preview_states() != self.preview_sent {
            self.snapshot();
        }
    }

    // ---- commands -------------------------------------------------------------------------

    fn command(&mut self, cmd: EngineCommand) {
        let busy = self.phase.is_busy();
        match &cmd {
            EngineCommand::RefreshDevices => {
                self.notice(Severity::Info, None, "Devices refreshed");
                self.snapshot();
            }
            EngineCommand::SelectCamera(_)
            | EngineCommand::SelectMicrophone(_)
            | EngineCommand::AddPortalSources
            | EngineCommand::AddTestSource
            | EngineCommand::RemoveSource(_)
            | EngineCommand::SetOutputRoot(_)
            | EngineCommand::NewVideo { .. }
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
                        if s.kind != SourceKind::TestPattern {
                            s.frame_rate = *mode;
                        }
                    }
                    self.snapshot();
                }
                Some(id) => match self.sources.iter_mut().find(|s| s.id == *id) {
                    Some(s) => {
                        s.frame_rate = *mode;
                        self.snapshot();
                    }
                    None => self.reject(&cmd, "unknown source"),
                },
            },
            EngineCommand::SelectCameraMode(label) => {
                let known = match label {
                    None => true,
                    Some(l) => self
                        .selected_camera
                        .and_then(|id| self.cameras.iter().find(|c| c.id == id))
                        .is_some_and(|c| c.modes.iter().any(|m| &m.label == l)),
                };
                if known {
                    self.camera_mode = label.clone();
                    self.snapshot();
                } else {
                    self.reject(&cmd, "the selected camera has no such mode");
                }
            }
            EngineCommand::AddVocabularyTerm(t) => match vocab::term(t) {
                Ok(t)
                    if self
                        .vocabulary
                        .terms
                        .iter()
                        .any(|x| x.eq_ignore_ascii_case(&t)) =>
                {
                    self.reject(&cmd, &format!("{t:?} is already in the glossary"))
                }
                Ok(t) => {
                    self.vocabulary.terms.push(t);
                    self.snapshot();
                }
                Err(e) => self.reject(&cmd, &e),
            },
            EngineCommand::RemoveVocabularyTerm(t) => {
                let before = self.vocabulary.terms.len();
                self.vocabulary
                    .terms
                    .retain(|x| !x.eq_ignore_ascii_case(t.trim()));
                if self.vocabulary.terms.len() == before {
                    self.reject(&cmd, "not in the glossary");
                } else {
                    self.snapshot();
                }
            }
            EngineCommand::AddReplacement { from, to } => match vocab::rule(from, to) {
                Ok(r) => {
                    self.vocabulary
                        .replacements
                        .retain(|x| !x.from.eq_ignore_ascii_case(&r.from));
                    self.vocabulary.replacements.push(ReplacementRule {
                        from: r.from,
                        to: r.to,
                    });
                    self.snapshot();
                }
                Err(e) => self.reject(&cmd, &e),
            },
            EngineCommand::RemoveReplacement { from } => {
                let before = self.vocabulary.replacements.len();
                self.vocabulary
                    .replacements
                    .retain(|x| !x.from.eq_ignore_ascii_case(from.trim()));
                if self.vocabulary.replacements.len() == before {
                    self.reject(&cmd, "no such replacement");
                } else {
                    self.snapshot();
                }
            }
            EngineCommand::SelectCamera(id) => match id {
                None => {
                    self.selected_camera = None;
                    self.camera_mode = None;
                    self.snapshot();
                }
                Some(id) => match self.cameras.iter().find(|c| c.id == *id) {
                    Some(c) if c.unsupported.is_none() => {
                        if self.selected_camera != Some(*id) {
                            self.camera_mode = None;
                        }
                        self.selected_camera = Some(*id);
                        self.snapshot();
                    }
                    Some(_) => self.reject(&cmd, "camera is not usable"),
                    None => self.reject(&cmd, "unknown camera"),
                },
            },
            EngineCommand::SelectMicrophone(id) => match id {
                None => {
                    self.selected_microphone = None;
                    self.snapshot();
                }
                Some(id) if self.microphones.iter().any(|m| m.id == *id) => {
                    self.selected_microphone = Some(*id);
                    self.snapshot();
                }
                Some(_) => self.reject(&cmd, "unknown microphone"),
            },
            EngineCommand::AddPortalSources => {
                if self.portal_busy {
                    self.reject(&cmd, "the picker is already open");
                } else if !matches!(self.caps.portal, PortalAvailability::Available { .. }) {
                    self.reject(&cmd, "the screencast portal is unavailable");
                } else {
                    self.portal_busy = true;
                    self.after(PICKER_DELAY, Timer::PickerDone);
                    self.snapshot();
                }
            }
            EngineCommand::AddTestSource => {
                let id = self.alloc_source();
                self.sources.push(SourceInfo {
                    id,
                    capture: None,
                    kind: SourceKind::TestPattern,
                    label: format!("Test pattern {}", id.0),
                    logical_size: Some((1280, 720)),
                    state: SourceState::Checking,
                    frame_rate: FrameRateMode::Cfr30,
                    refresh: Some((60, 1)),
                    size: Some((1280, 720)),
                });
                self.after(CHECK_DELAY, Timer::SourceReady(id));
                self.snapshot();
            }
            EngineCommand::RemoveSource(id) => {
                let before = self.sources.len();
                self.sources.retain(|s| s.id != *id);
                if self.sources.len() == before {
                    self.reject(&cmd, "unknown source");
                } else {
                    self.timers.retain(|(_, t)| *t != Timer::SourceReady(*id));
                    self.snapshot();
                }
            }
            EngineCommand::SetOutputRoot(root) => {
                if self.session.is_some() {
                    self.reject(&cmd, "a session is open");
                } else {
                    self.output_root = std::path::absolute(root).unwrap_or_else(|_| root.clone());
                    self.snapshot();
                }
            }
            EngineCommand::NewVideo { name } => match super::session::video_dir_name(name) {
                Err(why) => self.reject(&cmd, why),
                Ok(name) => {
                    self.open_session(name);
                    self.snapshot();
                }
            },
            EngineCommand::Record => self.record(&cmd),
            EngineCommand::Stop => self.stop(&cmd),
            EngineCommand::ToggleRecord => match self.phase {
                Phase::Idle => self.record(&cmd),
                Phase::Recording { .. } => self.stop(&cmd),
                _ => self.reject(&cmd, "engine is busy"),
            },
            EngineCommand::Cut => self.cut(&cmd),
            EngineCommand::DownloadModel => match self.model {
                ModelState::Missing { download_bytes }
                | ModelState::Failed { download_bytes, .. } => {
                    self.model = ModelState::Downloading {
                        received: 0,
                        total: download_bytes,
                    };
                    self.snapshot();
                }
                _ => self.reject(&cmd, "nothing to download"),
            },
            EngineCommand::CancelModelDownload => match self.model {
                ModelState::Downloading { .. } | ModelState::Verifying => {
                    self.model = ModelState::Cancelling;
                    self.cancel_at = Some(Instant::now() + STEP);
                    self.snapshot();
                }
                _ => self.reject(&cmd, "no download is running"),
            },
            EngineCommand::SetAutoTranscribe(on) => {
                self.auto = *on;
                if *on {
                    self.backfill();
                }
                self.snapshot();
            }
            EngineCommand::Transcribe { segment } => {
                let usable = self.model.usable();
                match self.segment_mut(*segment) {
                    None => self.reject(&cmd, "unknown segment"),
                    Some(_) if !usable => self.reject(&cmd, "the Whisper model is not ready"),
                    Some(seg) if seg.status == SegmentStatus::Recording => {
                        self.reject(&cmd, "the segment is still recording")
                    }
                    Some(seg)
                        if matches!(
                            seg.transcript,
                            TranscriptStatus::Queued { .. } | TranscriptStatus::Running { .. }
                        ) =>
                    {
                        self.reject(&cmd, "already queued or transcribing")
                    }
                    Some(seg) => {
                        seg.transcript = TranscriptStatus::Queued { deferred: false };
                        self.snapshot();
                    }
                }
            }
            EngineCommand::CancelTranscription { segment } => match self.segment_mut(*segment) {
                Some(seg)
                    if matches!(
                        seg.transcript,
                        TranscriptStatus::Queued { .. } | TranscriptStatus::Running { .. }
                    ) =>
                {
                    seg.transcript = TranscriptStatus::None;
                    self.snapshot();
                }
                _ => self.reject(&cmd, "not queued or transcribing"),
            },
            EngineCommand::CancelOtherSessionTranscriptions => {
                self.reject(&cmd, "no jobs from earlier sessions")
            }
            EngineCommand::RecoverSession { .. }
            | EngineCommand::ShowSessionFolder { .. }
            | EngineCommand::DismissRecovery { .. } => self.reject(
                &cmd,
                "interrupted-session recovery is unavailable in the fake engine",
            ),
        }
    }

    /// Queues every finished segment with a mic file and no transcript.
    fn backfill(&mut self) {
        if !self.auto || !self.model.usable() {
            return;
        }
        if let Some(s) = self.session.as_mut() {
            for g in &mut s.segments {
                let mic = g
                    .files
                    .iter()
                    .any(|f| f.input == InputKey::Microphone && f.status == InputStatus::Complete);
                if mic
                    && g.status != SegmentStatus::Recording
                    && g.transcript == TranscriptStatus::None
                {
                    g.transcript = TranscriptStatus::Queued { deferred: false };
                }
            }
        }
    }

    /// Advances the fake download and the fake transcription jobs.
    fn poll_transcription(&mut self) {
        let now = Instant::now();
        if self.model == ModelState::Cancelling && self.cancel_at.is_some_and(|t| now >= t) {
            self.cancel_at = None;
            self.model = ModelState::Missing {
                download_bytes: FAKE_MODEL_BYTES + FAKE_VAD_BYTES,
            };
            self.snapshot();
        }
        if now.duration_since(self.last_tx_step) < TX_STEP {
            return;
        }
        self.last_tx_step = now;
        let mut changed = false;
        if let ModelState::Downloading { received, total } = self.model {
            let next = received + total / 20;
            self.model = if next >= total {
                ModelState::Ready {
                    id: "large-v3-turbo-q8_0".into(),
                    path: self
                        .output_root
                        .join("fake-models/ggml-large-v3-turbo-q8_0.bin"),
                    vad_download: None,
                }
            } else {
                ModelState::Downloading {
                    received: next,
                    total,
                }
            };
            if self.model.usable() {
                self.backfill();
            }
            changed = true;
        }
        if let Some(s) = self.session.as_mut() {
            let running = s
                .segments
                .iter_mut()
                .find(|g| matches!(g.transcript, TranscriptStatus::Running { .. }));
            match running {
                Some(g) => {
                    if let TranscriptStatus::Running { percent, .. } = g.transcript {
                        let p = percent.saturating_add(10);
                        g.transcript = if p >= 100 {
                            TranscriptStatus::Done {
                                json: g.dir.join("transcript.json"),
                                srt: g.dir.join("mic.srt"),
                                vtt: g.dir.join("mic.vtt"),
                                language: "en".into(),
                                words: 42,
                                backend: WhisperBackend::Vulkan,
                            }
                        } else {
                            TranscriptStatus::Running {
                                stage: TranscribeStage::Transcribing,
                                percent: p,
                                paused: false,
                            }
                        };
                    }
                    changed = true;
                }
                None => {
                    if let Some(g) = s
                        .segments
                        .iter_mut()
                        .find(|g| matches!(g.transcript, TranscriptStatus::Queued { .. }))
                    {
                        g.transcript = TranscriptStatus::Running {
                            stage: TranscribeStage::Decoding,
                            percent: 0,
                            paused: false,
                        };
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.snapshot();
        }
    }

    fn alloc_source(&mut self) -> SourceId {
        let id = SourceId(self.next_source);
        self.next_source += 1;
        id
    }

    fn record(&mut self, cmd: &EngineCommand) {
        if !self.can_record() {
            let why = if self.phase.is_busy() {
                "engine is busy"
            } else {
                "nothing recordable is selected, or a source is still being checked"
            };
            self.reject(cmd, why);
            return;
        }
        if self.session.is_none() {
            let name = gst::glib::DateTime::now_local()
                .ok()
                .and_then(|now| now.format("%Y-%m-%dT%H-%M-%S").ok())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "fake-session".into());
            self.open_session(name);
        }
        self.inputs = self.frozen_inputs();
        let segment = self.begin_segment(None);
        self.phase = Phase::Starting { segment };
        self.after(STEP, Timer::Started(segment));
        self.snapshot();
    }

    fn stop(&mut self, cmd: &EngineCommand) {
        let Phase::Recording { segment, .. } = self.phase else {
            self.reject(cmd, "not recording");
            return;
        };
        self.stop_clock = Some(Instant::now());
        self.set_file_status(segment, |s| {
            matches!(s, InputStatus::Recording).then_some(InputStatus::Finalizing)
        });
        self.phase = Phase::Stopping { segment };
        self.after(STEP, Timer::Stopped(segment));
        self.snapshot();
    }

    fn cut(&mut self, cmd: &EngineCommand) {
        let Phase::Recording { segment, .. } = self.phase else {
            self.reject(cmd, "not recording");
            return;
        };
        self.stop_clock = Some(Instant::now());
        self.set_file_status(segment, |s| {
            matches!(s, InputStatus::Recording).then_some(InputStatus::Finalizing)
        });
        self.phase = Phase::Cutting { from: segment };
        self.after(STEP, Timer::CutFinalized(segment));
        self.snapshot();
    }

    // ---- segments -------------------------------------------------------------------------

    fn file_for(&self, dir: &std::path::Path, input: InputKey) -> InputFile {
        let (label, file) = match input {
            InputKey::Camera => ("Camera".to_string(), "camera.mkv".to_string()),
            InputKey::Microphone => ("Microphone".to_string(), "mic.mkv".to_string()),
            InputKey::Source(id) => {
                let src = self.sources.iter().find(|s| s.id == id);
                let label = src.map_or_else(|| format!("Source {}", id.0), |s| s.label.clone());
                let stem = match src.map(|s| s.kind) {
                    Some(SourceKind::Window) => "window",
                    Some(SourceKind::TestPattern) => "test",
                    _ => "screen",
                };
                (label, format!("{stem}-{}.mkv", id.0))
            }
        };
        InputFile {
            input,
            label,
            path: dir.join(file),
            status: InputStatus::Starting,
            bytes: 0,
            waiting_for_frame: false,
            quality: None,
        }
    }

    fn open_session(&mut self, name: String) {
        let dir = self.output_root.join(&name);
        self.session = Some(SessionSummary {
            manifest: dir.join("session.json"),
            name,
            dir,
            earlier_segments: 0,
            segments: Vec::new(),
        });
    }

    /// Appends a new segment row for the frozen input set and returns its index.
    // (Quality numbers are filled in by `poll` while recording.)
    fn begin_segment(&mut self, gap_before: Option<Duration>) -> u32 {
        let session = self
            .session
            .as_ref()
            .expect("session exists when recording");
        let index = session.segments.len() as u32 + 1;
        let dir = session.dir.join(format!("segment-{index:03}"));
        let files = self
            .inputs
            .iter()
            .map(|&i| self.file_for(&dir, i))
            .collect();
        let session = self
            .session
            .as_mut()
            .expect("session exists when recording");
        session.segments.push(SegmentSummary {
            index,
            dir,
            status: SegmentStatus::Recording,
            duration: None,
            gap_before,
            files,
            transcript: TranscriptStatus::None,
        });
        index
    }

    fn segment_mut(&mut self, index: u32) -> Option<&mut SegmentSummary> {
        self.session
            .as_mut()?
            .segments
            .iter_mut()
            .find(|s| s.index == index)
    }

    fn set_file_status(&mut self, index: u32, f: impl Fn(&InputStatus) -> Option<InputStatus>) {
        if let Some(seg) = self.segment_mut(index) {
            for file in &mut seg.files {
                if let Some(next) = f(&file.status) {
                    file.status = next;
                }
            }
        }
    }

    /// Completes segment `index`: finalizing inputs become Complete; returns the inputs that survived.
    fn finish_segment(&mut self, index: u32) -> Vec<InputKey> {
        let stop = self.stop_clock.unwrap_or_else(Instant::now);
        let recording_since = self
            .segment_started
            .and_then(|(i, t)| (i == index).then_some(t));
        let Some(seg) = self.segment_mut(index) else {
            return Vec::new();
        };
        let mut survivors = Vec::new();
        for file in &mut seg.files {
            match file.status {
                InputStatus::Failed { .. } => {}
                InputStatus::Starting => {
                    file.status = InputStatus::Failed {
                        reason: "aborted before recording started".into(),
                    };
                }
                _ => {
                    file.status = InputStatus::Complete;
                    survivors.push(file.input);
                }
            }
        }
        seg.duration = recording_since.map(|s| stop.saturating_duration_since(s));
        seg.status = if survivors.is_empty() {
            SegmentStatus::Incomplete {
                reason: "every input failed".into(),
            }
        } else {
            SegmentStatus::Complete
        };
        self.backfill();
        survivors
    }

    // ---- timers and periodic work ---------------------------------------------------------

    fn poll(&mut self) {
        let now = Instant::now();
        let mut due = Vec::new();
        self.timers.retain(|&(at, t)| {
            if at <= now {
                due.push(t);
                false
            } else {
                true
            }
        });
        for t in due {
            self.fire(t);
        }
        self.poll_transcription();
        if now.duration_since(self.last_preview) >= preview::PREVIEW_EVERY {
            self.last_preview = now;
            self.publish_previews();
        }

        if let Phase::Recording { segment, .. } = self.phase {
            if now.duration_since(self.last_level) >= LEVEL_EVERY {
                self.last_level = now;
                if self.inputs.contains(&InputKey::Microphone) {
                    self.level_phase += 0.37;
                    let rms = -30.0 + 12.0 * self.level_phase.sin();
                    let peak = (rms + 6.0 + 4.0 * (self.level_phase * 2.3).cos()).min(0.0);
                    let _ = self.events.send_blocking(EngineEvent::Level {
                        rms_db: rms,
                        peak_db: peak,
                    });
                }
            }
            if now.duration_since(self.last_bytes) >= BYTES_EVERY {
                self.last_bytes = now;
                let since = self
                    .segment_started
                    .map_or(Duration::ZERO, |(_, t)| t.elapsed());
                let plan = self.capacity();
                if let Some(seg) = self.segment_mut(segment) {
                    for file in &mut seg.files {
                        if file.status == InputStatus::Recording {
                            file.bytes += match file.input {
                                InputKey::Microphone => 24_000,
                                _ => 1_500_000,
                            };
                            fake_quality(file, since, &plan);
                        }
                    }
                }
                self.snapshot();
            }
        }
    }

    fn fire(&mut self, timer: Timer) {
        match timer {
            Timer::SourceReady(id) => {
                if let Some(s) = self.sources.iter_mut().find(|s| s.id == id)
                    && s.state == SourceState::Checking
                {
                    s.state = SourceState::Ready;
                }
                self.snapshot();
            }
            Timer::PickerDone => {
                self.portal_busy = false;
                let capture = CaptureId(self.next_capture);
                self.next_capture += 1;
                for _ in 0..2 {
                    let id = self.alloc_source();
                    self.sources.push(SourceInfo {
                        id,
                        capture: Some(capture),
                        kind: SourceKind::Window,
                        label: format!("Window {}", id.0),
                        logical_size: Some((1600, 1000)),
                        state: SourceState::Checking,
                        frame_rate: self.frame_rate_default,
                        refresh: Some((143_999, 1000)),
                        size: Some((3840, 2082)),
                    });
                    self.after(CHECK_DELAY, Timer::SourceReady(id));
                }
                self.snapshot();
            }
            Timer::Started(segment) => {
                if self.phase != (Phase::Starting { segment }) {
                    return;
                }
                let now = Instant::now();
                self.segment_started = Some((segment, now));
                self.phase = Phase::Recording {
                    segment,
                    started_at: now,
                };
                self.last_bytes = now;
                self.set_file_status(segment, |s| {
                    matches!(s, InputStatus::Starting).then_some(InputStatus::Recording)
                });
                self.arm_capture_close();
                self.snapshot();
            }
            Timer::Stopped(segment) => {
                if self.phase != (Phase::Stopping { segment }) {
                    return;
                }
                self.finish_segment(segment);
                self.phase = Phase::Idle;
                self.inputs.clear();
                self.snapshot();
            }
            Timer::CutFinalized(from) => {
                if self.phase != (Phase::Cutting { from }) {
                    return;
                }
                let survivors = self.finish_segment(from);
                // Frozen set minus Failed or Lost inputs.
                self.inputs = survivors
                    .into_iter()
                    .filter(|i| match i {
                        InputKey::Source(id) => self
                            .sources
                            .iter()
                            .any(|s| s.id == *id && s.state == SourceState::Ready),
                        _ => true,
                    })
                    .collect();
                if self.inputs.is_empty() {
                    self.phase = Phase::Idle;
                    self.notice(Severity::Warning, None, "Cut: no inputs left to record");
                    self.snapshot();
                    return;
                }
                let gap = self.stop_clock.map(|s| s.elapsed() + STEP);
                let segment = self.begin_segment(gap);
                self.phase = Phase::Starting { segment };
                self.after(STEP, Timer::Started(segment));
                self.snapshot();
            }
            Timer::CaptureClosed(capture) => self.capture_closed(capture),
        }
    }

    fn arm_capture_close(&mut self) {
        let captures: Vec<CaptureId> = self
            .inputs
            .iter()
            .filter_map(|i| match i {
                InputKey::Source(id) => self.sources.iter().find(|s| s.id == *id)?.capture,
                _ => None,
            })
            .collect();
        for capture in captures {
            if !self.close_armed.contains(&capture) {
                self.close_armed.push(capture);
                self.after(CLOSE_AFTER, Timer::CaptureClosed(capture));
            }
        }
    }

    fn capture_closed(&mut self, capture: CaptureId) {
        let mut lost = Vec::new();
        for s in self
            .sources
            .iter_mut()
            .filter(|s| s.capture == Some(capture))
        {
            s.state = SourceState::Lost {
                reason: "Share ended".into(),
            };
            lost.push(s.id);
        }
        if lost.is_empty() {
            return;
        }
        self.notice(
            Severity::Warning,
            lost.first().map(|&id| InputKey::Source(id)),
            format!("Screen share ended ({} source(s) lost)", lost.len()),
        );
        if let Phase::Recording { segment, .. } = self.phase
            && let Some(seg) = self.segment_mut(segment)
        {
            for file in &mut seg.files {
                if matches!(file.input, InputKey::Source(id) if lost.contains(&id)) {
                    file.status = InputStatus::Failed {
                        reason: "Source lost".into(),
                    };
                }
            }
            let all_failed = seg
                .files
                .iter()
                .all(|f| matches!(f.status, InputStatus::Failed { .. }));
            if all_failed {
                self.stop_clock = Some(Instant::now());
                self.finish_segment(segment);
                self.phase = Phase::Idle;
                self.inputs.clear();
            }
        }
        self.snapshot();
    }

    /// Finalizes whatever is active, immediately (the fake has nothing to wait for).
    fn shutdown(&mut self) {
        let segment = match self.phase {
            Phase::Idle => None,
            Phase::Starting { segment }
            | Phase::Recording { segment, .. }
            | Phase::Stopping { segment } => Some(segment),
            Phase::Cutting { from } => Some(from),
        };
        if let Some(segment) = segment {
            if self.stop_clock.is_none() || matches!(self.phase, Phase::Recording { .. }) {
                self.stop_clock = Some(Instant::now());
            }
            self.finish_segment(segment);
            self.phase = Phase::Idle;
            self.inputs.clear();
            self.snapshot();
        }
    }
}

/// A synthetic 480×270 preview frame: a gradient in the input's own hue, a bar sweeping
/// across every 2 s, and a red frame while recording.
fn fake_frame(key: InputKey, origin: PreviewOrigin, t: f64) -> PreviewFrame {
    let (w, h) = preview::fit(1920, 1080);
    let (w, h) = (w as usize, h as usize);
    let hue = match key {
        InputKey::Camera => 200.0,
        InputKey::Microphone => 0.0,
        InputKey::Source(id) => (id.0 as f64 * 67.0) % 360.0,
    };
    let bar = ((t / 2.0).fract() * w as f64) as usize;
    let recording = matches!(origin, PreviewOrigin::Recording { .. });
    let mut bgra = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let border = recording && (x < 6 || y < 6 || x >= w - 6 || y >= h - 6);
            let (r, g, b) = if border {
                (230, 40, 50)
            } else if x.abs_diff(bar) < 8 {
                (240, 240, 240)
            } else {
                hsv(hue, 0.6, 0.25 + 0.55 * y as f64 / h as f64)
            };
            bgra.extend_from_slice(&[b, g, r, 255]);
        }
    }
    PreviewFrame {
        width: w as u32,
        height: h as u32,
        bgra,
        origin,
        at: Instant::now(),
    }
}

/// HSV (hue in degrees) to 8-bit RGB.
fn hsv(h: f64, s: f64, v: f64) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let q = |v: f64| ((v + m) * 255.0).round() as u8;
    (q(r), q(g), q(b))
}

/// Plausible live counters: windows wait for their first frame, then run near their planned
/// rate; the camera repeats a few frames; quality is a pure function of elapsed time.
fn fake_quality(file: &mut InputFile, since: Duration, plan: &CapacityPlan) {
    if file.input == InputKey::Microphone {
        return;
    }
    let window = matches!(file.input, InputKey::Source(_));
    let p = plan.inputs.iter().find(|p| p.input == file.input);
    let fps = p.map_or(30.0, |p| p.fps);
    let wait = if window {
        FIRST_FRAME_AFTER
    } else {
        Duration::ZERO
    };
    file.waiting_for_frame = since < wait;
    let live = since.saturating_sub(wait).as_secs_f64();
    let secs = since.as_secs_f64().max(0.001);
    // Native windows send frames on damage: about a third of the cap.
    let rate = if p.is_some_and(|p| p.native) {
        fps / 3.0
    } else {
        fps
    };
    let fresh = (live * rate) as u64;
    let padded = if window {
        (wait.as_secs_f64() * fps.min(30.0)) as u64
    } else {
        0
    };
    let duplicated = if window { 0 } else { fresh / 400 };
    let encoded = fresh + padded + duplicated;
    file.quality = Some(InputQuality {
        encoder: p.map(|p| p.encoder),
        nominal_fps: p.filter(|p| !p.native).map(|p| p.fps),
        max_fps: p.filter(|p| p.native).map(|p| p.fps),
        frames_in: fresh,
        dropped_corrupt: u64::from(!window) * 3,
        dropped_queue: 0,
        duplicated,
        padded,
        encoded,
        effective_fps: fresh as f64 / secs,
        repeat_pct: if encoded > 0 {
            duplicated as f64 * 100.0 / encoded as f64
        } else {
            0.0
        },
        bitrate_kbps: (file.bytes as f64 * 8.0 / secs / 1000.0) as u32,
        start_offset: Some(if window {
            wait
        } else {
            Duration::from_millis(280)
        }),
    });
}

#[cfg(test)]
mod tests {
    use super::super::engine::{EngineConfig, start};
    use super::*;

    fn config() -> EngineConfig {
        EngineConfig {
            output_root: PathBuf::from("/tmp/chinstrap-fake-test"),
            preferred_encoder: None,
            preferred_convert: None,
            enable_portal: true,
            fake: true,
            frame_rate: FrameRateMode::Native,
            test_sources: Vec::new(),
            preview: true,
            selftest_free_bytes: None,
            selftest_min_free_mb: None,
        }
    }

    fn wait_for(
        rx: &async_channel::Receiver<EngineEvent>,
        timeout: Duration,
        pred: impl Fn(&Snapshot) -> bool,
    ) -> Snapshot {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(EngineEvent::Snapshot(s)) if pred(&s) => return *s,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        panic!("timed out waiting for snapshot");
    }

    #[test]
    fn record_cut_stop_and_shutdown() {
        let (engine, rx) = start(config()).expect("start fake engine");
        let first = wait_for(&rx, Duration::from_secs(1), |_| true);
        assert_eq!(first.cameras.len(), 2);
        assert!(first.cameras.iter().any(|c| c.unsupported.is_some()));
        assert_eq!(first.microphones.len(), 2);
        assert_eq!(first.selected_microphone, Some(DeviceId(3)));
        assert!(first.can_record);

        engine.send(EngineCommand::AddTestSource);
        wait_for(&rx, Duration::from_secs(1), |s| {
            s.sources.len() == 1 && s.sources[0].state == SourceState::Checking && !s.can_record
        });
        wait_for(&rx, Duration::from_secs(2), |s| {
            s.sources[0].state == SourceState::Ready && s.can_record
        });

        engine.send(EngineCommand::ToggleRecord);
        wait_for(&rx, Duration::from_secs(2), |s| {
            matches!(s.phase, Phase::Recording { segment: 1, .. })
        });
        // Rejected while busy.
        engine.send(EngineCommand::AddTestSource);
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut warned = false;
        while !warned && Instant::now() < deadline {
            if let Ok(EngineEvent::Notice(n)) = rx.try_recv() {
                warned = n.severity == Severity::Warning;
            }
        }
        assert!(warned, "busy command must be rejected with a Warning");

        engine.send(EngineCommand::Cut);
        let s = wait_for(&rx, Duration::from_secs(2), |s| {
            matches!(s.phase, Phase::Recording { segment: 2, .. })
        });
        let session = s.session.unwrap();
        assert_eq!(session.segments[0].status, SegmentStatus::Complete);
        assert!(session.segments[1].gap_before.is_some());

        engine.send(EngineCommand::Stop);
        let s = wait_for(&rx, Duration::from_secs(2), |s| s.phase == Phase::Idle);
        assert_eq!(s.session.unwrap().segments.len(), 2);

        engine.send(EngineCommand::Record);
        wait_for(&rx, Duration::from_secs(2), |s| {
            matches!(s.phase, Phase::Recording { segment: 3, .. })
        });
        assert!(engine.shutdown_blocking(Duration::from_secs(2)));
        assert!(
            engine.shutdown_blocking(Duration::from_secs(2)),
            "idempotent"
        );
        let last = wait_for(&rx, Duration::from_secs(1), |s| s.phase == Phase::Idle);
        assert_eq!(
            last.session.unwrap().segments[2].status,
            SegmentStatus::Complete
        );
    }

    #[test]
    fn new_contract_commands() {
        let (engine, rx) = start(config()).expect("start fake engine");
        let first = wait_for(&rx, Duration::from_secs(1), |_| true);
        assert_eq!(first.frame_rate_default, FrameRateMode::Native);
        assert!(first.cameras[0].modes.iter().filter(|m| m.auto).count() == 1);
        assert_eq!(first.vocabulary.terms, vec!["Rawkode", "Kubernetes"]);
        engine.send(EngineCommand::SelectCamera(Some(DeviceId(1))));
        engine.send(EngineCommand::SelectCameraMode(Some(
            "3840×2160 @25 · NV12".into(),
        )));
        let s = wait_for(&rx, Duration::from_secs(1), |s| {
            s.selected_camera_mode.is_some()
        });
        assert_eq!(s.capacity.inputs.len(), 1);
        assert_eq!(s.capacity.inputs[0].width, 3840);
        engine.send(EngineCommand::AddPortalSources);
        let s = wait_for(&rx, Duration::from_secs(5), |s| {
            s.sources.len() == 2 && s.sources.iter().all(|x| x.state == SourceState::Ready)
        });
        // Two native 144 Hz windows plus a 4K camera: the plan fits by moving load around.
        assert_eq!(s.capacity.inputs.len(), 3);
        assert!(!s.capacity.over, "{:?}", s.capacity);
        engine.send(EngineCommand::SetFrameRateMode {
            source: None,
            mode: FrameRateMode::Cfr60,
        });
        let s = wait_for(&rx, Duration::from_secs(1), |s| {
            s.sources
                .iter()
                .all(|x| x.frame_rate == FrameRateMode::Cfr60)
        });
        assert_eq!(s.frame_rate_default, FrameRateMode::Cfr60);
        engine.send(EngineCommand::AddVocabularyTerm("Talos".into()));
        engine.send(EngineCommand::AddReplacement {
            from: "rock wood".into(),
            to: "Rawkode".into(),
        });
        engine.send(EngineCommand::RemoveVocabularyTerm("kubernetes".into()));
        let s = wait_for(&rx, Duration::from_secs(1), |s| {
            s.vocabulary.terms == vec!["Rawkode", "Talos"] && s.vocabulary.replacements.len() == 2
        });
        assert_eq!(s.vocabulary.replacements[1].to, "Rawkode");
        engine.send(EngineCommand::Record);
        let s = wait_for(&rx, Duration::from_secs(4), |s| {
            s.session
                .as_ref()
                .is_some_and(|x| x.segments[0].files.iter().any(|f| f.waiting_for_frame))
        });
        assert!(matches!(s.phase, Phase::Recording { .. }));
        let s = wait_for(&rx, Duration::from_secs(5), |s| {
            s.session.as_ref().is_some_and(|x| {
                x.segments[0]
                    .files
                    .iter()
                    .filter(|f| f.input != InputKey::Microphone)
                    .all(|f| {
                        f.quality.as_ref().is_some_and(|q| q.encoded > 0) && !f.waiting_for_frame
                    })
            })
        });
        assert!(s.session.is_some());
        assert!(engine.shutdown_blocking(Duration::from_secs(2)));
    }

    #[test]
    fn synthetic_previews_follow_selection_and_recording() {
        let (engine, rx) = start(config()).expect("start fake engine");
        let hub = engine.previews();
        engine.send(EngineCommand::SelectCamera(Some(DeviceId(1))));
        engine.send(EngineCommand::AddTestSource);
        let source = InputKey::Source(SourceId(1));
        let s = wait_for(&rx, Duration::from_secs(3), |s| {
            s.previews.contains(&(InputKey::Camera, PreviewState::Live))
                && s.previews.contains(&(source, PreviewState::Live))
        });
        assert_eq!(s.previews.len(), 2);
        let (g0, f) = hub.latest(InputKey::Camera).expect("camera frame");
        assert_eq!((f.width, f.height), (480, 270));
        assert_eq!(f.bgra.len(), 480 * 270 * 4);
        assert_eq!(f.origin, PreviewOrigin::Idle);
        std::thread::sleep(Duration::from_millis(500));
        let g1 = hub.generation(InputKey::Camera);
        assert!((4..=9).contains(&(g1 - g0)), "{} frames in 0.5 s", g1 - g0);

        engine.send(EngineCommand::Record);
        wait_for(&rx, Duration::from_secs(2), |s| {
            matches!(s.phase, Phase::Recording { segment: 1, .. })
        });
        std::thread::sleep(Duration::from_millis(200));
        let (_, f) = hub.latest(source).expect("source frame");
        assert_eq!(f.origin, PreviewOrigin::Recording { segment: 1 });
        // The red recording frame.
        assert_eq!(&f.bgra[..4], &[50, 40, 230, 255]);

        engine.send(EngineCommand::Stop);
        wait_for(&rx, Duration::from_secs(2), |s| s.phase == Phase::Idle);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(hub.latest(source).unwrap().1.origin, PreviewOrigin::Idle);

        engine.send(EngineCommand::RemoveSource(SourceId(1)));
        wait_for(&rx, Duration::from_secs(1), |s| s.previews.len() == 1);
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            hub.latest(source).is_none(),
            "a removed source keeps no frame"
        );
        assert!(engine.shutdown_blocking(Duration::from_secs(2)));
    }

    #[test]
    fn portal_pick_groups_and_closes() {
        let (engine, rx) = start(config()).expect("start fake engine");
        engine.send(EngineCommand::AddPortalSources);
        wait_for(&rx, Duration::from_secs(1), |s| {
            s.portal_busy && !s.can_record
        });
        let s = wait_for(&rx, Duration::from_secs(5), |s| {
            s.sources.len() == 2 && s.sources.iter().all(|x| x.state == SourceState::Ready)
        });
        assert_eq!(s.sources[0].capture, s.sources[1].capture);
        assert!(s.sources.iter().all(|x| x.kind == SourceKind::Window));
        engine.send(EngineCommand::Record);
        let s = wait_for(&rx, Duration::from_secs(13), |s| {
            s.sources
                .iter()
                .all(|x| matches!(x.state, SourceState::Lost { .. }))
        });
        // Mic keeps recording; both window files are Failed.
        assert!(matches!(s.phase, Phase::Recording { .. }));
        let files = &s.session.unwrap().segments[0].files;
        let failed = files
            .iter()
            .filter(|f| matches!(f.status, InputStatus::Failed { .. }))
            .count();
        assert_eq!(failed, 2);
        assert!(engine.shutdown_blocking(Duration::from_secs(2)));
    }
}
