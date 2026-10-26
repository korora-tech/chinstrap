//! `StudioView` (SPEC §1, §4.2): renders the latest engine `Snapshot` and sends commands.
//!
//! The view never reconstructs engine state: every `Snapshot` replaces the previous one, and
//! the engine re-validates every command, so disabled controls are a convenience, not a gate.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, ClickEvent, Context, Div, EventEmitter, FocusHandle, Focusable, FontWeight, KeyDownEvent,
    RenderImage, ScrollHandle, SharedString, Stateful, Task, Window, div, prelude::*, px, rgb,
};
use smallvec::SmallVec;

use super::theme::*;
use super::widgets::{
    ButtonStyle, OnClick, SegmentActions, SourceRowExtras, badge, button, caption, checkbox_row,
    chip, format_bytes, format_duration, format_fps, format_rate, icon_button, level_meter,
    notice_row, preview_tile, progress_bar, radio_row, segment_row, segmented, source_row,
    text_field,
};
use super::{Cut, ToggleRecord};
use crate::studio::{
    CaptureId, DeviceId, DeviceInfo, DeviceKind, EngineCommand, EngineEvent, EngineHandle,
    Fraction, FrameRateMode, InputKey, InputStatus, ModelState, Notice, Phase, PlannedInput,
    PortalAvailability, PreviewFrame, Previews, SegmentStatus, SegmentSummary, Snapshot, SourceId,
    SourceInfo, SourceKind, SourceState, TranscriptStatus, WhisperBackend,
};

const NOTICE_CAP: usize = 50;
const CLOSE_FALLBACK: Duration = Duration::from_secs(10);
/// New preview frames redraw the view at most this often.
const PREVIEW_REDRAW: Duration = Duration::from_millis(66);

/// A preview on screen: the frame drawn now and the one before it. As in Zed's
/// `remote_video_track_view`, the previous image is kept one more frame (the last scene may
/// still use it) and then dropped from the GPU sprite atlas, which would otherwise keep every
/// frame ever shown.
struct Shown {
    generation: u64,
    current: Arc<RenderImage>,
    previous: Option<Arc<RenderImage>>,
}

impl Shown {
    fn release(self, window: &mut Window) {
        if let Some(previous) = self.previous
            && previous.id != self.current.id
        {
            let _ = window.drop_image(previous);
        }
        let _ = window.drop_image(self.current);
    }
}

/// gpui images are BGRA, which is what the engine delivers.
fn render_image(frame: &PreviewFrame) -> Option<Arc<RenderImage>> {
    let buffer = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra.clone())?;
    Some(Arc::new(RenderImage::new(SmallVec::from_elem(
        image::Frame::new(buffer),
        1,
    ))))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseIntent {
    CloseWindow,
    QuitApp,
}

/// Emitted when a pending `CloseWindow` may proceed; `AppShell` removes the window.
pub struct CloseWindowNow;

/// The vocabulary panel's text fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Term,
    RuleFrom,
    RuleTo,
}

impl Field {
    fn next(self) -> Self {
        match self {
            Self::Term => Self::RuleFrom,
            Self::RuleFrom => Self::RuleTo,
            Self::RuleTo => Self::Term,
        }
    }
    fn prev(self) -> Self {
        self.next().next()
    }
}

/// Text typed into a field but not yet sent to the engine (UI-only state).
struct Draft {
    text: String,
    focus: FocusHandle,
}

impl Draft {
    fn new(cx: &mut App) -> Self {
        Self {
            text: String::new(),
            focus: cx.focus_handle(),
        }
    }
}

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
    term: Draft,
    rule_from: Draft,
    rule_to: Draft,
    previews: Previews,
    shown: HashMap<InputKey, Shown>,
    _events: Task<()>,
    _ticker: Option<Task<()>>,
    _preview_redraw: Task<()>,
}

impl EventEmitter<CloseWindowNow> for StudioView {}

impl StudioView {
    pub fn new(
        engine: EngineHandle,
        events: async_channel::Receiver<EngineEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let previews = engine.previews();
        let wake = previews.changed();
        let _preview_redraw = cx.spawn(async move |this, cx| {
            while wake.recv().await.is_ok() {
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
                cx.background_executor().timer(PREVIEW_REDRAW).await;
            }
        });
        cx.on_release_in(window, |this: &mut Self, window, _| {
            for (_, shown) in this.shown.drain() {
                shown.release(window);
            }
        })
        .detach();
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
            term: Draft::new(cx),
            rule_from: Draft::new(cx),
            rule_to: Draft::new(cx),
            previews,
            shown: HashMap::new(),
            _events,
            _ticker: None,
            _preview_redraw,
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

    // ----- vocabulary fields ----------------------------------------------------------------

    fn draft(&mut self, field: Field) -> &mut Draft {
        match field {
            Field::Term => &mut self.term,
            Field::RuleFrom => &mut self.rule_from,
            Field::RuleTo => &mut self.rule_to,
        }
    }

    fn focus_field(&mut self, field: Field, window: &mut Window, cx: &mut App) {
        let handle = self.draft(field).focus.clone();
        window.focus(&handle, cx);
    }

    /// Sends what the field holds. Rules need both halves; Enter in "from" moves to "to".
    fn submit(&mut self, field: Field, window: &mut Window, cx: &mut App) {
        match field {
            Field::Term => {
                let term = self.term.text.trim().to_string();
                if !term.is_empty() {
                    self.engine.send(EngineCommand::AddVocabularyTerm(term));
                    self.term.text.clear();
                }
            }
            Field::RuleFrom | Field::RuleTo => {
                let from = self.rule_from.text.trim().to_string();
                let to = self.rule_to.text.trim().to_string();
                if from.is_empty() {
                    self.focus_field(Field::RuleFrom, window, cx);
                } else if to.is_empty() {
                    self.focus_field(Field::RuleTo, window, cx);
                } else {
                    self.engine.send(EngineCommand::AddReplacement { from, to });
                    self.rule_from.text.clear();
                    self.rule_to.text.clear();
                    self.focus_field(Field::RuleFrom, window, cx);
                }
            }
        }
    }

    /// Minimal line editing: type, Backspace, Ctrl+Backspace/Ctrl+U (clear), Ctrl+V (paste the
    /// first line), Enter (add), Tab/Shift+Tab (next/previous field), Escape (leave the field).
    fn field_key(
        &mut self,
        field: Field,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ks = &ev.keystroke;
        let m = ks.modifiers;
        match ks.key.as_str() {
            "enter" => self.submit(field, window, cx),
            "escape" => window.focus(&self.focus_handle, cx),
            "tab" => {
                let to = if m.shift { field.prev() } else { field.next() };
                self.focus_field(to, window, cx);
            }
            "backspace" if m.control => self.draft(field).text.clear(),
            "backspace" => {
                self.draft(field).text.pop();
            }
            "u" if m.control => self.draft(field).text.clear(),
            "v" if m.control => {
                if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    let line = text.lines().next().unwrap_or_default().to_string();
                    self.draft(field)
                        .text
                        .extend(line.chars().filter(|c| !c.is_control()));
                }
            }
            _ if m.control || m.alt || m.platform => return,
            _ => match &ks.key_char {
                Some(ch) => self
                    .draft(field)
                    .text
                    .extend(ch.chars().filter(|c| !c.is_control())),
                None => return,
            },
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn field(
        &self,
        field: Field,
        placeholder: &'static str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let draft = match field {
            Field::Term => &self.term,
            Field::RuleFrom => &self.rule_from,
            Field::RuleTo => &self.rule_to,
        };
        let id = match field {
            Field::Term => "vocab-term",
            Field::RuleFrom => "vocab-from",
            Field::RuleTo => "vocab-to",
        };
        text_field(
            id,
            &draft.focus,
            &draft.text,
            placeholder,
            draft.focus.is_focused(window),
        )
        .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
            this.field_key(field, ev, window, cx)
        }))
    }

    // ----- helpers --------------------------------------------------------------------------

    fn input_label(snap: &Snapshot, key: InputKey) -> String {
        match key {
            InputKey::Camera => snap
                .selected_camera
                .and_then(|id| snap.cameras.iter().find(|c| c.id == id))
                .map_or_else(|| "Camera".to_string(), |c| c.name.clone()),
            InputKey::Microphone => "Microphone".to_string(),
            InputKey::Source(id) => snap
                .sources
                .iter()
                .find(|s| s.id == id)
                .map_or_else(|| format!("Source {}", id.0), |s| s.label.clone()),
        }
    }

    /// "vah264enc · ≤50 fps" for a planned input.
    fn plan_text(p: &PlannedInput) -> String {
        format!(
            "{} · {}{} fps",
            p.encoder.element_name(),
            if p.native { "≤" } else { "" },
            format_fps(p.fps)
        )
    }

    /// The source has an input in the Recording segment that is still waiting for a frame.
    fn waiting_for_frame(snap: &Snapshot, id: SourceId) -> bool {
        snap.session.as_ref().is_some_and(|s| {
            s.segments
                .iter()
                .filter(|seg| seg.status == SegmentStatus::Recording)
                .flat_map(|seg| &seg.files)
                .any(|f| f.input == InputKey::Source(id) && f.waiting_for_frame)
        })
    }

    fn rate_picker(
        &self,
        id: &'static str,
        index: usize,
        current: FrameRateMode,
        source: Option<SourceId>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        segmented(
            FrameRateMode::ALL
                .into_iter()
                .map(|mode| {
                    let label = match mode {
                        FrameRateMode::Native => "Native",
                        FrameRateMode::Cfr60 => "60",
                        FrameRateMode::Cfr30 => "30",
                    };
                    let selected = mode == current;
                    let click = (enabled && !selected).then(|| {
                        self.send_on_click(EngineCommand::SetFrameRateMode { source, mode }, cx)
                    });
                    (
                        div().id((id, index * 4 + mode as usize)).child(label),
                        selected,
                        click,
                    )
                })
                .collect(),
        )
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
        } else if idle && snap.capacity.over {
            "Over the measured encoder capacity: see Sources".into()
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
                            .text_color(rgb(if idle && snap.capacity.over {
                                AMBER
                            } else {
                                MUTED
                            }))
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
        if kind == DeviceKind::Camera
            && let Some(cam) = selected.and_then(|id| devices.iter().find(|d| d.id == id))
        {
            list = list.child(self.camera_modes(cam, snap, cx));
        }
        list
    }

    /// Mode picker for the selected camera (P2-9): "Automatic" plus every advertised mode.
    fn camera_modes(&self, cam: &DeviceInfo, snap: &Snapshot, cx: &mut Context<Self>) -> Div {
        let locked = snap.phase.is_busy();
        let current = snap.selected_camera_mode.as_deref();
        let auto = cam.modes.iter().find(|m| m.auto);
        let mut list = div()
            .flex()
            .flex_col()
            .mt_3()
            .child(div().px_2().mb_2().child(caption("CAMERA MODE")));
        if cam.modes.is_empty() {
            return list.child(
                div()
                    .px_2()
                    .text_size(px(10.0))
                    .text_color(rgb(MUTED))
                    .child("No modes advertised"),
            );
        }
        list = list.child(radio_row(
            ("cam-mode", usize::MAX),
            "Automatic",
            auto.map(|m| m.label.clone().into()),
            None,
            current.is_none(),
            (!locked && current.is_some())
                .then(|| self.send_on_click(EngineCommand::SelectCameraMode(None), cx)),
        ));
        for (i, mode) in cam.modes.iter().enumerate() {
            let selected = current == Some(mode.label.as_str());
            list = list.child(radio_row(
                ("cam-mode", i),
                format!(
                    "{}×{} @{}",
                    mode.width,
                    mode.height,
                    format_rate(Fraction {
                        num: mode.fps_n,
                        den: mode.fps_d
                    })
                ),
                Some(mode.format.clone().into()),
                None,
                selected,
                (!locked && !selected).then(|| {
                    self.send_on_click(
                        EngineCommand::SelectCameraMode(Some(mode.label.clone())),
                        cx,
                    )
                }),
            ));
        }
        // 25/50 fps as the best rate usually means a PAL-region camera (Sony: NTSC/PAL switch).
        let best = cam
            .modes
            .iter()
            .filter(|m| m.fps_d > 0)
            .map(|m| m.fps_n as f64 / m.fps_d as f64)
            .fold(0.0, f64::max);
        if [25.0, 50.0].iter().any(|r| (best - r).abs() < 0.01) {
            list = list.child(
                div()
                    .px_2()
                    .mt_1()
                    .text_size(px(9.0))
                    .text_color(rgb(MUTED))
                    .child(
                        "Only 25/50 fps advertised: the camera is probably in PAL mode. \
                         Switch it to NTSC for 30/60 fps.",
                    ),
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

    /// Brings `shown` up to date with the newest frame of each previewed input.
    fn refresh_previews(&mut self, keys: &[InputKey], window: &mut Window) {
        let gone: Vec<InputKey> = self
            .shown
            .keys()
            .filter(|k| !keys.contains(k))
            .copied()
            .collect();
        for key in gone {
            if let Some(shown) = self.shown.remove(&key) {
                shown.release(window);
            }
        }
        for &key in keys {
            let Some((generation, frame)) = self.previews.latest(key) else {
                if let Some(shown) = self.shown.remove(&key) {
                    shown.release(window);
                }
                continue;
            };
            if self
                .shown
                .get(&key)
                .is_some_and(|s| s.generation == generation)
            {
                continue;
            }
            let Some(image) = render_image(&frame) else {
                continue;
            };
            match self.shown.get_mut(&key) {
                Some(shown) => {
                    if let Some(previous) = shown.previous.take()
                        && previous.id != shown.current.id
                    {
                        let _ = window.drop_image(previous);
                    }
                    shown.previous = Some(std::mem::replace(&mut shown.current, image));
                    shown.generation = generation;
                }
                None => {
                    self.shown.insert(
                        key,
                        Shown {
                            generation,
                            current: image,
                            previous: None,
                        },
                    );
                }
            }
        }
    }

    /// Live previews of the selected camera and every source (what each file will show).
    fn previews_section(&mut self, snap: &Snapshot, window: &mut Window) -> Option<Div> {
        let keys: Vec<InputKey> = snap.previews.iter().map(|(k, _)| *k).collect();
        self.refresh_previews(&keys, window);
        if keys.is_empty() {
            return None;
        }
        let recorded = |key: InputKey| {
            matches!(snap.phase, Phase::Recording { segment, .. }
            if snap.session.as_ref().is_some_and(|s| s.segments.iter().any(|g| {
                g.index == segment
                    && g.files.iter().any(|f| f.input == key && f.status == InputStatus::Recording)
            })))
        };
        let mut tiles = div().flex().flex_wrap().gap_3();
        for (key, state) in &snap.previews {
            tiles = tiles.child(preview_tile(
                Self::input_label(snap, *key),
                self.shown.get(key).map(|s| s.current.clone()),
                Some(state),
                recorded(*key),
            ));
        }
        Some(
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .text_size(px(16.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT))
                        .child("Preview"),
                )
                .child(tiles),
        )
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
                let extras = SourceRowExtras {
                    blocked: Self::source_blocked(snap, src),
                    waiting: Self::waiting_for_frame(snap, src.id),
                    plan: snap
                        .capacity
                        .inputs
                        .iter()
                        .find(|p| p.input == InputKey::Source(src.id))
                        .map(Self::plan_text),
                    rate_picker: Some(self.rate_picker(
                        "rate",
                        src.id.0 as usize,
                        src.frame_rate,
                        Some(src.id),
                        idle,
                        cx,
                    )),
                };
                group = group.child(source_row(src, extras, remove));
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
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(SECONDARY))
                            .child("Frame rate for all windows/screens"),
                    )
                    .child(self.rate_picker(
                        "rate-default",
                        0,
                        snap.frame_rate_default,
                        None,
                        idle,
                        cx,
                    ))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(MUTED))
                            .child(
                                "Native: variable rate up to the monitor refresh, frames only on change. \
                                 60/30: constant rate for editors that dislike VFR.",
                            ),
                    ),
            )
            .when_some(self.capacity_panel(snap), |d, p| d.child(p))
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

    /// The encoder plan for the current selection, with the over-capacity warning shown before
    /// recording (P1-5).
    fn capacity_panel(&self, snap: &Snapshot) -> Option<Div> {
        let plan = &snap.capacity;
        if plan.inputs.is_empty() && plan.warning.is_none() && plan.notes.is_empty() {
            return None;
        }
        let rows = plan.inputs.iter().map(|p| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .w(px(150.0))
                        .flex_shrink_0()
                        .text_size(px(11.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(SECONDARY))
                        .truncate()
                        .child(Self::input_label(snap, p.input)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(10.0))
                        .text_color(rgb(MUTED))
                        .truncate()
                        .child(format!(
                            "{} · {}×{} · {}{} fps{}",
                            p.encoder.element_name(),
                            p.width,
                            p.height,
                            if p.native { "≤" } else { "" },
                            format_fps(p.fps),
                            if p.native { " VFR" } else { "" },
                        )),
                )
                .child(
                    div()
                        .w(px(60.0))
                        .flex_shrink_0()
                        .text_size(px(10.0))
                        .text_color(rgb(SECONDARY))
                        .child(format!("{}%", p.load_pct)),
                )
        });
        let load_color = if plan.over { AMBER } else { MUTED };
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .rounded_lg()
                .border_1()
                .border_color(rgb(if plan.over { 0x5a3a1a } else { BORDER }))
                .bg(rgb(if plan.over { 0x2a1a12 } else { PANEL }))
                .px_3()
                .py_2()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .mb_1()
                        .child(caption("ENCODING PLAN"))
                        .child(div().text_size(px(10.0)).text_color(rgb(load_color)).child(
                            format!(
                                "video engine {}% · software {}%",
                                plan.gpu_load_pct, plan.cpu_load_pct
                            ),
                        )),
                )
                .children(rows)
                .when_some(plan.warning.clone(), |d, w| {
                    d.child(
                        div()
                            .mt_1()
                            .flex()
                            .items_start()
                            .gap_2()
                            .child(
                                div()
                                    .mt(px(4.0))
                                    .flex_shrink_0()
                                    .size(px(6.0))
                                    .rounded_full()
                                    .bg(rgb(AMBER)),
                            )
                            .child(div().text_size(px(11.0)).text_color(rgb(0xffd9a0)).child(w)),
                    )
                })
                .children(plan.notes.iter().map(|n| {
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(MUTED))
                        .child(n.clone())
                })),
        )
    }

    /// Glossary terms (the Whisper prompt) and replacement rules (applied after transcription).
    fn vocabulary_section(&self, snap: &Snapshot, window: &Window, cx: &mut Context<Self>) -> Div {
        let v = &snap.vocabulary;
        let muted = |text: String| div().text_size(px(10.0)).text_color(rgb(MUTED)).child(text);
        let terms = div()
            .flex()
            .flex_wrap()
            .gap_1()
            .children(v.terms.iter().enumerate().map(|(i, t)| {
                chip(
                    ("remove-term", i),
                    t.clone(),
                    Some(self.send_on_click(EngineCommand::RemoveVocabularyTerm(t.clone()), cx)),
                )
            }))
            .when(v.terms.is_empty(), |d| {
                d.child(muted("No terms yet.".into()))
            });
        let rules = div()
            .flex()
            .flex_col()
            .gap_1()
            .children(v.replacements.iter().enumerate().map(|(i, r)| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_size(px(11.0))
                            .child(div().text_color(rgb(SECONDARY)).child(r.from.clone()))
                            .child(div().text_color(rgb(MUTED)).child("→"))
                            .child(div().text_color(rgb(TEXT)).child(r.to.clone())),
                    )
                    .child(icon_button(
                        ("remove-rule", i),
                        "✕",
                        Some(self.send_on_click(
                            EngineCommand::RemoveReplacement {
                                from: r.from.clone(),
                            },
                            cx,
                        )),
                    ))
            }))
            .when(v.replacements.is_empty(), |d| {
                d.child(muted("No rules yet.".into()))
            });
        let add_term: OnClick = Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.submit(Field::Term, window, cx);
            cx.notify();
        }));
        let add_rule: OnClick = Box::new(cx.listener(|this, _: &ClickEvent, window, cx| {
            this.submit(Field::RuleTo, window, cx);
            cx.notify();
        }));
        let sub = |text: &'static str| {
            div()
                .text_size(px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(TEXT))
                .child(text)
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(caption("VOCABULARY"))
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
                    .child(sub("Glossary terms"))
                    .child(muted(
                        "Names and jargon Whisper should spell this way (sent as its prompt)."
                            .into(),
                    ))
                    .child(terms)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(self.field(Field::Term, "Add a term, e.g. Rawkode", window, cx))
                            .child(button(
                                "add-term",
                                "Add",
                                None,
                                ButtonStyle::Secondary,
                                Some(add_term),
                            )),
                    )
                    .child(div().h(px(1.0)).my_1().bg(rgb(0x1d222b)))
                    .child(sub("Replacement rules"))
                    .child(muted(
                        "Applied after transcription: whole words, any case. The original text stays in the JSON."
                            .into(),
                    ))
                    .child(rules)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(self.field(Field::RuleFrom, "Heard, e.g. raw code", window, cx))
                            .child(div().text_size(px(11.0)).text_color(rgb(MUTED)).child("→"))
                            .child(self.field(Field::RuleTo, "Write, e.g. Rawkode", window, cx))
                            .child(button(
                                "add-rule",
                                "Add",
                                None,
                                ButtonStyle::Secondary,
                                Some(add_rule),
                            )),
                    )
                    .children(v.warnings.iter().map(|w| {
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(AMBER))
                            .child(w.clone())
                    }))
                    .child(muted(format!(
                        "Saved to {} and {}. New transcripts use them; Re-transcribe updates finished segments.",
                        v.glossary_path.display(),
                        v.replacements_path.display()
                    ))),
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
                    transcribe: can_transcribe
                        .then(|| self.send_on_click(EngineCommand::Transcribe { segment }, cx)),
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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

        let previews = self.previews_section(&snap, window);
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
            .when_some(previews, |d, p| d.child(p))
            .child(self.sources_section(&snap, cx))
            .child(self.output_section(&snap))
            .child(self.transcription_section(&snap, cx))
            .child(self.segments_section(&snap, cx))
            .child(self.vocabulary_section(&snap, window, cx));

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
