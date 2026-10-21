//! Headless self-test: `chinstrap --selftest <dir>` (SPEC §8.1).
//!
//! Drives the real engine (`enable_portal: false`) through Record → Cut → Stop, validates every
//! file with Discoverer and the manifest, then checks finalize-on-shutdown. Prints a JSON
//! summary and returns the process exit code.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer_pbutils as pbutils;
use gstreamer_pbutils::prelude::*;

use crate::studio::{
    self, EngineCommand, EngineEvent, InputKey, InputStatus, Phase, SegmentStatus, Severity,
    Snapshot, SourceKind, SourceState,
};

const DURATION_TOLERANCE: Duration = Duration::from_millis(600);
const RESIZE_AFTER_MS: &str = "1500";

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
    if name != "video/x-h264" {
        return Err(format!("video stream is {name}, not H.264"));
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
        return Ok(summary(&h, &caps, &[], &warnings, serde_json::Value::Null));
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
    engine.send(EngineCommand::AddTestSource);
    let snap = h
        .wait_for(Duration::from_secs(5), |s| {
            s.sources
                .iter()
                .any(|x| x.kind == SourceKind::TestPattern && x.state == SourceState::Ready)
                && s.selected_camera == camera
                && s.selected_microphone == mic
                && s.can_record
        })
        .ok_or("test source never became Ready / can_record")?;
    let test_id = snap
        .sources
        .iter()
        .find(|x| x.kind == SourceKind::TestPattern)
        .map(|x| x.id)
        .ok_or("no test source")?;
    let test_key = InputKey::Source(test_id);

    // 3. Record.
    engine.send(EngineCommand::Record);
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
        for f in &seg.files {
            let critical = f.input == test_key || (camera_critical && f.input == InputKey::Camera);
            let mut problem: Option<String> = None;
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
                            files_json.push(serde_json::json!({
                                "file": f.path.file_name().map(|n| n.to_string_lossy().to_string()),
                                "width": fc.width, "height": fc.height,
                                "framerate": fc.fps.map(|(n, d)| format!("{n}/{d}")),
                                "profile": fc.profile, "level": fc.level,
                                "duration_s": fc.duration.map(|d| d.as_secs_f64()),
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
    Ok(summary(&h, &caps, &seg_json, &warnings, targeting_json))
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
            if i["codec"] == "h264" {
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
) -> bool {
    let ok = h.errors.is_empty();
    let out = serde_json::json!({
        "ok": ok,
        "encoder": caps.h264_encoder.map(|e| e.element_name()),
        "convert": caps.gpu_convert.map(|c| format!("{c:?}").to_lowercase()),
        "test_size": format!("{}×{}", studio::test_size().0, studio::test_size().1),
        "targeting": targeting,
        "segments": segments,
        "warnings": warnings,
        "errors": h.errors,
    });
    println!("{out}");
    let _ = Severity::Info;
    ok
}
