//! Headless transcription and explicit model-download CLI commands.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use super::driver::{DriverEvent, ExitClass, Running};
use super::model::{DlError, DlEvent, FileCheck, Resolved};
use super::proto::{BackendReq, Job, WorkerLine};
use super::{driver, model, prompt};

/// Result of one headless transcription.
#[derive(Clone, Debug)]
pub struct FileResult {
    pub json: PathBuf,
    pub srt: PathBuf,
    pub vtt: PathBuf,
    pub language: String,
    pub words: u32,
    pub backend: String,
    pub audio_s: f64,
    pub rtf: f64,
}

/// A headless CLI failure, kept separate from the process-level exit status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliError {
    pub exit_code: i32,
    pub message: String,
}

impl CliError {
    fn new(exit_code: i32, message: impl Into<String>) -> Self {
        Self {
            exit_code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

fn mib(b: u64) -> f64 {
    b as f64 / (1u64 << 20) as f64
}

/// Transcribes `media` through a worker child (with the single Vulkan-fault → CPU retry) and
/// writes the outputs next to it. Never touches a `session.json`.
pub fn transcribe_file(
    media: &Path,
    resolved: &Resolved,
    language: Option<String>,
    cpu: bool,
    quiet: bool,
) -> Result<FileResult, CliError> {
    let media = std::path::absolute(media).map_err(|e| CliError::new(2, e.to_string()))?;
    if !media.is_file() {
        return Err(CliError::new(
            1,
            format!("{}: no such file", media.display()),
        ));
    }
    let dir = model::models_dir();
    let (model_path, model_id, sha) = match resolved {
        Resolved::Catalog(spec) => match model::check_file(&dir, spec) {
            Ok(FileCheck::Ok) => (
                dir.join(spec.file),
                spec.name.to_string(),
                Some(spec.sha256.to_string()),
            ),
            Ok(FileCheck::Missing) => {
                return Err(CliError::new(
                    3,
                    format!(
                        "model {} is not downloaded: run `chinstrap --download-model`",
                        spec.name
                    ),
                ));
            }
            Ok(bad @ FileCheck::Quarantined(_)) => {
                return Err(CliError::new(
                    3,
                    format!(
                        "model {}: {}; run `chinstrap --download-model`",
                        spec.name,
                        bad.describe(spec)
                    ),
                ));
            }
            Err(e) => {
                return Err(CliError::new(
                    3,
                    format!("model {}: cannot check {}: {e}", spec.name, spec.file),
                ));
            }
        },
        Resolved::External { id, path } => {
            if !path.is_file() {
                return Err(CliError::new(
                    3,
                    format!("model {} not found", path.display()),
                ));
            }
            (path.clone(), id.clone(), None)
        }
    };
    let vad_model = match model::check_file(&dir, &model::VAD) {
        Ok(FileCheck::Ok) => Some(dir.join(model::VAD.file)),
        _ => {
            eprintln!(
                "chinstrap: VAD model missing (run `chinstrap --download-model`); silence may produce text"
            );
            None
        }
    };
    let mut job = Job {
        prompt_hint: prompt::session_hint_for_media(&media),
        media,
        model: model_path,
        model_id,
        model_sha256: sha,
        vad_model,
        language,
        backend: if cpu {
            BackendReq::Cpu
        } else {
            BackendReq::Auto
        },
        gpu_device: std::env::var("CHINSTRAP_WHISPER_GPU_DEVICE")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0),
    };
    for attempt in 0..2 {
        let cmd = driver::worker_command().map_err(|e| CliError::new(1, e.to_string()))?;
        let run = Running::spawn(cmd, &job, Some("whisper: "))
            .map_err(|e| CliError::new(1, format!("cannot start the worker: {e}")))?;
        let mut backend = String::new();
        let mut last_stage = String::new();
        let exit = run.wait(|ev| match ev {
            DriverEvent::Line(WorkerLine::Backend { backend: b, device }) => {
                backend = b.clone();
                if !quiet {
                    eprintln!("chinstrap: backend {b} ({device})");
                }
            }
            DriverEvent::Line(WorkerLine::Progress { stage, .. })
                if !quiet && *stage != last_stage =>
            {
                eprintln!("chinstrap: {stage}…");
                last_stage = stage.clone();
            }
            _ => {}
        });
        match (exit.class, exit.done) {
            (
                ExitClass::Ok,
                Some(WorkerLine::Done {
                    json,
                    srt,
                    vtt,
                    language,
                    words,
                    audio_s,
                    rtf,
                }),
            ) => {
                return Ok(FileResult {
                    json,
                    srt,
                    vtt,
                    language,
                    words,
                    backend,
                    audio_s,
                    rtf,
                });
            }
            (ExitClass::VulkanFault, _) if attempt == 0 => {
                eprintln!(
                    "chinstrap: Vulkan transcription failed ({}); retrying on CPU",
                    exit.reason
                );
                job.backend = BackendReq::Cpu;
            }
            (_, _) => return Err(CliError::new(1, exit.reason)),
        }
    }
    Err(CliError::new(1, "transcription failed"))
}

const USAGE: &str =
    "usage: chinstrap --transcribe <media file> [--model <path|name>] [--lang <code|auto>] [--cpu]";

/// `chinstrap --transcribe …` (args after the flag).
pub fn run_cli(args: &[String]) -> i32 {
    let mut media: Option<PathBuf> = None;
    let mut model_arg: Option<String> = std::env::var("CHINSTRAP_WHISPER_MODEL").ok();
    let mut lang: Option<String> = std::env::var("CHINSTRAP_WHISPER_LANG").ok();
    let mut cpu = std::env::var("CHINSTRAP_WHISPER_BACKEND")
        .is_ok_and(|v| v.trim().eq_ignore_ascii_case("cpu"));
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--model" => match it.next() {
                Some(v) => model_arg = Some(v.clone()),
                None => {
                    eprintln!("{USAGE}");
                    return 2;
                }
            },
            "--lang" => match it.next() {
                Some(v) => lang = Some(v.clone()),
                None => {
                    eprintln!("{USAGE}");
                    return 2;
                }
            },
            "--cpu" => cpu = true,
            s if s.starts_with("--") || media.is_some() => {
                eprintln!("{USAGE}");
                return 2;
            }
            s => media = Some(PathBuf::from(s)),
        }
    }
    let Some(media) = media else {
        eprintln!("{USAGE}");
        return 2;
    };
    let (resolved, warning) = model::resolve(model_arg.as_deref());
    if let Some(w) = warning {
        eprintln!("chinstrap: warning: {w}");
    }
    let language = lang
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty() && l != "auto");
    let quiet = std::env::var("CHINSTRAP_WHISPER_DEBUG").is_err();
    match transcribe_file(&media, &resolved, language, cpu, quiet) {
        Ok(r) => {
            println!(
                "{}",
                serde_json::json!({
                    "ok": true,
                    "json": r.json, "srt": r.srt, "vtt": r.vtt,
                    "language": r.language, "words": r.words, "backend": r.backend,
                    "audio_s": r.audio_s, "rtf": r.rtf,
                })
            );
            0
        }
        Err(error) => {
            let CliError { exit_code, message } = error;
            eprintln!("chinstrap: {message}");
            println!("{}", serde_json::json!({"ok": false, "error": message}));
            exit_code
        }
    }
}

/// `chinstrap --download-model [name]`: the explicit download (missing files only).
pub fn download_cli(name: Option<&str>) -> i32 {
    let value = name
        .map(String::from)
        .or_else(|| std::env::var("CHINSTRAP_WHISPER_MODEL").ok());
    let (resolved, warning) = model::resolve(value.as_deref());
    if let Some(w) = warning {
        eprintln!("chinstrap: warning: {w}");
        if name.is_some() {
            return 2;
        }
    }
    let Resolved::Catalog(spec) = resolved else {
        eprintln!("chinstrap: CHINSTRAP_WHISPER_MODEL is a custom path; nothing to download");
        return 2;
    };
    let dir = model::models_dir();
    if let Err(e) = model::ensure_dir(&dir) {
        eprintln!("chinstrap: cannot create {}: {e}", dir.display());
        return 1;
    }
    let files = model::needed_files(&dir, spec);
    if files.is_empty() {
        eprintln!(
            "chinstrap: {} and the VAD model are already downloaded and verified in {}",
            spec.name,
            dir.display()
        );
        return 0;
    }
    for f in &files {
        eprintln!(
            "chinstrap: downloading {} ({:.1} MiB) from {}",
            f.spec.file,
            mib(f.spec.bytes),
            f.url
        );
    }
    let cancel = AtomicBool::new(false);
    let mut last = Instant::now() - Duration::from_secs(10);
    let r = model::download(
        &dir,
        &files,
        &model::DlOpts::default(),
        &cancel,
        &mut |ev| match ev {
            DlEvent::Progress { received, total } => {
                if last.elapsed() >= Duration::from_secs(2) || received == total {
                    last = Instant::now();
                    eprintln!(
                        "chinstrap: {:.1} / {:.1} MiB ({:.0}%)",
                        mib(received),
                        mib(total),
                        received as f64 * 100.0 / total.max(1) as f64
                    );
                }
            }
            DlEvent::Verifying => eprintln!("chinstrap: verifying SHA-256…"),
        },
    );
    match r {
        Ok(()) => {
            eprintln!("chinstrap: verified; models are in {}", dir.display());
            0
        }
        Err(DlError::Cancelled) => 1,
        Err(DlError::Failed(reason)) => {
            eprintln!("chinstrap: download failed: {reason} (run again to resume)");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_error_has_named_exit_code_and_is_an_error() {
        let error = CliError {
            exit_code: 2,
            message: "bad input".into(),
        };
        assert_eq!(error.exit_code, 2);
        assert_eq!(error.to_string(), "bad input");

        fn assert_error<T: std::error::Error>() {}
        assert_error::<CliError>();
    }
}
