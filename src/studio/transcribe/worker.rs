//! `chinstrap-transcribe`: the long-lived in-app executor thread around `driver.rs` (§12.3).
//!
//! It owns at most one running child. It is also the thread that spawns every child, so the
//! child's `PR_SET_PDEATHSIG` fires only when the whole process (or this thread) dies.

use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use super::driver::{DriverEvent, ExitClass, Running};
use super::proto::{self, Job, WorkerLine};
use crate::studio::types::{TranscribeStage, WhisperBackend};

pub enum WorkerCmd {
    Start { id: u64, job: Job },
    Cancel { id: u64 },
    Pause,
    Resume,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WorkerEvent {
    Backend {
        id: u64,
        backend: WhisperBackend,
        device: String,
    },
    Progress {
        id: u64,
        stage: TranscribeStage,
        percent: u8,
    },
    Done {
        id: u64,
        json: std::path::PathBuf,
        srt: std::path::PathBuf,
        vtt: std::path::PathBuf,
        language: String,
        words: u32,
        backend: WhisperBackend,
    },
    Failed {
        id: u64,
        reason: String,
        class: ExitClass,
    },
}

pub type WorkerSink = Arc<dyn Fn(WorkerEvent) + Send + Sync>;
pub type CommandFactory = Arc<dyn Fn() -> std::io::Result<Command> + Send + Sync>;

pub struct WorkerHandle {
    tx: Sender<WorkerCmd>,
}

impl WorkerHandle {
    pub fn send(&self, cmd: WorkerCmd) {
        let _ = self.tx.send(cmd);
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(WorkerCmd::Shutdown);
    }
}

struct Current {
    id: u64,
    run: Running,
    backend: WhisperBackend,
}

pub fn spawn(sink: WorkerSink, make_command: CommandFactory) -> std::io::Result<WorkerHandle> {
    let (tx, rx) = mpsc::channel::<WorkerCmd>();
    std::thread::Builder::new()
        .name("chinstrap-transcribe".into())
        .spawn(move || {
            let mut current: Option<Current> = None;
            loop {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(WorkerCmd::Start { id, job }) => {
                        if let Some(mut c) = current.take() {
                            // Never two children: the engine only starts after an exit.
                            c.run.kill_now();
                        }
                        let backend = if job.backend == proto::BackendReq::Cpu {
                            WhisperBackend::Cpu
                        } else {
                            WhisperBackend::Vulkan
                        };
                        match make_command()
                            .and_then(|cmd| Running::spawn(cmd, &job, Some("whisper: ")))
                        {
                            Ok(run) => current = Some(Current { id, run, backend }),
                            Err(e) => sink(WorkerEvent::Failed {
                                id,
                                reason: format!("cannot start the transcription worker: {e}"),
                                class: ExitClass::JobFailed,
                            }),
                        }
                    }
                    Ok(WorkerCmd::Cancel { id }) => {
                        if let Some(c) = current.as_mut().filter(|c| c.id == id) {
                            c.run.cancel();
                        }
                    }
                    Ok(WorkerCmd::Pause) => {
                        if let Some(c) = current.as_mut() {
                            c.run.pause();
                        }
                    }
                    Ok(WorkerCmd::Resume) => {
                        if let Some(c) = current.as_mut() {
                            c.run.resume();
                        }
                    }
                    Ok(WorkerCmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                        if let Some(mut c) = current.take() {
                            c.run.kill_now();
                            // Drop reaps the killed child (immediate after SIGKILL).
                        }
                        return;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }
                let Some(c) = current.as_mut() else {
                    continue;
                };
                let mut finished = false;
                for ev in c.run.poll() {
                    match ev {
                        DriverEvent::Line(WorkerLine::Backend { backend, device }) => {
                            c.backend = proto::backend_from_str(&backend);
                            sink(WorkerEvent::Backend {
                                id: c.id,
                                backend: c.backend,
                                device,
                            });
                        }
                        DriverEvent::Line(WorkerLine::Progress { stage, percent }) => {
                            sink(WorkerEvent::Progress {
                                id: c.id,
                                stage: proto::stage_from_str(&stage),
                                percent,
                            })
                        }
                        DriverEvent::Line(_) => {}
                        DriverEvent::Exit(exit) => {
                            finished = true;
                            match (exit.class, exit.done) {
                                (
                                    ExitClass::Ok,
                                    Some(WorkerLine::Done {
                                        json,
                                        srt,
                                        vtt,
                                        language,
                                        words,
                                        ..
                                    }),
                                ) => sink(WorkerEvent::Done {
                                    id: c.id,
                                    json,
                                    srt,
                                    vtt,
                                    language,
                                    words,
                                    backend: c.backend,
                                }),
                                (ExitClass::Ok, _) => sink(WorkerEvent::Failed {
                                    id: c.id,
                                    reason: "worker exited without a result".into(),
                                    class: ExitClass::JobFailed,
                                }),
                                (class, _) => sink(WorkerEvent::Failed {
                                    id: c.id,
                                    reason: exit.reason,
                                    class,
                                }),
                            }
                        }
                    }
                }
                if finished {
                    current = None;
                }
            }
        })?;
    Ok(WorkerHandle { tx })
}
