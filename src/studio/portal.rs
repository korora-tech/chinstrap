//! Portal thread (SPEC §4.1): ashpd on async-io under a `smol::LocalExecutor`.
//!
//! The command loop only dispatches. Each picker flow (and the capture watcher it turns into)
//! is a spawned task. The portal thread owns the `Session`s and the original PipeWire remote
//! fds; the engine receives one `try_clone()` per capture.

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, ResponseError};
use futures::StreamExt;
use smol::{LocalExecutor, Timer};

use super::engine::Msg;
use super::types::{CaptureId, PortalAvailability};

const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) enum PortalCmd {
    Pick { capture: CaptureId },
    Release { capture: CaptureId },
    Shutdown,
}

/// One stream of a capture: (PipeWire node id, source type, logical size).
pub(super) type StreamDesc = (u32, Option<SourceType>, Option<(i32, i32)>);

pub(super) enum PortalOutcome {
    Availability(PortalAvailability),
    Opened {
        capture: CaptureId,
        streams: Vec<StreamDesc>,
        fd: OwnedFd,
    },
    Failed {
        capture: CaptureId,
        reason: String,
        cancelled: bool,
    },
    Closed {
        capture: CaptureId,
    },
}

pub(super) fn spawn(
    engine: mpsc::Sender<Msg>,
) -> std::io::Result<(async_channel::Sender<PortalCmd>, JoinHandle<()>)> {
    let (tx, rx) = async_channel::unbounded();
    let join = std::thread::Builder::new()
        .name("chinstrap-portal".into())
        .spawn(move || {
            let ex = LocalExecutor::new();
            smol::block_on(ex.run(main_loop(&ex, rx, engine)));
        })?;
    Ok((tx, join))
}

fn post(engine: &mpsc::Sender<Msg>, outcome: PortalOutcome) {
    let _ = engine.send(Msg::Portal(outcome));
}

type Watchers = Rc<RefCell<HashMap<CaptureId, async_channel::Sender<()>>>>;

async fn main_loop(
    ex: &LocalExecutor<'_>,
    cmds: async_channel::Receiver<PortalCmd>,
    engine: mpsc::Sender<Msg>,
) {
    let proxy = match Screencast::new().await {
        Ok(p) => {
            post(
                &engine,
                PortalOutcome::Availability(PortalAvailability::Available {
                    version: p.version(),
                }),
            );
            Some(Rc::new(p))
        }
        Err(e) => {
            post(
                &engine,
                PortalOutcome::Availability(PortalAvailability::Unavailable {
                    reason: e.to_string(),
                }),
            );
            None
        }
    };
    let watchers: Watchers = Rc::new(RefCell::new(HashMap::new()));
    let mut tasks: Vec<smol::Task<()>> = Vec::new();
    while let Ok(cmd) = cmds.recv().await {
        match cmd {
            PortalCmd::Pick { capture } => match &proxy {
                Some(p) => tasks.push(ex.spawn(pick_flow(
                    p.clone(),
                    capture,
                    engine.clone(),
                    watchers.clone(),
                ))),
                None => post(
                    &engine,
                    PortalOutcome::Failed {
                        capture,
                        reason: "the screencast portal is unavailable".into(),
                        cancelled: false,
                    },
                ),
            },
            PortalCmd::Release { capture } => {
                if let Some(tx) = watchers.borrow_mut().remove(&capture) {
                    let _ = tx.try_send(());
                }
            }
            PortalCmd::Shutdown => break,
        }
        tasks.retain(|t| !t.is_finished());
    }
    // Shutdown (or the engine dropped its sender): close every session, each bounded at 2 s
    // inside its watcher; bound the whole wait as well. Dropping the remaining tasks (and the
    // executor) cancels any open picker request.
    for (_, tx) in watchers.borrow_mut().drain() {
        let _ = tx.try_send(());
    }
    let all = futures::future::join_all(tasks);
    smol::future::or(
        async {
            all.await;
        },
        async {
            Timer::after(CLOSE_TIMEOUT + Duration::from_millis(200)).await;
        },
    )
    .await;
}

enum Why {
    Closed,
    Release,
}

async fn pick_flow(
    proxy: Rc<Screencast>,
    capture: CaptureId,
    engine: mpsc::Sender<Msg>,
    watchers: Watchers,
) {
    let fail = |reason: String, cancelled: bool| {
        post(
            &engine,
            PortalOutcome::Failed {
                capture,
                reason,
                cancelled,
            },
        )
    };
    let is_cancel =
        |e: &ashpd::Error| matches!(e, ashpd::Error::Response(ResponseError::Cancelled));

    let session = match proxy.create_session(Default::default()).await {
        Ok(s) => s,
        Err(e) => return fail(format!("CreateSession failed: {e}"), is_cancel(&e)),
    };
    let close_session = async |s: &ashpd::desktop::Session<Screencast>| {
        let _ = smol::future::or(async { s.close().await.map_err(|_| ()) }, async {
            Timer::after(CLOSE_TIMEOUT).await;
            Err(())
        })
        .await;
    };
    let options = SelectSourcesOptions::default()
        .set_cursor_mode(CursorMode::Embedded)
        .set_sources(SourceType::Monitor | SourceType::Window)
        .set_multiple(true)
        .set_persist_mode(PersistMode::DoNot);
    if let Err(e) = proxy
        .select_sources(&session, options)
        .await
        .and_then(|r| r.response())
    {
        close_session(&session).await;
        return fail(format!("SelectSources failed: {e}"), is_cancel(&e));
    }
    let streams = match proxy
        .start(&session, None, Default::default())
        .await
        .and_then(|r| r.response())
    {
        Ok(s) => s,
        Err(e) => {
            close_session(&session).await;
            let cancelled = is_cancel(&e);
            return fail(
                if cancelled {
                    "Picker cancelled".into()
                } else {
                    format!("Start failed: {e}")
                },
                cancelled,
            );
        }
    };
    // Subscribe to Closed right after start() (§4.1), so a share that ends during the rest of
    // the setup is still seen; the subscription is dropped on every early return below.
    let closed = match session.receive_closed().await {
        Ok(c) => c,
        Err(e) => {
            close_session(&session).await;
            return fail(format!("cannot watch the portal session: {e}"), false);
        }
    };
    let fd = match proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
    {
        Ok(fd) => fd,
        Err(e) => {
            close_session(&session).await;
            return fail(format!("OpenPipeWireRemote failed: {e}"), false);
        }
    };
    let engine_fd = match fd.try_clone() {
        Ok(f) => f,
        Err(e) => {
            close_session(&session).await;
            return fail(format!("cannot duplicate the PipeWire fd: {e}"), false);
        }
    };
    let descs: Vec<StreamDesc> = streams
        .streams()
        .iter()
        .map(|s| (s.pipe_wire_node_id(), s.source_type(), s.size()))
        .collect();
    if descs.is_empty() {
        close_session(&session).await;
        return fail("the portal returned no streams".into(), false);
    }
    let mut closed = std::pin::pin!(closed);
    let (release_tx, release_rx) = async_channel::bounded::<()>(1);
    watchers.borrow_mut().insert(capture, release_tx);
    post(
        &engine,
        PortalOutcome::Opened {
            capture,
            streams: descs,
            fd: engine_fd,
        },
    );
    let why = smol::future::or(
        async {
            closed.next().await;
            Why::Closed
        },
        async {
            let _ = release_rx.recv().await;
            Why::Release
        },
    )
    .await;
    watchers.borrow_mut().remove(&capture);
    match why {
        Why::Closed => post(&engine, PortalOutcome::Closed { capture }),
        Why::Release => close_session(&session).await,
    }
    drop(fd);
}
