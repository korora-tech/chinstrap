//! Fake engine (SPEC §11 step 1), used while the real backend is built and for UI work
//! (`CHINSTRAP_FAKE_ENGINE=1`). It touches no devices, portals or files: every path it
//! reports is synthetic. It follows the same command rules as the real engine (§3.1):
//! out-of-phase commands are rejected with a Warning notice, never queued.
//!
//! Transcription is simulated too (§12): the model starts Missing, `DownloadModel` "downloads"
//! it in about 4 s, and each finished segment is "transcribed" in about 2 s.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gstreamer as gst;

use super::engine::{EngineConfig, Msg};
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

pub(super) fn spawn(
    config: EngineConfig,
    rx: Receiver<Msg>,
    events: async_channel::Sender<EngineEvent>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("chinstrap-engine".into())
        .spawn(move || FakeEngine::new(config, events).run(rx))
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
}

impl FakeEngine {
    fn new(config: EngineConfig, events: async_channel::Sender<EngineEvent>) -> Self {
        let portal = if config.enable_portal {
            PortalAvailability::Available { version: 5 }
        } else {
            PortalAvailability::Unavailable {
                reason: "portal disabled".into(),
            }
        };
        let caps = Capabilities {
            h264_encoder: Some(config.preferred_encoder.unwrap_or(H264Encoder::X264)),
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
            },
            DeviceInfo {
                id: DeviceId(2),
                kind: DeviceKind::Camera,
                name: "Fake Loopback Dummy".into(),
                detail: "fake #81 · 3840×2160 @25 · DMA-BUF NV12".into(),
                unsupported: Some("no usable format: video/x-raw(memory:DMABuf)".into()),
            },
        ];
        let microphones = vec![
            DeviceInfo {
                id: DeviceId(3),
                kind: DeviceKind::Microphone,
                name: "Fake USB Microphone".into(),
                detail: "fake #61 · 48 kHz · 2 ch".into(),
                unsupported: None,
            },
            DeviceInfo {
                id: DeviceId(4),
                kind: DeviceKind::Microphone,
                name: "Fake Built-in Audio".into(),
                detail: "fake #62 · 44.1 kHz · 1 ch".into(),
                unsupported: None,
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

    fn snapshot(&self) {
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
            | EngineCommand::EndSession
                if busy =>
            {
                self.reject(&cmd, "not allowed while recording");
            }
            EngineCommand::SelectCamera(id) => match id {
                None => {
                    self.selected_camera = None;
                    self.snapshot();
                }
                Some(id) => match self.cameras.iter().find(|c| c.id == *id) {
                    Some(c) if c.unsupported.is_none() => {
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
            EngineCommand::EndSession => {
                if self.session.take().is_none() {
                    self.reject(&cmd, "no open session");
                } else {
                    self.snapshot();
                }
            }
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
            let dir = self.output_root.join(&name);
            self.session = Some(SessionSummary {
                manifest: dir.join("session.json"),
                name,
                dir,
                segments: Vec::new(),
            });
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
        }
    }

    /// Appends a new segment row for the frozen input set and returns its index.
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
                if let Some(seg) = self.segment_mut(segment) {
                    for file in &mut seg.files {
                        if file.status == InputStatus::Recording {
                            file.bytes += match file.input {
                                InputKey::Microphone => 24_000,
                                _ => 1_500_000,
                            };
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
