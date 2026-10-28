//! Headless self-test: `chinstrap --selftest <dir>` (SPEC §8.1).
//!
//! Drives the real engine (`enable_portal: false`) through Record → Cut → Stop, validates every
//! file with Discoverer and the manifest, then checks finalize-on-shutdown. Prints a JSON
//! summary and returns the process exit code.
//!
//! Test sources: 1 is the plain CFR pattern; 2 records Native (bursty variable rate, timed
//! keyframes, last frame held to the stop); 3 starts 1.5 s late (its file must still start at
//! zero); 4 never sends a frame (it must end Failed "no frames received", never complete).
//! With `CHINSTRAP_SELFTEST_CAMERA`, a second engine then records that camera with two 4K test
//! windows (Native at 144 Hz by default, `CHINSTRAP_SELFTEST_LOAD_MODE=30|60` for CFR) for
//! `CHINSTRAP_SELFTEST_LOAD_SECS` (default 60) and asserts < 1% repeated camera frames over the
//! whole recording, by the counters and by consecutive-frame differences
//! (`CHINSTRAP_SELFTEST_LOAD=standin` without a camera: a 4K30 test pattern in its place).
//!
//! Previews (unless `CHINSTRAP_PREVIEW=0`): sources 1-3 deliver preview frames while idle and,
//! from the recording pipelines, while recording; the silent source never does; idle previews
//! come back after Stop.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer_pbutils as pbutils;
use gstreamer_pbutils::prelude::*;

use crate::control::{Acquisition, ControlOwner, PortalStatus};
use crate::studio::{
    self, EngineCommand, EngineEvent, EngineHandle, FrameRateMode, InputKey, InputStatus,
    ModelState, Phase, PreviewOrigin, Previews, SegmentStatus, Severity, Snapshot, SourceId,
    SourceKind, SourceState, TestBehaviour, TranscriptStatus,
};

const DURATION_TOLERANCE: Duration = Duration::from_millis(600);
const RESIZE_AFTER_MS: &str = "1500";
/// Source ids of the extra test patterns (portal is off, so ids are 1, 2, 3, 4 in order).
const NATIVE_ID: u32 = 2;
const LATE_ID: u32 = 3;
const SILENT_ID: u32 = 4;
const LATE: Duration = Duration::from_millis(1500);
/// A file "starts at zero" when its first frame is this close to it.
const START_SLACK_NS: u64 = 50_000_000;
/// Consecutive decoded frames (480×270 gray) closer than this are a repeat: a re-encoded
/// identical frame measures 0.10-0.15 on this camera, a fresh one 0.3 and up.
const REPEAT_DIFF: f64 = 0.2;
const MAX_REPEAT_PCT: f64 = 1.0;
/// Preview frames measured over this long; at least this many frames, this rate while
/// recording (one frame per 66 ms is ~15 fps).
const PREVIEW_WINDOW: Duration = Duration::from_secs(3);
const PREVIEW_MIN_FRAMES: u64 = 5;
const PREVIEW_MIN_FPS: f64 = 8.0;

struct Harness {
    rx: async_channel::Receiver<EngineEvent>,
    snap: Option<Snapshot>,
    levels: u32,
    errors: Vec<String>,
    notices: Vec<String>,
}

impl Harness {
    fn pump_one(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::Snapshot(s) => self.snap = Some(*s),
            EngineEvent::Notice(n) => {
                self.notices.push(n.message.clone());
                eprintln!(
                    "selftest: notice {:?}{}: {}{}",
                    n.severity,
                    n.input.map(|i| format!(" [{i:?}]")).unwrap_or_default(),
                    n.message,
                    n.detail.map(|d| format!(" — {d}")).unwrap_or_default()
                );
            }
            EngineEvent::Level { .. } => self.levels += 1,
        }
    }

    fn pump(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            self.pump_one(ev);
        }
    }

    /// Processes events until `pred` holds for the latest snapshot, or `timeout`.
    fn wait_for(
        &mut self,
        timeout: Duration,
        pred: impl Fn(&Snapshot) -> bool,
    ) -> Option<Snapshot> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(s) = &self.snap
                && pred(s)
            {
                return Some(s.clone());
            }
            if Instant::now() >= deadline {
                return None;
            }
            match self.rx.try_recv() {
                Ok(ev) => self.pump_one(ev),
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(async_channel::TryRecvError::Closed) => return None,
            }
        }
    }

    /// Keeps processing events for `d`.
    fn sleep(&mut self, d: Duration) {
        let deadline = Instant::now() + d;
        while Instant::now() < deadline {
            match self.rx.try_recv() {
                Ok(ev) => self.pump_one(ev),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    fn drain(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            self.pump_one(ev);
        }
    }

    fn fail(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        eprintln!("selftest: FAIL: {msg}");
        self.errors.push(msg);
    }
}

struct FileCheck {
    width: Option<u32>,
    height: Option<u32>,
    duration: Option<Duration>,
    /// Frame rate (num, den) and H.264 profile/level, video only.
    fps: Option<(i32, i32)>,
    profile: Option<String>,
    level: Option<String>,
}

/// Parses "W×H @F" out of a camera detail label ("pipewire #2148 · 3840×2160 @25 · NV12").
fn detail_mode(detail: &str) -> Option<(u32, u32, f64)> {
    let part = detail
        .split(" · ")
        .find(|p| p.contains('×') && p.contains('@'))?;
    let (size, rate) = part.split_once(" @")?;
    let (w, h) = size.trim().split_once('×')?;
    Some((w.parse().ok()?, h.parse().ok()?, rate.trim().parse().ok()?))
}

/// The camera's PipeWire object id (for the targeting check), by display-name substring.
fn camera_object_id(substring: &str) -> Option<(String, u32)> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Video/Source"), None);
    monitor.start().ok()?;
    let want = substring.to_lowercase();
    let found = monitor.devices().into_iter().find_map(|d| {
        let name = d.display_name().to_string();
        if !name.to_lowercase().contains(&want) {
            return None;
        }
        let props = d.properties()?;
        let v = props.value("object.id").ok()?;
        let id = v
            .get::<u32>()
            .ok()
            .or_else(|| v.get::<i32>().ok().map(|i| i as u32))
            .or_else(|| v.get::<String>().ok().and_then(|s| s.parse().ok()))?;
        Some((name, id))
    });
    monitor.stop();
    found
}

fn discover(disc: &pbutils::Discoverer, path: &Path) -> Result<pbutils::DiscovererInfo, String> {
    let uri = gst::glib::filename_to_uri(path, None).map_err(|e| e.to_string())?;
    disc.discover_uri(&uri).map_err(|e| e.to_string())
}

fn check_video(disc: &pbutils::Discoverer, path: &Path) -> Result<FileCheck, String> {
    let info = discover(disc, path)?;
    let video = info.video_streams();
    if video.len() != 1 {
        return Err(format!("expected 1 video stream, found {}", video.len()));
    }
    let v = &video[0];
    let caps = v.caps().ok_or("video stream has no caps")?;
    let name = caps
        .structure(0)
        .map(|s| s.name().to_string())
        .unwrap_or_default();
    if name != "video/x-h264" && name != "video/x-h265" {
        return Err(format!("video stream is {name}, not H.264/H.265"));
    }
    let (w, h) = (v.width(), v.height());
    if w % 2 != 0 || h % 2 != 0 || w < 128 || h < 128 {
        return Err(format!("bad size {w}×{h}"));
    }
    let s = caps.structure(0);
    let field = |k: &str| s.and_then(|s| s.get::<String>(k).ok());
    let rate = v.framerate();
    Ok(FileCheck {
        width: Some(w),
        height: Some(h),
        duration: info.duration().map(|d| Duration::from_nanos(d.nseconds())),
        fps: (rate.numer() > 0).then(|| (rate.numer(), rate.denom())),
        profile: field("profile"),
        level: field("level"),
    })
}

fn check_audio(disc: &pbutils::Discoverer, path: &Path) -> Result<FileCheck, String> {
    let info = discover(disc, path)?;
    let audio = info.audio_streams();
    if audio.len() != 1 {
        return Err(format!("expected 1 audio stream, found {}", audio.len()));
    }
    let a = &audio[0];
    let caps = a.caps().ok_or("audio stream has no caps")?;
    let name = caps
        .structure(0)
        .map(|s| s.name().to_string())
        .unwrap_or_default();
    if name != "audio/x-opus" {
        return Err(format!("audio stream is {name}, not Opus"));
    }
    if a.sample_rate() != 48_000 {
        return Err(format!("sample rate {} ≠ 48000", a.sample_rate()));
    }
    Ok(FileCheck {
        width: None,
        height: None,
        duration: info.duration().map(|d| Duration::from_nanos(d.nseconds())),
        fps: None,
        profile: None,
        level: None,
    })
}

/// Verifies that a preserved file is indexed by Matroska and that its stream is discoverable and
/// decodable, rather than merely checking that its pathname survived the stop.
fn verify_preserved_media(
    disc: &pbutils::Discoverer,
    path: &Path,
    input: InputKey,
) -> Result<serde_json::Value, String> {
    let duration_ns = studio::recovery::verify_recovered_file(path)?;
    let stream = if input == InputKey::Microphone {
        let info = check_audio(disc, path)?;
        decode_audio(path)?;
        serde_json::json!({
            "kind": "audio",
            "duration_s": info.duration.map(|duration| duration.as_secs_f64()),
            "decoded": true,
        })
    } else {
        let info = check_video(disc, path)?;
        let decoded_frames = frame_diffs(path)?.len();
        if decoded_frames == 0 {
            return Err("video stream decoded no frames".into());
        }
        serde_json::json!({
            "kind": "video",
            "width": info.width,
            "height": info.height,
            "duration_s": info.duration.map(|duration| duration.as_secs_f64()),
            "decoded_frames": decoded_frames,
        })
    };
    Ok(serde_json::json!({
        "path": path.display().to_string(),
        "bytes": std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
        "matroska_cues_and_duration": true,
        "discoverable_and_playable": stream,
        "verified_duration_s": duration_ns as f64 / 1e9,
    }))
}

/// (PTS ns, keyframe) of every video frame of a Matroska file, demuxed (not decoded).
fn frame_times(path: &Path) -> Result<Vec<(u64, bool)>, String> {
    let desc = format!(
        "filesrc location=\"{}\" ! matroskademux ! fakesink name=s sync=false",
        path.display()
    );
    let frames = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| e.to_string())?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline")?;
    let sink = pipeline.by_name("s").ok_or("no sink")?;
    let f = frames.clone();
    sink.static_pad("sink").ok_or("no sink pad")?.add_probe(
        gst::PadProbeType::BUFFER,
        move |_, info| {
            if let Some(b) = info.buffer()
                && let Some(pts) = b.pts()
                && let Ok(mut v) = f.lock()
            {
                v.push((
                    pts.nseconds(),
                    !b.flags().contains(gst::BufferFlags::DELTA_UNIT),
                ));
            }
            gst::PadProbeReturn::Ok
        },
    );
    run_to_eos(&pipeline, Duration::from_secs(60))?;
    let mut v = frames.lock().map_err(|_| "poisoned")?.clone();
    v.sort_by_key(|x| x.0);
    Ok(v)
}

/// Mean absolute difference (0-255 scale) between consecutive decoded frames, scaled to
/// 480×270 gray, with each pair's later PTS.
fn frame_diffs(path: &Path) -> Result<Vec<(u64, f64)>, String> {
    let desc = format!(
        "filesrc location=\"{}\" ! decodebin ! videoconvert ! videoscale ! \
         video/x-raw,format=GRAY8,width=480,height=270 ! fakesink name=s sync=false",
        path.display()
    );
    let out = std::sync::Arc::new(std::sync::Mutex::new((None::<Vec<u8>>, Vec::new())));
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| e.to_string())?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline")?;
    let sink = pipeline.by_name("s").ok_or("no sink")?;
    let o = out.clone();
    sink.static_pad("sink").ok_or("no sink pad")?.add_probe(
        gst::PadProbeType::BUFFER,
        move |_, info| {
            if let Some(b) = info.buffer()
                && let Ok(map) = b.map_readable()
                && let Ok(mut st) = o.lock()
            {
                let cur = map.as_slice().to_vec();
                if let Some(prev) = &st.0
                    && prev.len() == cur.len()
                    && !cur.is_empty()
                {
                    let sum: u64 = prev
                        .iter()
                        .zip(&cur)
                        .map(|(a, b)| a.abs_diff(*b) as u64)
                        .sum();
                    let pts = b.pts().map_or(0, |p| p.nseconds());
                    st.1.push((pts, sum as f64 / cur.len() as f64));
                }
                st.0 = Some(cur);
            }
            gst::PadProbeReturn::Ok
        },
    );
    run_to_eos(&pipeline, Duration::from_secs(300))?;
    let v = out.lock().map_err(|_| "poisoned")?.1.clone();
    Ok(v)
}

fn run_to_eos(pipeline: &gst::Pipeline, timeout: Duration) -> Result<(), String> {
    let bus = pipeline.bus().ok_or("no bus")?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| e.to_string())?;
    let res = match bus.timed_pop_filtered(
        gst::ClockTime::from_nseconds(timeout.as_nanos() as u64),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) {
        Some(m) => match m.view() {
            gst::MessageView::Error(e) => Err(e.error().to_string()),
            _ => Ok(()),
        },
        None => Err("timed out".into()),
    };
    let _ = pipeline.set_state(gst::State::Null);
    res
}

fn decode_audio(path: &Path) -> Result<(), String> {
    let desc = format!(
        "filesrc location=\"{}\" ! decodebin ! audioconvert ! fakesink sync=false",
        path.display()
    );
    let pipeline = gst::parse::launch(&desc)
        .map_err(|error| format!("cannot build audio decoder: {error}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "audio decoder is not a pipeline")?;
    run_to_eos(&pipeline, Duration::from_secs(60))
}

/// Problems with a variable-rate file: it must not be constant-rate, must reach `end_ns`, and
/// must have a keyframe at least every ~2 s.
fn check_vfr(frames: &[(u64, bool)], end_ns: u64) -> Result<serde_json::Value, String> {
    if frames.len() < 10 {
        return Err(format!("only {} frames", frames.len()));
    }
    let mut deltas: Vec<u64> = frames
        .windows(2)
        .map(|w| (w[1].0 - w[0].0 + 500_000) / 1_000_000)
        .collect();
    deltas.sort_unstable();
    let mut distinct = deltas.clone();
    distinct.dedup();
    let keys: Vec<u64> = frames.iter().filter(|f| f.1).map(|f| f.0).collect();
    let max_key_gap = keys.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(0);
    let last = frames.last().map_or(0, |f| f.0);
    let json = serde_json::json!({
        "frames": frames.len(), "distinct_deltas_ms": distinct.len(),
        "min_delta_ms": deltas.first(), "max_delta_ms": deltas.last(),
        "keyframes": keys.len(), "max_keyframe_gap_s": max_key_gap as f64 / 1e9,
        "last_frame_s": last as f64 / 1e9,
    });
    if distinct.len() < 3 {
        return Err(format!("frame intervals look constant: {json}"));
    }
    if keys.len() < 2 || max_key_gap > 2_600_000_000 {
        return Err(format!("keyframes not every ~2 s: {json}"));
    }
    if last + 400_000_000 < end_ns {
        return Err(format!(
            "last frame at {:.3}s, segment ends {:.3}s",
            last as f64 / 1e9,
            end_ns as f64 / 1e9
        ));
    }
    Ok(json)
}

fn ctl_process(args: &[&str]) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let status = Command::new(executable)
        .arg("ctl")
        .args(args)
        .status()
        .map_err(|error| format!("cannot start chinstrap ctl: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "chinstrap ctl exited with status {}",
            status.code().unwrap_or(-1)
        ))
    }
}

fn ctl_or_engine_command(
    owner: Option<&ControlOwner>,
    args: &[&str],
    command: EngineCommand,
    engine: &EngineHandle,
    harness: &mut Harness,
) {
    if owner.is_some() {
        if let Err(error) = ctl_process(args) {
            harness.fail(format!("ctl {}: {error}", args.join(" ")));
            engine.send(command);
        }
    } else {
        engine.send(command);
    }
}

fn portal_check_skip(owner: &ControlOwner) -> Option<String> {
    let control = owner.handle();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match control.portal_status() {
            PortalStatus::Checking => {
                if Instant::now() >= deadline {
                    return Some(
                        "skipped: GlobalShortcuts portal check (discovery timed out)".into(),
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            PortalStatus::Available => {
                return Some(
                    "skipped: GlobalShortcuts binding check (desktop confirmation is interactive)"
                        .into(),
                );
            }
            PortalStatus::Unavailable { reason } | PortalStatus::Failed { reason } => {
                return Some(format!("skipped: GlobalShortcuts portal check ({reason})"));
            }
            PortalStatus::Configuring | PortalStatus::Enabled { .. } => return None,
        }
    }
}

pub fn run(dir: &Path) -> i32 {
    match run_inner(dir) {
        Ok(ok) => i32::from(!ok),
        Err(e) => {
            eprintln!("selftest: aborted: {e}");
            println!(
                "{}",
                serde_json::json!({"ok": false, "error": e, "segments": [], "warnings": []})
            );
            1
        }
    }
}

pub fn crash_child(root: &Path, ready: &Path) -> i32 {
    child_recording(root, ready, false)
}

pub fn signal_child(root: &Path, ready: &Path) -> i32 {
    child_recording(root, ready, true)
}

fn child_recording(root: &Path, ready: &Path, handle_signals: bool) -> i32 {
    let result = (|| -> Result<(), String> {
        std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
        let mut config = studio::EngineConfig::from_env();
        config.output_root = root.to_path_buf();
        config.enable_portal = false;
        config.fake = false;
        config.preview = false;
        config.test_sources = vec![(1, TestBehaviour::default())];
        let (engine, rx) = studio::start(config).map_err(|e| e.to_string())?;
        if handle_signals {
            crate::signal_shutdown::install_handlers().map_err(|e| e.to_string())?;
            crate::signal_shutdown::spawn_shutdown_watcher(engine.clone())
                .map_err(|e| e.to_string())?;
        }
        let mut harness = Harness {
            rx,
            snap: None,
            levels: 0,
            errors: Vec::new(),
            notices: Vec::new(),
        };
        engine.send(EngineCommand::AddTestSource);
        harness
            .wait_for(Duration::from_secs(30), |snapshot| {
                snapshot.can_record
                    && snapshot.sources.iter().any(|source| {
                        source.id == SourceId(1) && source.state == SourceState::Ready
                    })
            })
            .ok_or("child test source did not become recordable")?;
        engine.send(EngineCommand::Record);
        let recording = harness
            .wait_for(Duration::from_secs(30), |snapshot| {
                matches!(snapshot.phase, Phase::Recording { segment: 1, .. })
            })
            .ok_or("child did not start recording")?;
        let session_dir = recording
            .session
            .as_ref()
            .ok_or("child has no session")?
            .dir
            .clone();
        let recording_started_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        harness.sleep(if handle_signals {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(7)
        });
        std::fs::write(
            ready,
            format!("{}\n{recording_started_ns}", session_dir.display()),
        )
        .map_err(|e| e.to_string())?;
        loop {
            harness.sleep(Duration::from_millis(100));
        }
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("selftest child: {error}");
            1
        }
    }
}

fn run_inner(dir: &Path) -> Result<bool, String> {
    let dir = std::path::absolute(dir).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    if std::env::var_os("CHINSTRAP_TEST_RESIZE_AFTER_MS").is_none() {
        // SAFETY: called before the engine (or any other thread of ours) is started.
        unsafe { std::env::set_var("CHINSTRAP_TEST_RESIZE_AFTER_MS", RESIZE_AFTER_MS) };
    }
    let mut config = studio::EngineConfig::from_env();
    config.output_root = dir.clone();
    config.enable_portal = false;
    config.fake = false;
    config.test_sources = vec![
        (
            LATE_ID,
            TestBehaviour {
                late: Some(LATE),
                ..Default::default()
            },
        ),
        (
            SILENT_ID,
            TestBehaviour {
                silent: true,
                ..Default::default()
            },
        ),
    ];
    let load_config = config.clone();
    let preview_on = config.preview;
    let mut warnings: Vec<String> = Vec::new();
    let (mut control_owner, control_skip) = match ControlOwner::acquire_for_selftest() {
        Ok(Acquisition::Owner(owner)) => (Some(owner), None),
        Ok(Acquisition::AlreadyRunning) => {
            return Err("cannot run --selftest while another Chinstrap instance is running".into());
        }
        Err(error) => (None, Some(error)),
    };
    if let Some(reason) = &control_skip {
        warnings.push(format!(
            "skipped: chinstrap ctl checks (session bus unavailable: {reason})"
        ));
        warnings.push(format!(
            "skipped: GlobalShortcuts portal check (session bus unavailable: {reason})"
        ));
    }
    let (engine, rx) = studio::start(config).map_err(|e: studio::EngineError| e.to_string())?;
    if let Some(owner) = &mut control_owner {
        owner.attach_engine(engine.clone())?;
        if let Some(skip) = portal_check_skip(owner) {
            warnings.push(skip);
        }
    }
    let previews = engine.previews();
    let mut h = Harness {
        rx,
        snap: None,
        levels: 0,
        errors: Vec::new(),
        notices: Vec::new(),
    };

    // 1. Capabilities.
    let first = h
        .wait_for(Duration::from_secs(60), |_| true)
        .ok_or("no first Snapshot")?;
    let caps = first.capabilities.clone();
    eprintln!(
        "selftest: capabilities: encoder={:?} convert={:?} missing={:?} portal={:?}",
        caps.h264_encoder, caps.gpu_convert, caps.missing_elements, caps.portal
    );
    if caps.h264_encoder.is_none() {
        h.fail("no working H.264 encoder");
    }
    let hard_missing: Vec<&String> = caps
        .missing_elements
        .iter()
        .filter(|m| {
            studio::REQUIRED_ELEMENTS.iter().any(|(n, c, _)| {
                n == m && matches!(c, studio::InputClass::Any | studio::InputClass::Video)
            })
        })
        .collect();
    if !hard_missing.is_empty() {
        h.fail(format!("missing required elements: {hard_missing:?}"));
    }
    if caps.gpu_convert.is_none() {
        warnings.push("no GPU conversion path: portal capture unavailable".into());
    }
    if !h.errors.is_empty() {
        engine.shutdown_blocking(Duration::from_secs(5));
        return Ok(summary(
            &h,
            &caps,
            &[],
            &warnings,
            serde_json::Value::Null,
            serde_json::Value::Null,
        ));
    }

    // 2. Inputs. (Re-assert the output root through the command path as well.)
    engine.send(EngineCommand::SetOutputRoot(dir.clone()));
    // CHINSTRAP_SELFTEST_NO_CAMERA=1 skips the camera (e.g. a loopback dummy with no producer).
    // CHINSTRAP_SELFTEST_CAMERA=<substring> picks that camera and makes its file critical.
    let want_camera = std::env::var("CHINSTRAP_SELFTEST_CAMERA")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let camera_info = match &want_camera {
        Some(want) => {
            let w = want.to_lowercase();
            let found = first
                .cameras
                .iter()
                .find(|c| c.name.to_lowercase().contains(&w));
            let problem = match found {
                None => Some(format!("no camera matching {want:?}")),
                Some(c) => c
                    .unsupported
                    .as_ref()
                    .map(|r| format!("camera {:?} is unsupported: {r}", c.name)),
            };
            if let Some(p) = problem {
                engine.shutdown_blocking(Duration::from_secs(5));
                return Err(p);
            }
            found.cloned()
        }
        None => first
            .cameras
            .iter()
            .find(|c| c.unsupported.is_none())
            .filter(|_| std::env::var_os("CHINSTRAP_SELFTEST_NO_CAMERA").is_none())
            .cloned(),
    };
    let camera = camera_info.as_ref().map(|c| c.id);
    let camera_critical = want_camera.is_some();
    let camera_expect = camera_info.as_ref().and_then(|c| detail_mode(&c.detail));
    let mut targeting_json = serde_json::json!(null);
    if gst::ElementFactory::find("pipewiresrc").is_some() {
        targeting_json = check_targeting(&mut h, &mut warnings, want_camera.as_deref());
    }
    engine.send(EngineCommand::SelectCamera(camera));
    let mic = first.microphones.first().map(|m| m.id);
    engine.send(EngineCommand::SelectMicrophone(mic));
    for d in first.cameras.iter().chain(first.microphones.iter()) {
        eprintln!(
            "selftest: device {:?} {:?} \"{}\" [{}]{}",
            d.id,
            d.kind,
            d.name,
            d.detail,
            d.unsupported
                .as_ref()
                .map(|r| format!(" unsupported: {r}"))
                .unwrap_or_default()
        );
    }
    eprintln!("selftest: camera={camera:?} mic={mic:?}");
    for _ in 0..4 {
        engine.send(EngineCommand::AddTestSource);
    }
    engine.send(EngineCommand::SetFrameRateMode {
        source: Some(SourceId(NATIVE_ID)),
        mode: FrameRateMode::Native,
    });
    let snap = h
        .wait_for(Duration::from_secs(5), |s| {
            s.sources
                .iter()
                .filter(|x| x.kind == SourceKind::TestPattern && x.state == SourceState::Ready)
                .count()
                == 4
                && s.sources
                    .iter()
                    .any(|x| x.id == SourceId(NATIVE_ID) && x.frame_rate == FrameRateMode::Native)
                && s.selected_camera == camera
                && s.selected_microphone == mic
                && s.can_record
        })
        .ok_or("test sources never became Ready / can_record")?;
    let test_id = snap
        .sources
        .iter()
        .find(|x| x.kind == SourceKind::TestPattern)
        .map(|x| x.id)
        .ok_or("no test source")?;
    let test_key = InputKey::Source(test_id);
    let (native_key, late_key, silent_key) = (
        InputKey::Source(SourceId(NATIVE_ID)),
        InputKey::Source(SourceId(LATE_ID)),
        InputKey::Source(SourceId(SILENT_ID)),
    );
    eprintln!("selftest: capacity plan: {:?}", snap.capacity);
    let plan_json = serde_json::json!({
        "gpu_load_pct": snap.capacity.gpu_load_pct, "over": snap.capacity.over,
        "inputs": snap.capacity.inputs.iter().map(|p| serde_json::json!({
            "input": format!("{:?}", p.input), "encoder": p.encoder.element_name(),
            "fps": p.fps, "native": p.native})).collect::<Vec<_>>(),
        "notes": snap.capacity.notes,
    });
    let preview_keys = [test_key, native_key, late_key];
    let mut previews_json = serde_json::json!("disabled (CHINSTRAP_PREVIEW=0)");
    if preview_on {
        let idle = check_previews(
            &mut h,
            &previews,
            &preview_keys,
            silent_key,
            PreviewOrigin::Idle,
            None,
        );
        previews_json = serde_json::json!({ "idle": idle });
    }
    // P1-6: a recording window with no frame yet says so. Use the real ctl path when the session
    // bus is available so this also exercises control from outside the GUI process.
    ctl_or_engine_command(
        control_owner.as_ref(),
        &["toggle"],
        EngineCommand::Record,
        &engine,
        &mut h,
    );
    let waiting = h.wait_for(Duration::from_secs(15), |s| {
        s.session.as_ref().is_some_and(|x| {
            x.segments.iter().any(|g| {
                g.status == SegmentStatus::Recording
                    && g.files
                        .iter()
                        .any(|f| f.input == silent_key && f.waiting_for_frame)
                    && matches!(s.phase, Phase::Recording { segment: 1, .. })
            })
        })
    });
    if waiting.is_none() {
        h.fail("the silent source never showed waiting_for_frame while recording");
    }
    let reserve_path = waiting
        .as_ref()
        .and_then(|snapshot| snapshot.session.as_ref())
        .map(|session| session.dir.join(".chinstrap-reserve"));
    if reserve_path.as_ref().is_none_or(|path| !path.exists()) {
        h.fail("the emergency stop reserve was not present while recording");
    }

    // 3. Record (sent above).
    h.wait_for(Duration::from_secs(15), |s| {
        matches!(s.phase, Phase::Recording { segment: 1, .. })
    })
    .ok_or("segment 1 never reached Recording")?;
    if preview_on {
        // Measured over the 3 s segment 1 runs anyway.
        let recording = check_previews(
            &mut h,
            &previews,
            &preview_keys,
            silent_key,
            PreviewOrigin::Recording { segment: 1 },
            Some(late_key),
        );
        previews_json["recording"] = recording;
    } else {
        h.sleep(Duration::from_secs(3));
    }

    // 4. Cut.
    ctl_or_engine_command(
        control_owner.as_ref(),
        &["cut"],
        EngineCommand::Cut,
        &engine,
        &mut h,
    );
    h.wait_for(Duration::from_secs(20), |s| {
        matches!(s.phase, Phase::Recording { segment: 2, .. })
    })
    .ok_or("segment 2 never reached Recording")?;
    h.sleep(Duration::from_secs(3));

    // 5. Stop.
    ctl_or_engine_command(
        control_owner.as_ref(),
        &["stop"],
        EngineCommand::Stop,
        &engine,
        &mut h,
    );
    let done = h
        .wait_for(Duration::from_secs(20), |s| s.phase == Phase::Idle)
        .ok_or("Stop never reached Idle")?;
    let session = done.session.clone().ok_or("no session after Stop")?;
    if reserve_path.as_ref().is_some_and(|path| path.exists()) {
        h.fail("the emergency stop reserve remained after normal Stop");
    }
    eprintln!("selftest: session {}", session.dir.display());
    if preview_on {
        // Idle previews come back once the recording pipelines are gone.
        let t0 = Instant::now();
        let resumed = loop {
            let all = preview_keys.iter().all(|k| {
                previews
                    .latest(*k)
                    .is_some_and(|(_, f)| f.origin == PreviewOrigin::Idle && f.at > t0)
            });
            if all || t0.elapsed() > Duration::from_secs(10) {
                break all;
            }
            h.sleep(Duration::from_millis(50));
        };
        if !resumed {
            h.fail("previews did not resume within 10 s of Stop");
        }
        previews_json["resumed_after_stop_s"] =
            serde_json::json!(resumed.then(|| t0.elapsed().as_secs_f64()));
    }

    // 6. Validate files. The test file keeps the first caps size (CHINSTRAP_TEST_SIZE).
    let (test_w, test_h) = {
        let (w, h) = studio::test_size();
        let (w, h) = studio::lock_size(w, h);
        (w as u32, h as u32)
    };
    let disc = pbutils::Discoverer::new(gst::ClockTime::from_seconds(5))
        .map_err(|e| format!("Discoverer: {e}"))?;
    let mut seg_json = Vec::new();
    for seg in session.segments.iter().take(2) {
        let mut files_json = Vec::new();
        let seg_dur = seg.duration;
        if seg.status != SegmentStatus::Complete {
            h.fail(format!("segment {} status {:?}", seg.index, seg.status));
        }
        // A source that sent no frame stays selected on Cut (a static window may redraw later).
        if seg.index == 2 && !seg.files.iter().any(|f| f.input == silent_key) {
            h.fail("segment 2 dropped the silent source after Cut");
        }
        for f in &seg.files {
            let critical = f.input == test_key
                || f.input == native_key
                || f.input == late_key
                || (camera_critical && f.input == InputKey::Camera);
            let mut problem: Option<String> = None;
            if f.input == silent_key {
                // P1-6: zero frames is a failure, never a complete (empty) file.
                match &f.status {
                    InputStatus::Failed { reason } if reason.contains("no frames received") => {}
                    s => h.fail(format!(
                        "segment {} silent source: {s:?}, expected Failed \"no frames received\"",
                        seg.index
                    )),
                }
                if f.path.exists() {
                    h.fail("the silent source left a file behind");
                }
                continue;
            }
            if f.status != InputStatus::Complete {
                problem = Some(format!("status {:?}", f.status));
            } else {
                let meta = std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0);
                if meta == 0 {
                    problem = Some("missing or empty file".into());
                } else {
                    let res = if f.input == InputKey::Microphone {
                        check_audio(&disc, &f.path)
                    } else {
                        check_video(&disc, &f.path)
                    };
                    match res {
                        Err(e) => problem = Some(e),
                        Ok(fc) => {
                            if let Err(error) = studio::recovery::verify_matroska_index(&f.path) {
                                problem = Some(format!("Matroska Cues/Duration: {error}"));
                            }
                            if let (Some(d), Some(sd)) = (fc.duration, seg_dur)
                                && d.abs_diff(sd) > DURATION_TOLERANCE
                            {
                                problem = Some(format!(
                                    "duration {:.3}s vs segment {:.3}s",
                                    d.as_secs_f64(),
                                    sd.as_secs_f64()
                                ));
                            }
                            if fc.duration.is_none() {
                                problem = Some("no duration".into());
                            }
                            if f.input == test_key
                                && seg.index == 1
                                && (fc.width, fc.height) != (Some(test_w), Some(test_h))
                            {
                                problem = Some(format!(
                                    "test file is {:?}×{:?}, expected the first caps size {test_w}×{test_h}",
                                    fc.width, fc.height
                                ));
                            }
                            if f.input == InputKey::Camera
                                && let Some((ew, eh, efps)) = camera_expect
                            {
                                let fps = fc.fps.map(|(n, d)| n as f64 / d.max(1) as f64);
                                if (fc.width, fc.height) != (Some(ew), Some(eh)) {
                                    problem = Some(format!(
                                        "camera file is {:?}×{:?}, expected the native {ew}×{eh}",
                                        fc.width, fc.height
                                    ));
                                } else if fps.is_none_or(|f| (f - efps).abs() > 0.05) {
                                    problem = Some(format!(
                                        "camera file is {fps:?} fps, expected the native {efps}"
                                    ));
                                }
                            }
                            // P1-7: every video file starts at segment time zero.
                            let mut frames_json = serde_json::Value::Null;
                            if f.input != InputKey::Microphone {
                                match frame_times(&f.path) {
                                    Ok(frames) => {
                                        let first = frames.first().map_or(u64::MAX, |x| x.0);
                                        if first > START_SLACK_NS {
                                            problem = Some(format!(
                                                "first frame at {:.3}s, not at zero",
                                                first as f64 / 1e9
                                            ));
                                        }
                                        if f.input == native_key {
                                            let end = seg_dur.map_or(0, |d| d.as_nanos() as u64);
                                            match check_vfr(&frames, end) {
                                                Ok(j) => frames_json = j,
                                                Err(e) => problem = Some(format!("native: {e}")),
                                            }
                                        }
                                    }
                                    Err(e) => problem = Some(format!("cannot read frames: {e}")),
                                }
                            }
                            files_json.push(serde_json::json!({
                                "file": f.path.file_name().map(|n| n.to_string_lossy().to_string()),
                                "width": fc.width, "height": fc.height,
                                "framerate": fc.fps.map(|(n, d)| format!("{n}/{d}")),
                                "profile": fc.profile, "level": fc.level,
                                "duration_s": fc.duration.map(|d| d.as_secs_f64()),
                                "quality": f.quality.as_ref().map(|q| serde_json::json!({
                                    "encoder": q.encoder.map(|e| e.element_name()),
                                    "frames_in": q.frames_in, "encoded": q.encoded,
                                    "duplicated": q.duplicated, "padded": q.padded,
                                    "dropped_queue": q.dropped_queue, "dropped_corrupt": q.dropped_corrupt,
                                    "effective_fps": q.effective_fps, "repeat_pct": q.repeat_pct,
                                    "start_offset_s": q.start_offset.map(|d| d.as_secs_f64()),
                                })),
                                "vfr": frames_json,
                            }));
                        }
                    }
                }
            }
            if let Some(p) = problem {
                let msg = format!("segment {} {}: {p}", seg.index, f.label);
                if critical {
                    h.fail(msg);
                } else {
                    warnings.push(msg);
                }
            }
        }
        seg_json.push(serde_json::json!({
            "index": seg.index,
            "status": format!("{:?}", seg.status),
            "duration_s": seg_dur.map(|d| d.as_secs_f64()),
            "gap_before_s": seg.gap_before.map(|d| d.as_secs_f64()),
            "files": files_json,
        }));
    }
    if session.segments.len() < 2 {
        h.fail(format!(
            "expected 2 segments, found {}",
            session.segments.len()
        ));
    }

    // 7. Manifest.
    let manifest_ok = check_manifest(
        &mut h,
        &mut warnings,
        &session.manifest,
        &session.dir,
        test_id.0,
        mic.is_some(),
        camera.is_some(),
    );
    let _ = manifest_ok;
    check_manifest_extras(&mut h, &session.manifest);

    // 7b. Transcription (§12.12), through the worker child like the app.
    let transcription_json = check_transcription(&mut h, &mut warnings, &engine, &session.manifest);

    // 8. Finalize-on-quit. The Discoverer is dropped first: its in-process VA decoder session
    // must not share the GPU with the next segment's encoder.
    drop(disc);
    engine.send(EngineCommand::Record);
    if h.wait_for(Duration::from_secs(15), |s| {
        matches!(s.phase, Phase::Recording { segment: 3, .. })
    })
    .is_none()
    {
        h.fail("segment 3 never reached Recording");
    }
    h.sleep(Duration::from_secs(2));
    let t0 = Instant::now();
    let acked = engine.shutdown_blocking(Duration::from_secs(10));
    eprintln!(
        "selftest: shutdown_blocking -> {acked} in {:.2}s",
        t0.elapsed().as_secs_f64()
    );
    if !acked {
        h.fail("shutdown_blocking(10s) returned false");
    }
    if !engine.shutdown_blocking(Duration::from_secs(1)) && acked {
        h.fail("shutdown_blocking is not idempotent");
    }
    h.drain();
    match studio_manifest(&session.manifest) {
        Ok(m) => {
            let seg3 = m["segments"].as_array().and_then(|a| a.get(2));
            let status = seg3.and_then(|s| s["status"].as_str()).unwrap_or("missing");
            if status != "complete" {
                h.fail(format!("segment 3 manifest status is {status}"));
            }
            let test_file = session
                .dir
                .join("segment-003")
                .join(format!("test-{}.mkv", test_id.0));
            let disc = pbutils::Discoverer::new(gst::ClockTime::from_seconds(5))
                .map_err(|e| format!("Discoverer: {e}"))?;
            match discover(&disc, &test_file) {
                Ok(info) if info.duration().is_some_and(|d| d.nseconds() > 0) => {}
                Ok(_) => h.fail("segment 3 test file has no Duration"),
                Err(e) => h.fail(format!("segment 3 test file: {e}")),
            }
        }
        Err(e) => h.fail(format!("manifest after shutdown: {e}")),
    }
    if mic.is_some() && h.levels == 0 {
        warnings.push("no Level events while recording".into());
    }
    // 9. Concurrent load (camera + two 4K sources), only with a named camera.
    let load_json = match (
        &want_camera,
        std::env::var("CHINSTRAP_SELFTEST_LOAD").as_deref(),
    ) {
        (Some(want), _) => check_load(&mut h, load_config, &dir, want),
        (None, Ok("standin")) => check_load(&mut h, load_config, &dir, "standin"),
        _ => serde_json::json!("skipped: CHINSTRAP_SELFTEST_CAMERA not set"),
    };
    let run_safety_check = |name: &str,
                            check: fn(&Path) -> Result<serde_json::Value, String>,
                            harness: &mut Harness| {
        match check(&dir) {
            Ok(result) => result,
            Err(error) => {
                harness.fail(format!("{name} selftest: {error}"));
                serde_json::json!({"ok": false, "error": error})
            }
        }
    };
    let crash_recovery_json = run_safety_check("kill-and-recover", check_kill_and_recover, &mut h);
    let signal_json = run_safety_check("signal-finalization", check_signal_finalization, &mut h);
    let preflight_json = run_safety_check(
        "limited-space preflight",
        check_limited_space_preflight,
        &mut h,
    );
    let reserve_json = run_safety_check(
        "storage warning and reserve stop",
        check_storage_warnings_and_reserve_stop,
        &mut h,
    );
    let enospc_json = run_safety_check(
        "ENOSPC preservation",
        check_enospc_preserves_all_inputs,
        &mut h,
    );
    let enospc_startup_json =
        run_safety_check("ENOSPC during startup", check_enospc_during_startup, &mut h);
    let mut seg_json = seg_json;
    seg_json.push(serde_json::json!({
        "plan": plan_json,
        "load": load_json,
        "previews": previews_json,
        "crash_recovery": crash_recovery_json,
        "signal_finalization": signal_json,
        "limited_space_preflight": preflight_json,
        "storage_warnings_and_reserve": reserve_json,
        "enospc_preservation": enospc_json,
        "enospc_startup_preservation": enospc_startup_json,
    }));
    Ok(summary(
        &h,
        &caps,
        &seg_json,
        &warnings,
        targeting_json,
        transcription_json,
    ))
}

fn scenario_root(base: &Path, label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    base.join(format!(".selftest-{label}-{}-{nonce}", std::process::id()))
}

fn selftest_engine_config(root: &Path, free_bytes: Option<u64>) -> studio::EngineConfig {
    let mut config = studio::EngineConfig::from_env();
    config.output_root = root.to_path_buf();
    config.enable_portal = false;
    config.fake = false;
    config.preview = false;
    config.test_sources = vec![(1, TestBehaviour::default())];
    config.selftest_free_bytes = free_bytes;
    config.selftest_min_free_mb = None;
    config
}

fn check_limited_space_preflight(base: &Path) -> Result<serde_json::Value, String> {
    let root = scenario_root(base, "preflight");
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let (engine, rx) =
        studio::start(selftest_engine_config(&root, Some(0))).map_err(|e| e.to_string())?;
    let mut harness = Harness {
        rx,
        snap: None,
        levels: 0,
        errors: Vec::new(),
        notices: Vec::new(),
    };
    engine.send(EngineCommand::AddTestSource);
    let blocked = harness
        .wait_for(Duration::from_secs(30), |snapshot| {
            snapshot
                .sources
                .iter()
                .any(|source| source.id == SourceId(1) && source.state == SourceState::Ready)
                && snapshot.storage.record_blocked_reason.is_some()
        })
        .ok_or("limited-space preflight did not block the test source")?;
    let reason = blocked
        .storage
        .record_blocked_reason
        .clone()
        .unwrap_or_default();
    if blocked.storage.free_bytes != Some(0) || blocked.can_record {
        return Err("injected zero free bytes were not reflected in preflight".into());
    }
    engine.send(EngineCommand::Record);
    harness.sleep(Duration::from_millis(500));
    let denied = harness
        .snap
        .as_ref()
        .is_some_and(|snapshot| snapshot.phase == Phase::Idle && snapshot.session.is_none());
    let rejection_notice = harness
        .notices
        .iter()
        .any(|message| message.contains("Not enough free space"));
    let stopped = engine.shutdown_blocking(Duration::from_secs(5));
    if !denied || !rejection_notice || !stopped {
        return Err(format!(
            "preflight did not safely refuse Record (denied={denied}, notice={rejection_notice}, shutdown={stopped})"
        ));
    }
    Ok(serde_json::json!({
        "ok": true,
        "free_bytes_injected": 0,
        "record_refused": denied,
        "reason": reason,
        "constrained_volume": false,
    }))
}

fn check_storage_warnings_and_reserve_stop(base: &Path) -> Result<serde_json::Value, String> {
    let root = scenario_root(base, "storage-reserve");
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let mut config = selftest_engine_config(&root, Some(2 * 1024 * 1024 * 1024));
    config.test_sources = vec![(
        1,
        TestBehaviour {
            size: Some((640, 360)),
            refresh: Some(30),
            ..TestBehaviour::default()
        },
    )];
    config.selftest_min_free_mb = Some(1);
    let (engine, rx) = studio::start(config).map_err(|e| e.to_string())?;
    let mut harness = Harness {
        rx,
        snap: None,
        levels: 0,
        errors: Vec::new(),
        notices: Vec::new(),
    };
    engine.send(EngineCommand::AddTestSource);
    harness
        .wait_for(Duration::from_secs(30), |snapshot| {
            snapshot.can_record
                && snapshot
                    .sources
                    .iter()
                    .any(|source| source.id == SourceId(1) && source.state == SourceState::Ready)
        })
        .ok_or("storage-reserve fixture source did not become ready")?;
    engine.send(EngineCommand::Record);
    harness
        .wait_for(Duration::from_secs(30), |snapshot| {
            matches!(snapshot.phase, Phase::Recording { .. })
        })
        .ok_or("storage-reserve fixture did not start recording")?;
    let measured = harness
        .wait_for(Duration::from_secs(20), |snapshot| {
            snapshot.storage.rate_measured
                && snapshot.storage.estimated_bytes_per_second > 0
                && snapshot.storage.stop_reserve_bytes > 0
        })
        .ok_or("storage-reserve fixture did not measure on-disk growth")?;
    let initial_rate = measured.storage.estimated_bytes_per_second;
    let initial_reserve = measured.storage.stop_reserve_bytes;
    let ten_minute_free = initial_reserve.saturating_add(initial_rate.saturating_mul(500));
    engine.set_free_bytes_for_selftest(ten_minute_free);
    let ten_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < ten_deadline
        && !harness
            .notices
            .iter()
            .any(|notice| notice.contains("Less than 10 minutes"))
    {
        harness.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    if !harness
        .notices
        .iter()
        .any(|notice| notice.contains("Less than 10 minutes"))
    {
        return Err("storage monitor did not issue its under-10-minute warning".into());
    }
    let three_snapshot = harness
        .snap
        .as_ref()
        .ok_or("storage monitor emitted no snapshot")?;
    let three_rate = three_snapshot.storage.estimated_bytes_per_second.max(1);
    let three_reserve = three_snapshot.storage.stop_reserve_bytes;
    engine
        .set_free_bytes_for_selftest(three_reserve.saturating_add(three_rate.saturating_mul(120)));
    let three_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < three_deadline
        && !harness
            .notices
            .iter()
            .any(|notice| notice.contains("Less than 3 minutes"))
    {
        harness.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    if !harness
        .notices
        .iter()
        .any(|notice| notice.contains("Less than 3 minutes"))
    {
        return Err("storage monitor did not issue its under-3-minute warning".into());
    }
    let stop_reserve = harness
        .snap
        .as_ref()
        .map(|snapshot| snapshot.storage.stop_reserve_bytes)
        .unwrap_or(three_reserve);
    engine.set_free_bytes_for_selftest(0);
    let stopped = harness
        .wait_for(Duration::from_secs(20), |snapshot| {
            snapshot.phase == Phase::Idle
                && snapshot.session.as_ref().is_some_and(|session| {
                    session.segments.last().is_some_and(|segment| {
                        matches!(
                            &segment.status,
                            SegmentStatus::Incomplete { reason }
                                if reason == "stopped: disk almost full"
                        )
                    })
                })
        })
        .ok_or("free-space threshold did not stop through normal finalization")?;
    let segment = stopped
        .session
        .as_ref()
        .and_then(|session| session.segments.last())
        .ok_or("storage-reserve segment missing")?;
    if segment.files.is_empty() || segment.files.iter().any(|file| !file.path.exists()) {
        return Err("storage-reserve stop did not preserve its output files".into());
    }
    for file in &segment.files {
        studio::recovery::verify_recovered_file(&file.path)?;
    }
    let session_dir = stopped
        .session
        .as_ref()
        .map(|session| session.dir.as_path())
        .ok_or("storage-reserve session missing")?;
    let reserve_released = !session_dir.join(".chinstrap-reserve").exists();
    if !reserve_released {
        return Err("storage-reserve stop left the emergency reserve behind".into());
    }
    let stop_notice = harness
        .notices
        .iter()
        .any(|notice| notice.contains("stopped: disk almost full"));
    let shutdown = engine.shutdown_blocking(Duration::from_secs(5));
    if !stop_notice || !shutdown {
        return Err(format!(
            "reserve stop notice/shutdown failed ({stop_notice}/{shutdown})"
        ));
    }
    Ok(serde_json::json!({
        "ok": true,
        "free_space_values_injected": true,
        "not_hardware_volume_test": true,
        "warning_under_10_minutes": true,
        "warning_under_3_minutes": true,
        "reserve_stop_eos_and_cues_verified": true,
        "initial_measured_rate_bytes_per_second": initial_rate,
        "initial_stop_reserve_bytes": initial_reserve,
        "stop_reserve_bytes": stop_reserve,
        "files": segment.files.len(),
        "reserve_released": reserve_released,
    }))
}

fn check_enospc_preserves_all_inputs(base: &Path) -> Result<serde_json::Value, String> {
    let root = scenario_root(base, "enospc");
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let mut config = selftest_engine_config(&root, None);
    config.test_sources = vec![(1, TestBehaviour::default()), (2, TestBehaviour::default())];
    let (engine, rx) = studio::start(config).map_err(|e| e.to_string())?;
    let mut harness = Harness {
        rx,
        snap: None,
        levels: 0,
        errors: Vec::new(),
        notices: Vec::new(),
    };
    engine.send(EngineCommand::AddTestSource);
    engine.send(EngineCommand::AddTestSource);
    harness
        .wait_for(Duration::from_secs(30), |snapshot| {
            snapshot.can_record
                && snapshot
                    .sources
                    .iter()
                    .filter(|source| {
                        source.id.0 <= 2
                            && source.kind == SourceKind::TestPattern
                            && source.state == SourceState::Ready
                    })
                    .count()
                    == 2
        })
        .ok_or("ENOSPC fixture sources did not become ready")?;
    engine.send(EngineCommand::Record);
    harness
        .wait_for(Duration::from_secs(30), |snapshot| {
            matches!(snapshot.phase, Phase::Recording { segment: 1, .. })
        })
        .ok_or("ENOSPC fixture did not enter Recording")?;
    // Let both media streams produce usable content. The separate startup case below exercises
    // the Pausing/Paused/Starting race; this case proves preserved peers are actually playable.
    harness.sleep(Duration::from_secs(2));
    engine.inject_storage_failure_for_selftest(InputKey::Source(SourceId(1)));
    let stopped = harness
        .wait_for(Duration::from_secs(30), |snapshot| {
            snapshot.phase == Phase::Idle
                && snapshot.session.as_ref().is_some_and(|session| {
                    session.segments.last().is_some_and(|segment| {
                        matches!(
                            &segment.status,
                            SegmentStatus::Incomplete { reason }
                                if reason.contains("disk almost full")
                        )
                    })
                })
        })
        .ok_or("injected ENOSPC did not stop the segment")?;
    let segment = stopped
        .session
        .as_ref()
        .and_then(|session| session.segments.last())
        .ok_or("ENOSPC segment is missing")?;
    if segment.files.len() < 2 {
        return Err(format!(
            "ENOSPC segment has {} files, expected both video inputs",
            segment.files.len()
        ));
    }
    if !segment
        .files
        .iter()
        .any(|file| file.input == InputKey::Source(SourceId(1)))
        || !segment
            .files
            .iter()
            .any(|file| file.input == InputKey::Source(SourceId(2)))
    {
        return Err("ENOSPC segment omitted one of its video peers".into());
    }
    let disc = pbutils::Discoverer::new(gst::ClockTime::from_seconds(5))
        .map_err(|e| format!("Discoverer: {e}"))?;
    let mut files = Vec::new();
    for file in &segment.files {
        if !file.path.exists() {
            return Err(format!(
                "ENOSPC deleted partial input {}",
                file.path.display()
            ));
        }
        let bytes = std::fs::metadata(&file.path)
            .map_err(|error| error.to_string())?
            .len();
        if bytes == 0 {
            return Err(format!(
                "ENOSPC left an empty peer file {}",
                file.path.display()
            ));
        }
        let media = verify_preserved_media(&disc, &file.path, file.input).map_err(|error| {
            format!(
                "preserved peer {} is not playable/indexed: {error}",
                file.path.display()
            )
        })?;
        files.push(serde_json::json!({
            "status": format!("{:?}", file.status),
            "media": media,
        }));
    }
    let reserve_released = stopped
        .session
        .as_ref()
        .is_some_and(|session| !session.dir.join(".chinstrap-reserve").exists());
    if !reserve_released {
        return Err("emergency reserve remained after the ENOSPC stop".into());
    }
    if !harness
        .notices
        .iter()
        .any(|message| message.contains("injected ENOSPC"))
        && !harness
            .notices
            .iter()
            .any(|message| message.contains("finalising all recording inputs"))
    {
        return Err("the injected ENOSPC event did not reach the storage-failure path".into());
    }
    let shutdown = engine.shutdown_blocking(Duration::from_secs(5));
    if !shutdown {
        return Err("ENOSPC test engine did not shut down cleanly".into());
    }
    if !studio::session::scan_interrupted(&root)
        .map_err(|error| error.to_string())?
        .is_empty()
    {
        return Err(
            "playable/indexed ENOSPC files remained falsely discoverable as interrupted".into(),
        );
    }
    Ok(serde_json::json!({
        "ok": true,
        "injected": "engine ENOSPC event seam; no disk fill, physical volume, or filesystem ENOSPC exercised",
        "preserved_peer_files_verified": files,
        "reserve_released": reserve_released,
        "audio_input_verified": segment.files.iter().any(|file| file.input == InputKey::Microphone),
    }))
}

fn check_enospc_during_startup(base: &Path) -> Result<serde_json::Value, String> {
    let root = scenario_root(base, "enospc-startup");
    std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let mut config = selftest_engine_config(&root, None);
    config.test_sources = vec![(1, TestBehaviour::default()), (2, TestBehaviour::default())];
    let (engine, rx) = studio::start(config).map_err(|error| error.to_string())?;
    let mut harness = Harness {
        rx,
        snap: None,
        levels: 0,
        errors: Vec::new(),
        notices: Vec::new(),
    };
    engine.send(EngineCommand::AddTestSource);
    engine.send(EngineCommand::AddTestSource);
    harness
        .wait_for(Duration::from_secs(30), |snapshot| {
            snapshot.can_record
                && snapshot
                    .sources
                    .iter()
                    .filter(|source| source.id.0 <= 2 && source.state == SourceState::Ready)
                    .count()
                    == 2
        })
        .ok_or("startup ENOSPC fixture sources did not become ready")?;
    engine.send(EngineCommand::Record);
    // Preserve the original early event ordering: it reaches the engine before the asynchronous
    // Paused/Playing state workers can all settle.
    engine.inject_storage_failure_for_selftest(InputKey::Source(SourceId(1)));
    let stopped = harness
        .wait_for(Duration::from_secs(30), |snapshot| {
            snapshot.phase == Phase::Idle
                && snapshot.session.as_ref().is_some_and(|session| {
                    session.segments.last().is_some_and(|segment| {
                        matches!(segment.status, SegmentStatus::Incomplete { .. })
                    })
                })
        })
        .ok_or("startup ENOSPC event did not stop the segment")?;
    let session = stopped
        .session
        .as_ref()
        .ok_or("startup ENOSPC omitted session")?;
    let segment = session
        .segments
        .last()
        .ok_or("startup ENOSPC omitted segment")?;
    let mut preserved = Vec::new();
    let mut unindexed_paths = Vec::new();
    let disc = pbutils::Discoverer::new(gst::ClockTime::from_seconds(5))
        .map_err(|error| format!("Discoverer: {error}"))?;
    for file in &segment.files {
        if !file.path.exists() {
            continue;
        }
        let bytes = std::fs::metadata(&file.path)
            .map_err(|error| error.to_string())?
            .len();
        if bytes == 0 {
            continue;
        }
        let media = match studio::recovery::verify_matroska_index(&file.path) {
            Ok(()) => verify_preserved_media(&disc, &file.path, file.input).map_err(|error| {
                format!(
                    "indexed startup peer {} is not playable: {error}",
                    file.path.display()
                )
            })?,
            Err(error) => {
                unindexed_paths.push(file.path.clone());
                serde_json::json!({ "index_error": error.to_string() })
            }
        };
        preserved.push(serde_json::json!({
            "path": file.path.display().to_string(),
            "bytes": bytes,
            "status": format!("{:?}", file.status),
            "media": media,
        }));
    }
    if preserved.is_empty() {
        return Err("startup ENOSPC left no opened peer files to inspect".into());
    }
    let session_dir = session.dir.clone();
    let shutdown = engine.shutdown_blocking(Duration::from_secs(5));
    if !shutdown {
        return Err("startup ENOSPC test engine did not shut down cleanly".into());
    }
    let candidates = studio::session::scan_interrupted(&root).map_err(|error| error.to_string())?;
    let discoverable = candidates
        .iter()
        .any(|candidate| candidate.dir == session_dir);
    if !unindexed_paths.is_empty() && !discoverable {
        return Err("unindexed startup peer files were not offered for recovery".into());
    }
    if discoverable {
        for path in unindexed_paths {
            if !path.exists() {
                return Err(format!(
                    "recovery candidate hid peer file {}",
                    path.display()
                ));
            }
            let current = std::fs::metadata(&path)
                .map_err(|error| error.to_string())?
                .len();
            if current == 0 {
                return Err(format!(
                    "recovery candidate hid footage in {}",
                    path.display()
                ));
            }
        }
    }
    Ok(serde_json::json!({
        "ok": true,
        "injected": "engine ENOSPC event seam; no hardware-volume or filesystem-capacity test",
        "opened_peer_files_retained": preserved,
        "recovery_candidate_when_unindexed": discoverable,
    }))
}

struct SelftestChild(std::process::Child);

impl Drop for SelftestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_child(mode: &str, root: &Path, ready: &Path) -> Result<SelftestChild, String> {
    std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .arg(mode)
        .arg(root)
        .arg(ready)
        .env("CHINSTRAP_PREVIEW", "0")
        .env("CHINSTRAP_TEST_SIZE", "320x240")
        .env("CHINSTRAP_SCREEN_FPS", "30")
        .spawn()
        .map(SelftestChild)
        .map_err(|e| format!("cannot start {mode} child: {e}"))
}

fn wait_child_ready(
    child: &mut SelftestChild,
    ready: &Path,
    timeout: Duration,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready.exists() {
            return std::fs::read_to_string(ready).map_err(|e| e.to_string());
        }
        if let Some(status) = child.0.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("self-test child exited before readiness: {status}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err("timed out waiting for self-test child readiness".into())
}

fn wait_child_exit(
    child: &mut SelftestChild,
    timeout: Duration,
) -> Result<std::process::ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.0.try_wait().map_err(|e| e.to_string())? {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err("self-test child did not exit before its deadline".into())
}

fn check_kill_and_recover(base: &Path) -> Result<serde_json::Value, String> {
    use std::os::unix::process::ExitStatusExt;

    let root = scenario_root(base, "crash-recovery");
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let ready = root.join("child.ready");
    let mut child = spawn_child("--selftest-crash-child", &root, &ready)?;
    let ready_data = wait_child_ready(&mut child, &ready, Duration::from_secs(60))?;
    let mut lines = ready_data.lines();
    let session_dir = PathBuf::from(lines.next().ok_or("child omitted its session path")?);
    let started_ns: u128 = lines
        .next()
        .ok_or("child omitted its recording start time")?
        .parse()
        .map_err(|e| format!("bad child start time: {e}"))?;
    let killed_at_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    child
        .0
        .kill()
        .map_err(|e| format!("cannot SIGKILL fixture: {e}"))?;
    let status = child
        .0
        .wait()
        .map_err(|e| format!("cannot wait for SIGKILL fixture: {e}"))?;
    if status.signal() != Some(libc::SIGKILL) {
        return Err(format!(
            "crash fixture was not killed with SIGKILL: {status}"
        ));
    }
    let kill_elapsed_ns = killed_at_ns
        .saturating_sub(started_ns)
        .min(u64::MAX as u128) as u64;

    let candidates = studio::session::scan_interrupted(&root).map_err(|e| e.to_string())?;
    if candidates.len() != 1 || candidates[0].dir != session_dir {
        return Err(format!(
            "killed session was not offered for recovery: {candidates:?}"
        ));
    }
    let manifest_path = session_dir.join(studio::session::MANIFEST_NAME);
    let checkpoint_age = std::time::SystemTime::now()
        .duration_since(
            std::fs::metadata(&manifest_path)
                .and_then(|meta| meta.modified())
                .map_err(|e| e.to_string())?,
        )
        .unwrap_or_default();
    if checkpoint_age > Duration::from_secs(6) {
        return Err(format!("checkpoint is stale: {checkpoint_age:?}"));
    }
    let before = studio::session::read_manifest(&manifest_path).map_err(|e| e.to_string())?;
    let segment = before
        .segments
        .iter()
        .find(|segment| segment.index == 1)
        .ok_or("crash checkpoint omitted segment 1")?;
    if segment.status != "recording" {
        return Err(format!(
            "crash checkpoint segment status is {:?}",
            segment.status
        ));
    }
    let video_input = segment
        .inputs
        .iter()
        .find(|input| input.kind != "microphone")
        .ok_or("crash checkpoint omitted video input")?;
    let video = session_dir.join(&segment.dir).join(&video_input.file);
    if video_input.status != "recording" || video_input.last_pts_ns.is_none() {
        return Err(format!("video checkpoint was not fresh: {video_input:?}"));
    }
    let checkpoint_pts = video_input.last_pts_ns.unwrap();
    let actual_pts = frame_times(&video)?
        .last()
        .map(|frame| frame.0)
        .ok_or("no persisted frames in killed video file")?;
    let checkpoint_lag_ns = actual_pts.abs_diff(checkpoint_pts);
    if checkpoint_lag_ns > 5_000_000_000 {
        return Err(format!(
            "last_pts checkpoint lagged the last frame by {checkpoint_lag_ns} ns"
        ));
    }
    let last_frame_to_kill_ns = kill_elapsed_ns.checked_sub(actual_pts).ok_or_else(|| {
        format!(
            "last persisted video frame PTS {actual_pts} ns is after SIGKILL at {kill_elapsed_ns} ns"
        )
    })?;
    if last_frame_to_kill_ns > 2_000_000_000 {
        return Err(format!(
            "last persisted video frame was {last_frame_to_kill_ns} ns before SIGKILL, over the 2 s test bound"
        ));
    }
    let audio_input = segment
        .inputs
        .iter()
        .find(|input| input.kind == "microphone");
    let audio_path = audio_input.map(|input| session_dir.join(&segment.dir).join(&input.file));
    if let Some(input) = audio_input {
        if input.status != "recording" {
            return Err(format!("audio checkpoint was not recording: {input:?}"));
        }
        if input.last_pts_ns.is_none() {
            return Err("crash checkpoint omitted the microphone's last timestamp".into());
        }
    }
    if studio::recovery::verify_matroska_index(&video).is_ok() {
        return Err("SIGKILL fixture unexpectedly already had Cues and Duration".into());
    }
    let recovery = studio::recovery::recover_session(&session_dir)?;
    if recovery.recovered_files == 0 || recovery.failed_files != 0 {
        return Err(format!("kill-and-recover result was {recovery:?}"));
    }
    let after = studio::session::read_manifest(&manifest_path).map_err(|e| e.to_string())?;
    let recovered_segment = after
        .segments
        .iter()
        .find(|segment| segment.index == 1)
        .ok_or("recovery omitted segment 1")?;
    if recovered_segment.status != "incomplete"
        || recovered_segment.reason.as_deref() != Some("recovered after interruption")
        || recovered_segment
            .inputs
            .iter()
            .any(|input| input.status != "complete")
    {
        return Err(format!(
            "recovery manifest is not terminal and repaired: {recovered_segment:?}"
        ));
    }
    let mut recovered_files = 0;
    for input in &recovered_segment.inputs {
        let path = session_dir.join(&recovered_segment.dir).join(&input.file);
        studio::recovery::verify_recovered_file(&path)?;
        recovered_files += 1;
    }
    let duration_ns = recovered_segment.duration_ns.unwrap_or(0);
    if duration_ns == 0 || duration_ns.abs_diff(kill_elapsed_ns) > 2_000_000_000 {
        return Err(format!(
            "recovered media duration {duration_ns} ns is not within two seconds of kill time {kill_elapsed_ns} ns"
        ));
    }
    if frame_diffs(&video)?.is_empty() {
        return Err("recovered video did not decode any frames".into());
    }
    let audio_duration_s = if let Some(path) = audio_path {
        let disc = pbutils::Discoverer::new(gst::ClockTime::from_seconds(5))
            .map_err(|e| format!("Discoverer: {e}"))?;
        let audio = check_audio(&disc, &path)?;
        decode_audio(&path)?;
        let duration = audio.duration.ok_or("recovered audio has no duration")?;
        if duration.is_zero() {
            return Err("recovered audio has zero duration".into());
        }
        Some(duration.as_secs_f64())
    } else {
        None
    };
    if !studio::session::scan_interrupted(&root)
        .map_err(|e| e.to_string())?
        .is_empty()
    {
        return Err("successfully recovered session remained a candidate".into());
    }
    Ok(serde_json::json!({
        "ok": true,
        "child_signal": "SIGKILL",
        "recovered_files": recovered_files,
        "duration_s": duration_ns as f64 / 1e9,
        "kill_elapsed_s": kill_elapsed_ns as f64 / 1e9,
        "last_frame_to_kill_s": last_frame_to_kill_ns as f64 / 1e9,
        "checkpoint_age_s": checkpoint_age.as_secs_f64(),
        "checkpoint_lag_s": checkpoint_lag_ns as f64 / 1e9,
        "audio_recovered_and_discoverable": audio_duration_s.is_some(),
        "audio_duration_s": audio_duration_s,
        "cues_duration_verified": true,
        "playable": true,
    }))
}

fn check_signal_finalization(base: &Path) -> Result<serde_json::Value, String> {
    let mut signals = Vec::new();
    for (name, signal) in [
        ("SIGTERM", libc::SIGTERM),
        ("SIGINT", libc::SIGINT),
        ("SIGHUP", libc::SIGHUP),
    ] {
        signals.push(check_signal_finalization_one(base, name, signal)?);
    }
    Ok(serde_json::json!({"ok": true, "signals": signals}))
}

fn check_signal_finalization_one(
    base: &Path,
    signal_name: &str,
    signal_number: libc::c_int,
) -> Result<serde_json::Value, String> {
    use std::os::unix::process::ExitStatusExt;

    let root = scenario_root(
        base,
        &format!("signal-{}", signal_name.to_ascii_lowercase()),
    );
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let ready = root.join("child.ready");
    let mut child = spawn_child("--selftest-signal-child", &root, &ready)?;
    let ready_data = wait_child_ready(&mut child, &ready, Duration::from_secs(60))?;
    let session_dir = PathBuf::from(
        ready_data
            .lines()
            .next()
            .ok_or("signal child omitted its session path")?,
    );
    if !studio::session::scan_interrupted(&root)
        .map_err(|e| e.to_string())?
        .is_empty()
    {
        return Err("live recording lock did not suppress recovery discovery".into());
    }
    // SAFETY: sending the selected handled signal to the explicitly spawned self-test child PID.
    if unsafe { libc::kill(child.0.id() as libc::pid_t, signal_number) } != 0 {
        return Err(format!(
            "cannot send {signal_name} to self-test child: {}",
            std::io::Error::last_os_error()
        ));
    }
    let status = wait_child_exit(&mut child, Duration::from_secs(12))?;
    if status.code() != Some(128 + signal_number) || status.signal().is_some() {
        return Err(format!("{signal_name} child exited unexpectedly: {status}"));
    }
    let manifest =
        studio::session::read_manifest(&session_dir.join(studio::session::MANIFEST_NAME))
            .map_err(|e| e.to_string())?;
    let segment = manifest
        .segments
        .iter()
        .find(|segment| segment.index == 1)
        .ok_or("signal shutdown manifest omitted segment 1")?;
    if !matches!(segment.status.as_str(), "complete" | "incomplete")
        || segment.inputs.iter().any(|input| {
            matches!(
                input.status.as_str(),
                "starting" | "recording" | "finalizing"
            )
        })
    {
        return Err(format!(
            "signal shutdown left nonterminal manifest state: {segment:?}"
        ));
    }
    let disc = pbutils::Discoverer::new(gst::ClockTime::from_seconds(5))
        .map_err(|e| format!("Discoverer: {e}"))?;
    let mut playable_video = None;
    let mut verified_inputs = Vec::new();
    let mut audio_verified = false;
    for input in &segment.inputs {
        if input.status != "complete" {
            return Err(format!(
                "handled {signal_name} left input {} in state {}: {:?}",
                input.key, input.status, input.error
            ));
        }
        let path = session_dir.join(&segment.dir).join(&input.file);
        let media = verify_preserved_media(
            &disc,
            &path,
            if input.kind == "microphone" {
                InputKey::Microphone
            } else {
                InputKey::Camera
            },
        )?;
        if input.kind == "microphone" {
            audio_verified = true;
        } else {
            playable_video = Some(path);
        }
        verified_inputs.push(media);
    }
    if playable_video.is_none() {
        return Err("signal shutdown did not finalize a playable video".into());
    }
    let reserve_released = !session_dir.join(".chinstrap-reserve").exists();
    if !reserve_released {
        return Err("signal shutdown left the emergency reserve behind".into());
    }
    if !studio::session::scan_interrupted(&root)
        .map_err(|e| e.to_string())?
        .is_empty()
    {
        return Err("signal-finalized session was still offered as interrupted".into());
    }
    Ok(serde_json::json!({
        "ok": true,
        "signal": signal_name,
        "exit_code": status.code(),
        "terminal_manifest": true,
        "verified_inputs": verified_inputs,
        "audio_finalized_and_playable": audio_verified,
        "cues_duration_verified": true,
        "playable": true,
        "reserve_released": reserve_released,
        "live_lock_suppressed_recovery": true,
    }))
}

/// Preview frames of `keys` over [`PREVIEW_WINDOW`] (after each has its first one): enough of
/// them, the newest from `origin`, 480×270-ish BGRA with opaque alpha and real content.
/// `silent` must have none. Recording: at least [`PREVIEW_MIN_FPS`], except for `late` (whose
/// source only starts 1.5 s in).
fn check_previews(
    h: &mut Harness,
    hub: &Previews,
    keys: &[InputKey],
    silent: InputKey,
    origin: PreviewOrigin,
    late: Option<InputKey>,
) -> serde_json::Value {
    let what = match origin {
        PreviewOrigin::Idle => "idle",
        PreviewOrigin::Recording { .. } => "recording",
    };
    let from = Instant::now();
    let start: Vec<u64> = keys.iter().map(|k| hub.generation(*k)).collect();
    if origin == PreviewOrigin::Idle {
        // Idle previews start once the sources are Ready.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && keys.iter().any(|k| hub.generation(*k) == 0) {
            h.sleep(Duration::from_millis(50));
        }
    }
    let start = if origin == PreviewOrigin::Idle {
        keys.iter().map(|k| hub.generation(*k)).collect()
    } else {
        start
    };
    let t0 = Instant::now();
    h.sleep(PREVIEW_WINDOW);
    let secs = t0.elapsed().as_secs_f64();
    let mut out = serde_json::Map::new();
    for (key, g0) in keys.iter().zip(start) {
        let Some((g1, frame)) = hub.latest(*key) else {
            h.fail(format!("{what} preview of {key:?}: no frame"));
            continue;
        };
        let frames = g1 - g0;
        let fps = frames as f64 / secs;
        let mut problems = Vec::new();
        if frames < PREVIEW_MIN_FRAMES {
            problems.push(format!("{frames} frames in {secs:.1} s"));
        }
        if matches!(origin, PreviewOrigin::Recording { .. })
            && Some(*key) != late
            && fps < PREVIEW_MIN_FPS
        {
            problems.push(format!("{fps:.1} fps"));
        }
        if frame.origin != origin || frame.at < from {
            problems.push(format!("newest frame is from {:?}", frame.origin));
        }
        let (w, hh) = (frame.width, frame.height);
        if w > 480 || hh > 300 || (w != 480 && hh != 300) {
            problems.push(format!("{w}×{hh}, expected to fit 480×300"));
        }
        if frame.bgra.len() != (w * hh * 4) as usize
            || frame.bgra.as_chunks::<4>().0.iter().any(|p| p[3] != 255)
        {
            problems.push("not opaque BGRA".into());
        }
        let luma: Vec<f64> = frame
            .bgra
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| 0.114 * p[0] as f64 + 0.587 * p[1] as f64 + 0.299 * p[2] as f64)
            .collect();
        let mean = luma.iter().sum::<f64>() / luma.len().max(1) as f64;
        let variance =
            luma.iter().map(|l| (l - mean).powi(2)).sum::<f64>() / luma.len().max(1) as f64;
        if variance < 100.0 {
            problems.push(format!("flat image (luma variance {variance:.0})"));
        }
        eprintln!(
            "selftest: {what} preview {key:?}: {frames} frames in {secs:.1} s ({fps:.1} fps),              {w}×{hh}, luma variance {variance:.0}"
        );
        for p in &problems {
            h.fail(format!("{what} preview of {key:?}: {p}"));
        }
        out.insert(
            format!("{key:?}"),
            serde_json::json!({
                "frames": frames, "fps": (fps * 10.0).round() / 10.0,
                "size": format!("{w}×{hh}"), "variance": variance.round(),
            }),
        );
    }
    let silent_frames = hub.generation(silent);
    if silent_frames != 0 {
        h.fail(format!(
            "{what}: the silent source has {silent_frames} preview frames"
        ));
    }
    out.insert("silent_frames".into(), serde_json::json!(silent_frames));
    serde_json::Value::Object(out)
}

/// Segment 1's mic transcript through the engine (only when the model is already present; the
/// selftest never downloads), plus `CHINSTRAP_SELFTEST_SPEECH=<mkv>:<ref.txt>` through the CLI
/// driver with a WER gate.
fn check_transcription(
    h: &mut Harness,
    warnings: &mut Vec<String>,
    engine: &studio::EngineHandle,
    manifest: &Path,
) -> serde_json::Value {
    use studio::transcribe::{model, wer};
    let mut out = serde_json::Map::new();
    let require = std::env::var("CHINSTRAP_SELFTEST_REQUIRE_MODEL").is_ok_and(|v| v == "1");
    // The startup check hashes the model once (about 2 s) unless its sidecar is current.
    let snap = h.wait_for(Duration::from_secs(60), |s| {
        s.transcription.model != ModelState::Checking
    });
    let usable = snap
        .as_ref()
        .is_some_and(|s| s.transcription.model.usable());
    if !usable {
        let state = snap.map(|s| format!("{:?}", s.transcription.model));
        let msg = format!("transcription skipped: model not downloaded ({state:?})");
        if require {
            h.fail(msg.clone());
        } else {
            warnings.push(msg.clone());
        }
        out.insert("segment_1".into(), msg.into());
    } else {
        let seg1 = |s: &Snapshot| {
            s.session
                .as_ref()
                .and_then(|x| x.segments.iter().find(|g| g.index == 1))
                .map(|g| g.transcript.clone())
        };
        if let Some(s) = &h.snap
            && matches!(
                seg1(s),
                Some(TranscriptStatus::None | TranscriptStatus::Failed { .. })
            )
        {
            engine.send(EngineCommand::Transcribe { segment: 1 });
        }
        let t0 = Instant::now();
        let done = h.wait_for(Duration::from_secs(60), |s| {
            matches!(
                seg1(s),
                Some(TranscriptStatus::Done { .. } | TranscriptStatus::Failed { .. })
            )
        });
        match done.as_ref().and_then(seg1) {
            Some(TranscriptStatus::Done {
                json,
                srt,
                vtt,
                language,
                words,
                backend,
            }) => {
                eprintln!(
                    "selftest: segment 1 transcript: {words} words, language {language:?}, {backend:?}, {:.1}s",
                    t0.elapsed().as_secs_f64()
                );
                let parsed: Option<serde_json::Value> = std::fs::read(&json)
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok());
                match &parsed {
                    Some(v) if v["format"] == "chinstrap-transcript" && v["version"] == 1 => {}
                    _ => h.fail(format!("{}: not a chinstrap-transcript v1", json.display())),
                }
                let vtt_ok = std::fs::read_to_string(&vtt).is_ok_and(|t| t.starts_with("WEBVTT"));
                if !srt.is_file() || !vtt_ok {
                    h.fail("segment 1: SRT/VTT missing or malformed");
                }
                if words > 0 {
                    warnings.push(format!(
                        "segment 1 (room-noise mic) transcribed to {words} words: {:?}",
                        parsed.as_ref().map(|v| v["text"].clone())
                    ));
                }
                let status = studio_manifest(manifest).ok().and_then(|m| {
                    m["segments"][0]["inputs"]
                        .as_array()?
                        .iter()
                        .find(|i| i["key"] == "mic")
                        .map(|i| i["transcript"]["status"].clone())
                });
                if status != Some(serde_json::json!("done")) {
                    h.fail(format!(
                        "manifest: segment 1 mic transcript status is {status:?}"
                    ));
                }
                out.insert(
                    "segment_1".into(),
                    serde_json::json!({
                        "words": words, "language": language,
                        "backend": format!("{backend:?}").to_lowercase(),
                        "seconds": t0.elapsed().as_secs_f64(),
                        "rtf": parsed.as_ref().map(|v| v["timing"]["rtf"].clone()),
                        "speech_spans": parsed.as_ref().map(|v| v["speech"].as_array().map_or(0, |a| a.len())),
                    }),
                );
            }
            Some(TranscriptStatus::Failed { reason }) => {
                h.fail(format!("segment 1 transcription failed: {reason}"))
            }
            other => h.fail(format!(
                "segment 1 transcription did not finish within 60 s ({other:?})"
            )),
        }
    }
    if let Ok(spec) = std::env::var("CHINSTRAP_SELFTEST_SPEECH")
        && let Some((mkv, reference)) = spec.rsplit_once(':')
    {
        let (resolved, _) = model::resolve_env();
        let reference_text = std::fs::read_to_string(reference).unwrap_or_default();
        match studio::transcribe::transcribe_file(Path::new(mkv), &resolved, None, false, true) {
            Ok(r) => {
                let text = std::fs::read(&r.json)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    .and_then(|v| v["text"].as_str().map(String::from))
                    .unwrap_or_default();
                let (e, n) = wer::wer(&reference_text, &text);
                let rate = wer::wer_rate(&reference_text, &text);
                eprintln!(
                    "selftest: speech WER {e}/{n} = {rate:.3} ({}): {text:?}",
                    r.backend
                );
                if rate > 0.10 {
                    h.fail(format!("speech WER {rate:.3} > 0.10: {text:?}"));
                }
                let style = check_transcript_style(h, &r.json, &text);
                out.insert(
                    "speech".into(),
                    serde_json::json!({"wer": rate, "errors": e, "words": n, "backend": r.backend, "rtf": r.rtf, "text": text, "style": style}),
                );
            }
            Err((_, msg)) => {
                if require || usable {
                    h.fail(format!("speech transcription failed: {msg}"));
                } else {
                    warnings.push(format!("speech check skipped: {msg}"));
                }
            }
        }
    }
    out.into()
}

/// P0-2/P0-3/P0-4 on a transcript: cased and punctuated text, no word starting in silence,
/// no text left that a replacement rule should have changed, and the words of
/// `CHINSTRAP_SELFTEST_SPEECH_EXPECT` (comma-separated) present.
fn check_transcript_style(h: &mut Harness, json: &Path, text: &str) -> serde_json::Value {
    use studio::transcribe::vocab;
    let t: serde_json::Value = std::fs::read(json)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let cased = text.chars().any(char::is_uppercase);
    let punctuated = text.chars().any(|c| matches!(c, '.' | ',' | '?' | '!'));
    if !cased || !punctuated {
        h.fail(format!("transcript is not cased and punctuated: {text:?}"));
    }
    let spans: Vec<(f64, f64)> = t["speech"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| Some((s["start"].as_f64()?, s["end"].as_f64()?)))
                .collect()
        })
        .unwrap_or_default();
    let mut in_silence = Vec::new();
    for seg in t["segments"].as_array().cloned().unwrap_or_default() {
        for w in seg["words"].as_array().cloned().unwrap_or_default() {
            let start = w["start"].as_f64().unwrap_or(0.0);
            if !spans.is_empty() && !spans.iter().any(|(a, b)| *a - 0.05 <= start && start < *b) {
                in_silence.push(format!("{}@{start}", w["word"]));
            }
        }
    }
    if !in_silence.is_empty() {
        h.fail(format!("words start in silence: {in_silence:?}"));
    }
    let (rules, _) = vocab::load_replacements(&vocab::replacements_path());
    let mut leftover = Vec::new();
    for r in &rules {
        let to_has_from = vocab::replace_text(&r.to, r).1 > 0;
        if !to_has_from && vocab::replace_text(text, r).1 > 0 {
            leftover.push(r.from.clone());
        }
    }
    if !leftover.is_empty() {
        h.fail(format!(
            "replacement rules not applied: {leftover:?} in {text:?}"
        ));
    }
    let expect: Vec<String> = std::env::var("CHINSTRAP_SELFTEST_SPEECH_EXPECT")
        .unwrap_or_default()
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect();
    for w in &expect {
        if !text.contains(w.as_str()) {
            h.fail(format!("transcript lacks {w:?}: {text:?}"));
        }
    }
    serde_json::json!({
        "cased": cased, "punctuated": punctuated,
        "first_word_s": t["segments"][0]["words"][0]["start"],
        "speech": spans.len(), "rules": rules.len(), "expect": expect,
        "replacements": t["replacements"], "prompt": t["prompt"]["text"],
    })
}

/// PipeWire targeting regression (portal `target-object` bug):
/// - a missing node id must fail with WirePlumber's "target not found"; any other error (for
///   example EBUSY from a fallback to the busy default camera) fails the selftest, unless
///   PipeWire itself is unreachable;
/// - a temporary Video/Source node's *object id* must be the only node linked (always runs, so
///   the positive check never depends on a camera being attached);
/// - with `camera`, that camera's object id must be the only node linked.
fn check_targeting(
    h: &mut Harness,
    warnings: &mut Vec<String>,
    camera: Option<&str>,
) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    match studio::check_targeting(999_999_999, Duration::from_secs(10)) {
        Ok(c) => h.fail(format!(
            "targeting a missing node fell back to another node (linked {:?}, caps {})",
            c.linked_from, c.caps
        )),
        Err(e) if e.contains("target not found") => {
            eprintln!("selftest: targeting a missing node fails loudly: {e}");
            out.insert("missing_node".into(), "fails".into());
        }
        Err(e) if pipewire_unreachable(&e) => {
            warnings.push(format!(
                "targeting check skipped: PipeWire unreachable: {e}"
            ));
            out.insert("missing_node".into(), format!("skipped: {e}").into());
            out.insert("test_node".into(), "skipped: PipeWire unreachable".into());
            return out.into();
        }
        Err(e) => h.fail(format!(
            "targeting a missing node failed with {e:?} instead of \"target not found\" \
             (did it fall back to another node?)"
        )),
    }
    match studio::provide_test_node(Duration::from_secs(5)) {
        Ok(node) => {
            let value = linked_check(h, warnings, "test node", node.id);
            out.insert("test_node".into(), value);
        }
        Err(e) => {
            warnings.push(format!(
                "targeting: positive check skipped (no test node: {e})"
            ));
            out.insert("test_node".into(), format!("skipped: {e}").into());
        }
    }
    let Some(want) = camera else {
        out.insert("camera".into(), "skipped: no camera named".into());
        return out.into();
    };
    let Some((name, id)) = camera_object_id(want) else {
        h.fail(format!("no PipeWire object.id for camera {want:?}"));
        return out.into();
    };
    let value = linked_check(h, warnings, &name, id);
    out.insert("camera".into(), value);
    out.into()
}

/// pipewiresrc errors that mean the daemon could not be reached at all.
fn pipewire_unreachable(e: &str) -> bool {
    e.contains("Failed to connect") || e.contains("can't create stream")
}

/// Targets object id `id` and checks that it is the only node linked.
fn linked_check(
    h: &mut Harness,
    warnings: &mut Vec<String>,
    name: &str,
    id: u32,
) -> serde_json::Value {
    match studio::check_targeting(id, Duration::from_secs(10)) {
        Ok(c) => {
            eprintln!(
                "selftest: targeting {name} (object.id {id}) linked {:?}, caps {}",
                c.linked_from, c.caps
            );
            let verdict = match &c.linked_from {
                Some(l) if l == &vec![id] => "ok",
                Some(l) => {
                    h.fail(format!(
                        "targeting {name} (object.id {id}) linked {l:?} instead of only {id}"
                    ));
                    "wrong node"
                }
                None => {
                    warnings.push(format!(
                        "pw-dump unavailable: linked node for {name} not verified"
                    ));
                    "unverified"
                }
            };
            serde_json::json!({
                "object_id": id,
                "linked_from": c.linked_from,
                "caps": c.caps,
                "result": verdict,
            })
        }
        Err(e) => {
            h.fail(format!("targeting {name} (object.id {id}) failed: {e}"));
            serde_json::json!({ "object_id": id, "result": format!("failed: {e}") })
        }
    }
}

/// P1-6/P1-7 manifest facts: the late source's first_pts is its real start and its file was
/// padded; the native source is recorded as variable rate; every video input has quality.
fn check_manifest_extras(h: &mut Harness, path: &Path) {
    let Ok(m) = studio_manifest(path) else {
        return;
    };
    let seg1 = &m["segments"][0]["inputs"];
    let find = |key: &str| {
        seg1.as_array()
            .and_then(|a| a.iter().find(|i| i["key"] == key).cloned())
            .unwrap_or_default()
    };
    let late = find(&format!("source:{LATE_ID}"));
    let first = late["first_pts_ns"].as_u64().unwrap_or(0);
    if first < LATE.as_nanos() as u64 - 200_000_000 {
        h.fail(format!(
            "late source first_pts_ns {first} is not its real (late) start"
        ));
    }
    if late["quality"]["padded"].as_u64().unwrap_or(0) == 0 {
        h.fail(format!(
            "late source was not padded to zero: {}",
            late["quality"]
        ));
    }
    let native = find(&format!("source:{NATIVE_ID}"));
    if native["framerate"] != "variable" {
        h.fail(format!(
            "native source framerate is {}",
            native["framerate"]
        ));
    }
    for i in seg1.as_array().cloned().unwrap_or_default() {
        if i["codec"] != "opus" && i["status"] == "complete" && i["quality"].is_null() {
            h.fail(format!("{} has no quality in the manifest", i["key"]));
        }
    }
    let silent = find(&format!("source:{SILENT_ID}"));
    if silent["status"] != "failed" {
        h.fail(format!(
            "silent source manifest status {}",
            silent["status"]
        ));
    }
}

/// Load windows' frame-rate mode: `CHINSTRAP_SELFTEST_LOAD_MODE` = native (default, the app's
/// default for windows: VFR up to a 144 Hz refresh), 30 or 60.
fn load_mode() -> FrameRateMode {
    match std::env::var("CHINSTRAP_SELFTEST_LOAD_MODE")
        .as_deref()
        .map(str::trim)
    {
        Ok("30") => FrameRateMode::Cfr30,
        Ok("60") => FrameRateMode::Cfr60,
        _ => FrameRateMode::Native,
    }
}

/// Repeats among consecutive-frame differences after `from_ns` (the start padding before the
/// first real frame is excluded): (repeats, pairs, percent, median diff).
fn repeat_stats(diffs: &[(u64, f64)], from_ns: u64) -> (usize, usize, f64, f64) {
    let kept: Vec<f64> = diffs
        .iter()
        .filter(|(pts, _)| *pts > from_ns)
        .map(|x| x.1)
        .collect();
    let repeats = kept.iter().filter(|d| **d < REPEAT_DIFF).count();
    let pct = repeats as f64 * 100.0 / kept.len().max(1) as f64;
    let mut sorted = kept.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
    (repeats, kept.len(), pct, median)
}

/// P0-1 acceptance: the named camera plus two 4K test windows (Native at a 144 Hz refresh by
/// default, like real niri windows on this host's monitors) for 60 s: < 1% repeated camera
/// frames by the counters and by consecutive-frame differences over the whole recording (only
/// the start padding before the camera's first real frame is left out). Logs the plan.
///
/// `CHINSTRAP_SELFTEST_LOAD=standin` (no camera named) records a third 4K test pattern at 30 fps
/// CFR instead of a camera: it exercises the planner and encoders, not the camera's path.
///
/// Also checks the Native bitrate: bytes per frame of a Native window are within 2x of a CFR
/// input's with the same content (the stand-in), and never below half of it.
fn check_load(
    h: &mut Harness,
    mut config: studio::EngineConfig,
    dir: &Path,
    want: &str,
) -> serde_json::Value {
    let secs: u64 = std::env::var("CHINSTRAP_SELFTEST_LOAD_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(60);
    let standin = want.eq_ignore_ascii_case("standin");
    let mode = load_mode();
    let window = TestBehaviour {
        size: Some((3840, 2160)),
        refresh: Some(144),
        keep_size: true,
        ..Default::default()
    };
    let uhd = TestBehaviour {
        size: Some((3840, 2160)),
        keep_size: true,
        ..Default::default()
    };
    config.test_sources = vec![(1, window), (2, window), (3, uhd)];
    config.output_root = dir.join("load");
    let (engine, rx) = match studio::start(config) {
        Ok(x) => x,
        Err(e) => {
            h.fail(format!("load: engine: {e}"));
            return serde_json::Value::Null;
        }
    };
    let mut l = Harness {
        rx,
        snap: None,
        levels: 0,
        errors: Vec::new(),
        notices: Vec::new(),
    };
    let result = (|| -> Result<serde_json::Value, String> {
        let first = l
            .wait_for(Duration::from_secs(60), |_| true)
            .ok_or("no first Snapshot")?;
        let cam = if standin {
            None
        } else {
            Some(
                first
                    .cameras
                    .iter()
                    .find(|c| c.name.to_lowercase().contains(&want.to_lowercase()))
                    .ok_or("camera gone")?
                    .id,
            )
        };
        engine.send(EngineCommand::SelectCamera(cam));
        engine.send(EngineCommand::SelectMicrophone(
            first.microphones.first().map(|m| m.id),
        ));
        let n = if standin { 3 } else { 2 };
        for _ in 0..n {
            engine.send(EngineCommand::AddTestSource);
        }
        for id in [1, 2] {
            engine.send(EngineCommand::SetFrameRateMode {
                source: Some(SourceId(id)),
                mode,
            });
        }
        if standin {
            engine.send(EngineCommand::SetFrameRateMode {
                source: Some(SourceId(3)),
                mode: FrameRateMode::Cfr30,
            });
        }
        let ready = l
            .wait_for(Duration::from_secs(10), |s| {
                s.sources
                    .iter()
                    .filter(|x| x.state == SourceState::Ready)
                    .count()
                    == n
                    && s.sources
                        .iter()
                        .filter(|x| x.id.0 <= 2 && x.frame_rate == mode)
                        .count()
                        == 2
                    && s.selected_camera == cam
                    && s.can_record
            })
            .ok_or("load sources never ready")?;
        eprintln!(
            "selftest: load ({mode:?} windows): plan {:?}",
            ready.capacity
        );
        let plan = serde_json::json!({
            "gpu_load_pct": ready.capacity.gpu_load_pct,
            "cpu_load_pct": ready.capacity.cpu_load_pct,
            "over": ready.capacity.over, "warning": ready.capacity.warning,
            "inputs": ready.capacity.inputs.iter().map(|p| serde_json::json!({
                "input": format!("{:?}", p.input), "encoder": p.encoder.element_name(),
                "fps": p.fps, "native": p.native})).collect::<Vec<_>>(),
        });
        engine.send(EngineCommand::Record);
        l.wait_for(Duration::from_secs(20), |s| {
            matches!(s.phase, Phase::Recording { .. })
        })
        .ok_or("load never reached Recording")?;
        l.sleep(Duration::from_secs(secs));
        let live_storage = l.snap.as_ref().map(|snapshot| snapshot.storage.clone());
        engine.send(EngineCommand::Stop);
        let done = l
            .wait_for(Duration::from_secs(30), |s| s.phase == Phase::Idle)
            .ok_or("load Stop never reached Idle")?;
        let seg = done
            .session
            .as_ref()
            .and_then(|s| s.segments.first().cloned())
            .ok_or("no load segment")?;
        let output_bytes = seg.files.iter().fold(0u64, |total, file| {
            total.saturating_add(
                std::fs::metadata(&file.path)
                    .map(|meta| meta.len())
                    .unwrap_or(0),
            )
        });
        let duration_secs = seg
            .duration
            .map(|duration| duration.as_secs_f64())
            .unwrap_or(secs as f64);
        let measured_rate = if duration_secs > 0.0 {
            output_bytes as f64 / duration_secs
        } else {
            0.0
        };
        let planned_rate = ready.storage.estimated_bytes_per_second as f64;
        let error_pct = if measured_rate > 0.0 {
            (planned_rate - measured_rate).abs() / measured_rate * 100.0
        } else {
            f64::INFINITY
        };
        let live_rate = live_storage
            .as_ref()
            .map(|storage| storage.estimated_bytes_per_second);
        let rate_was_measured = live_storage
            .as_ref()
            .is_some_and(|storage| storage.rate_measured);
        if !rate_was_measured {
            l.fail("4K load did not expose an on-disk measured storage rate");
        }
        if error_pct > 35.0 {
            l.fail(format!(
                "planned rate {:.0} B/s is {:.1}% from measured disk growth {:.0} B/s (limit 35%)",
                planned_rate, error_pct, measured_rate
            ));
        }
        let storage_rate_json = serde_json::json!({
            "planned_bytes_per_second": planned_rate,
            "measured_bytes_per_second": measured_rate,
            "live_measured_bytes_per_second": live_rate,
            "relative_error_pct": error_pct,
            "within_35_percent": error_pct <= 35.0 && rate_was_measured,
        });
        let reference = if standin {
            InputKey::Source(SourceId(3))
        } else {
            InputKey::Camera
        };
        let mut files = Vec::new();
        let mut camera = None;
        let mut per_frame = Vec::new();
        for f in &seg.files {
            let Some(q) = &f.quality else { continue };
            let bytes = std::fs::metadata(&f.path).map_or(0, |m| m.len());
            let bpf = bytes as f64 / q.encoded.max(1) as f64;
            files.push(serde_json::json!({
                "input": format!("{:?}", f.input), "status": format!("{:?}", f.status),
                "encoder": q.encoder.map(|e| e.element_name()),
                "max_fps": q.max_fps, "nominal_fps": q.nominal_fps,
                "frames_in": q.frames_in, "encoded": q.encoded, "duplicated": q.duplicated,
                "padded": q.padded, "dropped_queue": q.dropped_queue,
                "dropped_corrupt": q.dropped_corrupt, "effective_fps": q.effective_fps,
                "repeat_pct": q.repeat_pct, "bitrate_kbps": q.bitrate_kbps,
                "bytes_per_frame": bpf.round(),
            }));
            per_frame.push((f.input, q.nominal_fps.is_none(), bpf));
            if f.input == reference {
                if f.status != InputStatus::Complete {
                    l.fail(format!("load {reference:?} status {:?}", f.status));
                }
                camera = Some((q.clone(), f.path.clone()));
            }
        }
        let (q, path) = camera.ok_or("no camera quality")?;
        let counter_pct = q.repeat_pct;
        if counter_pct >= MAX_REPEAT_PCT {
            l.fail(format!(
                "{reference:?} repeated {counter_pct:.2}% of frames (counters)"
            ));
        }
        let diffs = frame_diffs(&path)?;
        // The camera's first real frame: everything before it is start padding.
        let from = q
            .start_offset
            .map_or(0, |d| d.as_nanos() as u64)
            .saturating_add(1_000_000);
        let (repeats, pairs, pct, median) = repeat_stats(&diffs, from);
        let (_, late_pairs, late_pct, _) = repeat_stats(&diffs, 5_000_000_000);
        eprintln!(
            "selftest: load: {reference:?} repeats {repeats}/{pairs} pairs over the whole \
             recording ({pct:.2}%; after 5 s {late_pct:.2}% of {late_pairs}), median diff \
             {median:.3}; counters {counter_pct:.2}%"
        );
        if pct >= MAX_REPEAT_PCT {
            l.fail(format!(
                "{reference:?}: {repeats} of {pairs} frame pairs are repeats ({pct:.2}%)"
            ));
        }
        // Native bitrate: per-frame budget comparable to the CFR input with the same content.
        let cfr_bpf = per_frame
            .iter()
            .find(|(k, native, _)| *k == reference && !native)
            .map(|x| x.2);
        if standin && let Some(cfr) = cfr_bpf {
            for (k, native, bpf) in &per_frame {
                if *native && !(cfr / 2.0..=cfr * 2.0).contains(bpf) {
                    l.fail(format!(
                        "{k:?}: Native {bpf:.0} bytes/frame vs CFR {cfr:.0} (not within 2x)"
                    ));
                }
            }
        }
        Ok(serde_json::json!({
            "seconds": secs, "windows": format!("{mode:?}"), "reference": format!("{reference:?}"),
            "plan": plan, "files": files,
            "storage_rate": storage_rate_json,
            "repeat_pct_counters": counter_pct,
            "repeat_pct_diff": pct, "pairs": pairs,
            "repeat_pct_diff_after_5s": late_pct,
            "median_diff": median,
        }))
    })();
    engine.shutdown_blocking(Duration::from_secs(10));
    l.drain();
    let out = match result {
        Ok(j) => j,
        Err(e) => {
            l.fail(format!("load: {e}"));
            serde_json::Value::Null
        }
    };
    for e in l.errors {
        h.fail(e);
    }
    out
}

fn studio_manifest(path: &Path) -> Result<serde_json::Value, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn check_manifest(
    h: &mut Harness,
    warnings: &mut Vec<String>,
    path: &Path,
    dir: &Path,
    test_id: u32,
    mic: bool,
    camera: bool,
) -> bool {
    let before = h.errors.len();
    let m = match studio_manifest(path) {
        Ok(m) => m,
        Err(e) => {
            h.fail(format!("manifest does not parse: {e}"));
            return false;
        }
    };
    let segs = m["segments"].as_array().cloned().unwrap_or_default();
    let complete = segs.iter().filter(|s| s["status"] == "complete").count();
    if complete != 2 || segs.len() != 2 {
        h.fail(format!(
            "manifest: expected 2 complete segments, found {complete} of {}",
            segs.len()
        ));
    }
    if let Some(gap) = segs.get(1).and_then(|s| s["gap_before_ns"].as_u64()) {
        eprintln!("selftest: gap before segment 2: {:.3}s", gap as f64 / 1e9);
        if gap >= 1_500_000_000 {
            h.fail(format!("gap_before {:.3}s ≥ 1.5s", gap as f64 / 1e9));
        }
    } else {
        h.fail("segment 2 has no gap_before_ns");
    }
    let test_key = format!("source:{test_id}");
    for s in &segs {
        let idx = s["index"].as_u64().unwrap_or(0);
        if s["base_time_ns"].as_u64().is_none() {
            h.fail(format!("segment {idx}: no base_time_ns"));
        }
        let seg_dir: PathBuf = dir.join(s["dir"].as_str().unwrap_or(""));
        let inputs = s["inputs"].as_array().cloned().unwrap_or_default();
        let listed: BTreeSet<String> = inputs
            .iter()
            .filter(|i| {
                i["status"] != "failed" || seg_dir.join(i["file"].as_str().unwrap_or("")).exists()
            })
            .filter_map(|i| i["file"].as_str().map(String::from))
            .collect();
        let on_disk: BTreeSet<String> = std::fs::read_dir(&seg_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.ends_with(".mkv"))
                    .collect()
            })
            .unwrap_or_default();
        if listed != on_disk {
            h.fail(format!(
                "segment {idx}: manifest files {listed:?} ≠ files on disk {on_disk:?}"
            ));
        }
        let first = |key: &str| {
            inputs
                .iter()
                .find(|i| i["key"] == key && i["status"] == "complete")
                .and_then(|i| i["first_pts_ns"].as_u64())
        };
        if mic {
            match (first(&test_key), first("mic")) {
                (Some(t), Some(a)) => {
                    let skew = t.abs_diff(a);
                    eprintln!(
                        "selftest: segment {idx} first_pts test={:.3}s mic={:.3}s skew={:.1}ms",
                        t as f64 / 1e9,
                        a as f64 / 1e9,
                        skew as f64 / 1e6
                    );
                    if skew >= 100_000_000 {
                        h.fail(format!(
                            "segment {idx}: first_pts skew {:.1} ms ≥ 100 ms",
                            skew as f64 / 1e6
                        ));
                    }
                }
                (t, a) => {
                    eprintln!("selftest: segment {idx}: skew not checked (test={t:?}, mic={a:?})")
                }
            }
        }
        // The camera's first frame includes its real start-up latency (skip-to-first), so it
        // is reported, and only warned about above 500 ms.
        if camera && let (Some(t), Some(c)) = (first(&test_key), first("camera")) {
            let skew = t.abs_diff(c);
            eprintln!(
                "selftest: segment {idx} first_pts camera={:.3}s skew to test={:.1}ms",
                c as f64 / 1e9,
                skew as f64 / 1e6
            );
            if skew >= 500_000_000 {
                warnings.push(format!(
                    "segment {idx}: camera first_pts skew {:.1} ms",
                    skew as f64 / 1e6
                ));
            }
        }
        for i in &inputs {
            if i["codec"] == "h264" || i["codec"] == "hevc" {
                eprintln!(
                    "selftest: segment {idx} {}: {}×{} @{} via {}",
                    i["key"].as_str().unwrap_or("?"),
                    i["width"],
                    i["height"],
                    i["framerate"].as_str().unwrap_or("?"),
                    i["encoder"].as_str().unwrap_or("?")
                );
            }
        }
    }
    h.errors.len() == before
}

fn summary(
    h: &Harness,
    caps: &studio::Capabilities,
    segments: &[serde_json::Value],
    warnings: &[String],
    targeting: serde_json::Value,
    transcription: serde_json::Value,
) -> bool {
    let ok = h.errors.is_empty();
    let out = serde_json::json!({
        "ok": ok,
        "encoder": caps.h264_encoder.map(|e| e.element_name()),
        "convert": caps.gpu_convert.map(|c| format!("{c:?}").to_lowercase()),
        "test_size": format!("{}×{}", studio::test_size().0, studio::test_size().1),
        "targeting": targeting,
        "transcription": transcription,
        "segments": segments,
        "warnings": warnings,
        "errors": h.errors,
    });
    println!("{out}");
    let _ = Severity::Info;
    ok
}
