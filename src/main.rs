// Moved verbatim from the old main.rs; the lint predates the move (SPEC §2: explorer unchanged).
#[allow(clippy::too_many_arguments)]
mod explorer;
mod selftest;
mod signal_shutdown;
mod studio;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use gpui::{App, Bounds, KeyBinding, WindowBounds, WindowOptions, prelude::*, px, size};
use gpui_platform::application;

use ui::{Cut, Quit, ShowExplorer, ShowStudio, ToggleRecord};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Transcription (SPEC §12.11), handled before anything else starts.
    match args.get(1).map(String::as_str) {
        Some("--transcribe-worker") => std::process::exit(studio::transcribe::run_worker()),
        Some("--transcribe") => std::process::exit(studio::transcribe::run_cli(&args[2..])),
        Some("--selftest-crash-child") | Some("--selftest-signal-child") => {
            let (Some(root), Some(ready)) = (args.get(2), args.get(3)) else {
                eprintln!("usage: chinstrap --selftest-(crash|signal)-child <root> <ready-file>");
                std::process::exit(2);
            };
            let result = if args[1] == "--selftest-crash-child" {
                selftest::crash_child(&PathBuf::from(root), &PathBuf::from(ready))
            } else {
                selftest::signal_child(&PathBuf::from(root), &PathBuf::from(ready))
            };
            std::process::exit(result);
        }
        Some("--download-model") => {
            let name = args.get(2).map(String::as_str);
            if args.len() > 3 || name.is_some_and(|n| n.starts_with('-')) {
                eprintln!("usage: chinstrap --download-model [name]");
                std::process::exit(2);
            }
            std::process::exit(studio::transcribe::download_cli(name));
        }
        _ => {}
    }
    let arg_value = |flag: &str| -> Option<Option<String>> {
        args.iter()
            .position(|a| a == flag)
            .map(|i| args.get(i + 1).cloned())
    };
    if let Some(v) = arg_value("--selftest") {
        let Some(dir) = v else {
            eprintln!("usage: chinstrap --selftest <dir>");
            std::process::exit(2)
        };
        std::process::exit(selftest::run(&PathBuf::from(dir)));
    }
    let mut config = studio::EngineConfig::from_env();
    if let Some(v) = arg_value("--output") {
        let Some(dir) = v else {
            eprintln!("usage: chinstrap --output <dir>");
            std::process::exit(2)
        };
        config.output_root = std::path::absolute(dir).expect("absolute output path");
    }
    let (engine, events) = studio::start(config).unwrap_or_else(|e| {
        eprintln!("chinstrap: {e}");
        std::process::exit(1)
    });
    if let Err(e) = signal_shutdown::install_handlers()
        .and_then(|_| signal_shutdown::spawn_shutdown_watcher(engine.clone()))
    {
        eprintln!("chinstrap: cannot install signal shutdown handlers: {e}");
        let _ = engine.shutdown_blocking(Duration::from_secs(8));
        std::process::exit(1);
    }

    application().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("r", ToggleRecord, Some("Studio && !TextField")),
            KeyBinding::new("c", Cut, Some("Studio && !TextField")),
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("ctrl-1", ShowStudio, None),
            KeyBinding::new("ctrl-2", ShowExplorer, None),
        ]);
        // Quit action: routed to AppShell (busy → request_close(QuitApp); idle → shutdown_blocking(3s); cx.quit()).
        // Safety net (documented exception to "UI never blocks"): only reached when the UI path didn't finalize.
        {
            let e = engine.clone();
            cx.on_app_quit(move |_| {
                e.shutdown_blocking(Duration::from_secs(8));
                async {}
            })
            .detach();
        }
        // Closing the last window ends the process (the quit hook above then finalizes).
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| {
                        ui::shell::AppShell::new(engine.clone(), events.clone(), window, cx)
                    })
                },
            )
            .expect("open Chinstrap window");
        window
            .update(cx, |shell, window, cx| {
                shell.install_close_handler(window, cx);
                window.focus(&shell.studio_focus(cx), cx);
            })
            .ok();
        cx.activate(true);
    });
}
