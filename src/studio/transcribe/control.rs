//! Engine-side transcription state (§12.3): the model state, the FIFO job queue and per-segment
//! transcript status. It lives on the `chinstrap-engine` thread inside the engine and is the
//! source of truth the snapshot is built from; the engine forwards commands, segment
//! completions, worker and model events, and its busy state into it.
//!
//! It writes ended sessions' manifests itself (§12.3 policy 10); for the current session it only
//! raises `take_manifest_dirty`, and the engine writes the manifest with `manifest_entry`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::driver::ExitClass;
use super::model::{self, DlError, DlEvent, DlOpts, Resolved};
use super::proto::{self, BackendReq, Job};
use super::worker::{self, CommandFactory, WorkerCmd, WorkerEvent, WorkerHandle, WorkerSink};
use crate::studio::session::{self, ManifestTranscript};
use crate::studio::types::*;

const PROGRESS_EVERY: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JobKey {
    pub session_dir: PathBuf,
    pub segment: u32,
}

/// What the engine knows about one current-session segment.
#[derive(Clone, Debug)]
pub struct SegView {
    pub index: u32,
    /// The mic file, when it is `InputStatus::Complete`.
    pub mic: Option<PathBuf>,
    pub recording: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModelEvent {
    State(ModelState),
    Progress {
        received: u64,
        total: u64,
    },
    Verifying,
    Error(String),
    /// Always the last event of a model thread.
    Exited,
}

pub type ModelSink = Arc<dyn Fn(ModelEvent) + Send + Sync>;
pub type UrlFor = Arc<dyn Fn(&model::ModelSpec) -> String + Send + Sync>;

pub struct TxConfig {
    pub models_dir: PathBuf,
    pub resolved: Resolved,
    pub resolve_warning: Option<String>,
    /// `CHINSTRAP_WHISPER_BACKEND=cpu`.
    pub force_cpu: bool,
    pub language: Option<String>,
    pub gpu_device: i32,
    pub make_command: CommandFactory,
    pub worker_sink: WorkerSink,
    pub model_sink: ModelSink,
    pub dl_opts: DlOpts,
    pub url_for: UrlFor,
}

impl TxConfig {
    pub fn from_env(worker_sink: WorkerSink, model_sink: ModelSink) -> Self {
        let (resolved, resolve_warning) = model::resolve_env();
        let env = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let force_cpu =
            env("CHINSTRAP_WHISPER_BACKEND").is_some_and(|v| v.eq_ignore_ascii_case("cpu"));
        let language = env("CHINSTRAP_WHISPER_LANG").filter(|l| l != "auto");
        let gpu_device = env("CHINSTRAP_WHISPER_GPU_DEVICE")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        Self {
            models_dir: model::models_dir(),
            resolved,
            resolve_warning,
            force_cpu,
            language,
            gpu_device,
            make_command: Arc::new(super::driver::worker_command),
            worker_sink,
            model_sink,
            dl_opts: DlOpts::default(),
            url_for: Arc::new(model::url_for),
        }
    }
}

struct Entry {
    media: PathBuf,
    status: TranscriptStatus,
    manifest: Option<ManifestTranscript>,
    /// Re-queued after a Vulkan fault: this job runs on CPU.
    force_cpu: bool,
    /// A re-transcription's earlier Done status and manifest entry: its files stay on disk
    /// until the new job writes over them, so a cancel or failure goes back to them.
    previous: Option<(TranscriptStatus, Option<ManifestTranscript>)>,
}

struct Run {
    key: JobKey,
    id: u64,
    backend: Option<WhisperBackend>,
    requested_cpu: bool,
    paused: bool,
}

pub struct Transcription {
    cfg: TxConfig,
    model: ModelState,
    /// State reported while Cancelling; applied on `Exited`.
    after_cancel: Option<ModelState>,
    model_thread_alive: bool,
    model_cancel: Option<Arc<AtomicBool>>,
    auto: bool,
    queue: VecDeque<JobKey>,
    running: Option<Run>,
    entries: HashMap<JobKey, Entry>,
    last_backend: Option<WhisperBackend>,
    /// The last job that asked for the GPU was given the CPU by whisper (no usable GPU).
    auto_reported_cpu: bool,
    vulkan_faults: u32,
    vulkan_broken: bool,
    busy: bool,
    current: Option<PathBuf>,
    worker: Option<WorkerHandle>,
    next_id: u64,
    dirty: bool,
    manifest_dirty: bool,
    progress_pending: bool,
    last_progress: Instant,
    notices: Vec<(Severity, String)>,
    shut_down: bool,
}

fn manifest(status: &str) -> ManifestTranscript {
    ManifestTranscript::status_only(status)
}

fn file_name(p: &Path) -> Option<String> {
    p.file_name().map(|n| n.to_string_lossy().to_string())
}

impl Transcription {
    pub fn new(cfg: TxConfig) -> Self {
        let mut notices = Vec::new();
        if let Some(w) = &cfg.resolve_warning {
            notices.push((Severity::Warning, w.clone()));
        }
        let mut t = Self {
            model: ModelState::Checking,
            after_cancel: None,
            model_thread_alive: false,
            model_cancel: None,
            auto: true,
            queue: VecDeque::new(),
            running: None,
            entries: HashMap::new(),
            last_backend: None,
            auto_reported_cpu: false,
            vulkan_faults: 0,
            vulkan_broken: false,
            busy: false,
            current: None,
            worker: None,
            next_id: 1,
            dirty: true,
            manifest_dirty: false,
            progress_pending: false,
            last_progress: Instant::now(),
            notices,
            shut_down: false,
            cfg,
        };
        t.spawn_model_thread(false);
        t
    }

    // ---- read side ---------------------------------------------------------------------------

    pub fn info(&self) -> TranscriptionInfo {
        let others = |k: &JobKey| self.current.as_deref() != Some(k.session_dir.as_path());
        let queued_other = self.queue.iter().filter(|k| others(k)).count() as u32;
        let running_other = self.running.as_ref().filter(|r| others(&r.key)).map(|r| {
            match self.entries.get(&r.key).map(|e| &e.status) {
                Some(TranscriptStatus::Running { percent, .. }) => *percent,
                _ => 0,
            }
        });
        TranscriptionInfo {
            model: self.model.clone(),
            auto: self.auto,
            backend: self.last_backend,
            vulkan_broken: self.vulkan_broken,
            queued: self.queue.len() as u32,
            other_sessions: (queued_other > 0 || running_other.is_some()).then_some(
                OtherSessionJobs {
                    queued: queued_other,
                    running_percent: running_other,
                },
            ),
        }
    }

    /// Policy 4: CPU when forced by env, latched, or when the last GPU request ended up on the
    /// CPU. A job that runs on CPU because it was re-queued after a Vulkan fault does not count,
    /// so one fault only moves that job.
    fn effective_cpu(&self) -> bool {
        self.cfg.force_cpu || self.vulkan_broken || self.auto_reported_cpu
    }

    pub fn status(&self, key: &JobKey) -> TranscriptStatus {
        match self.entries.get(key) {
            None => TranscriptStatus::None,
            Some(e) => match &e.status {
                TranscriptStatus::Queued { .. } => TranscriptStatus::Queued {
                    deferred: self.busy && (self.effective_cpu() || e.force_cpu),
                },
                s => s.clone(),
            },
        }
    }

    pub fn manifest_entry(&self, key: &JobKey) -> Option<ManifestTranscript> {
        self.entries.get(key).and_then(|e| e.manifest.clone())
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn take_manifest_dirty(&mut self) -> bool {
        std::mem::take(&mut self.manifest_dirty)
    }

    pub fn take_notices(&mut self) -> Vec<(Severity, String)> {
        std::mem::take(&mut self.notices)
    }

    pub fn set_session(&mut self, dir: Option<PathBuf>) {
        if self.current != dir {
            let old = std::mem::replace(&mut self.current, dir);
            // Entries changed since the engine last wrote the old session's manifest can no
            // longer go through its `write_manifest`: patch them into the file directly.
            if let Some(old) = old
                && std::mem::take(&mut self.manifest_dirty)
            {
                let mut pending: Vec<(JobKey, ManifestTranscript)> = self
                    .entries
                    .iter()
                    .filter(|(k, _)| k.session_dir == old)
                    .filter_map(|(k, e)| e.manifest.clone().map(|m| (k.clone(), m)))
                    .collect();
                pending.sort_by_key(|(k, _)| k.segment);
                for (k, mt) in pending {
                    self.set_manifest(&k, mt);
                }
            }
            self.dirty = true;
        }
    }

    // ---- manifest ----------------------------------------------------------------------------

    fn set_manifest(&mut self, key: &JobKey, mt: ManifestTranscript) {
        if let Some(e) = self.entries.get_mut(key) {
            e.manifest = Some(mt.clone());
        }
        if self.current.as_deref() == Some(key.session_dir.as_path()) {
            self.manifest_dirty = true;
        } else if let Err(e) = patch_ended_manifest(key, &mt, |seg| {
            let k = JobKey {
                session_dir: key.session_dir.clone(),
                segment: seg,
            };
            self.queue.contains(&k) || self.running.as_ref().is_some_and(|r| r.key == k)
        }) {
            self.notices.push((
                Severity::Warning,
                format!(
                    "Cannot update {}: {e}",
                    key.session_dir.join(session::MANIFEST_NAME).display()
                ),
            ));
        }
    }

    // ---- queue -------------------------------------------------------------------------------

    fn enqueue(&mut self, key: JobKey, media: PathBuf) {
        let e = self.entries.entry(key.clone()).or_insert(Entry {
            media: media.clone(),
            status: TranscriptStatus::None,
            manifest: None,
            force_cpu: false,
            previous: None,
        });
        e.media = media;
        e.force_cpu = false;
        e.previous = matches!(e.status, TranscriptStatus::Done { .. })
            .then(|| (e.status.clone(), e.manifest.clone()));
        e.status = TranscriptStatus::Queued { deferred: false };
        self.queue.push_back(key.clone());
        self.set_manifest(&key, manifest("queued"));
        self.dirty = true;
    }

    fn is_idle_status(&self, key: &JobKey) -> bool {
        !matches!(
            self.entries.get(key).map(|e| &e.status),
            Some(TranscriptStatus::Queued { .. } | TranscriptStatus::Running { .. })
        )
    }

    /// Policy 1: called by the engine when a segment completes.
    pub fn segment_completed(&mut self, key: JobKey, mic: Option<PathBuf>) {
        let Some(mic) = mic else {
            return;
        };
        if self.auto
            && self.model.usable()
            && !self.shut_down
            && matches!(self.status(&key), TranscriptStatus::None)
        {
            self.enqueue(key, mic);
        }
    }

    /// Policy 2: every current-session segment with no transcript.
    fn backfill(&mut self, segs: &[SegView]) {
        if !self.auto || !self.model.usable() || self.shut_down {
            return;
        }
        let Some(cur) = self.current.clone() else {
            return;
        };
        for s in segs {
            let (Some(mic), false) = (&s.mic, s.recording) else {
                continue;
            };
            let key = JobKey {
                session_dir: cur.clone(),
                segment: s.index,
            };
            if matches!(self.status(&key), TranscriptStatus::None) {
                self.enqueue(key, mic.clone());
            }
        }
    }

    /// Puts a re-transcription's segment back to its earlier transcript. False: there was none.
    fn restore_previous(&mut self, key: &JobKey) -> bool {
        let Some((status, mt)) = self.entries.get_mut(key).and_then(|e| e.previous.take()) else {
            return false;
        };
        if let Some(e) = self.entries.get_mut(key) {
            e.status = status;
        }
        if let Some(mt) = mt {
            self.set_manifest(key, mt);
        }
        self.dirty = true;
        true
    }

    fn cancel_key(&mut self, key: &JobKey) -> bool {
        if let Some(pos) = self.queue.iter().position(|k| k == key) {
            self.queue.remove(pos);
        } else if let Some(r) = self.running.as_ref().filter(|r| &r.key == key) {
            if let Some(w) = &self.worker {
                w.send(WorkerCmd::Cancel { id: r.id });
            }
        } else {
            return false;
        }
        if !self.restore_previous(key) {
            if let Some(e) = self.entries.get_mut(key) {
                e.status = TranscriptStatus::None;
            }
            self.set_manifest(key, manifest("cancelled"));
        }
        self.dirty = true;
        true
    }

    fn job_for(&self, key: &JobKey, backend: BackendReq) -> Option<Job> {
        let media = self.entries.get(key)?.media.clone();
        let (path, id) = match &self.model {
            ModelState::Ready { id, path, .. } | ModelState::External { id, path } => {
                (path.clone(), id.clone())
            }
            _ => return None,
        };
        let sha = match &self.cfg.resolved {
            Resolved::Catalog(spec) => Some(spec.sha256.to_string()),
            Resolved::External { .. } => None,
        };
        Some(Job {
            prompt_hint: super::prompt::session_hint_for_media(&media),
            media,
            model: path,
            model_id: id,
            model_sha256: sha,
            vad_model: model::vad_path(&self.cfg.models_dir),
            language: self.cfg.language.clone(),
            backend: if backend == BackendReq::Cpu || self.effective_cpu() {
                BackendReq::Cpu
            } else {
                BackendReq::Auto
            },
            gpu_device: self.cfg.gpu_device,
        })
    }

    /// Policy 4.
    fn start_next(&mut self) {
        if self.running.is_some() || self.shut_down || !self.model.usable() {
            return;
        }
        let Some(key) = self.queue.front().cloned() else {
            return;
        };
        let force_cpu = self.entries.get(&key).is_some_and(|e| e.force_cpu);
        let cpu = force_cpu || self.effective_cpu();
        if cpu && self.busy {
            return;
        }
        self.queue.pop_front();
        let requested_backend = if force_cpu {
            BackendReq::Cpu
        } else {
            BackendReq::Auto
        };
        let Some(job) = self.job_for(&key, requested_backend) else {
            return;
        };
        if self.worker.is_none() {
            match worker::spawn(self.cfg.worker_sink.clone(), self.cfg.make_command.clone()) {
                Ok(w) => self.worker = Some(w),
                Err(e) => {
                    self.fail(&key, format!("cannot start the transcription thread: {e}"));
                    return;
                }
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        let requested_cpu = job.backend == BackendReq::Cpu;
        if let Some(w) = &self.worker {
            w.send(WorkerCmd::Start { id, job });
        }
        self.running = Some(Run {
            key: key.clone(),
            id,
            backend: requested_cpu.then_some(WhisperBackend::Cpu),
            requested_cpu,
            paused: false,
        });
        if let Some(e) = self.entries.get_mut(&key) {
            e.status = TranscriptStatus::Running {
                stage: TranscribeStage::Starting,
                percent: 0,
                paused: false,
            };
        }
        self.set_manifest(&key, manifest("running"));
        self.dirty = true;
    }

    fn fail(&mut self, key: &JobKey, reason: String) {
        if let Some(e) = self.entries.get_mut(key) {
            e.status = TranscriptStatus::Failed {
                reason: reason.clone(),
            };
        }
        let mut mt = manifest("failed");
        mt.error = Some(reason);
        self.set_manifest(key, mt);
        self.dirty = true;
    }

    // ---- inputs from the engine --------------------------------------------------------------

    /// The six transcription commands. `Err` is the rejection reason.
    pub fn command(&mut self, cmd: &EngineCommand, segs: &[SegView]) -> Result<(), String> {
        match cmd {
            EngineCommand::DownloadModel => {
                if matches!(self.cfg.resolved, Resolved::External { .. }) {
                    return Err("a custom model is configured (CHINSTRAP_WHISPER_MODEL)".into());
                }
                match &self.model {
                    _ if self.model_thread_alive => Err(match self.model {
                        ModelState::Cancelling => "the cancelled download has not stopped yet",
                        ModelState::Checking => "the model is still being checked",
                        _ => "a download is already running",
                    }
                    .into()),
                    ModelState::Missing { download_bytes }
                    | ModelState::Failed { download_bytes, .. } => {
                        let total = *download_bytes;
                        self.model = ModelState::Downloading { received: 0, total };
                        self.spawn_model_thread(true);
                        self.dirty = true;
                        Ok(())
                    }
                    ModelState::Ready {
                        vad_download: Some(b),
                        ..
                    } => {
                        let total = *b;
                        self.model = ModelState::Downloading { received: 0, total };
                        self.spawn_model_thread(true);
                        self.dirty = true;
                        Ok(())
                    }
                    _ => Err("nothing to download".into()),
                }
            }
            EngineCommand::CancelModelDownload => match self.model {
                ModelState::Downloading { .. } | ModelState::Verifying => {
                    if let Some(c) = &self.model_cancel {
                        c.store(true, Ordering::SeqCst);
                    }
                    self.model = ModelState::Cancelling;
                    self.dirty = true;
                    Ok(())
                }
                _ => Err("no download is running".into()),
            },
            EngineCommand::SetAutoTranscribe(on) => {
                self.auto = *on;
                self.dirty = true;
                if *on {
                    self.backfill(segs);
                }
                Ok(())
            }
            EngineCommand::Transcribe { segment } => {
                let cur = self.current.clone().ok_or("no open session")?;
                let seg = segs
                    .iter()
                    .find(|s| s.index == *segment)
                    .ok_or("unknown segment")?;
                if seg.recording {
                    return Err("the segment is still recording".into());
                }
                let mic = seg
                    .mic
                    .clone()
                    .ok_or("the segment has no complete microphone file")?;
                if !self.model.usable() {
                    return Err("the Whisper model is not ready".into());
                }
                let key = JobKey {
                    session_dir: cur,
                    segment: *segment,
                };
                if !self.is_idle_status(&key) {
                    return Err("already queued or transcribing".into());
                }
                self.enqueue(key, mic);
                Ok(())
            }
            EngineCommand::CancelTranscription { segment } => {
                let cur = self.current.clone().ok_or("no open session")?;
                let key = JobKey {
                    session_dir: cur,
                    segment: *segment,
                };
                if self.cancel_key(&key) {
                    Ok(())
                } else {
                    Err("not queued or transcribing".into())
                }
            }
            EngineCommand::CancelOtherSessionTranscriptions => {
                let cur = self.current.clone();
                let keys: Vec<JobKey> = self
                    .queue
                    .iter()
                    .cloned()
                    .chain(self.running.as_ref().map(|r| r.key.clone()))
                    .filter(|k| cur.as_deref() != Some(k.session_dir.as_path()))
                    .collect();
                if keys.is_empty() {
                    return Err("no jobs from earlier sessions".into());
                }
                for k in keys {
                    self.cancel_key(&k);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn on_model(&mut self, ev: ModelEvent, segs: &[SegView]) {
        let was_usable = self.model.usable();
        match ev {
            ModelEvent::State(s) => {
                if self.model == ModelState::Cancelling {
                    self.after_cancel = Some(s);
                } else {
                    self.model = s;
                }
            }
            ModelEvent::Progress { received, total } => {
                if matches!(
                    self.model,
                    ModelState::Downloading { .. } | ModelState::Verifying
                ) {
                    self.model = ModelState::Downloading { received, total };
                }
            }
            ModelEvent::Verifying => {
                if matches!(self.model, ModelState::Downloading { .. }) {
                    self.model = ModelState::Verifying;
                }
            }
            ModelEvent::Error(reason) => {
                let download_bytes =
                    match model::state_quick(&self.cfg.resolved, &self.cfg.models_dir) {
                        ModelState::Missing { download_bytes } => download_bytes,
                        ModelState::Ready {
                            vad_download: Some(b),
                            ..
                        } => b,
                        _ => 0,
                    };
                self.notices
                    .push((Severity::Error, format!("Model download failed: {reason}")));
                self.model = ModelState::Failed {
                    reason,
                    download_bytes,
                };
                self.after_cancel = None;
            }
            ModelEvent::Exited => {
                self.model_thread_alive = false;
                self.model_cancel = None;
                if matches!(
                    self.model,
                    ModelState::Cancelling
                        | ModelState::Downloading { .. }
                        | ModelState::Verifying
                        | ModelState::Checking
                ) {
                    self.model = self.after_cancel.take().unwrap_or_else(|| {
                        model::state_quick(&self.cfg.resolved, &self.cfg.models_dir)
                    });
                }
                self.after_cancel = None;
            }
        }
        self.dirty = true;
        if !was_usable && self.model.usable() {
            self.backfill(segs);
        }
    }

    pub fn on_worker(&mut self, ev: WorkerEvent) {
        let id = match &ev {
            WorkerEvent::Backend { id, .. }
            | WorkerEvent::Progress { id, .. }
            | WorkerEvent::Done { id, .. }
            | WorkerEvent::Failed { id, .. } => *id,
        };
        let Some(run) = self.running.as_mut().filter(|r| r.id == id) else {
            return;
        };
        let key = run.key.clone();
        match ev {
            WorkerEvent::Backend { backend, .. } => {
                run.backend = Some(backend);
                self.last_backend = Some(backend);
                if !run.requested_cpu {
                    self.auto_reported_cpu = backend == WhisperBackend::Cpu;
                }
                if backend == WhisperBackend::Cpu && self.busy && !run.paused {
                    run.paused = true;
                    if let Some(w) = &self.worker {
                        w.send(WorkerCmd::Pause);
                    }
                    self.set_paused(&key, true);
                }
                self.dirty = true;
            }
            WorkerEvent::Progress { stage, percent, .. } => {
                let paused = run.paused;
                if let Some(e) = self.entries.get_mut(&key)
                    && let TranscriptStatus::Running {
                        stage: s,
                        percent: p,
                        ..
                    } = &e.status
                    && (*s != stage || *p != percent)
                {
                    e.status = TranscriptStatus::Running {
                        stage,
                        percent,
                        paused,
                    };
                    self.progress_pending = true;
                }
            }
            WorkerEvent::Done {
                json,
                srt,
                vtt,
                language,
                words,
                backend,
                ..
            } => {
                self.running = None;
                let model_id = match &self.model {
                    ModelState::Ready { id, .. } | ModelState::External { id, .. } => {
                        Some(id.clone())
                    }
                    _ => None,
                };
                let mt = ManifestTranscript {
                    status: "done".into(),
                    json: file_name(&json),
                    srt: file_name(&srt),
                    vtt: file_name(&vtt),
                    model: model_id,
                    backend: Some(proto::backend_name(backend).into()),
                    language: Some(language.clone()),
                    words: Some(words),
                    error: None,
                    updated: session::wall_now(),
                };
                let mut retried_after_fault = false;
                if let Some(e) = self.entries.get_mut(&key) {
                    e.status = TranscriptStatus::Done {
                        json,
                        srt,
                        vtt,
                        language,
                        words,
                        backend,
                    };
                    retried_after_fault = std::mem::take(&mut e.force_cpu);
                    e.previous = None;
                }
                if retried_after_fault {
                    // The same job worked on CPU, so the crash really was Vulkan's.
                    self.count_vulkan_fault();
                }
                self.set_manifest(&key, mt);
                self.dirty = true;
            }
            WorkerEvent::Failed { reason, class, .. } => {
                self.running = None;
                match class {
                    ExitClass::Cancelled => {
                        // Status and manifest were set by the cancel or the shutdown.
                        if let Some(e) = self.entries.get_mut(&key)
                            && matches!(e.status, TranscriptStatus::Running { .. })
                        {
                            e.status = TranscriptStatus::None;
                        }
                    }
                    ExitClass::VulkanFault => {
                        // Counted toward the latch only once the CPU retry of this job
                        // succeeds: a crash that also happens on CPU (bad input, OOM, a
                        // panic in decode) is not Vulkan's fault.
                        let segment = key.segment;
                        self.notices.push((
                            Severity::Warning,
                            format!(
                                "Transcription of segment {segment} failed on Vulkan ({reason}); retrying on CPU"
                            ),
                        ));
                        if let Some(e) = self.entries.get_mut(&key) {
                            e.force_cpu = true;
                            e.status = TranscriptStatus::Queued { deferred: false };
                        }
                        self.queue.push_front(key.clone());
                        self.set_manifest(&key, manifest("queued"));
                    }
                    ExitClass::JobFailed | ExitClass::Ok => {
                        if let Some(e) = self.entries.get_mut(&key) {
                            // A CPU retry that fails too: the earlier crash is not counted.
                            e.force_cpu = false;
                        }
                        if self.restore_previous(&key) {
                            self.notices.push((
                                Severity::Error,
                                format!(
                                    "Re-transcription of segment {} failed: {reason}; the earlier transcript is kept",
                                    key.segment
                                ),
                            ));
                        } else {
                            self.notices.push((
                                Severity::Error,
                                format!(
                                    "Transcription of segment {} failed: {reason}",
                                    key.segment
                                ),
                            ));
                            self.fail(&key, reason);
                        }
                    }
                }
                self.dirty = true;
            }
        }
    }

    /// One confirmed Vulkan fault; the second latches CPU for the rest of the process.
    fn count_vulkan_fault(&mut self) {
        self.vulkan_faults += 1;
        if self.vulkan_faults >= 2 && !self.vulkan_broken {
            self.vulkan_broken = true;
            self.notices.push((
                Severity::Warning,
                "Vulkan failed twice, transcribing on CPU".into(),
            ));
        }
        self.dirty = true;
    }

    fn set_paused(&mut self, key: &JobKey, paused: bool) {
        if let Some(e) = self.entries.get_mut(key)
            && let TranscriptStatus::Running { stage, percent, .. } = e.status
        {
            e.status = TranscriptStatus::Running {
                stage,
                percent,
                paused,
            };
        }
        self.dirty = true;
    }

    /// Every engine tick: policies 4–6.
    pub fn tick(&mut self, busy: bool) {
        if busy != self.busy {
            self.busy = busy;
            self.dirty = true;
            // Policy 5: CPU jobs yield to recording.
            if let Some(r) = self.running.as_mut()
                && (r.requested_cpu || r.backend == Some(WhisperBackend::Cpu))
                && r.paused != busy
            {
                r.paused = busy;
                let key = r.key.clone();
                if let Some(w) = &self.worker {
                    w.send(if busy {
                        WorkerCmd::Pause
                    } else {
                        WorkerCmd::Resume
                    });
                }
                self.set_paused(&key, busy);
            }
        }
        self.start_next();
        if self.progress_pending && self.last_progress.elapsed() >= PROGRESS_EVERY {
            self.progress_pending = false;
            self.last_progress = Instant::now();
            self.dirty = true;
        }
    }

    /// Policy 9: never waits. Kills the child, cancels the model thread, marks running and
    /// queued jobs interrupted.
    pub fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        if let Some(c) = &self.model_cancel {
            c.store(true, Ordering::SeqCst);
        }
        if let Some(w) = self.worker.take() {
            w.send(WorkerCmd::Shutdown);
        }
        let keys: Vec<JobKey> = self
            .running
            .take()
            .map(|r| r.key)
            .into_iter()
            .chain(self.queue.drain(..))
            .collect();
        for k in keys {
            if self.restore_previous(&k) {
                continue;
            }
            if let Some(e) = self.entries.get_mut(&k) {
                e.status = TranscriptStatus::None;
            }
            self.set_manifest(&k, manifest("interrupted"));
        }
        self.dirty = true;
    }

    // ---- model thread ------------------------------------------------------------------------

    fn spawn_model_thread(&mut self, download: bool) {
        let resolved = self.cfg.resolved.clone();
        let dir = self.cfg.models_dir.clone();
        if !download && let Resolved::External { .. } = resolved {
            // No hash check and no download for a custom model.
            self.model = model::state_quick(&resolved, &dir);
            return;
        }
        let sink = self.cfg.model_sink.clone();
        let opts = self.cfg.dl_opts.clone();
        let url_for = self.cfg.url_for.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.model_cancel = Some(cancel.clone());
        self.model_thread_alive = true;
        let spawned = std::thread::Builder::new()
            .name("chinstrap-model".into())
            .spawn(move || {
                if download {
                    if let Resolved::Catalog(spec) = &resolved {
                        let files: Vec<model::DlFile> = model::needed_files(&dir, spec)
                            .into_iter()
                            .map(|mut f| {
                                f.url = url_for(&f.spec);
                                f
                            })
                            .collect();
                        let r = model::download(&dir, &files, &opts, &cancel, &mut |ev| {
                            sink(match ev {
                                DlEvent::Progress { received, total } => {
                                    ModelEvent::Progress { received, total }
                                }
                                DlEvent::Verifying => ModelEvent::Verifying,
                            })
                        });
                        match r {
                            Ok(()) | Err(DlError::Cancelled) => {
                                sink(ModelEvent::State(model::state_quick(&resolved, &dir)))
                            }
                            Err(DlError::Failed(reason)) => sink(ModelEvent::Error(reason)),
                        }
                    }
                } else {
                    sink(ModelEvent::State(model::state_checked(&resolved, &dir)));
                }
                sink(ModelEvent::Exited);
            });
        if let Err(e) = spawned {
            self.model_thread_alive = false;
            self.model = ModelState::Failed {
                reason: format!("cannot start the model thread: {e}"),
                download_bytes: 0,
            };
        }
    }
}

/// Patches `segments[index].inputs[key=="mic"].transcript` of an ended session's manifest. Any
/// other `queued`/`running` entry with no live job behind it (`live(segment)`) is rewritten as
/// `interrupted` (§12.3 policy 10).
fn patch_ended_manifest(
    key: &JobKey,
    mt: &ManifestTranscript,
    live: impl Fn(u32) -> bool,
) -> std::io::Result<()> {
    let path = key.session_dir.join(session::MANIFEST_NAME);
    let mut m = session::read_manifest(&path)?;
    for seg in &mut m.segments {
        let is_live = live(seg.index);
        for t in seg.inputs.iter_mut().filter_map(|i| i.transcript.as_mut()) {
            let eff = t.effective_status(is_live);
            if eff != t.status {
                t.status = eff.to_string();
                t.updated = session::wall_now();
            }
        }
    }
    let input = m
        .segments
        .iter_mut()
        .find(|s| s.index == key.segment)
        .and_then(|s| s.inputs.iter_mut().find(|i| i.key == "mic"))
        .ok_or_else(|| {
            std::io::Error::other(format!("segment {} has no mic input", key.segment))
        })?;
    input.transcript = Some(mt.clone());
    session::write_manifest(&key.session_dir, &m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::sync::mpsc;

    enum Ev {
        W(WorkerEvent),
        M(ModelEvent),
    }

    struct Rig {
        t: Transcription,
        rx: mpsc::Receiver<Ev>,
        dir: PathBuf,
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "chinstrap-control-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A rig with an external (fake) model so jobs can start at once, and `script` as the
    /// worker: a shell snippet that reads the job line into `$job`.
    fn rig(tag: &str, script: &str, force_cpu: bool) -> Rig {
        let dir = scratch(tag);
        let model_file = dir.join("ggml-fake.bin");
        std::fs::write(&model_file, b"x").unwrap();
        rig_with(
            dir,
            Resolved::External {
                id: "fake".into(),
                path: model_file,
            },
            script,
            force_cpu,
            None,
        )
    }

    fn rig_with(
        dir: PathBuf,
        resolved: Resolved,
        script: &str,
        force_cpu: bool,
        url: Option<String>,
    ) -> Rig {
        let (tx, rx) = mpsc::channel();
        let (tx1, tx2) = (Mutex::new(tx.clone()), Mutex::new(tx));
        let script = format!("read -r job; {script}");
        let cfg = TxConfig {
            models_dir: dir.join("models"),
            resolved,
            resolve_warning: None,
            force_cpu,
            language: None,
            gpu_device: 0,
            make_command: Arc::new(move || {
                let mut c = std::process::Command::new("sh");
                c.arg("-c").arg(&script);
                Ok(c)
            }),
            worker_sink: Arc::new(move |e| {
                let _ = tx1.lock().unwrap().send(Ev::W(e));
            }),
            model_sink: Arc::new(move |e| {
                let _ = tx2.lock().unwrap().send(Ev::M(e));
            }),
            dl_opts: DlOpts {
                backoff: [Duration::from_millis(10); 4],
                progress_every: Duration::ZERO,
                ..DlOpts::default()
            },
            url_for: Arc::new(move |_| url.clone().unwrap_or_default()),
        };
        Rig {
            t: Transcription::new(cfg),
            rx,
            dir,
        }
    }

    use std::sync::Mutex;

    impl Rig {
        fn session(&mut self) -> PathBuf {
            let s = self.dir.join("sess");
            std::fs::create_dir_all(s.join("segment-001")).unwrap();
            self.t.set_session(Some(s.clone()));
            s
        }

        fn key(&self, seg: u32) -> JobKey {
            JobKey {
                session_dir: self.dir.join("sess"),
                segment: seg,
            }
        }

        fn segs(&self, n: u32) -> Vec<SegView> {
            (1..=n)
                .map(|i| SegView {
                    index: i,
                    mic: Some(
                        self.dir
                            .join("sess")
                            .join(format!("segment-{i:03}/mic.mkv")),
                    ),
                    recording: false,
                })
                .collect()
        }

        /// Pumps events (and ticks) until `pred` holds.
        fn pump_until(
            &mut self,
            busy: bool,
            timeout: Duration,
            pred: impl Fn(&Transcription) -> bool,
        ) {
            let deadline = Instant::now() + timeout;
            let segs = self.segs(3);
            loop {
                self.t.tick(busy);
                if pred(&self.t) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out; model {:?}",
                    self.t.model
                );
                match self.rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(Ev::W(e)) => self.t.on_worker(e),
                    Ok(Ev::M(e)) => self.t.on_model(e, &segs),
                    Err(_) => {}
                }
            }
        }
    }

    const DONE_VK: &str = r#"echo '{"type":"backend","backend":"vulkan","device":"GPU"}'; echo '{"type":"progress","stage":"transcribing","percent":50}'; echo '{"type":"done","json":"/s/transcript.json","srt":"/s/mic.srt","vtt":"/s/mic.vtt","language":"en","words":7,"audio_s":3.0,"rtf":0.03}'"#;

    #[test]
    fn cancelled_retranscription_keeps_the_earlier_transcript() {
        let mut r = rig("redo", DONE_VK, false);
        r.session();
        let key = r.key(1);
        r.t.command(&EngineCommand::Transcribe { segment: 1 }, &r.segs(1))
            .unwrap();
        r.pump_until(false, Duration::from_secs(5), |t| {
            matches!(t.status(&key), TranscriptStatus::Done { .. })
        });
        let done = r.t.status(&key);
        // Re-transcribe, then cancel before it runs: back to the earlier Done (files kept).
        r.t.command(&EngineCommand::Transcribe { segment: 1 }, &r.segs(1))
            .unwrap();
        assert_eq!(
            r.t.status(&key),
            TranscriptStatus::Queued { deferred: false }
        );
        r.t.command(
            &EngineCommand::CancelTranscription { segment: 1 },
            &r.segs(1),
        )
        .unwrap();
        assert_eq!(r.t.status(&key), done);
        assert_eq!(r.t.manifest_entry(&key).unwrap().status, "done");
    }

    #[test]
    fn job_for_uses_named_backend_request() {
        let mut r = rig("backend_request", "exit 0", false);
        r.pump_until(false, Duration::from_secs(5), |t| {
            matches!(t.model, ModelState::External { .. })
        });
        let key = r.key(1);
        r.t.enqueue(key.clone(), r.dir.join("sess/segment-001/mic.mkv"));

        let automatic = r.t.job_for(&key, BackendReq::Auto).unwrap();
        let cpu = r.t.job_for(&key, BackendReq::Cpu).unwrap();
        assert_eq!(automatic.backend, BackendReq::Auto);
        assert_eq!(cpu.backend, BackendReq::Cpu);
        r.t.shutdown();
    }

    #[test]
    fn auto_enqueue_runs_to_done() {
        let mut r = rig("done", DONE_VK, false);
        assert!(matches!(r.t.info().model, ModelState::External { .. }));
        r.session();
        let key = r.key(1);
        r.t.segment_completed(key.clone(), Some(r.dir.join("sess/segment-001/mic.mkv")));
        assert_eq!(
            r.t.status(&key),
            TranscriptStatus::Queued { deferred: false }
        );
        assert!(r.t.take_manifest_dirty());
        assert_eq!(r.t.manifest_entry(&key).unwrap().status, "queued");
        r.pump_until(false, Duration::from_secs(5), |t| {
            matches!(t.status(&key), TranscriptStatus::Done { .. })
        });
        match r.t.status(&key) {
            TranscriptStatus::Done {
                words,
                backend,
                language,
                ..
            } => {
                assert_eq!(
                    (words, backend, language.as_str()),
                    (7, WhisperBackend::Vulkan, "en")
                )
            }
            s => panic!("{s:?}"),
        }
        let m = r.t.manifest_entry(&key).unwrap();
        assert_eq!(m.status, "done");
        assert_eq!(m.srt.as_deref(), Some("mic.srt"));
        assert_eq!(r.t.info().backend, Some(WhisperBackend::Vulkan));
        // Auto off: a completed segment is not queued.
        r.t.command(&EngineCommand::SetAutoTranscribe(false), &[])
            .unwrap();
        r.t.segment_completed(r.key(2), Some("/x/mic.mkv".into()));
        assert_eq!(r.t.status(&r.key(2)), TranscriptStatus::None);
        // Manual re-run of a Done segment is accepted.
        let segs = r.segs(2);
        r.t.command(&EngineCommand::Transcribe { segment: 1 }, &segs)
            .unwrap();
        assert!(
            r.t.command(&EngineCommand::Transcribe { segment: 1 }, &segs)
                .is_err()
        );
        // Auto on again backfills segment 2.
        r.t.command(&EngineCommand::SetAutoTranscribe(true), &segs)
            .unwrap();
        assert!(matches!(
            r.t.status(&r.key(2)),
            TranscriptStatus::Queued { .. }
        ));
        r.t.shutdown();
    }

    #[test]
    fn vulkan_faults_retry_on_cpu_then_latch() {
        // Vulkan requests crash right after the backend line; CPU requests succeed.
        let script = r#"case "$job" in *'"backend":"cpu"'*) echo '{"type":"backend","backend":"cpu","device":"CPU"}'; echo '{"type":"done","json":"/j","srt":"/s","vtt":"/v","language":"en","words":1,"audio_s":1.0,"rtf":1.0}';; *) echo '{"type":"backend","backend":"vulkan","device":"GPU"}'; kill -KILL $$;; esac"#;
        let mut r = rig("vkfault", script, false);
        r.session();
        let segs = r.segs(2);
        r.t.command(&EngineCommand::Transcribe { segment: 1 }, &segs)
            .unwrap();
        r.pump_until(false, Duration::from_secs(5), |t| {
            matches!(
                t.status(&JobKey {
                    session_dir: t.current.clone().unwrap(),
                    segment: 1
                }),
                TranscriptStatus::Done {
                    backend: WhisperBackend::Cpu,
                    ..
                }
            )
        });
        assert!(
            !r.t.info().vulkan_broken,
            "one fault only marks that job CPU"
        );
        assert_eq!(r.t.info().backend, Some(WhisperBackend::Cpu));
        // The next job asks for Vulkan again, faults again, and that latches.
        r.t.command(&EngineCommand::Transcribe { segment: 2 }, &segs)
            .unwrap();
        r.pump_until(false, Duration::from_secs(5), |t| {
            matches!(
                t.status(&JobKey {
                    session_dir: t.current.clone().unwrap(),
                    segment: 2
                }),
                TranscriptStatus::Done { .. }
            )
        });
        assert!(r.t.info().vulkan_broken, "second fault latches");
        let notices = r.t.take_notices();
        assert!(
            notices
                .iter()
                .any(|(_, m)| m == "Vulkan failed twice, transcribing on CPU")
        );
        r.t.shutdown();
    }

    #[test]
    fn crash_that_also_happens_on_cpu_never_latches() {
        // Every request crashes before transcribing; the CPU retry fails as well.
        let script = r#"case "$job" in *'"backend":"cpu"'*) echo '{"type":"backend","backend":"cpu","device":"CPU"}'; kill -KILL $$;; *) echo '{"type":"backend","backend":"vulkan","device":"GPU"}'; kill -KILL $$;; esac"#;
        let mut r = rig("vkfalse", script, false);
        r.session();
        let segs = r.segs(3);
        for seg in 1..=3 {
            r.t.command(&EngineCommand::Transcribe { segment: seg }, &segs)
                .unwrap();
            r.pump_until(false, Duration::from_secs(5), |t| {
                matches!(
                    t.status(&JobKey {
                        session_dir: t.current.clone().unwrap(),
                        segment: seg
                    }),
                    TranscriptStatus::Failed { .. }
                )
            });
        }
        assert!(
            !r.t.info().vulkan_broken,
            "faults whose CPU retry also failed are not Vulkan's"
        );
        assert_eq!(r.t.vulkan_faults, 0);
        r.t.shutdown();
    }

    #[test]
    fn ending_the_session_keeps_a_pending_manifest_update() {
        let mut r = rig("endrace", DONE_VK, false);
        let sess = r.session();
        let mut m = session::Manifest::new("sess", session::wall_now());
        m.segments.push(session::ManifestSegment {
            index: 1,
            dir: session::segment_dir_name(1),
            status: "complete".into(),
            reason: None,
            base_time_ns: None,
            stop_clock_ns: None,
            duration_ns: None,
            gap_before_ns: None,
            started_wall: None,
            inputs: vec![session::ManifestInput {
                key: "mic".into(),
                kind: "microphone".into(),
                label: "Microphone".into(),
                file: "mic.mkv".into(),
                codec: "opus".into(),
                encoder: None,
                convert: None,
                capture: None,
                width: None,
                height: None,
                framerate: None,
                max_framerate: None,
                first_pts_ns: None,
                last_pts_ns: None,
                status: "complete".into(),
                error: None,
                device: None,
                transcript: Some(ManifestTranscript::status_only("running")),
                quality: None,
            }],
        });
        session::write_manifest(&sess, &m).unwrap();
        let key = r.key(1);
        r.t.command(&EngineCommand::Transcribe { segment: 1 }, &r.segs(1))
            .unwrap();
        // Done arrives, and the session ends before the engine flushes its manifest.
        r.pump_until(false, Duration::from_secs(5), |t| {
            matches!(t.status(&key), TranscriptStatus::Done { .. })
        });
        r.t.set_session(None);
        assert!(!r.t.take_manifest_dirty(), "handled by set_session");
        let back = session::read_manifest(&sess.join(session::MANIFEST_NAME)).unwrap();
        let t = back.segments[0].inputs[0].transcript.as_ref().unwrap();
        assert_eq!(t.status, "done");
        assert_eq!(t.srt.as_deref(), Some("mic.srt"));
        r.t.shutdown();
    }

    #[test]
    fn cpu_jobs_wait_for_recording_and_pause() {
        let slow_cpu = r#"echo '{"type":"backend","backend":"cpu","device":"CPU"}'; sleep 0.6; echo '{"type":"done","json":"/j","srt":"/s","vtt":"/v","language":"en","words":1,"audio_s":1.0,"rtf":1.0}'"#;
        let mut r = rig("cpu", slow_cpu, true);
        r.session();
        let key = r.key(1);
        r.t.tick(true);
        r.t.command(&EngineCommand::Transcribe { segment: 1 }, &r.segs(1))
            .unwrap();
        r.t.tick(true);
        assert_eq!(
            r.t.status(&key),
            TranscriptStatus::Queued { deferred: true }
        );
        r.t.tick(false);
        assert!(matches!(
            r.t.status(&key),
            TranscriptStatus::Running { paused: false, .. }
        ));
        // Recording starts: the CPU child is stopped.
        r.t.tick(true);
        assert!(matches!(
            r.t.status(&key),
            TranscriptStatus::Running { paused: true, .. }
        ));
        std::thread::sleep(Duration::from_millis(900));
        assert!(
            matches!(r.t.status(&key), TranscriptStatus::Running { .. }),
            "still stopped"
        );
        r.pump_until(false, Duration::from_secs(5), |t| {
            matches!(
                t.status(&JobKey {
                    session_dir: t.current.clone().unwrap(),
                    segment: 1
                }),
                TranscriptStatus::Done { .. }
            )
        });
        r.t.shutdown();
    }

    #[test]
    fn cancel_queued_and_running() {
        let hang = r#"echo '{"type":"backend","backend":"vulkan","device":"GPU"}'; sleep 30"#;
        let mut r = rig("cancel", hang, false);
        r.session();
        let segs = r.segs(2);
        r.t.command(&EngineCommand::Transcribe { segment: 1 }, &segs)
            .unwrap();
        r.t.command(&EngineCommand::Transcribe { segment: 2 }, &segs)
            .unwrap();
        r.t.tick(false);
        assert!(matches!(
            r.t.status(&r.key(1)),
            TranscriptStatus::Running { .. }
        ));
        r.t.command(&EngineCommand::CancelTranscription { segment: 2 }, &segs)
            .unwrap();
        assert_eq!(r.t.status(&r.key(2)), TranscriptStatus::None);
        assert_eq!(r.t.manifest_entry(&r.key(2)).unwrap().status, "cancelled");
        r.t.command(&EngineCommand::CancelTranscription { segment: 1 }, &segs)
            .unwrap();
        assert_eq!(r.t.status(&r.key(1)), TranscriptStatus::None);
        r.pump_until(false, Duration::from_secs(5), |t| t.running.is_none());
        assert!(!r.t.info().vulkan_broken, "a cancel never latches");
        assert!(
            r.t.command(&EngineCommand::CancelTranscription { segment: 1 }, &segs)
                .is_err()
        );
        r.t.shutdown();
    }

    #[test]
    fn ended_session_manifest_is_patched_and_shutdown_interrupts() {
        let hang = r#"echo '{"type":"backend","backend":"vulkan","device":"GPU"}'; sleep 30"#;
        let mut r = rig("ended", hang, false);
        let sess = r.session();
        let mut m = session::Manifest::new("sess", session::wall_now());
        for i in 1..=2 {
            m.segments.push(session::ManifestSegment {
                index: i,
                dir: session::segment_dir_name(i),
                status: "complete".into(),
                reason: None,
                base_time_ns: None,
                stop_clock_ns: None,
                duration_ns: None,
                gap_before_ns: None,
                started_wall: None,
                inputs: vec![session::ManifestInput {
                    key: "mic".into(),
                    kind: "microphone".into(),
                    label: "Microphone".into(),
                    file: "mic.mkv".into(),
                    codec: "opus".into(),
                    encoder: None,
                    convert: None,
                    capture: None,
                    width: None,
                    height: None,
                    framerate: None,
                    max_framerate: None,
                    first_pts_ns: None,
                    last_pts_ns: None,
                    status: "complete".into(),
                    error: None,
                    device: None,
                    transcript: None,
                    quality: None,
                }],
            });
        }
        session::write_manifest(&sess, &m).unwrap();
        let segs = r.segs(2);
        r.t.command(&EngineCommand::Transcribe { segment: 1 }, &segs)
            .unwrap();
        r.t.command(&EngineCommand::Transcribe { segment: 2 }, &segs)
            .unwrap();
        r.t.tick(false);
        // The session ends with a job running and one queued.
        r.t.set_session(None);
        let info = r.t.info();
        assert_eq!(
            info.other_sessions,
            Some(OtherSessionJobs {
                queued: 1,
                running_percent: Some(0)
            })
        );
        r.t.command(&EngineCommand::CancelOtherSessionTranscriptions, &[])
            .unwrap();
        let back = session::read_manifest(&sess.join(session::MANIFEST_NAME)).unwrap();
        for s in &back.segments {
            assert_eq!(s.inputs[0].transcript.as_ref().unwrap().status, "cancelled");
        }
        r.pump_until(false, Duration::from_secs(5), |t| t.running.is_none());
        assert_eq!(r.t.info().other_sessions, None);
        // Shutdown with a queued job in an ended session: interrupted on disk.
        r.t.set_session(Some(sess.clone()));
        r.t.command(&EngineCommand::Transcribe { segment: 1 }, &segs)
            .unwrap();
        r.t.command(&EngineCommand::Transcribe { segment: 2 }, &segs)
            .unwrap();
        r.t.tick(false);
        r.t.set_session(None);
        r.t.shutdown();
        let back = session::read_manifest(&sess.join(session::MANIFEST_NAME)).unwrap();
        for s in &back.segments {
            let t = s.inputs[0].transcript.as_ref().unwrap();
            assert_eq!(t.status, "interrupted");
        }
        r.t.set_session(Some(sess.clone()));
        assert!(
            r.t.command(&EngineCommand::Transcribe { segment: 1 }, &segs)
                .is_ok()
        );
        r.t.tick(false);
        assert!(r.t.running.is_none(), "nothing starts after shutdown");
    }

    /// Serves `data` with Range support, slowly (16 KiB every 4 ms).
    fn slow_server(data: Vec<u8>) -> String {
        use std::io::{BufRead, BufReader};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/m.bin", l.local_addr().unwrap());
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let data = data.clone();
                std::thread::spawn(move || {
                    let mut c = c;
                    let mut rd = BufReader::new(c.try_clone().unwrap());
                    let mut range = (0, data.len() - 1);
                    loop {
                        let mut line = String::new();
                        if rd.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let l = line.trim_end().to_ascii_lowercase();
                        if l.is_empty() {
                            break;
                        }
                        if let Some(v) = l.strip_prefix("range: bytes=") {
                            let (a, b) = v.split_once('-').unwrap();
                            range = (
                                a.parse().unwrap(),
                                b.parse::<usize>().unwrap().min(data.len() - 1),
                            );
                        }
                    }
                    let (a, b) = range;
                    let _ = write!(
                        c,
                        "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {a}-{b}/{}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        data.len(),
                        b + 1 - a
                    );
                    for ch in data[a..=b].chunks(16 << 10) {
                        if c.write_all(ch).is_err() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(4));
                    }
                });
            }
        });
        url
    }

    #[test]
    fn download_cancel_then_immediate_redownload() {
        use sha2::Digest;
        let data: Vec<u8> = (0..(6usize << 20)).map(|i| (i % 253) as u8).collect();
        let sha: &'static str = Box::leak(
            sha2::Sha256::digest(&data)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
                .into_boxed_str(),
        );
        let vad_data = model::VAD; // VAD comes from the same server: make it "present".
        let spec: &'static model::ModelSpec = Box::leak(Box::new(model::ModelSpec {
            name: "fake-model",
            file: "fake.bin",
            bytes: data.len() as u64,
            sha256: sha,
        }));
        let dir = scratch("dl");
        let models = dir.join("models");
        std::fs::create_dir_all(&models).unwrap();
        // Pretend the VAD file is present and verified (sidecar), so only the model downloads.
        let vad_path = models.join(vad_data.file);
        let vad_bytes = vec![0u8; vad_data.bytes as usize];
        std::fs::write(&vad_path, &vad_bytes).unwrap();
        let meta = std::fs::metadata(&vad_path).unwrap();
        use std::os::unix::fs::MetadataExt;
        std::fs::write(
            models.join(format!("{}.sha256ok", vad_data.file)),
            format!(
                "{} {} {}",
                vad_data.sha256,
                meta.len(),
                meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128
            ),
        )
        .unwrap();
        let url = slow_server(data.clone());
        let mut r = rig_with(
            dir.clone(),
            Resolved::Catalog(spec),
            "exit 1",
            false,
            Some(url),
        );
        // The startup check accepts the VAD by its sidecar and finds the model missing.
        r.pump_until(false, Duration::from_secs(5), |t| !t.model_thread_alive);
        assert_eq!(
            r.t.model,
            ModelState::Missing {
                download_bytes: data.len() as u64
            }
        );
        r.t.command(&EngineCommand::DownloadModel, &[]).unwrap();
        assert!(r.t.command(&EngineCommand::DownloadModel, &[]).is_err());
        r.pump_until(
            false,
            Duration::from_secs(10),
            |t| matches!(t.model, ModelState::Downloading { received, .. } if received > 0),
        );
        r.t.command(&EngineCommand::CancelModelDownload, &[])
            .unwrap();
        assert_eq!(r.t.model, ModelState::Cancelling);
        // Immediately again: rejected while the thread is still alive, never "another
        // download is running".
        let err = r.t.command(&EngineCommand::DownloadModel, &[]).unwrap_err();
        assert!(err.contains("has not stopped"), "{err}");
        r.pump_until(false, Duration::from_secs(10), |t| !t.model_thread_alive);
        assert!(
            matches!(r.t.model, ModelState::Missing { .. }),
            "{:?}",
            r.t.model
        );
        assert!(models.join("fake.bin.part").exists());
        r.t.command(&EngineCommand::DownloadModel, &[]).unwrap();
        r.pump_until(false, Duration::from_secs(30), |t| !t.model_thread_alive);
        let notices = r.t.take_notices();
        assert!(
            matches!(
                &r.t.model,
                ModelState::Ready {
                    vad_download: None,
                    ..
                }
            ),
            "{:?} / {notices:?}",
            r.t.model
        );
        assert_eq!(std::fs::read(models.join("fake.bin")).unwrap(), data);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
