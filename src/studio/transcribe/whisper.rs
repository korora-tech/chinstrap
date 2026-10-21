//! `chinstrap --transcribe-worker` (§12.3): the only process that runs ggml.
//!
//! Start-up order: PDEATHSIG, parent check, nice 10, CPU feature check, then one `Job` from
//! stdin. Output is NDJSON `WorkerLine`s on stdout. Never call `whisper_rs::vulkan::list_devices`
//! (it aborts the process on a broken ICD, T3) and never `set_abort_callback_safe` (T2).

use std::ffi::{CStr, c_char, c_void};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use whisper_rs::whisper_rs_sys as sys;
use whisper_rs::{
    DtwMode, DtwModelPreset, DtwParameters, FullParams, SamplingStrategy, WhisperContext,
    WhisperContextParameters, WhisperVadContext, WhisperVadContextParams, WhisperVadParams,
};

use super::output::{self, RawSegment, RawToken, Span, Transcript, r3};
use super::proto::{self, BackendReq, Job, WorkerLine};
use super::{decode, prompt};
use crate::studio::session;
use crate::studio::types::WhisperBackend;

const WHISPER_RS_VERSION: &str = "0.16.0";

static GPU_INIT_FAILED: AtomicBool = AtomicBool::new(false);
static LOG_DEBUG: AtomicBool = AtomicBool::new(false);
static LAST_LOG_SHOWN: AtomicBool = AtomicBool::new(false);

fn emit(line: &WorkerLine) {
    let Ok(mut s) = serde_json::to_string(line) else {
        return;
    };
    s.push('\n');
    let out = std::io::stdout();
    let mut lock = out.lock();
    let _ = lock.write_all(s.as_bytes());
    let _ = lock.flush();
}

fn progress(stage: &str, percent: u8) {
    emit(&WorkerLine::Progress {
        stage: stage.into(),
        percent: percent.min(100),
    });
}

unsafe extern "C" fn log_callback(level: sys::ggml_log_level, text: *const c_char, _: *mut c_void) {
    if text.is_null() {
        return;
    }
    // SAFETY: ggml passes a NUL-terminated string valid for the duration of the call.
    let s = unsafe { CStr::from_ptr(text) }.to_string_lossy();
    if s.contains("no GPU found") || s.contains("failed to initialize") {
        GPU_INIT_FAILED.store(true, Ordering::Relaxed);
    }
    let show = if level == sys::ggml_log_level_GGML_LOG_LEVEL_CONT {
        LAST_LOG_SHOWN.load(Ordering::Relaxed)
    } else {
        LOG_DEBUG.load(Ordering::Relaxed)
            || level == sys::ggml_log_level_GGML_LOG_LEVEL_WARN
            || level == sys::ggml_log_level_GGML_LOG_LEVEL_ERROR
    };
    LAST_LOG_SHOWN.store(show, Ordering::Relaxed);
    if show {
        eprint!("{s}");
    }
}

fn install_log_hooks() {
    LOG_DEBUG.store(
        std::env::var("CHINSTRAP_WHISPER_DEBUG").is_ok_and(|v| v.trim() == "1"),
        Ordering::Relaxed,
    );
    // SAFETY: registering a plain extern "C" function with no user data.
    unsafe {
        sys::whisper_log_set(Some(log_callback), std::ptr::null_mut());
        sys::ggml_log_set(Some(log_callback), std::ptr::null_mut());
    }
}

/// §12.10: this build needs x86-64-v3.
fn cpu_check() -> Result<(), String> {
    #[cfg(target_arch = "x86_64")]
    {
        let ok = std::is_x86_feature_detected!("avx2")
            && std::is_x86_feature_detected!("fma")
            && std::is_x86_feature_detected!("f16c")
            && std::is_x86_feature_detected!("bmi2");
        if !ok {
            return Err("cpu-unsupported: this build needs x86-64-v3 (AVX2/FMA/F16C)".into());
        }
    }
    Ok(())
}

pub fn run_worker() -> i32 {
    // 1. Die with the spawning thread of the parent.
    // SAFETY: prctl/getppid/setpriority are plain syscalls on this process.
    unsafe {
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGKILL as libc::c_ulong,
            0,
            0,
            0,
        );
    }
    // 2. The parent may already be gone (the signal would then never come).
    let expected: Option<i32> = std::env::var("CHINSTRAP_WORKER_PARENT")
        .ok()
        .and_then(|v| v.trim().parse().ok());
    let ppid = unsafe { libc::getppid() };
    if expected != Some(ppid) {
        eprintln!(
            "chinstrap: --transcribe-worker is internal (parent {ppid}, expected {expected:?})"
        );
        return 1;
    }
    // 3. Lower priority for every thread created from here on.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
    // 4. CPU features before touching ggml.
    if let Err(reason) = cpu_check() {
        emit(&WorkerLine::Error { reason });
        return 1;
    }
    // 5. One job.
    let mut line = String::new();
    if let Err(e) = std::io::stdin().lock().read_line(&mut line) {
        emit(&WorkerLine::Error {
            reason: format!("cannot read the job: {e}"),
        });
        return 1;
    }
    let job: Job = match serde_json::from_str(line.trim()) {
        Ok(j) => j,
        Err(e) => {
            emit(&WorkerLine::Error {
                reason: format!("bad job: {e}"),
            });
            return 1;
        }
    };
    // 6. Run.
    match run_job(&job) {
        Ok(done) => {
            emit(&done);
            0
        }
        Err(reason) => {
            emit(&WorkerLine::Error { reason });
            1
        }
    }
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: ggml returns NUL-terminated static strings.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().to_string()
}

/// The ggml whisper hparams header (`ggml` magic, then 11 little-endian i32s: n_vocab,
/// n_audio_ctx, n_audio_state, n_audio_head, n_audio_layer, n_text_ctx, n_text_state,
/// n_text_head, n_text_layer, n_mels, ftype).
fn read_hparams(path: &std::path::Path) -> Option<[i32; 11]> {
    use std::io::Read as _;
    let mut buf = [0u8; 48];
    std::fs::File::open(path).ok()?.read_exact(&mut buf).ok()?;
    if u32::from_le_bytes(buf[..4].try_into().ok()?) != 0x6767_6d6c {
        return None;
    }
    let mut h = [0i32; 11];
    for (i, v) in h.iter_mut().enumerate() {
        *v = i32::from_le_bytes(buf[4 + i * 4..8 + i * 4].try_into().ok()?);
    }
    Some(h)
}

/// large-v3-turbo: 51866 tokens, 128 mels, 32 audio layers of 1280, 4 text layers of 1280.
fn hparams_are_large_v3_turbo(h: &[i32; 11]) -> bool {
    h[0] == 51866 && h[2] == 1280 && h[4] == 32 && h[6] == 1280 && h[8] == 4 && h[9] == 128
}

fn is_large_v3_turbo(path: &std::path::Path) -> bool {
    read_hparams(path).is_some_and(|h| hparams_are_large_v3_turbo(&h))
}

/// Which backend whisper actually picked, mirroring `whisper_backend_init_gpu` (T3). Only
/// called after the context exists, so the ggml registry is initialised.
fn detect_backend(req: BackendReq, gpu_device: i32) -> (WhisperBackend, String) {
    let mut cpu = None;
    let mut gpu = None;
    // SAFETY: read-only queries of the ggml backend registry.
    unsafe {
        let mut cnt = 0;
        for i in 0..sys::ggml_backend_dev_count() {
            let dev = sys::ggml_backend_dev_get(i);
            let ty = sys::ggml_backend_dev_type(dev);
            let desc = cstr(sys::ggml_backend_dev_description(dev));
            if ty == sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_CPU {
                cpu.get_or_insert(desc);
            } else if ty == sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU
                || ty == sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU
            {
                if cnt == gpu_device {
                    gpu = Some(desc);
                }
                cnt += 1;
            }
        }
    }
    match gpu {
        Some(g) if req == BackendReq::Auto && !GPU_INIT_FAILED.load(Ordering::Relaxed) => {
            (WhisperBackend::Vulkan, g)
        }
        _ => (WhisperBackend::Cpu, cpu.unwrap_or_else(|| "CPU".into())),
    }
}

/// Merges tokens whose bytes are an incomplete UTF-8 sequence with the following ones.
fn raw_tokens(seg: &whisper_rs::WhisperSegment) -> Vec<RawToken> {
    let mut out = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    let mut head: Option<whisper_rs::WhisperTokenData> = None;
    for i in 0..seg.n_tokens() {
        let Some(tok) = seg.get_token(i) else {
            continue;
        };
        let d = tok.token_data();
        let bytes = tok.to_bytes().unwrap_or_default();
        if head.is_none() {
            head = Some(d);
        }
        pending.extend_from_slice(bytes);
        let complete = std::str::from_utf8(&pending).is_ok() || pending.len() > 16;
        if complete && let Some(h) = head.take() {
            out.push(RawToken {
                id: h.id,
                text: String::from_utf8_lossy(&pending).to_string(),
                p: h.p,
                t0: h.t0,
                t_dtw: h.t_dtw,
            });
            pending.clear();
        }
    }
    if let Some(h) = head {
        out.push(RawToken {
            id: h.id,
            text: String::from_utf8_lossy(&pending).to_string(),
            p: h.p,
            t0: h.t0,
            t_dtw: h.t_dtw,
        });
    }
    out
}

fn run_job(job: &Job) -> Result<WorkerLine, String> {
    let t_total = Instant::now();
    let started = session::wall_now();
    progress(proto::STAGE_STARTING, 0);
    install_log_hooks();

    let (language, language_source) = match job.language.as_deref().map(str::trim) {
        Some(l) if !l.is_empty() && l != "auto" => {
            if whisper_rs::get_lang_id(l).is_some() {
                (Some(l.to_string()), "override")
            } else {
                eprintln!("chinstrap: unknown language {l:?}; auto-detecting");
                (None, "detected")
            }
        }
        _ => (None, "detected"),
    };

    // Model (Vulkan unless CPU was requested; whisper falls back to CPU by itself).
    let model_str = job
        .model
        .to_str()
        .ok_or("model path is not UTF-8")?
        .to_string();
    // The DTW alignment heads are architecture-specific: key them off the file's header,
    // never its name (a custom CHINSTRAP_WHISPER_MODEL can be called anything).
    let dtw = is_large_v3_turbo(&job.model);
    if !dtw {
        eprintln!("chinstrap: model is not large-v3-turbo; using legacy word timestamps");
    }
    let mut cp = WhisperContextParameters::default();
    cp.use_gpu(job.backend == BackendReq::Auto)
        .gpu_device(job.gpu_device)
        .flash_attn(false);
    if dtw {
        cp.dtw_parameters(DtwParameters {
            mode: DtwMode::ModelPreset {
                model_preset: DtwModelPreset::LargeV3Turbo,
            },
            dtw_mem_size: 128 << 20,
        });
    }
    let ctx = WhisperContext::new_with_params(&model_str, cp)
        .map_err(|e| format!("cannot load model {}: {e}", job.model.display()))?;
    let (backend, device) = detect_backend(job.backend, job.gpu_device);
    emit(&WorkerLine::Backend {
        backend: proto::backend_name(backend).into(),
        device: device.clone(),
    });
    // Simulates a Vulkan-only crash: a CPU run (the retry) is not affected.
    if cfg!(debug_assertions)
        && backend == WhisperBackend::Vulkan
        && std::env::var("CHINSTRAP_WORKER_TEST_ABORT").is_ok_and(|v| v == "after_backend")
    {
        eprintln!("ggml_vulkan: test hook: aborting after the backend line");
        std::process::abort();
    }

    // Decode.
    progress(proto::STAGE_DECODING, 0);
    let t = Instant::now();
    let decoded = decode::decode(&job.media, None, |f| {
        progress(proto::STAGE_DECODING, (f * 10.0) as u8)
    })?;
    let decode_s = t.elapsed().as_secs_f64();
    let pcm = decoded.pcm;
    if pcm.is_empty() {
        return Err("no audio decoded".into());
    }
    let audio_s = pcm.len() as f64 / decode::RATE as f64;

    // VAD (CPU; tiny).
    progress(proto::STAGE_DETECTING, 10);
    let t = Instant::now();
    let speech: Option<Vec<Span>> = match &job.vad_model {
        Some(vp) => {
            let mut vp_params = WhisperVadContextParams::new();
            vp_params.set_use_gpu(false);
            vp_params.set_n_threads(4);
            let vad_path = vp.to_str().ok_or("VAD path is not UTF-8")?;
            let mut vctx = WhisperVadContext::new(vad_path, vp_params)
                .map_err(|e| format!("cannot load VAD model: {e}"))?;
            let segs = vctx
                .segments_from_samples(WhisperVadParams::new(), &pcm)
                .map_err(|e| format!("VAD failed: {e}"))?;
            Some(
                (0..segs.num_segments())
                    .filter_map(|i| {
                        Some(Span {
                            start: r3(segs.get_segment_start_timestamp(i)? as f64 / 100.0),
                            end: r3(segs.get_segment_end_timestamp(i)? as f64 / 100.0),
                        })
                    })
                    .collect(),
            )
        }
        None => None,
    };
    let vad_s = t.elapsed().as_secs_f64();
    progress(proto::STAGE_DETECTING, 12);

    // Prompt (§12.6): the only tokenize call, on a capped PromptBytes.
    let glossary = prompt::load_glossary(&prompt::glossary_path());
    for w in &glossary.warnings {
        eprintln!("chinstrap: {w}");
    }
    let capped = prompt::cap(job.prompt_hint.as_deref(), &glossary.terms);
    let fitted = prompt::fit(&capped, |p| {
        debug_assert!(p.len() <= prompt::PROMPT_MAX_BYTES);
        ctx.tokenize(p.as_str(), prompt::PROMPT_MAX_BYTES)
            .map(|t| t.len())
            .map_err(|e| format!("cannot tokenize the prompt: {e}"))
    })?;

    let sampling = if backend == WhisperBackend::Vulkan {
        "beam5"
    } else {
        "greedy"
    };
    let mut raw: Vec<RawSegment> = Vec::new();
    let mut detected = language.clone();
    let mut infer_s = 0.0;
    let no_speech = speech.as_ref().is_some_and(|s| s.is_empty());
    if !no_speech {
        let mut state = ctx
            .create_state()
            .map_err(|e| format!("cannot create the whisper state: {e}"))?;
        let mut params = if backend == WhisperBackend::Vulkan {
            FullParams::new(SamplingStrategy::BeamSearch {
                beam_size: 5,
                patience: -1.0,
            })
        } else {
            let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            let n = std::thread::available_parallelism().map_or(2, |n| n.get());
            p.set_n_threads((n / 2).clamp(1, 16) as i32);
            p
        };
        params.set_language(Some(language.as_deref().unwrap_or("auto")));
        params.set_token_timestamps(true);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        if !fitted.text.is_empty() {
            params.set_initial_prompt(&fitted.text);
        }
        let mut last = -1i32;
        params.set_progress_callback_safe(move |p: i32| {
            let pct = 12 + p.clamp(0, 100) * 86 / 100;
            if pct != last {
                last = pct;
                progress(proto::STAGE_TRANSCRIBING, pct as u8);
            }
        });
        progress(proto::STAGE_TRANSCRIBING, 12);
        let t = Instant::now();
        state
            .full(params, &pcm)
            .map_err(|e| format!("whisper failed: {e}"))?;
        infer_s = t.elapsed().as_secs_f64();
        if detected.is_none() {
            detected = whisper_rs::get_lang_str(state.full_lang_id_from_state()).map(String::from);
        }
        for seg in state.as_iter() {
            raw.push(RawSegment {
                t0: seg.start_timestamp(),
                t1: seg.end_timestamp(),
                text: seg
                    .to_str_lossy()
                    .map(|c| c.to_string())
                    .unwrap_or_default(),
                tokens: raw_tokens(&seg),
            });
        }
    }
    progress(proto::STAGE_WRITING, 98);
    let segments = output::assemble(&raw, ctx.token_eot(), speech.as_deref(), audio_s);
    let media_name = job
        .media
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let model_file = job
        .model
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let total_s = t_total.elapsed().as_secs_f64();
    let rtf = if audio_s > 0.0 {
        infer_s / audio_s
    } else {
        0.0
    };
    let language = detected.unwrap_or_default();
    let transcript = Transcript {
        format: output::FORMAT.into(),
        version: output::VERSION,
        media: media_name,
        timeline: "file".into(),
        media_start_ns: decoded.media_start_ns,
        duration_s: r3(audio_s),
        language: language.clone(),
        language_source: language_source.into(),
        model: output::ModelInfo {
            id: job.model_id.clone(),
            file: model_file,
            sha256: job.model_sha256.clone(),
        },
        engine: output::EngineInfo {
            whisper_rs: WHISPER_RS_VERSION.into(),
            whisper_cpp: whisper_rs::WHISPER_CPP_VERSION.into(),
            backend: proto::backend_name(backend).into(),
            device,
            sampling: sampling.into(),
            timestamps: if dtw { "dtw" } else { "legacy" }.into(),
            vad: job
                .vad_model
                .as_ref()
                .map(|_| super::model::VAD.name.to_string()),
        },
        prompt: output::PromptInfo {
            hint: fitted.hint.clone(),
            terms_used: fitted.terms_used,
            terms_dropped: fitted.terms_dropped,
            bytes: fitted.bytes,
            tokens: fitted.tokens,
        },
        timing: output::Timing {
            started,
            decode_s: r3(decode_s),
            vad_s: r3(vad_s),
            infer_s: r3(infer_s),
            total_s: r3(total_s),
            rtf: r3(rtf),
        },
        speech: speech.unwrap_or_default(),
        text: output::joined_text(&segments),
        segments,
    };
    let words = transcript.word_count();
    let (json, srt, vtt) = output::write_outputs(&job.media, &transcript)
        .map_err(|e| format!("cannot write the transcript: {e}"))?;
    progress(proto::STAGE_WRITING, 100);
    Ok(WorkerLine::Done {
        json,
        srt,
        vtt,
        language,
        words,
        audio_s: r3(audio_s),
        rtf: r3(rtf),
    })
}

/// For the ignored real-model test: tokenizes the capped prompt of a 2000-term glossary.
#[cfg(test)]
pub fn tokenize_capped_with_model(
    model: &std::path::Path,
    terms: &[String],
) -> Result<usize, String> {
    let mut cp = WhisperContextParameters::default();
    cp.use_gpu(false);
    let ctx = WhisperContext::new_with_params(model.to_str().ok_or("path")?, cp)
        .map_err(|e| e.to_string())?;
    let capped = prompt::cap(Some("kubernetes demo"), terms);
    let fitted = prompt::fit(&capped, |p| {
        ctx.tokenize(p.as_str(), prompt::PROMPT_MAX_BYTES)
            .map(|t| t.len())
            .map_err(|e| e.to_string())
    })?;
    assert!(fitted.bytes <= prompt::PROMPT_MAX_BYTES);
    Ok(fitted.tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(h: [i32; 11]) -> Vec<u8> {
        let mut b = 0x6767_6d6cu32.to_le_bytes().to_vec();
        for v in h {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn dtw_preset_follows_the_header_not_the_name() {
        let dir = std::env::temp_dir().join(format!("chinstrap-hdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let turbo = [51866, 1500, 1280, 20, 32, 448, 1280, 20, 4, 128, 2007];
        let small = [51865, 1500, 768, 12, 12, 448, 768, 12, 12, 80, 1];
        let a = dir.join("my-model.bin");
        std::fs::write(&a, header(turbo)).unwrap();
        assert!(is_large_v3_turbo(&a));
        let b = dir.join("ggml-large-v3-turbo-but-small.bin");
        std::fs::write(&b, header(small)).unwrap();
        assert!(!is_large_v3_turbo(&b), "name is ignored");
        let c = dir.join("garbage.bin");
        std::fs::write(&c, b"not a model").unwrap();
        assert!(!is_large_v3_turbo(&c));
        assert!(!is_large_v3_turbo(&dir.join("missing.bin")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// `cargo test -- --ignored real_model_tokenize` with CHINSTRAP_TEST_MODEL=<ggml file>.
    #[test]
    #[ignore]
    fn real_model_tokenize_capped_prompt() {
        let model = std::env::var("CHINSTRAP_TEST_MODEL")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                super::super::model::models_dir().join(super::super::model::MODELS[0].file)
            });
        let terms: Vec<String> = (0..2000).map(|i| format!("Kubernetes{i}")).collect();
        let tokens = tokenize_capped_with_model(&model, &terms).unwrap();
        assert!(tokens <= prompt::PROMPT_MAX_TOKENS, "{tokens}");
        assert!(tokens > 50);
    }
}
