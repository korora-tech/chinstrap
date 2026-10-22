//! Stateless building blocks for `StudioView`. Disabled controls never get an `on_click`.

use std::time::Duration;

use gpui::{
    App, ClickEvent, Div, ElementId, FocusHandle, FontWeight, SharedString, Stateful, Window, div,
    prelude::*, px, relative, rgb,
};

use super::theme::*;
use crate::studio::{
    InputFile, InputQuality, InputStatus, Notice, SegmentStatus, SegmentSummary, Severity,
    SourceInfo, SourceKind, SourceState, TranscribeStage, TranscriptStatus, WhisperBackend,
};

pub type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    Primary,
    Danger,
    Secondary,
}

/// A pill button. `on_click: None` renders it disabled.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    hint: Option<&'static str>,
    style: ButtonStyle,
    on_click: Option<OnClick>,
) -> Stateful<Div> {
    let enabled = on_click.is_some();
    let (bg, fg, border) = match (style, enabled) {
        (_, false) => (PANEL, DISABLED, BORDER),
        (ButtonStyle::Primary, true) => (ACCENT, 0x11111a, ACCENT),
        (ButtonStyle::Danger, true) => (RED, 0x1a0d10, RED),
        (ButtonStyle::Secondary, true) => (PANEL_RAISED, TEXT, 0x303646),
    };
    let mut el = div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(border))
        .bg(rgb(bg))
        .px_3()
        .py(px(6.0))
        .text_size(px(12.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(fg))
        .child(label.into());
    if let Some(hint) = hint {
        el = el.child(
            div()
                .rounded_sm()
                .px(px(5.0))
                .text_size(px(9.0))
                .font_weight(FontWeight::BOLD)
                .bg(if enabled {
                    gpui::rgba(0x00000030)
                } else {
                    gpui::rgba(0x00000000)
                })
                .text_color(rgb(fg))
                .child(hint),
        );
    }
    match on_click {
        Some(handler) => el
            .hover(|s| s.opacity(0.88).cursor_pointer())
            .on_click(handler),
        None => el,
    }
}

/// A small text link-style button (used for ✕ and "Refresh").
pub fn icon_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on_click: Option<OnClick>,
) -> Stateful<Div> {
    let el = div()
        .id(id)
        .flex_shrink_0()
        .rounded_sm()
        .px(px(6.0))
        .py(px(2.0))
        .text_size(px(11.0))
        .text_color(rgb(if on_click.is_some() { MUTED } else { DISABLED }))
        .child(label.into());
    match on_click {
        Some(handler) => el
            .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)).cursor_pointer())
            .on_click(handler),
        None => el,
    }
}

/// Uppercase section caption, matching the explorer's "BROWSE LIBRARY" style.
pub fn caption(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(10.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(CAPTION))
        .child(text.into())
}

pub fn badge(text: impl Into<SharedString>, color: u32) -> Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(5.0))
        .rounded_full()
        .bg(rgb(PANEL_RAISED))
        .px_2()
        .py(px(2.0))
        .child(div().size(px(6.0)).rounded_full().bg(rgb(color)))
        .child(
            div()
                .text_size(px(10.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(color))
                .child(text.into()),
        )
}

/// One radio choice. `on_click: None` renders it disabled; `reason` is shown under the title.
pub fn radio_row(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    detail: Option<SharedString>,
    reason: Option<SharedString>,
    selected: bool,
    on_click: Option<OnClick>,
) -> Stateful<Div> {
    let enabled = on_click.is_some();
    let title_color = match (selected, enabled) {
        (true, _) => TEXT,
        (false, true) => SECONDARY,
        (false, false) => DISABLED,
    };
    let dot = div()
        .size(px(14.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_1()
        .border_color(rgb(if selected { ACCENT } else { 0x3a4150 }))
        .when(selected, |d| {
            d.child(div().size(px(6.0)).rounded_full().bg(rgb(ACCENT)))
        });
    let mut text = div().flex_1().min_w(px(0.0)).flex().flex_col().child(
        div()
            .text_size(px(11.0))
            .font_weight(if selected {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::MEDIUM
            })
            .text_color(rgb(title_color))
            .truncate()
            .child(title.into()),
    );
    if let Some(detail) = detail {
        text = text.child(
            div()
                .text_size(px(9.0))
                .text_color(rgb(if enabled || selected { MUTED } else { DISABLED }))
                .truncate()
                .child(detail),
        );
    }
    if let Some(reason) = reason {
        text = text.child(
            div()
                .text_size(px(9.0))
                .text_color(rgb(AMBER))
                .child(reason),
        );
    }
    let mut row = div()
        .id(id)
        .w_full()
        .flex()
        .items_center()
        .gap_2()
        .rounded_md()
        .px_2()
        .py(px(6.0))
        .mb(px(2.0))
        .border_1()
        .border_color(rgb(if selected { SELECTED_BORDER } else { SIDEBAR }))
        .bg(rgb(if selected { SELECTED } else { SIDEBAR }))
        .child(dot)
        .child(text);
    if let Some(handler) = on_click {
        row = row
            .hover(|s| s.bg(rgb(HOVER)).cursor_pointer())
            .on_click(handler);
    }
    row
}

/// A checkbox-style variant of `radio_row`: a square box with a tick when `checked`.
pub fn checkbox_row(
    id: impl Into<ElementId>,
    title: impl Into<SharedString>,
    checked: bool,
    on_click: Option<OnClick>,
) -> Stateful<Div> {
    let enabled = on_click.is_some();
    let bx = div()
        .size(px(14.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .border_1()
        .border_color(rgb(if checked { ACCENT } else { 0x3a4150 }))
        .bg(rgb(if checked { ACCENT } else { SIDEBAR }))
        .when(checked, |d| {
            d.child(
                div()
                    .text_size(px(10.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(rgb(0x11111a))
                    .child("✓"),
            )
        });
    let mut row = div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .rounded_md()
        .px_2()
        .py(px(6.0))
        .child(bx)
        .child(
            div()
                .text_size(px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(match (checked, enabled) {
                    (_, false) => DISABLED,
                    (true, true) => TEXT,
                    (false, true) => SECONDARY,
                }))
                .child(title.into()),
        );
    if let Some(handler) = on_click {
        row = row
            .hover(|s| s.bg(rgb(HOVER)).cursor_pointer())
            .on_click(handler);
    }
    row
}

/// A thin progress bar on the level-meter track; `fraction` is clamped to 0..1.
pub fn progress_bar(fraction: f32) -> Div {
    div()
        .relative()
        .h(px(6.0))
        .w_full()
        .rounded_full()
        .bg(rgb(PANEL_RAISED))
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .w(relative(fraction.clamp(0.0, 1.0)))
                .rounded_full()
                .bg(rgb(ACCENT)),
        )
}

/// Maps dBFS to a 0..1 meter fraction over a 60 dB range.
fn db_to_fraction(db: f32) -> f32 {
    if !db.is_finite() {
        return 0.0;
    }
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// Horizontal RMS bar with a peak tick. `None` renders an empty meter.
pub fn level_meter(level: Option<(f32, f32)>) -> Div {
    let (rms, peak) = level
        .map(|(r, p)| (db_to_fraction(r), db_to_fraction(p)))
        .unwrap_or((0.0, 0.0));
    let color = match level.map(|(_, p)| p) {
        Some(p) if p > -1.0 => RED,
        Some(p) if p > -9.0 => AMBER,
        _ => GREEN,
    };
    let label = match level {
        Some((r, p)) => format!("RMS {r:>5.1} dB · peak {p:>5.1} dB"),
        None => "No signal yet".to_string(),
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .relative()
                .h(px(8.0))
                .w_full()
                .rounded_full()
                .bg(rgb(PANEL_RAISED))
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .h_full()
                        .w(relative(rms))
                        .rounded_full()
                        .bg(rgb(color)),
                )
                .when(level.is_some(), |d| {
                    d.child(
                        div()
                            .absolute()
                            .top_0()
                            .left(relative(peak))
                            .h_full()
                            .w(px(2.0))
                            .bg(rgb(TEXT)),
                    )
                }),
        )
        .child(div().text_size(px(9.0)).text_color(rgb(MUTED)).child(label))
}

fn source_kind_tag(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::Monitor => "SCR",
        SourceKind::Window => "WIN",
        SourceKind::Virtual => "VIR",
        SourceKind::TestPattern => "TST",
    }
}

/// Formats a rational rate: "144", "29.97".
pub fn format_rate(n: i32, d: i32) -> String {
    if d <= 0 {
        return "?".into();
    }
    format_fps(n as f64 / d as f64)
}

/// "60", "29.97", "143.86" without trailing zeros.
pub fn format_fps(fps: f64) -> String {
    let s = format!("{fps:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A row of pill choices (frame-rate modes). Options without a click are disabled.
pub fn segmented(options: Vec<(Stateful<Div>, bool, Option<OnClick>)>) -> Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(2.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(SIDEBAR))
        .p(px(2.0))
        .children(options.into_iter().map(|(el, selected, click)| {
            let enabled = click.is_some();
            let el = el
                .rounded_sm()
                .px(px(7.0))
                .py(px(2.0))
                .text_size(px(10.0))
                .font_weight(if selected {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::MEDIUM
                })
                .text_color(rgb(match (selected, enabled) {
                    (true, _) => ACCENT_TEXT,
                    (false, true) => SECONDARY,
                    (false, false) => DISABLED,
                }))
                .when(selected, |d| d.bg(rgb(SELECTED)));
            match click {
                Some(handler) => el
                    .hover(|s| s.bg(rgb(HOVER)).cursor_pointer())
                    .on_click(handler),
                None => el,
            }
        }))
}

/// A single-line text field drawn by hand (GPUI has no stock input). The caller attaches the
/// key handler; `focused` shows the caret and the accent border.
pub fn text_field(
    id: impl Into<ElementId>,
    focus: &FocusHandle,
    value: &str,
    placeholder: &'static str,
    focused: bool,
) -> Stateful<Div> {
    let empty = value.is_empty();
    div()
        .id(id)
        .key_context("TextField")
        .track_focus(focus)
        .flex_1()
        .min_w(px(80.0))
        .flex()
        .items_center()
        .h(px(28.0))
        .rounded_md()
        .border_1()
        .border_color(rgb(if focused { SELECTED_BORDER } else { BORDER }))
        .bg(rgb(if focused { PANEL_RAISED } else { SIDEBAR }))
        .px_2()
        .overflow_hidden()
        .cursor_text()
        .text_size(px(11.0))
        .child(
            div()
                .min_w(px(0.0))
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis_start()
                .text_color(rgb(if empty { DISABLED } else { TEXT }))
                .child(if empty && !focused {
                    placeholder.to_string()
                } else {
                    value.to_string()
                }),
        )
        .when(focused, |d| {
            d.child(div().flex_shrink_0().w(px(1.5)).h(px(14.0)).bg(rgb(ACCENT)))
        })
}

/// A removable chip ("Rawkode ✕").
pub fn chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    on_remove: Option<OnClick>,
) -> Div {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_1()
        .rounded_full()
        .border_1()
        .border_color(rgb(0x303646))
        .bg(rgb(PANEL_RAISED))
        .pl_2()
        .pr(px(2.0))
        .py(px(1.0))
        .child(
            div()
                .text_size(px(11.0))
                .text_color(rgb(TEXT))
                .child(label.into()),
        )
        .child(icon_button(id, "✕", on_remove))
}

/// What a source row shows besides the `SourceInfo` itself.
pub struct SourceRowExtras {
    /// Why a Ready source still cannot record.
    pub blocked: Option<String>,
    /// Recording, but no frame has arrived yet.
    pub waiting: bool,
    /// Planned rate/encoder ("vah264enc · ≤50 fps"), appended to the detail line.
    pub plan: Option<String>,
    /// The frame-rate picker, when the source has one.
    pub rate_picker: Option<Div>,
}

/// One source row.
pub fn source_row(info: &SourceInfo, extras: SourceRowExtras, on_remove: Option<OnClick>) -> Div {
    let lost = matches!(info.state, SourceState::Lost { .. });
    let (state_text, state_color, reason): (&str, u32, Option<String>) = match &info.state {
        SourceState::Checking => ("Checking…", AMBER, None),
        SourceState::Ready if extras.waiting => ("Waiting for first frame", AMBER, None),
        SourceState::Ready => match extras.blocked {
            Some(why) => ("Not recordable", AMBER, Some(why)),
            None => ("Ready", GREEN, None),
        },
        SourceState::Unsupported { reason } => ("Unsupported", AMBER, Some(reason.clone())),
        SourceState::Lost { reason } => ("Lost", RED, Some(reason.clone())),
    };
    let mut detail = match info.size.or(info.logical_size) {
        Some((w, h)) => format!("{w}×{h}"),
        None => match info.kind {
            SourceKind::TestPattern => "videotestsrc · 1280×720".to_string(),
            _ => "size unknown".to_string(),
        },
    };
    if let Some((n, d)) = info.refresh {
        detail = format!("{detail} · {} Hz", format_rate(n, d));
    }
    if let Some(plan) = extras.plan {
        detail = format!("{detail} · {plan}");
    }
    if let Some(reason) = reason {
        detail = format!("{detail} · {reason}");
    }
    div()
        .w_full()
        .flex()
        .items_center()
        .gap_3()
        .rounded_lg()
        .border_1()
        .border_color(rgb(if lost { 0x5a2630 } else { BORDER }))
        .bg(rgb(if lost { 0x22141a } else { PANEL }))
        .px_3()
        .py_2()
        .child(
            div()
                .size(px(30.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .bg(rgb(PANEL_RAISED))
                .text_size(px(9.0))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(if lost { RED } else { ACCENT }))
                .child(source_kind_tag(info.kind)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(if lost { RED } else { TEXT }))
                        .child(info.label.clone()),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(MUTED))
                        .truncate()
                        .child(detail),
                ),
        )
        .when_some(extras.rate_picker, |d, p| d.child(p))
        .child(badge(state_text, state_color))
        .child(icon_button(
            ("remove-source", info.id.0 as usize),
            "✕",
            on_remove,
        ))
}

pub fn format_duration(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn format_gap(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// One-line quality summary for a video input, and whether anything in it deserves attention
/// (repeats over 1 %, any dropped frame).
pub fn quality_summary(q: &InputQuality) -> (String, bool) {
    let mut parts: Vec<String> = Vec::new();
    if let Some(enc) = q.encoder {
        parts.push(enc.element_name().to_string());
    }
    parts.push(match (q.nominal_fps, q.max_fps) {
        (Some(f), _) => format!("{} fps CFR", format_fps(f)),
        (None, Some(m)) => format!("VFR ≤{} fps", format_fps(m)),
        (None, None) => "VFR".to_string(),
    });
    parts.push(format!("{:.1} fps effective", q.effective_fps));
    parts.push(format!("{:.1}% repeated", q.repeat_pct));
    let drops = q.dropped_queue + q.dropped_corrupt;
    parts.push(if q.dropped_corrupt > 0 {
        format!("{drops} dropped ({} corrupt)", q.dropped_corrupt)
    } else {
        format!("{drops} dropped")
    });
    parts.push(if q.bitrate_kbps >= 1000 {
        format!("{:.1} Mb/s", q.bitrate_kbps as f64 / 1000.0)
    } else {
        format!("{} kb/s", q.bitrate_kbps)
    });
    if let Some(off) = q.start_offset {
        parts.push(format!("starts +{:.2} s", off.as_secs_f64()));
    }
    (parts.join(" · "), q.repeat_pct > 1.0 || drops > 0)
}

fn file_line(file: &InputFile) -> Div {
    let quality = file.quality.as_ref().map(quality_summary);
    let (status, color, reason) = match &file.status {
        InputStatus::Starting => ("starting", AMBER, None),
        InputStatus::Recording if file.waiting_for_frame => ("waiting", AMBER, None),
        InputStatus::Recording => ("recording", RED, None),
        InputStatus::Finalizing => ("finalizing", AMBER, None),
        InputStatus::Complete => ("complete", GREEN, None),
        InputStatus::Failed { reason } => ("failed", RED, Some(reason.clone())),
    };
    div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .py(px(4.0))
        .border_t_1()
        .border_color(rgb(0x1d222b))
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .w(px(110.0))
                        .flex_shrink_0()
                        .text_size(px(11.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(rgb(SECONDARY))
                        .truncate()
                        .child(file.label.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(10.0))
                        .text_color(rgb(MUTED))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis_start()
                        .child(file.path.display().to_string()),
                )
                .child(
                    div()
                        .w(px(70.0))
                        .flex_shrink_0()
                        .text_size(px(10.0))
                        .text_color(rgb(SECONDARY))
                        .child(format_bytes(file.bytes)),
                )
                .child(
                    div()
                        .w(px(70.0))
                        .flex_shrink_0()
                        .text_size(px(10.0))
                        .text_color(rgb(color))
                        .child(status),
                ),
        )
        .when(file.waiting_for_frame, |d| {
            d.child(
                div()
                    .pl(px(122.0))
                    .text_size(px(10.0))
                    .text_color(rgb(AMBER))
                    .child("Waiting for first frame"),
            )
        })
        .when_some(quality, |d, (text, warn)| {
            d.child(
                div()
                    .pl(px(122.0))
                    .text_size(px(10.0))
                    .text_color(rgb(if warn { AMBER } else { MUTED }))
                    .child(text),
            )
        })
        .when_some(reason, |d, reason| {
            d.child(
                div()
                    .pl(px(122.0))
                    .text_size(px(10.0))
                    .text_color(rgb(RED))
                    .child(reason),
            )
        })
}

/// Per-segment transcript actions. `None` hides (or disables) the matching control.
#[derive(Default)]
pub struct SegmentActions {
    /// "Transcribe" (no transcript), "Retry" (failed) and "Re-transcribe" (done: uses the
    /// current vocabulary).
    pub transcribe: Option<OnClick>,
    /// "Cancel" while queued or transcribing.
    pub cancel: Option<OnClick>,
    /// Reveal the SRT file once done.
    pub open: Option<OnClick>,
}

fn backend_label(b: WhisperBackend) -> &'static str {
    match b {
        WhisperBackend::Vulkan => "Vulkan",
        WhisperBackend::Cpu => "CPU",
    }
}

fn transcript_line(index: u32, status: &TranscriptStatus, actions: SegmentActions) -> Div {
    let id = index as usize;
    let mut row = div()
        .flex()
        .items_center()
        .gap_2()
        .pt(px(4.0))
        .border_t_1()
        .border_color(rgb(0x1d222b))
        .child(
            div()
                .w(px(110.0))
                .flex_shrink_0()
                .text_size(px(11.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(SECONDARY))
                .child("Transcript"),
        );
    let cancel = |a: Option<OnClick>| icon_button(("cancel-transcript", id), "Cancel", a);
    match status {
        TranscriptStatus::None => {
            row = match actions.transcribe {
                Some(click) => {
                    row.child(icon_button(("transcribe", id), "Transcribe", Some(click)))
                }
                None => row.child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(MUTED))
                        .child("No transcript"),
                ),
            };
        }
        TranscriptStatus::Queued { deferred } => {
            row = row
                .child(badge(
                    if *deferred {
                        "Queued · after recording"
                    } else {
                        "Queued"
                    },
                    MUTED,
                ))
                .child(cancel(actions.cancel));
        }
        TranscriptStatus::Running {
            stage,
            percent,
            paused,
        } => {
            let label = match stage {
                TranscribeStage::Starting => "Starting…".to_string(),
                TranscribeStage::Decoding => "Decoding…".to_string(),
                _ => format!("Transcribing {percent}%"),
            };
            row = row
                .child(badge(label, ACCENT))
                .when(*paused, |d| d.child(badge("Paused", AMBER)))
                .child(cancel(actions.cancel));
        }
        TranscriptStatus::Done {
            srt,
            language,
            words,
            backend,
            ..
        } => {
            let name = srt
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let detail = format!(
                "{name} · {language} · {words} words · {}",
                backend_label(*backend)
            );
            let mut link = div()
                .id(("open-transcript", id))
                .min_w(px(0.0))
                .text_size(px(10.0))
                .text_color(rgb(SECONDARY))
                .truncate()
                .child(detail);
            if let Some(open) = actions.open {
                link = link
                    .hover(|s| s.text_color(rgb(ACCENT_TEXT)).cursor_pointer())
                    .on_click(open);
            }
            row = row
                .child(badge("Transcript", GREEN))
                .child(link)
                .child(div().flex_1())
                .child(icon_button(
                    ("retranscribe", id),
                    "Re-transcribe",
                    actions.transcribe,
                ));
        }
        TranscriptStatus::Failed { reason } => {
            let first = reason.lines().next().unwrap_or_default().to_string();
            row = row
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(px(10.0))
                        .text_color(rgb(RED))
                        .truncate()
                        .child(format!("Failed: {first}")),
                )
                .child(icon_button(
                    ("retry-transcript", id),
                    "Retry",
                    actions.transcribe,
                ));
        }
    }
    row
}

pub fn segment_row(seg: &SegmentSummary, actions: SegmentActions) -> Div {
    let (status, color, reason) = match &seg.status {
        SegmentStatus::Recording => ("Recording", RED, None),
        SegmentStatus::Complete => ("Complete", GREEN, None),
        SegmentStatus::Incomplete { reason } => ("Incomplete", AMBER, Some(reason.clone())),
    };
    let duration = seg
        .duration
        .map(format_duration)
        .unwrap_or_else(|| "—".to_string());
    let gap = seg
        .gap_before
        .map(format_gap)
        .unwrap_or_else(|| "—".to_string());
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .rounded_lg()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
        .px_3()
        .py_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .text_size(px(12.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT))
                        .child(format!("Segment {:03}", seg.index)),
                )
                .child(badge(status, color))
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(MUTED))
                        .child(format!("duration {duration} · gap before {gap}")),
                ),
        )
        .when_some(reason, |d, reason| {
            d.child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(AMBER))
                    .child(reason),
            )
        })
        .children(seg.files.iter().map(file_line))
        .child(transcript_line(seg.index, &seg.transcript, actions))
}

pub fn notice_row(index: usize, notice: &Notice, on_dismiss: OnClick) -> Div {
    let (tag, color) = match notice.severity {
        Severity::Info => ("INFO", 0x61b6f0),
        Severity::Warning => ("WARN", AMBER),
        Severity::Error => ("ERROR", RED),
    };
    div()
        .w_full()
        .flex()
        .items_start()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(PANEL))
        .p_2()
        .child(
            div()
                .flex_shrink_0()
                .mt(px(1.0))
                .text_size(px(9.0))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(color))
                .child(tag),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(TEXT))
                        .child(notice.message.clone()),
                )
                .when_some(notice.detail.clone(), |d, detail| {
                    d.child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(MUTED))
                            .child(detail),
                    )
                }),
        )
        .child(icon_button(
            ("dismiss-notice", index),
            "✕",
            Some(on_dismiss),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(format_duration(Duration::from_secs(3723)), "01:02:03");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(db_to_fraction(-60.0), 0.0);
        assert_eq!(db_to_fraction(0.0), 1.0);
        assert_eq!(db_to_fraction(f32::NEG_INFINITY), 0.0);
        assert_eq!(format_rate(144, 1), "144");
        assert_eq!(format_rate(30000, 1001), "29.97");
        assert_eq!(format_fps(25.0), "25");
    }

    #[test]
    fn quality_line() {
        let q = InputQuality {
            nominal_fps: Some(25.0),
            effective_fps: 24.96,
            repeat_pct: 0.07,
            bitrate_kbps: 41_300,
            start_offset: Some(Duration::from_millis(280)),
            ..Default::default()
        };
        let (text, warn) = quality_summary(&q);
        assert_eq!(
            text,
            "25 fps CFR · 25.0 fps effective · 0.1% repeated · 0 dropped · 41.3 Mb/s · starts +0.28 s"
        );
        assert!(!warn);
        let q = InputQuality {
            max_fps: Some(144.0),
            repeat_pct: 3.0,
            dropped_corrupt: 2,
            ..Default::default()
        };
        let (text, warn) = quality_summary(&q);
        assert!(text.starts_with("VFR ≤144 fps"), "{text}");
        assert!(text.contains("2 dropped (2 corrupt)"), "{text}");
        assert!(warn);
    }
}
