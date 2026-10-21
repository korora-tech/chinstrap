//! `AppShell` (SPEC §4.3, §4.4): the tab bar, focus routing and close/quit coordination.

use std::time::Duration;

use gpui::{
    ClickEvent, Context, Entity, FocusHandle, Focusable, FontWeight, Subscription, Window, div,
    prelude::*, px, rgb,
};

use super::studio_view::{CloseIntent, CloseWindowNow, StudioView};
use super::theme::*;
use super::{Quit, ShowExplorer, ShowStudio};
use crate::explorer::FeatureExplorer;
use crate::studio::{EngineEvent, EngineHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Studio,
    Explorer,
}

pub struct AppShell {
    engine: EngineHandle,
    studio: Entity<StudioView>,
    explorer: Entity<FeatureExplorer>,
    screen: Screen,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl AppShell {
    pub fn new(
        engine: EngineHandle,
        events: async_channel::Receiver<EngineEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let studio = cx.new(|cx| StudioView::new(engine.clone(), events, window, cx));
        let explorer = cx.new(|_| FeatureExplorer::new());
        let close_sub = cx.subscribe_in(
            &studio,
            window,
            |_, _, _: &CloseWindowNow, window: &mut Window, _| window.remove_window(),
        );
        Self {
            engine,
            studio,
            explorer,
            screen: Screen::Studio,
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![close_sub],
        }
    }

    pub fn studio_focus(&self, cx: &gpui::App) -> FocusHandle {
        self.studio.focus_handle(cx)
    }

    /// on_window_should_close: if the studio is busy, request a Stop and keep the window open
    /// ("Finalizing…"); the studio closes it on the Idle Snapshot or after 10 s.
    pub fn install_close_handler(&self, window: &mut Window, cx: &mut Context<Self>) {
        let studio = self.studio.clone();
        window.on_window_should_close(cx, move |_window, cx| {
            if studio.read(cx).is_busy() {
                studio.update(cx, |s, cx| s.request_close(CloseIntent::CloseWindow, cx));
                false
            } else {
                true
            }
        });
    }

    fn show(&mut self, screen: Screen, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = screen;
        match screen {
            Screen::Studio => window.focus(&self.studio.focus_handle(cx), cx),
            Screen::Explorer => window.focus(&self.focus_handle, cx),
        }
        cx.notify();
    }

    fn quit(&mut self, _: &Quit, _window: &mut Window, cx: &mut Context<Self>) {
        if self.studio.read(cx).is_busy() {
            self.studio
                .update(cx, |s, cx| s.request_close(CloseIntent::QuitApp, cx));
        } else {
            // Idle: only portal sessions to close, bounded by the engine at 2 s.
            self.engine.shutdown_blocking(Duration::from_secs(3));
            cx.quit();
        }
    }

    fn tab(
        &self,
        id: &'static str,
        label: &'static str,
        keys: &'static str,
        screen: Screen,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = self.screen == screen;
        div()
            .id(id)
            .flex()
            .items_center()
            .gap_2()
            .rounded_md()
            .px_3()
            .py(px(5.0))
            .bg(rgb(if active { SELECTED } else { HEADER }))
            .border_1()
            .border_color(rgb(if active { SELECTED_BORDER } else { HEADER }))
            .hover(|s| s.bg(rgb(HOVER)).cursor_pointer())
            .on_click(
                cx.listener(move |this, _: &ClickEvent, window, cx| this.show(screen, window, cx)),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(if active {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::MEDIUM
                    })
                    .text_color(rgb(if active { TEXT } else { SECONDARY }))
                    .child(label),
            )
            .child(
                div()
                    .text_size(px(9.0))
                    .text_color(rgb(if active { ACCENT_TEXT } else { 0x6f7784 }))
                    .child(keys),
            )
    }
}

impl Render for AppShell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content: gpui::AnyView = match self.screen {
            Screen::Studio => self.studio.clone().into(),
            Screen::Explorer => self.explorer.clone().into(),
        };
        div()
            .key_context("Shell")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::quit))
            .on_action(
                cx.listener(|this, _: &ShowStudio, window, cx| {
                    this.show(Screen::Studio, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ShowExplorer, window, cx| {
                this.show(Screen::Explorer, window, cx)
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(
                div()
                    .h(px(48.0))
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(22.0))
                    .bg(rgb(HEADER))
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .size(px(26.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_lg()
                                    .bg(rgb(ACCENT))
                                    .text_sm()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(rgb(0x11111a))
                                    .child("C"),
                            )
                            .child(
                                div()
                                    .text_color(rgb(TEXT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("chinstrap"),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(rgb(MUTED))
                                    .child("RECORDING STUDIO"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(self.tab("tab-studio", "Studio", "Ctrl+1", Screen::Studio, cx))
                            .child(self.tab(
                                "tab-explorer",
                                "Feature guide",
                                "Ctrl+2",
                                Screen::Explorer,
                                cx,
                            )),
                    )
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
                            .child(div().size(px(6.0)).rounded_full().bg(rgb(GREEN)))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(rgb(0xb5bdc9))
                                    .child("Local by design"),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .child(content),
            )
    }
}
