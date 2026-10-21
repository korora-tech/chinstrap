//! `StudioView` (SPEC §1, §4.2): renders the latest engine `Snapshot` and sends commands.
//!
//! The view never reconstructs engine state: every `Snapshot` replaces the previous one, and
//! the engine re-validates every command, so disabled controls are a convenience, not a gate.

use std::time::{Duration, Instant};

use gpui::{
    App, ClickEvent, Context, Div, EventEmitter, FocusHandle, Focusable, FontWeight, ScrollHandle,
    SharedString, Stateful, Task, Window, div, prelude::*, px, rgb,
};

use super::theme::*;
use super::widgets::{
    ButtonStyle, OnClick, SegmentActions, badge, button, caption, checkbox_row, format_bytes,
    format_duration, icon_button, level_meter, notice_row, progress_bar, radio_row, segment_row,
    source_row,
};
use super::{Cut, ToggleRecord};
use crate::studio::{
    CaptureId, DeviceId, DeviceInfo, DeviceKind, EngineCommand, EngineEvent, EngineHandle,
    InputKey, InputStatus, ModelState, Notice, Phase, PortalAvailability, SegmentStatus,
    SegmentSummary, Snapshot, SourceInfo, SourceKind, SourceState, TranscriptStatus,
    WhisperBackend,
};

const NOTICE_CAP: usize = 50;
const CLOSE_FALLBACK: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseIntent {
    CloseWindow,
    QuitApp,
}

/// Emitted when a pending `CloseWindow` may proceed; `AppShell` removes the window.
pub struct CloseWindowNow;

pub struct StudioView {
    engine: EngineHandle,
    snap: Option<Snapshot>,
    notices: Vec<Notice>,
    level: Option<(f32, f32)>,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    close_pending: Option<CloseIntent>,
    /// The Recording segment a close-driven Stop was already sent for (no duplicate Stops).
    stop_sent_for: Option<u32>,
    _events: Task<()>,
    _ticker: Option<Task<()>>,
}

impl EventEmitter<CloseWindowNow> for StudioView {}

impl StudioView {
    pub fn new(
        engine: EngineHandle,
        events: async_channel::Receiver<EngineEvent>,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let _events = cx.spawn(async move |this, cx| {
            while let Ok(ev) = events.recv().await {
                if this
                    .update(cx, |v, cx| {
                        v.apply(ev, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            engine,
            snap: None,
            notices: vec![],
            level: None,
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            close_pending: None,
            stop_sent_for: None,
            _events,
            _ticker: None,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.snap.as_ref().is_some_and(|s| s.phase.is_busy())
    }

    /// Called by AppShell: sends Stop, sets close_pending, starts a 10 s fallback timer that forces it.
    pub fn request_close(&mut self, intent: CloseIntent, cx: &mut Context<Self>) {
        if !self.is_busy() {
            self.finish_close(intent, cx);
            return;
        }
        let first = self.close_pending.is_none();
        // A quit request outranks a window close.
        self.close_pending = Some(match (self.close_pending, intent) {
            (Some(CloseIntent::QuitApp), _) | (_, CloseIntent::QuitApp) => CloseIntent::QuitApp,
            _ => CloseIntent::CloseWindow,
        });
        // Stop is only accepted while Recording; Starting/Cutting are followed up in `apply`.
        self.send_close_stop();
        if first {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(CLOSE_FALLBACK).await;
                let _ = this.update(cx, |v, cx| {
                    if let Some(intent) = v.close_pending.take() {
                        eprintln!(
                            "chinstrap: engine did not reach Idle within 10 s; closing anyway"
                        );
                        v.finish_close(intent, cx);
                    }
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn finish_close(&mut self, intent: CloseIntent, cx: &mut Context<Self>) {
        match intent {
            // The on_app_quit hook runs shutdown_blocking as the safety net.
            CloseIntent::QuitApp => cx.quit(),
            CloseIntent::CloseWindow => cx.emit(CloseWindowNow),
        }
    }

    /// Sends Stop for the current Recording segment, at most once per segment.
    fn send_close_stop(&mut self) {
        if let Some(Phase::Recording { segment, .. }) = self.phase()
            && self.stop_sent_for != Some(*segment)
        {
            self.stop_sent_for = Some(*segment);
            self.engine.send(EngineCommand::Stop);
        }
    }

    fn phase(&self) -> Option<&Phase> {
        self.snap.as_ref().map(|s| &s.phase)
    }

    fn apply(&mut self, ev: EngineEvent, cx: &mut Context<Self>) {
        match ev {
            EngineEvent::Snapshot(snap) => {
                let snap = *snap;
                let recording = matches!(snap.phase, Phase::Recording { .. });
                if recording {
                    if self._ticker.is_none() {
                        self._ticker = Some(Self::start_ticker(cx));
                    }
                } else {
                    self._ticker = None;
                    self.level = None;
                }
                let idle = snap.phase == Phase::Idle;
                self.snap = Some(snap);
                if idle {
                    self.stop_sent_for = None;
                    if let Some(intent) = self.close_pending.take() {
                        self.finish_close(intent, cx);
                    }
                } else if recording && self.close_pending.is_some() {
                    // Close was requested during Starting/Cutting, where Stop is rejected.
                    self.send_close_stop();
                }
            }
            EngineEvent::Notice(notice) => {
                if self.notices.len() >= NOTICE_CAP {
                    self.notices.remove(0);
                }
                self.notices.push(notice);
            }
            EngineEvent::Level { rms_db, peak_db } => {
                self.level = Some((rms_db, peak_db));
            }
        }
    }

    fn start_ticker(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
    }

    fn send_on_click(&self, cmd: EngineCommand, cx: &mut Context<Self>) -> OnClick {
        Box::new(cx.listener(move |this, _: &ClickEvent, _, _| this.engine.send(cmd.clone())))
    }

    // ----- sections -------------------------------------------------------------------------

    fn banner(&self, snap: &Snapshot) -> Option<Div> {
        let caps = &snap.capabilities;
        let mut lines: Vec<String> = Vec::new();
        if !caps.missing_elements.is_empty() {
            lines.push(format!(
                "Missing GStreamer elements: {}. Inputs that need them cannot record.",
                caps.missing_elements.join(", ")
            ));
        }
        if caps.h264_encoder.is_none() {
            lines.push(
                "No working H.264 encoder (vah264enc, x264enc, openh264enc): video inputs cannot record."
                    .to_string(),
            );
        }
        if caps.gpu_convert.is_none() {
            lines.push(
                "Screen/window capture unavailable: neither VA (gst-plugins-bad + a VA driver) nor GL conversion works."
                    .to_string(),
            );
        }
        if let PortalAvailability::Unavailable { reason } = &caps.portal {
            lines.push(format!("ScreenCast portal unavailable: {reason}"));
        }
        if lines.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .px_5()
                .py_2()
                .bg(rgb(0x2a1a12))
                .border_b_1()
                .border_color(rgb(0x5a3a1a))
                .children(lines.into_iter().map(|l| {
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().size(px(6.0)).rounded_full().bg(rgb(AMBER)))
                        .child(div().text_size(px(11.0)).text_color(rgb(0xffd9a0)).child(l))
                })),
        )
    }

    fn transport(&self, snap: &Snapshot, cx: &mut Context<Self>) -> Div {
        let (dot, label, elapsed, segment) = match &snap.phase {
            Phase::Idle => (0x4f5661, "IDLE", None, None),
            Phase::Starting { segment } => (AMBER, "STARTING", None, Some(*segment)),
            Phase::Recording {
                segment,
                started_at,
            } => (
                RED,
                "REC",
                Some(Instant::now().saturating_duration_since(*started_at)),
                Some(*segment),
            ),
            Phase::Cutting { from } => (AMBER, "CUTTING", None, Some(*from)),
            Phase::Stopping { segment } => (AMBER, "STOPPING", None, Some(*segment)),
        };
        let recording = matches!(snap.phase, Phase::Recording { .. });
        let idle = snap.phase == Phase::Idle;

        let record_button = if recording {
            button(
                "record",
                "Stop",
                Some("R"),
                ButtonStyle::Danger,
                Some(self.send_on_click(EngineCommand::ToggleRecord, cx)),
            )
        } else {
            button(
                "record",
                "Record",
                Some("R"),
                ButtonStyle::Primary,
                (idle && snap.can_record)
                    .then(|| self.send_on_click(EngineCommand::ToggleRecord, cx)),
            )
        };
        let cut_button = button(
            "cut",
            "Cut",
            Some("C"),
            ButtonStyle::Secondary,
            recording.then(|| self.send_on_click(EngineCommand::Cut, cx)),
        );
        let new_session = button(
            "new-session",
            "New session",
            None,
            ButtonStyle::Secondary,
            (idle && snap.session.is_some())
                .then(|| self.send_on_click(EngineCommand::EndSession, cx)),
        );

        let record_hint: SharedString = if idle && !snap.can_record {
            if snap.portal_busy {
                "Waiting for the picker…".into()
            } else if snap
                .sources
                .iter()
                .any(|s| s.state == SourceState::Checking)
            {
                "Waiting for sources to finish checking…".into()
            } else {
                "Select at least one recordable input".into()
            }
        } else if recording {
            "Selection is locked while recording".into()
        } else {
            "".into()
        };

        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .h(px(56.0))
            .px_5()
            .bg(rgb(TOOLBAR))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .rounded_full()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .bg(rgb(PANEL))
                            .px_3()
                            .py(px(5.0))
                            .child(div().size(px(9.0)).rounded_full().bg(rgb(dot)))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(if recording { RED } else { SECONDARY }))
                                    .child(label),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(22.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(if recording { TEXT } else { 0x5c6370 }))
                            .child(format_duration(elapsed.unwrap_or_default())),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(MUTED))
                            .child(match segment {
                                Some(n) => format!("Segment {n}"),
                                None => "No active segment".to_string(),
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(MUTED))
                            .child(record_hint),
                    )
                    .child(new_session)
                    .child(cut_button)
                    .child(record_button),
            )
    }

    fn device_list(&self, kind: DeviceKind, snap: &Snapshot, cx: &mut Context<Self>) -> Div {
        let locked = snap.phase.is_busy();
        let make_cmd: fn(Option<DeviceId>) -> EngineCommand = match kind {
            DeviceKind::Camera => EngineCommand::SelectCamera,
            DeviceKind::Microphone => EngineCommand::SelectMicrophone,
        };
        let (title, id_prefix, devices, selected): (_, _, &[DeviceInfo], _) = match kind {
            DeviceKind::Camera => ("CAMERA", "cam", &snap.cameras, snap.selected_camera),
            DeviceKind::Microphone => (
                "MICROPHONE",
                "mic",
                &snap.microphones,
                snap.selected_microphone,
            ),
        };
        let none_click =
            (!locked && selected.is_some()).then(|| self.send_on_click(make_cmd(None), cx));
        let mut list = div()
            .flex()
            .flex_col()
            .child(div().px_2().mb_2().child(caption(title)))
            .child(radio_row(
                (id_prefix, usize::MAX),
                "None",
                None,
                None,
                selected.is_none(),
                none_click,
            ));
        for dev in devices {
            let is_selected = selected == Some(dev.id);
            let click = (!locked && !is_selected && dev.unsupported.is_none())
                .then(|| self.send_on_click(make_cmd(Some(dev.id)), cx));
            list = list.child(radio_row(
                (id_prefix, dev.id.0 as usize),
                dev.name.clone(),
                Some(dev.detail.clone().into()),
                dev.unsupported.clone().map(Into::into),
                is_selected,
                click,
            ));
        }
        if devices.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .text_size(px(10.0))
                    .text_color(rgb(MUTED))
                    .child("No devices found"),
            );
        }
        list
    }

    fn sidebar(&self, snap: &Snapshot, cx: &mut Context<Self>) -> Stateful<Div> {
        let locked = snap.phase.is_busy();
        let recording = matches!(snap.phase, Phase::Recording { .. });
        let mic_selected = snap.selected_microphone.is_some();
        div()
            .id("studio-inputs")
            .w(px(272.0))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .gap_5()
            .bg(rgb(SIDEBAR))
            .border_r_1()
            .border_color(rgb(BORDER))
            .px_3()
            .py_4()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .child(
                        div()
                            .text_size(px(10.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(MUTED))
                            .child("INPUTS"),
                    )
                    .child(icon_button(
                        "refresh-devices",
                        "Refresh",
                        (!locked).then(|| self.send_on_click(EngineCommand::RefreshDevices, cx)),
                    )),
            )
            .child(self.device_list(DeviceKind::Camera, snap, cx))
            .child(
                self.device_list(DeviceKind::Microphone, snap, cx)
                    .when(recording && mic_selected, |d| {
                        d.child(div().px_2().mt_2().child(level_meter(self.level)))
                    }),
            )
            .when(locked, |d| {
                d.child(
                    div()
                        .px_2()
                        .text_size(px(10.0))
                        .text_color(rgb(MUTED))
                        .child("Selection is locked until the engine is idle."),
                )
            })
    }

    /// Why a Ready source still cannot record, from the engine capabilities.
    fn source_blocked(snap: &Snapshot, src: &SourceInfo) -> Option<String> {
        if snap.capabilities.h264_encoder.is_none() {
            return Some("no H.264 encoder".into());
        }
        if src.kind != SourceKind::TestPattern && snap.capabilities.gpu_convert.is_none() {
            return Some("no GPU conversion path".into());
        }
        None
    }

    fn sources_section(&self, snap: &Snapshot, cx: &mut Context<Self>) -> Div {
        let idle = snap.phase == Phase::Idle;
        let portal_ok = matches!(
            snap.capabilities.portal,
            PortalAvailability::Available { .. }
        );
        let pick_click = (idle && portal_ok && !snap.portal_busy)
            .then(|| self.send_on_click(EngineCommand::AddPortalSources, cx));
        let test_click = idle.then(|| self.send_on_click(EngineCommand::AddTestSource, cx));
        let pick_label = if snap.portal_busy {
            "Picker open…"
        } else {
            "Add windows/screens…"
        };
        let portal_note: Option<SharedString> = match &snap.capabilities.portal {
            PortalAvailability::Unknown => Some("Checking the ScreenCast portal…".into()),
            PortalAvailability::Unavailable { reason } => {
                Some(format!("Portal unavailable: {reason}").into())
            }
            PortalAvailability::Available { .. } => None,
        };

        // Group by capture, keeping first-appearance order. Test patterns stand alone.
        let mut groups: Vec<(Option<CaptureId>, Vec<&SourceInfo>)> = Vec::new();
        for src in &snap.sources {
            match src.capture {
                Some(cap) => match groups.iter_mut().find(|(c, _)| *c == Some(cap)) {
                    Some((_, members)) => members.push(src),
                    None => groups.push((Some(cap), vec![src])),
                },
                None => groups.push((None, vec![src])),
            }
        }

        let mut list = div().flex().flex_col().gap_2();
        for (capture, members) in groups {
            let grouped = capture.is_some() && members.len() > 1;
            let mut group = div().flex().flex_col().gap_2();
            if grouped {
                group = group
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(0x2f2a44))
                    .bg(rgb(0x14131c))
                    .p_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_1()
                            .child(div().size(px(5.0)).rounded_full().bg(rgb(ACCENT)))
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(ACCENT_TEXT))
                                    .child("Picked together · ends together"),
                            ),
                    );
            }
            for src in members {
                let remove =
                    idle.then(|| self.send_on_click(EngineCommand::RemoveSource(src.id), cx));
                let blocked = Self::source_blocked(snap, src);
                group = group.child(source_row(src, blocked, remove));
            }
            list = list.child(group);
        }
        if snap.sources.is_empty() {
            list = list.child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL))
                    .px_3()
                    .py_4()
                    .text_size(px(11.0))
                    .text_color(rgb(MUTED))
                    .child("No sources yet. Add a window, a screen or a test pattern."),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(16.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT))
                                    .child("Sources"),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(rgb(MUTED))
                                    .child("Each source is recorded to its own file."),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(button(
                                "add-test",
                                "Add test pattern",
                                None,
                                ButtonStyle::Secondary,
                                test_click,
                            ))
                            .child(button(
                                "add-portal",
                                pick_label,
                                None,
                                ButtonStyle::Secondary,
                                pick_click,
                            )),
                    ),
            )
            .when_some(portal_note, |d, note| {
                d.child(div().text_size(px(10.0)).text_color(rgb(AMBER)).child(note))
            })
            .child(list)
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(0x737b89))
                    .child(
                        "Windows picked in one dialog share a portal session: closing one ends all of them. \
                         Add windows one pick at a time to keep them independent.",
                    ),
            )
    }

    fn output_section(&self, snap: &Snapshot) -> Div {
        let (path, pending) = match &snap.session {
            Some(session) => (format!("{}/", session.dir.display()), false),
            None => (
                format!("{}/<new session>/", snap.output_root.display()),
                true,
            ),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(caption("OUTPUT"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL))
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(11.0))
                            .text_color(rgb(if pending { MUTED } else { TEXT }))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis_start()
                            .child(path),
                    )
                    .child(match &snap.session {
                        Some(_) => badge("Session open", GREEN),
                        None => badge("New session on Record", 0x737b89),
                    }),
            )
    }

    /// "Whisper large-v3-turbo · q8_0" for the configured model (a label only; the engine
    /// resolves the model itself).
    fn model_label(model: &ModelState) -> String {
        let id = match model {
            ModelState::Ready { id, .. } | ModelState::External { id, .. } => id.clone(),
            _ => crate::studio::transcribe::model::resolve_env().0.id(),
        };
        match id.rsplit_once('-') {
            Some((base, q)) if q.starts_with('q') => format!("Whisper {base} · {q}"),
            _ => format!("Whisper {id}"),
        }
    }

    fn transcription_section(&self, snap: &Snapshot, cx: &mut Context<Self>) -> Div {
        let t = &snap.transcription;
        let muted = |text: String| div().text_size(px(10.0)).text_color(rgb(MUTED)).child(text);
        let state: Div = match &t.model {
            ModelState::Checking => div().child(muted("Checking…".into())),
            ModelState::Missing { download_bytes } => div().child(button(
                "download-model",
                format!("Download model ({})", format_bytes(*download_bytes)),
                None,
                ButtonStyle::Primary,
                Some(self.send_on_click(EngineCommand::DownloadModel, cx)),
            )),
            ModelState::Downloading { received, total } => div()
                .flex()
                .flex_col()
                .gap_1()
                .w_full()
                .child(progress_bar(*received as f32 / (*total).max(1) as f32))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(muted(format!(
                            "{} / {}",
                            format_bytes(*received),
                            format_bytes(*total)
                        )))
                        .child(icon_button(
                            "cancel-download",
                            "Cancel",
                            Some(self.send_on_click(EngineCommand::CancelModelDownload, cx)),
                        )),
                ),
            ModelState::Cancelling => div().child(muted("Cancelling…".into())),
            ModelState::Verifying => div().child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(SECONDARY))
                    .child("Verifying…"),
            ),
            ModelState::Ready { vad_download, .. } => {
                let backend = t.backend.map(|b| match b {
                    WhisperBackend::Vulkan => "· Vulkan",
                    WhisperBackend::Cpu => "· CPU",
                });
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(badge("Ready", GREEN))
                    .when_some(backend, |d, b| d.child(muted(b.into())))
                    .when_some(*vad_download, |d, bytes| {
                        d.child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(AMBER))
                                .child("· no VAD"),
                        )
                        .child(button(
                            "download-vad",
                            format!("Download VAD ({})", format_bytes(bytes)),
                            None,
                            ButtonStyle::Secondary,
                            Some(self.send_on_click(EngineCommand::DownloadModel, cx)),
                        ))
                    })
            }
            ModelState::External { .. } => div().child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(ACCENT_TEXT))
                    .child("custom model"),
            ),
            ModelState::Failed { reason, .. } => div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(RED))
                        .child(reason.clone()),
                )
                .child(div().child(button(
                    "retry-download",
                    "Retry download",
                    None,
                    ButtonStyle::Secondary,
                    Some(self.send_on_click(EngineCommand::DownloadModel, cx)),
                ))),
        };
        let others = t.other_sessions.as_ref().map(|o| {
            let mut text = format!("Earlier sessions: {} queued", o.queued);
            if let Some(p) = o.running_percent {
                text.push_str(&format!(" · transcribing {p}%"));
            }
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(muted(text))
                .child(icon_button(
                    "cancel-other-sessions",
                    "Cancel",
                    Some(self.send_on_click(EngineCommand::CancelOtherSessionTranscriptions, cx)),
                ))
        });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(caption("TRANSCRIPTION"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL))
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(rgb(TEXT))
                                    .child(Self::model_label(&t.model)),
                            )
                            .child(state),
                    )
                    .child(checkbox_row(
                        "auto-transcribe",
                        "Transcribe segments automatically",
                        t.auto,
                        Some(self.send_on_click(EngineCommand::SetAutoTranscribe(!t.auto), cx)),
                    ))
                    .when(t.vulkan_broken, |d| {
                        d.child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(AMBER))
                                .child("Vulkan unavailable: using CPU (paused while recording)"),
                        )
                    })
                    .when_some(others, |d, o| d.child(o)),
            )
    }

    fn segment_actions(
        &self,
        snap: &Snapshot,
        seg: &SegmentSummary,
        cx: &mut Context<Self>,
    ) -> SegmentActions {
        let mic_complete = seg
            .files
            .iter()
            .any(|f| f.input == InputKey::Microphone && f.status == InputStatus::Complete);
        let can_transcribe = snap.transcription.model.usable()
            && seg.status != SegmentStatus::Recording
            && mic_complete;
        let segment = seg.index;
        match &seg.transcript {
            TranscriptStatus::None | TranscriptStatus::Failed { .. } => SegmentActions {
                transcribe: can_transcribe
                    .then(|| self.send_on_click(EngineCommand::Transcribe { segment }, cx)),
                ..Default::default()
            },
            TranscriptStatus::Queued { .. } | TranscriptStatus::Running { .. } => SegmentActions {
                cancel: Some(
                    self.send_on_click(EngineCommand::CancelTranscription { segment }, cx),
                ),
                ..Default::default()
            },
            TranscriptStatus::Done { srt, .. } => {
                let srt = srt.clone();
                SegmentActions {
                    open: Some(Box::new(
                        cx.listener(move |_, _: &ClickEvent, _, cx| cx.reveal_path(&srt)),
                    )),
                    ..Default::default()
                }
            }
        }
    }

    fn segments_section(&self, snap: &Snapshot, cx: &mut Context<Self>) -> Div {
        let mut section = div().flex().flex_col().gap_2().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(caption("SEGMENTS"))
                .when_some(snap.session.as_ref(), |d, s| {
                    d.child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(MUTED))
                            .child(format!("Session {}", s.name)),
                    )
                }),
        );
        match &snap.session {
            Some(session) if !session.segments.is_empty() => {
                let rows: Vec<Div> = session
                    .segments
                    .iter()
                    .rev()
                    .map(|seg| segment_row(seg, self.segment_actions(snap, seg, cx)))
                    .collect();
                section = section.children(rows);
            }
            _ => {
                section = section.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(MUTED))
                        .child("Nothing recorded in this session yet."),
                );
            }
        }
        section
    }

    fn notices_panel(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let rows = self
            .notices
            .iter()
            .enumerate()
            .rev()
            .map(|(index, notice)| {
                let dismiss: OnClick = Box::new(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    if index < this.notices.len() {
                        this.notices.remove(index);
                        cx.notify();
                    }
                }));
                notice_row(index, notice, dismiss)
            })
            .collect::<Vec<_>>();
        div()
            .w(px(300.0))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(SIDEBAR))
            .border_l_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(MUTED))
                            .child("NOTICES"),
                    )
                    .child(
                        div()
                            .rounded_full()
                            .bg(rgb(0x232332))
                            .px_2()
                            .py_1()
                            .text_size(px(10.0))
                            .text_color(rgb(ACCENT_TEXT))
                            .child(format!("{}", self.notices.len())),
                    ),
            )
            .child(
                div()
                    .id("studio-notices")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .when(rows.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(10.0))
                                .text_color(rgb(MUTED))
                                .child("Engine messages appear here."),
                        )
                    })
                    .children(rows),
            )
    }
}

impl Focusable for StudioView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StudioView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .key_context("Studio")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ToggleRecord, _, _| {
                this.engine.send(EngineCommand::ToggleRecord)
            }))
            .on_action(cx.listener(|this, _: &Cut, _, _| this.engine.send(EngineCommand::Cut)))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT));

        let Some(snap) = self.snap.clone() else {
            return root.child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(13.0))
                    .text_color(rgb(MUTED))
                    .child("Starting engine…"),
            );
        };

        let main = div()
            .id("studio-scroll")
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px_5()
            .py_5()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.sources_section(&snap, cx))
            .child(self.output_section(&snap))
            .child(self.transcription_section(&snap, cx))
            .child(self.segments_section(&snap, cx));

        root.when_some(self.banner(&snap), |d, b| d.child(b))
            .child(self.transport(&snap, cx))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .overflow_hidden()
                    .child(self.sidebar(&snap, cx))
                    .child(main)
                    .child(self.notices_panel(cx)),
            )
            .when(self.close_pending.is_some(), |d| {
                d.child(
                    div()
                        .id("finalizing-overlay")
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(gpui::rgba(0x0d0f14d9))
                        .occlude()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap_2()
                                .rounded_xl()
                                .border_1()
                                .border_color(rgb(0x303646))
                                .bg(rgb(PANEL_RAISED))
                                .px_6()
                                .py_5()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(div().size(px(9.0)).rounded_full().bg(rgb(AMBER)))
                                        .child(
                                            div()
                                                .text_size(px(15.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child("Finalizing recording…"),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.0))
                                        .text_color(rgb(MUTED))
                                        .child("Chinstrap closes once every file is written."),
                                ),
                        ),
                )
            })
    }
}
