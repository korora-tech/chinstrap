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
//! sources for `CHINSTRAP_SELFTEST_LOAD_SECS` (default 60) and asserts < 1% repeated camera
//! frames, by the counters and by consecutive-frame differences.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer_pbutils as pbutils;
use gstreamer_pbutils::prelude::*;

use crate::studio::{
    self, EngineCommand, EngineEvent, FrameRateMode, InputKey, InputStatus, ModelState, Phase,
    SegmentStatus, Severity, Snapshot, SourceId, SourceKind, SourceState, TestBehaviour,
    TranscriptStatus,
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

struct Harness {
    rx: async_channel::Receiver<EngineEvent>,
    snap: Option<Snapshot>,
    levels: u32,
    errors: Vec<String>,
}

impl Harness {
    fn pump_one(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::Snapshot(s) => self.snap = Some(*s),
            EngineEvent::Notice(n) => {
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
    let (engine, rx) = studio::start(config).map_err(|e: studio::EngineError| e.to_string())?;
    let mut h = Harness {
        rx,
        snap: None,
        levels: 0,
        errors: Vec::new(),
    };
    let mut warnings: Vec<String> = Vec::new();

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
    // P1-6: a recording window with no frame yet says so.
    engine.send(EngineCommand::Record);
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

    // 3. Record (sent above).
    h.wait_for(Duration::from_secs(15), |s| {
        matches!(s.phase, Phase::Recording { segment: 1, .. })
    })
    .ok_or("segment 1 never reached Recording")?;
    h.sleep(Duration::from_secs(3));

    // 4. Cut.
    engine.send(EngineCommand::Cut);
    h.wait_for(Duration::from_secs(20), |s| {
        matches!(s.phase, Phase::Recording { segment: 2, .. })
    })
    .ok_or("segment 2 never reached Recording")?;
    h.sleep(Duration::from_secs(3));

    // 5. Stop.
    engine.send(EngineCommand::Stop);
    let done = h
        .wait_for(Duration::from_secs(20), |s| s.phase == Phase::Idle)
        .ok_or("Stop never reached Idle")?;
    let session = done.session.clone().ok_or("no session after Stop")?;
    eprintln!("selftest: session {}", session.dir.display());

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
    let load_json = match &want_camera {
        Some(want) => check_load(&mut h, load_config, &dir, want),
        None => serde_json::json!("skipped: CHINSTRAP_SELFTEST_CAMERA not set"),
    };
    let mut seg_json = seg_json;
    seg_json.push(serde_json::json!({"plan": plan_json, "load": load_json}));
    Ok(summary(
        &h,
        &caps,
        &seg_json,
        &warnings,
        targeting_json,
        transcription_json,
    ))
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

/// P0-1 acceptance: the named camera plus two 4K test sources for 60 s, < 1% repeated camera
/// frames by the counters and by consecutive-frame differences (after the first 5 s).
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
    let uhd = TestBehaviour {
        size: Some((3840, 2160)),
        ..Default::default()
    };
    config.test_sources = vec![(1, uhd), (2, uhd)];
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
    };
    let result = (|| -> Result<serde_json::Value, String> {
        let first = l
            .wait_for(Duration::from_secs(60), |_| true)
            .ok_or("no first Snapshot")?;
        let cam = first
            .cameras
            .iter()
            .find(|c| c.name.to_lowercase().contains(&want.to_lowercase()))
            .ok_or("camera gone")?
            .id;
        engine.send(EngineCommand::SelectCamera(Some(cam)));
        engine.send(EngineCommand::SelectMicrophone(
            first.microphones.first().map(|m| m.id),
        ));
        engine.send(EngineCommand::AddTestSource);
        engine.send(EngineCommand::AddTestSource);
        let ready = l
            .wait_for(Duration::from_secs(10), |s| {
                s.sources
                    .iter()
                    .filter(|x| x.state == SourceState::Ready)
                    .count()
                    == 2
                    && s.selected_camera == Some(cam)
                    && s.can_record
            })
            .ok_or("load sources never ready")?;
        eprintln!("selftest: load plan: {:?}", ready.capacity);
        engine.send(EngineCommand::Record);
        l.wait_for(Duration::from_secs(20), |s| {
            matches!(s.phase, Phase::Recording { .. })
        })
        .ok_or("load never reached Recording")?;
        l.sleep(Duration::from_secs(secs));
        engine.send(EngineCommand::Stop);
        let done = l
            .wait_for(Duration::from_secs(30), |s| s.phase == Phase::Idle)
            .ok_or("load Stop never reached Idle")?;
        let seg = done
            .session
            .as_ref()
            .and_then(|s| s.segments.first().cloned())
            .ok_or("no load segment")?;
        let mut files = Vec::new();
        let mut camera_counter_pct = None;
        let mut camera_path = None;
        for f in &seg.files {
            if let Some(q) = &f.quality {
                files.push(serde_json::json!({
                    "input": format!("{:?}", f.input), "status": format!("{:?}", f.status),
                    "encoder": q.encoder.map(|e| e.element_name()),
                    "frames_in": q.frames_in, "encoded": q.encoded, "duplicated": q.duplicated,
                    "padded": q.padded, "dropped_queue": q.dropped_queue,
                    "dropped_corrupt": q.dropped_corrupt, "effective_fps": q.effective_fps,
                    "repeat_pct": q.repeat_pct, "bitrate_kbps": q.bitrate_kbps,
                }));
                if f.input == InputKey::Camera {
                    camera_counter_pct = Some(q.repeat_pct);
                    camera_path = Some(f.path.clone());
                    if f.status != InputStatus::Complete {
                        l.fail(format!("load camera status {:?}", f.status));
                    }
                }
            }
        }
        let counter_pct = camera_counter_pct.ok_or("no camera quality")?;
        if counter_pct >= MAX_REPEAT_PCT {
            l.fail(format!(
                "camera repeated {counter_pct:.2}% of frames (counters)"
            ));
        }
        let diffs = frame_diffs(&camera_path.ok_or("no camera file")?)?;
        let late: Vec<f64> = diffs
            .iter()
            .filter(|(pts, _)| *pts >= 5_000_000_000)
            .map(|x| x.1)
            .collect();
        let repeats = late.iter().filter(|d| **d < REPEAT_DIFF).count();
        let pct = repeats as f64 * 100.0 / late.len().max(1) as f64;
        let mut sorted = late.clone();
        sorted.sort_by(f64::total_cmp);
        let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
        eprintln!(
            "selftest: load: camera repeats {repeats}/{} pairs after 5 s ({pct:.2}%), median diff {median:.3}; counters {counter_pct:.2}%",
            late.len()
        );
        if pct >= MAX_REPEAT_PCT {
            l.fail(format!(
                "camera: {repeats} of {} frame pairs after 5 s are repeats ({pct:.2}%)",
                late.len()
            ));
        }
        Ok(serde_json::json!({
            "seconds": secs, "files": files,
            "camera_repeat_pct_counters": counter_pct,
            "camera_repeat_pct_diff": pct, "camera_pairs": late.len(),
            "camera_median_diff": median,
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
