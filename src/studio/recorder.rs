//! Pipeline builders (SPEC §5.2-§5.5) and the source-negotiation probe (§5.2).
//!
//! Builders only construct pipelines in NULL; every state change is driven by the engine on
//! worker threads (D10). Each input gets its own `gst::Pipeline` on the shared clock (D1).

use std::os::fd::RawFd;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;

use super::devices::{CameraMode, CameraPath, Entry};
use super::encoders::{self, Role, set_prop};
use super::preview::{self, Scaler, Tap};
use super::stamp::{self, Counters, SizeSlot, UNSET};
use super::types::{GpuConvert, VideoEncoder};

/// Caps accepted right after a pipewiresrc (D7): DMA-BUF with modifiers, or plain raw.
pub const SOURCE_CAPS: &str = "video/x-raw(memory:DMABuf),format=DMA_DRM; video/x-raw";
/// Default test pattern size; `CHINSTRAP_TEST_SIZE=WxH` overrides it.
pub const TEST_SIZE: (i32, i32) = (1280, 720);
pub const TEST_RESIZE_CAPS: &str = "video/x-raw,width=1000,height=700,framerate=30/1";
pub const MIC_CAPS: &str = "audio/x-raw,rate=48000,channels=[1,2]";

/// Parses a `WxH` size (both sides 16..=8192).
pub fn parse_size(v: &str) -> Option<(i32, i32)> {
    let (w, h) = v.trim().split_once(['x', 'X', '×'])?;
    let (w, h) = (w.trim().parse::<i32>().ok()?, h.trim().parse::<i32>().ok()?);
    ((16..=8192).contains(&w) && (16..=8192).contains(&h)).then_some((w, h))
}

/// The test pattern's size: `CHINSTRAP_TEST_SIZE=WxH` (e.g. 3840x2160), else 1280×720.
pub fn test_size() -> (i32, i32) {
    std::env::var("CHINSTRAP_TEST_SIZE")
        .ok()
        .and_then(|v| parse_size(&v))
        .unwrap_or(TEST_SIZE)
}

/// Test pattern caps at `fps` (`size` overrides `CHINSTRAP_TEST_SIZE`).
pub fn test_caps_at(fps: (i32, i32), size: Option<(i32, i32)>) -> String {
    let (w, h) = size.unwrap_or_else(test_size);
    format!(
        "video/x-raw,width={w},height={h},framerate={}/{}",
        fps.0,
        fps.1.max(1)
    )
}

/// How a video input's frames are timed (P1-5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    /// Constant rate: videorate (camera, test) or the repeater (portal) fills gaps.
    Cfr((i32, i32)),
    /// Variable rate: frames as the source sends them, stamped on arrival. `cap` is the
    /// maximum rate requested from the source (None: the source's own maximum, e.g. the
    /// output refresh); `gop_fps` sizes the encoder's frame-counted GOP limit.
    Native { cap: Option<i32>, gop_fps: i32 },
}

impl Rate {
    pub fn cfr(self) -> Option<(i32, i32)> {
        match self {
            Rate::Cfr(f) => Some(f),
            Rate::Native { .. } => None,
        }
    }
}

/// What one video input records: frame timing and the largest size before downscaling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoTarget {
    pub rate: Rate,
    pub max: (i32, i32),
}

/// Native test pattern: rendered at this rate with about half the frames dropped in an
/// irregular pattern, so the recording is bursty variable-rate like a compositor's
/// damage-driven stream.
pub const TEST_NATIVE_FPS: i32 = 60;
/// Of every 8 rendered frames, those whose bit is set are dropped (irregular gaps of 1-3
/// frames; identity's drop-probability would also send GAP events, which encoders turn into
/// empty frames with their own timestamps).
const TEST_NATIVE_DROP_MASK: u8 = 0b1011_0010;
/// Keyframe interval for variable-rate recordings (GOP limits count frames, not time).
pub const KEYFRAME_EVERY_NS: u64 = 2_000_000_000;

/// Frame rates offered to a portal source as its maximum, highest first: an enum, never a
/// range. PipeWire fixates a range to its default, which gstpipewireformat sets to the range
/// minimum (1 fps); an enum intersected with the producer's range keeps the first value in it.
/// 60000/1001, 5994/100 and 30000/1001 keep ~59.94 Hz outputs negotiable (niri reports the
/// refresh in mHz, e.g. 59940/1000, just below 60000/1001).
const MAX_RATE_STEPS: [(i32, i32); 17] = [
    (240, 1),
    (165, 1),
    (144, 1),
    (120, 1),
    (100, 1),
    (90, 1),
    (75, 1),
    (60, 1),
    (60000, 1001),
    (5994, 100),
    (50, 1),
    (48, 1),
    (30, 1),
    (30000, 1001),
    (25, 1),
    (24, 1),
    (15, 1),
];

/// `max-framerate` field asking a portal source for at most `cap` fps.
pub fn max_rate_field(cap: i32) -> String {
    let mut steps: Vec<String> = vec![format!("{cap}/1")];
    for (n, d) in MAX_RATE_STEPS {
        if (n as f64 / d as f64) < cap as f64 - 0.01 {
            steps.push(format!("{n}/{d}"));
        }
    }
    format!("max-framerate=(fraction){{ {} }}", steps.join(", "))
}

/// Caps right after a portal pipewiresrc: [`SOURCE_CAPS`], with a maximum rate when capped.
pub fn portal_source_caps(cap: Option<i32>) -> String {
    match cap {
        None => SOURCE_CAPS.to_string(),
        Some(c) => {
            let f = max_rate_field(c);
            format!("video/x-raw(memory:DMABuf),format=DMA_DRM,{f}; video/x-raw,{f}")
        }
    }
}

/// Inputs for one video recording pipeline.
pub struct VideoInputSpec<'a> {
    pub name: &'a str,
    pub source: VideoSource<'a>,
    pub convert: Option<GpuConvert>,
    pub encoder: VideoEncoder,
    pub role: Role,
    pub target: VideoTarget,
    pub clock: &'a gst::Clock,
    pub path: &'a Path,
    pub preview: Option<Tap>,
}

/// Selftest hooks for a test pattern (never set by the app).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestBehaviour {
    /// Size instead of `CHINSTRAP_TEST_SIZE`.
    pub size: Option<(i32, i32)>,
    /// No frame before this much running time (a source that starts late).
    pub late: Option<Duration>,
    /// No frame at all (a window that is never redrawn).
    pub silent: bool,
    /// Native mode: the source's refresh (its maximum rate) instead of [`TEST_NATIVE_FPS`].
    pub refresh: Option<i32>,
    /// Never renegotiated by the selftest resize (the load test keeps its 4K frames and rate).
    pub keep_size: bool,
}

impl TestBehaviour {
    /// The refresh a Native test pattern reports (and renders at, unless capped).
    pub fn refresh_fps(&self) -> i32 {
        self.refresh.filter(|r| *r > 0).unwrap_or(TEST_NATIVE_FPS)
    }
}

/// Where a video input's frames come from.
pub enum VideoSource<'a> {
    Test(TestBehaviour),
    Portal { fd: RawFd, node: u32 },
    Camera { entry: &'a Entry, mode: CameraMode },
}

/// The last frame sent to the encoder of a variable-rate input, to repeat at the stop time so
/// the file lasts the whole segment (a static window sends one frame, then nothing).
#[derive(Clone)]
pub struct Tail {
    last: Arc<Mutex<Option<gst::Buffer>>>,
    /// Sink pad of the leaky queue: where the repeat goes in (its chain takes the stream lock).
    pad: gst::Pad,
    counters: Arc<Counters>,
}

impl Tail {
    /// Pushes the last frame again, stamped `running_ns` (if later than it). Blocking only as
    /// long as the leaky queue's chain function (it never waits for space): call it on a
    /// worker thread.
    pub fn push_at(&self, running_ns: u64) {
        let Some(buf) = self.last.lock().ok().and_then(|g| g.clone()) else {
            return;
        };
        if buf.pts().is_some_and(|p| p.nseconds() >= running_ns) {
            return;
        }
        let mut copy = buf.copy();
        {
            let b = copy.make_mut();
            b.set_pts(gst::ClockTime::from_nseconds(running_ns));
            b.set_dts(gst::ClockTime::NONE);
            b.set_duration(gst::ClockTime::NONE);
            b.unset_flags(gst::BufferFlags::DISCONT);
        }
        if self.pad.chain(copy).is_ok() {
            self.counters.reinjected.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// One input's pipeline for one segment, plus the shared state its probes write.
pub struct InputPipeline {
    pub pipeline: gst::Pipeline,
    pub bus: gst::Bus,
    /// Sink pad of the first element after the source: EOS injection point (§6).
    pub eos_target: gst::Pad,
    /// Base time B for the stamp probes; `UNSET` until set before Playing.
    pub base: Arc<AtomicU64>,
    pub first_pts: Arc<AtomicU64>,
    pub last_pts: Arc<AtomicU64>,
    /// Size locked from the first CAPS event (video only).
    pub size: SizeSlot,
    /// Test pattern only: the capsfilter that is changed to renegotiate (selftest resize).
    pub resize_filter: Option<gst::Element>,
    /// The convert path actually used (manifest).
    pub convert: Option<GpuConvert>,
    /// The video encoder used (video only, manifest).
    pub encoder: Option<VideoEncoder>,
    /// Frame timing (video only, manifest).
    pub rate: Option<Rate>,
    /// Warning to surface once (e.g. `add-borders` missing).
    pub warning: Option<String>,
    pub counters: Arc<Counters>,
    /// videorate, whose `duplicate`/`drop` counts are read for the quality summary.
    pub videorate: Option<gst::Element>,
    /// The leaky queue (its current level, for the live drop count).
    pub leaky: Option<gst::Element>,
    /// Variable-rate inputs: the last frame, repeated at the stop time.
    pub tail: Option<Tail>,
    /// Video inputs with a preview branch: set once that branch has failed.
    pub preview: Option<Arc<AtomicBool>>,
}

pub(super) fn make(factory: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|e| format!("cannot create {factory}: {e}"))
}
pub(super) fn capsfilter(caps: &str) -> Result<gst::Element, String> {
    let caps = gst::Caps::from_str(caps).map_err(|e| format!("bad caps {caps}: {e}"))?;
    gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()
        .map_err(|e| format!("cannot create capsfilter: {e}"))
}

/// A plain (blocking) queue. Bounded by buffer count only: the default 10 MB byte limit holds
/// less than one 4K raw frame (3840×2160 NV12 is 12.4 MB).
fn queue() -> Result<gst::Element, String> {
    let q = make("queue")?;
    set_prop(&q, "max-size-buffers", "8");
    set_prop(&q, "max-size-bytes", "0");
    set_prop(&q, "max-size-time", "0");
    Ok(q)
}

/// videoconvert/videoscale on every core (4K conversion on one thread costs ~10 ms a frame).
fn threaded(factory: &str) -> Result<gst::Element, String> {
    let el = make(factory)?;
    set_prop(&el, "n-threads", "0");
    Ok(el)
}

fn leaky_queue() -> Result<gst::Element, String> {
    let q = make("queue")?;
    set_prop(&q, "max-size-buffers", "4");
    set_prop(&q, "max-size-bytes", "0");
    set_prop(&q, "max-size-time", "0");
    set_prop(&q, "leaky", "downstream");
    Ok(q)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepeaterBackend {
    Va,
    Cpu,
}

impl RepeaterBackend {
    fn factory(self) -> &'static str {
        match self {
            Self::Va => "vacompositor",
            Self::Cpu => "compositor",
        }
    }
}

/// The constant-rate repeater for portal sources: a live aggregator that outputs a frame every
/// `fps` period from the first input frame until EOS, repeating the last input frame.
/// Compositors only send frames on damage, so a static window would otherwise stop recording
/// after its first frame, and videorate only duplicates once a newer frame arrives.
fn repeater(backend: RepeaterBackend, fps: (i32, i32)) -> Result<gst::Element, String> {
    let factory = backend.factory();
    let agg = gst::ElementFactory::make(factory)
        .property("force-live", true)
        .build()
        .map_err(|e| format!("cannot create {factory}: {e}"))?;
    set_prop(&agg, "background", "black");
    // Output starts at the first input frame: an aggregate with no input yet fails on radeonsi
    // (vacompositor: "vaEndPicture: operation failed"), and the first frame's running time is
    // the input's real start (`first_pts_ns`), as for the camera.
    set_prop(&agg, "start-time-selection", "first");
    // One frame of slack for the convert step before an output frame is due.
    let frame_ns = 1_000_000_000u64 * fps.1.max(1) as u64 / fps.0.max(1) as u64;
    set_prop(&agg, "latency", &frame_ns.to_string());
    Ok(agg)
}

/// The `<CONVERT>` block. Returns the elements and whether `add-borders` was missing.
///
/// Deviation from §5.2 (GL row): `glcolorconvert` converts to the encoder's raw `format` on
/// the GPU (a GLMemory capsfilter) before `gldownload`. Downloading the default RGBA output
/// crashes inside Mesa's glReadPixels (SIGBUS) on this host's radeonsi; NV12/I420 does not.
fn convert_block(
    convert: Option<GpuConvert>,
    format: &str,
) -> Result<(Vec<gst::Element>, bool), String> {
    match convert {
        Some(GpuConvert::Va) => {
            let pp = make("vapostproc")?;
            let ok = set_prop(&pp, "add-borders", "true");
            Ok((vec![pp], !ok))
        }
        Some(GpuConvert::Gl) => {
            let scale = threaded("videoscale")?;
            let ok = set_prop(&scale, "add-borders", "true");
            Ok((
                vec![
                    make("glupload")?,
                    make("glcolorconvert")?,
                    capsfilter(&format!("video/x-raw(memory:GLMemory),format={format}"))?,
                    make("gldownload")?,
                    threaded("videoconvert")?,
                    scale,
                ],
                !ok,
            ))
        }
        None => {
            let scale = threaded("videoscale")?;
            let ok = set_prop(&scale, "add-borders", "true");
            Ok((vec![threaded("videoconvert")?, scale], !ok))
        }
    }
}

/// Stream properties that make WirePlumber link only the requested node: with
/// `node.dont-fallback` a missing target is an error ("defined target not found") instead of a
/// silent link to the default Video/Source; `node.dont-move` stops the default metadata from
/// redirecting the stream.
pub const NO_FALLBACK_PROPS: [(&str, &str); 2] =
    [("node.dont-fallback", "true"), ("node.dont-move", "true")];

/// How a pipewiresrc is pointed at a portal stream's node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalTarget {
    /// pipewiresrc `path`: the node's *object id*, passed to `pw_stream_connect` as target id
    /// (it becomes `node.target`, which WirePlumber matches against `node.id`).
    pub path: String,
    /// pipewiresrc `target-object`: must stay unset. pipewiresrc (1.6.9) puts a numeric value in
    /// `target.object`, which WirePlumber matches against `object.serial` (or `node.name`), never
    /// the object id the portal hands out; it also takes precedence over `node.target`.
    pub target_object: Option<String>,
    /// `stream-properties`: [`NO_FALLBACK_PROPS`] plus `node.target` = the object id, so the
    /// target survives even if a future pipewiresrc drops the deprecated `path` property
    /// (`pw_stream_connect` keeps a `node.target` that is already set, PipeWire 1.6.9 stream.c).
    pub stream_properties: Vec<(&'static str, String)>,
}

/// Targeting for a portal node id (SPEC §5.2, PipeWire 1.6.9 gstpipewiresrc.c / WirePlumber
/// 0.5 find-defined-target.lua).
pub fn portal_target(node: u32) -> PortalTarget {
    PortalTarget {
        path: node.to_string(),
        target_object: None,
        stream_properties: NO_FALLBACK_PROPS
            .iter()
            .map(|(k, v)| (*k, v.to_string()))
            .chain([("node.target", node.to_string())])
            .collect(),
    }
}

/// Sets [`NO_FALLBACK_PROPS`] as a pipewiresrc's `stream-properties`.
pub fn set_no_fallback(src: &gst::Element) {
    let props: Vec<(&str, String)> = NO_FALLBACK_PROPS
        .iter()
        .map(|(k, v)| (*k, v.to_string()))
        .collect();
    set_stream_properties(src, &props);
}

fn set_stream_properties(src: &gst::Element, props: &[(&str, String)]) {
    if src.find_property("stream-properties").is_none() {
        return;
    }
    let mut s = gst::Structure::builder("props");
    for (k, v) in props {
        s = s.field(*k, v.as_str());
    }
    src.set_property("stream-properties", s.build());
}

/// A pipewiresrc for a portal stream (§5.2). `on-disconnect` stays at its default (D9).
/// `fd` is the portal remote; `None` connects to the normal daemon (regression test).
pub fn portal_source(name: &str, fd: Option<RawFd>, node: u32) -> Result<gst::Element, String> {
    let src = gst::ElementFactory::make("pipewiresrc")
        .name(name)
        .build()
        .map_err(|e| format!("cannot create pipewiresrc: {e}"))?;
    if let Some(fd) = fd {
        src.set_property_from_str("fd", &fd.to_string());
    }
    let target = portal_target(node);
    // `path` is deprecated in pipewiresrc 1.6.9; fail loudly rather than panic in glib (or link
    // the default source) if it ever disappears. `node.target` below targets the node as well.
    if src.find_property("path").is_none() {
        return Err("pipewiresrc has no 'path' property: cannot target the portal node".into());
    }
    src.set_property("path", Some(target.path.as_str()));
    if src.find_property("target-object").is_some() {
        src.set_property("target-object", target.target_object.as_deref());
    }
    set_stream_properties(&src, &target.stream_properties);
    // No `keepalive-time`: it holds the last PipeWire buffer, and a stream returns at most one
    // buffer to the producer per graph cycle (PipeWire 1.6.9 stream.c). niri only cycles when it
    // renders damage, so every buffer still held at the next frame is lost to it for good and
    // the cast starves ("no available buffer in pw stream"). The repeater fills gaps instead.
    set_prop(&src, "provide-clock", "false");
    Ok(src)
}

fn new_pipeline(name: &str, clock: &gst::Clock) -> gst::Pipeline {
    let p = gst::Pipeline::with_name(name);
    p.use_clock(Some(clock));
    p.set_start_time(gst::ClockTime::NONE);
    p
}

fn file_tail(path: &Path) -> Result<(gst::Element, gst::Element), String> {
    let mux = make("matroskamux")?;
    set_prop(&mux, "writing-app", "Chinstrap");
    let sink = gst::ElementFactory::make("filesink")
        .property("location", path.to_string_lossy().to_string())
        .build()
        .map_err(|e| format!("cannot create filesink: {e}"))?;
    Ok((mux, sink))
}

fn link_chain(pipeline: &gst::Pipeline, chain: &[gst::Element]) -> Result<(), String> {
    pipeline
        .add_many(chain)
        .map_err(|e| format!("cannot add elements: {e}"))?;
    gst::Element::link_many(chain).map_err(|e| format!("cannot link: {e}"))
}

fn sink_pad(el: &gst::Element) -> Result<gst::Pad, String> {
    el.static_pad("sink")
        .ok_or_else(|| format!("{} has no sink pad", el.name()))
}

fn src_pad(el: &gst::Element) -> Result<gst::Pad, String> {
    el.static_pad("src")
        .ok_or_else(|| format!("{} has no src pad", el.name()))
}

fn mux_probe(
    before_mux: &gst::Element,
    counters: Arc<Counters>,
) -> Result<(Arc<AtomicU64>, Arc<AtomicU64>), String> {
    let first = Arc::new(AtomicU64::new(UNSET));
    let last = Arc::new(AtomicU64::new(UNSET));
    let pad = src_pad(before_mux)?.peer().ok_or("muxer pad not linked")?;
    stamp::install_mux_probe(&pad, first.clone(), last.clone(), counters);
    Ok((first, last))
}

/// Rewrites the framerate of every caps event leaving `pad` to `fps`.
fn rewrite_framerate(pad: &gst::Pad, fps: (i32, i32)) {
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        let Some(gst::PadProbeData::Event(ev)) = info.data.as_ref() else {
            return gst::PadProbeReturn::Ok;
        };
        let gst::EventView::Caps(c) = ev.view() else {
            return gst::PadProbeReturn::Ok;
        };
        let mut caps = c.caps_owned();
        let want = gst::Fraction::new(fps.0, fps.1);
        if caps
            .structure(0)
            .and_then(|s| s.get::<gst::Fraction>("framerate").ok())
            == Some(want)
        {
            return gst::PadProbeReturn::Ok;
        }
        if let Some(s) = caps.make_mut().structure_mut(0) {
            s.set("framerate", want);
        }
        let new = gst::event::Caps::builder(&caps).seqnum(ev.seqnum()).build();
        info.data = Some(gst::PadProbeData::Event(new));
        gst::PadProbeReturn::Ok
    });
}

/// Forces a keyframe at least every [`KEYFRAME_EVERY_NS`] of stream time on `pad` (upstream of
/// the encoder): variable-rate input makes a frame-counted GOP arbitrary in time.
fn install_timed_keyframes(pad: &gst::Pad) {
    let next = Mutex::new(None::<u64>);
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let Some(pts) = info.buffer().and_then(|b| b.pts()).map(|p| p.nseconds()) else {
            return gst::PadProbeReturn::Ok;
        };
        let Ok(mut next) = next.lock() else {
            return gst::PadProbeReturn::Ok;
        };
        match *next {
            // The first frame is a keyframe anyway.
            None => *next = Some(pts + KEYFRAME_EVERY_NS),
            Some(due) if pts >= due => {
                *next = Some(pts + KEYFRAME_EVERY_NS);
                let s = gst::Structure::builder("GstForceKeyUnit")
                    .field("timestamp", pts)
                    .field("stream-time", pts)
                    .field("running-time", pts)
                    .field("all-headers", true)
                    .field("count", 0u32)
                    .build();
                pad.push_event(gst::event::CustomDownstream::new(s));
            }
            Some(_) => {}
        }
        gst::PadProbeReturn::Ok
    });
}

/// Builds a video input (§5.2 screen/window/test, §5.4 camera) in NULL. With `preview`, a tee
/// feeds a throttled preview branch (see `preview.rs`) that can never fail the input.
pub fn build_video(spec: VideoInputSpec<'_>) -> Result<InputPipeline, String> {
    let VideoInputSpec {
        name,
        source,
        convert,
        encoder: enc,
        role,
        target,
        clock,
        path,
        preview,
    } = spec;
    let pipeline = new_pipeline(name, clock);
    let base = Arc::new(AtomicU64::new(UNSET));
    let counters = Arc::new(Counters::default());
    let native = matches!(target.rate, Rate::Native { .. });
    let portal = matches!(source, VideoSource::Portal { .. });

    // Source section: `src_chain` ends at the element whose src pad carries the source caps;
    // `stamp_at` is the element whose src pad gets the stamp probe.
    let mut src_chain: Vec<gst::Element> = Vec::new();
    let stamp_at = 0;
    let mut bursty = false;
    let mut resize_filter = None;
    let mut camera_mode = None;
    let mut gate = None;
    match &source {
        VideoSource::Test(behaviour) => {
            let src = make("videotestsrc")?;
            set_prop(&src, "is-live", "true");
            set_prop(&src, "pattern", "smpte");
            // Moving content, so repeated frames are visible in the file.
            set_prop(&src, "horizontal-speed", "4");
            let rate = match target.rate {
                Rate::Cfr(f) => f,
                // Like a compositor: the refresh, or the requested maximum when lower.
                Rate::Native { cap, .. } => (
                    cap.map_or(behaviour.refresh_fps(), |c| c.min(behaviour.refresh_fps())),
                    1,
                ),
            };
            let caps = capsfilter(&test_caps_at(rate, behaviour.size))?;
            if behaviour.silent || behaviour.late.is_some() {
                gate = Some(*behaviour);
            }
            if !behaviour.keep_size {
                resize_filter = Some(caps.clone());
            }
            src_chain.push(src);
            src_chain.push(caps);
            bursty = native;
            // Deviation from §5.3: videotestsrc stamps its first frame with the running time at
            // which it started, which is negative while B is still 100 ms in the future (it
            // wraps to ~2^64 and videorate then duplicates without bound). Stamp it like every
            // other source instead: arrival running time, frames before B dropped.
            if encoders::has_element("timeoverlay") {
                src_chain.push(make("timeoverlay")?);
            }
        }
        VideoSource::Portal { fd, node } => {
            src_chain.push(portal_source("src", Some(*fd), *node)?);
        }
        VideoSource::Camera { entry, mode } => {
            src_chain.push(entry.create_source("cam-src")?);
            camera_mode = Some(mode.clone());
        }
    }

    // Middle: source capsfilter, decode/convert, rate.
    let mut mid: Vec<gst::Element> = Vec::new();
    // Portal sources: the converter scales to the locked size (no frame rate) ahead of the
    // repeater, which then outputs at the target rate.
    let mut repeat_caps = None;
    let mut videorate = None;
    let mut repeater_el = None;
    let mut leaky = None;
    // The preview tee and how its branch scales.
    let mut tee = None;
    let tee_el = || -> Result<gst::Element, String> {
        let t = make("tee")?;
        set_prop(&t, "allow-not-linked", "true");
        Ok(t)
    };
    let format = encoders::encoder_format(enc);
    let (add_borders_missing, effective_convert) = match &camera_mode {
        Some(mode) if mode.path != CameraPath::DmaBuf => {
            // §5.4: exactly the chosen native mode, decoded/converted on the CPU.
            mid.push(capsfilter(&mode.source_caps())?);
            if mode.path == CameraPath::Jpeg {
                mid.push(make("jpegparse")?);
                mid.push(make("jpegdec")?);
            }
            // Preview: decoded frames at the camera's own size, scaled on the GPU when VA is
            // available.
            if preview.is_some() {
                let t = tee_el()?;
                let scaler = if convert == Some(GpuConvert::Va) {
                    Scaler::Va
                } else {
                    Scaler::Cpu
                };
                tee = Some((t.clone(), scaler));
                mid.push(t);
            }
            mid.push(queue()?);
            let (block, missing) = convert_block(None, format)?;
            mid.extend(block);
            // skip-to-first: no frames are invented before the camera's first real frame (the
            // stamp probe puts that one at 0, so the file still starts at segment time zero).
            let rate = make("videorate")?;
            set_prop(&rate, "skip-to-first", "true");
            videorate = Some(rate.clone());
            mid.push(rate);
            (missing, None)
        }
        _ => {
            // §5.2 chain (portal, test pattern, DMA-BUF camera).
            let caps = match (&camera_mode, portal, target.rate) {
                (Some(mode), _, _) => mode.source_caps(),
                // Ask the compositor for no more frames than are recorded.
                (None, true, Rate::Cfr((n, d))) => {
                    portal_source_caps(Some(((n as f64 / d.max(1) as f64).ceil()) as i32))
                }
                (None, true, Rate::Native { cap, .. }) => portal_source_caps(cap),
                (None, false, _) => SOURCE_CAPS.to_string(),
            };
            mid.push(capsfilter(&caps)?);
            let (block, missing) = convert_block(convert, format)?;
            mid.extend(block);
            // Preview: after the convert step (VA: VAMemory at the recorded size; vapostproc
            // scales it again on the GPU), ahead of the encoder's leaky queue.
            if preview.is_some() {
                let t = tee_el()?;
                let scaler = if convert == Some(GpuConvert::Va) {
                    Scaler::Va
                } else {
                    Scaler::Cpu
                };
                tee = Some((t.clone(), scaler));
                mid.push(t);
            }
            let q = leaky_queue()?;
            leaky = Some(q.clone());
            mid.push(q);
            match target.rate {
                Rate::Native { .. } => {}
                Rate::Cfr(fps) if portal => {
                    let caps = make("capsfilter")?;
                    repeat_caps = Some(caps.clone());
                    mid.push(caps);
                    // VAMemory end to end exactly when outcaps carries it (encoders::outcaps_string).
                    let backend = if convert == Some(GpuConvert::Va) && enc.is_gpu() {
                        RepeaterBackend::Va
                    } else {
                        RepeaterBackend::Cpu
                    };
                    let agg = repeater(backend, fps)?;
                    repeater_el = Some(agg.clone());
                    mid.push(agg);
                }
                Rate::Cfr(_) => {
                    let rate = make("videorate")?;
                    set_prop(&rate, "skip-to-first", "true");
                    videorate = Some(rate.clone());
                    mid.push(rate);
                }
            }
            (missing, convert)
        }
    };
    let outcaps = gst::ElementFactory::make("capsfilter")
        .name("outcaps")
        .build()
        .map_err(|e| format!("cannot create capsfilter: {e}"))?;
    mid.push(outcaps.clone());
    let enc_queue = queue()?;
    mid.push(enc_queue.clone());
    let gop_rate = match target.rate {
        Rate::Cfr(f) => f,
        Rate::Native { gop_fps, .. } => (gop_fps.max(1), 1),
    };
    let encoder = encoders::make_encoder(enc, role, gop_rate)?;
    mid.push(encoder.clone());
    let parse = make(encoders::parser_name(enc))?;
    set_prop(&parse, "config-interval", "-1");
    mid.push(parse);
    let coded = capsfilter(encoders::coded_caps(enc))?;
    mid.push(coded.clone());
    let (mux, sink) = file_tail(path)?;
    mid.push(mux);
    mid.push(sink);

    let eos_target = sink_pad(&mid[0])?;
    let mut chain = src_chain.clone();
    chain.extend(mid);
    link_chain(&pipeline, &chain)?;

    let last_src = src_chain.last().expect("source section");
    let src_out = src_pad(last_src)?;
    // §5.0 video stamp on the source's src pad (every video source, see the Test note).
    let deep_copy = camera_mode
        .as_ref()
        .is_some_and(|m| m.path != CameraPath::DmaBuf);
    if bursty {
        let n = std::sync::atomic::AtomicU32::new(0);
        src_pad(&src_chain[0])?.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            let i = n.fetch_add(1, Ordering::Relaxed);
            if TEST_NATIVE_DROP_MASK & (1 << (i % 8)) != 0 {
                gst::PadProbeReturn::Drop
            } else {
                gst::PadProbeReturn::Ok
            }
        });
    }
    if let Some(b) = gate {
        // Selftest: drop frames before the stamp probe sees them.
        let (base, clock) = (base.clone(), clock.clone());
        src_pad(&src_chain[0])?.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            let b0 = base.load(Ordering::Acquire);
            let running = clock
                .time()
                .nseconds()
                .checked_sub(b0)
                .filter(|_| b0 != UNSET);
            let open = !b.silent
                && running.is_some_and(|r| b.late.is_none_or(|l| r >= l.as_nanos() as u64));
            if open {
                gst::PadProbeReturn::Ok
            } else {
                gst::PadProbeReturn::Drop
            }
        });
    }
    stamp::install_video(
        &src_pad(&src_chain[stamp_at])?,
        clock.clone(),
        base.clone(),
        deep_copy,
        portal,
        counters.clone(),
    );
    // Counters around the leaky queue and the rate stage.
    let mut tail = None;
    if let Some(q) = &leaky {
        stamp::count_buffers(&sink_pad(q)?, counters.clone(), |c| &c.queue_in);
        if native {
            let last: Arc<Mutex<Option<gst::Buffer>>> = Arc::new(Mutex::new(None));
            let store = last.clone();
            src_pad(q)?.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let (Some(b), Ok(mut slot)) = (info.buffer(), store.lock()) {
                    // The stop-time repeat (Tail::push_at) enters from another thread; a frame
                    // stamped before it but still in the convert stage must not follow it with
                    // an earlier PTS.
                    let prev = slot.as_ref().and_then(|p| p.pts());
                    if let (Some(prev), Some(pts)) = (prev, b.pts())
                        && pts <= prev
                    {
                        return gst::PadProbeReturn::Drop;
                    }
                    *slot = Some(b.clone());
                }
                gst::PadProbeReturn::Ok
            });
            tail = Some(Tail {
                last,
                pad: sink_pad(q)?,
                counters: counters.clone(),
            });
        }
        stamp::count_buffers(&src_pad(q)?, counters.clone(), |c| &c.queue_out);
    }
    let rate_el = videorate.clone().or(repeater_el.clone());
    if let Some(r) = &rate_el {
        let input = match &repeater_el {
            Some(agg) => agg
                .sink_pads()
                .into_iter()
                .next()
                .ok_or("repeater has no sink pad")?,
            None => sink_pad(r)?,
        };
        stamp::count_buffers(&input, counters.clone(), |c| &c.rate_in);
        stamp::count_rate_out(&src_pad(r)?, counters.clone());
    }
    if let Rate::Native { gop_fps, .. } = target.rate {
        install_timed_keyframes(&src_pad(&enc_queue)?);
        // The encoder budgets each frame as bitrate / caps framerate. Tell it the planned rate
        // (what the bitrate was computed for), not the source's maximum (e.g. 144/1, which left
        // native inputs a quarter of their bitrate) nor 0/1 (a portal cast; the encoder then
        // assumes its own default). It also picks the codec level from it.
        rewrite_framerate(&src_pad(&enc_queue)?, (gop_fps.max(1), 1));
        // The file is variable-rate: no Matroska DefaultDuration claiming the maximum rate.
        rewrite_framerate(&src_pad(&coded)?, (0, 1));
    }
    let size = Arc::new(Mutex::new(None));
    let out_rate = target.rate.cfr();
    stamp::install_size_lock(
        &src_out,
        outcaps,
        target.max,
        move |w, h| {
            // The bitrate follows the locked size; the encoder has not seen caps yet.
            encoders::set_bitrate(
                &encoder,
                enc,
                encoders::bitrate_kbps(role, enc.codec(), w, h, gop_rate),
            );
            let caps = gst::Caps::from_str(&encoders::outcaps_string(
                enc,
                effective_convert,
                w,
                h,
                out_rate,
            ))
            .unwrap_or_else(|_| gst::Caps::new_any());
            if let Some(filter) = &repeat_caps {
                let mut unrated = caps.clone();
                if let Some(s) = unrated.make_mut().structure_mut(0) {
                    s.remove_field("framerate");
                }
                filter.set_property("caps", unrated);
            }
            caps
        },
        size.clone(),
    );
    let (first_pts, last_pts) = mux_probe(&coded, counters.clone())?;
    let bus = pipeline.bus().ok_or("pipeline has no bus")?;
    let mut preview_killed = None;
    if let (Some((t, scaler)), Some(tap)) = (&tee, &preview) {
        preview::isolate_errors(&bus, tap.killed.clone());
        let attached = t
            .request_pad_simple("src_%u")
            .ok_or_else(|| "tee has no request pad".to_string())
            .and_then(|pad| preview::branch(&pipeline, &pad, tap, Vec::new(), *scaler, true));
        if let Err(e) = attached {
            // Recording goes on without a preview.
            eprintln!("chinstrap: {name}: no preview: {e}");
            tap.killed.store(true, Ordering::Release);
        }
        preview_killed = Some(tap.killed.clone());
    }
    Ok(InputPipeline {
        pipeline,
        bus,
        eos_target,
        base,
        first_pts,
        last_pts,
        size,
        resize_filter,
        convert: effective_convert,
        encoder: Some(enc),
        rate: Some(target.rate),
        warning: add_borders_missing
            .then(|| "add-borders is unavailable: video will be stretched".to_string()),
        counters,
        videorate,
        leaky,
        tail,
        preview: preview_killed,
    })
}

/// Builds the microphone input (§5.5) in NULL.
pub fn build_mic(
    name: &str,
    entry: &Entry,
    clock: &gst::Clock,
    path: &Path,
) -> Result<InputPipeline, String> {
    let pipeline = new_pipeline(name, clock);
    let base = Arc::new(AtomicU64::new(UNSET));
    let src = entry.create_source("mic-src")?;
    let level = make("level")?;
    set_prop(&level, "interval", "100000000");
    set_prop(&level, "post-messages", "true");
    let opus = make("opusenc")?;
    set_prop(&opus, "bitrate", "192000");
    let first_q = queue()?;
    let (mux, sink) = file_tail(path)?;
    let chain = vec![
        src.clone(),
        first_q.clone(),
        make("audioconvert")?,
        make("audioresample")?,
        capsfilter(MIC_CAPS)?,
        level,
        queue()?,
        opus.clone(),
        mux,
        sink,
    ];
    link_chain(&pipeline, &chain)?;
    stamp::install_audio(&src_pad(&src)?, clock.clone(), base.clone());
    let counters = Arc::new(Counters::default());
    let (first_pts, last_pts) = mux_probe(&opus, counters.clone())?;
    let bus = pipeline.bus().ok_or("pipeline has no bus")?;
    Ok(InputPipeline {
        pipeline,
        bus,
        eos_target: sink_pad(&first_q)?,
        base,
        first_pts,
        last_pts,
        size: Arc::new(Mutex::new(None)),
        resize_filter: None,
        convert: None,
        encoder: None,
        rate: None,
        warning: None,
        counters,
        videorate: None,
        leaky: None,
        tail: None,
        preview: None,
    })
}

/// Builds an idle preview pipeline (`PipeKind::Preview`) in NULL: the input's source and its
/// caps, then the preview branch (`preview.rs`). The only consumer of its node while it runs:
/// recording starts only once it is Null again (D4).
pub fn build_preview(
    name: &str,
    source: VideoSource<'_>,
    convert: Option<GpuConvert>,
    tap: &Tap,
) -> Result<(gst::Pipeline, gst::Bus), String> {
    let pipeline = gst::Pipeline::with_name(name);
    // DMA-BUF in: VA scales it directly; GL brings it to system memory as NV12 first (never
    // RGBA: gldownload of RGBA crashes in Mesa here, see `convert_block`).
    let dmabuf = |convert: Option<GpuConvert>| -> Result<(Vec<gst::Element>, Scaler), String> {
        match convert {
            Some(GpuConvert::Va) => Ok((Vec::new(), Scaler::Va)),
            Some(GpuConvert::Gl) => Ok((
                vec![
                    make("glupload")?,
                    make("glcolorconvert")?,
                    capsfilter("video/x-raw(memory:GLMemory),format=NV12")?,
                    make("gldownload")?,
                ],
                Scaler::Cpu,
            )),
            None => Err("no GPU conversion path".into()),
        }
    };
    let (chain, pre, scaler) = match source {
        VideoSource::Test(b) => {
            // Rendered at preview size and rate: nothing to scale.
            let src = make("videotestsrc")?;
            set_prop(&src, "is-live", "true");
            set_prop(&src, "pattern", "smpte");
            set_prop(&src, "horizontal-speed", "4");
            let (w, h) = b.size.unwrap_or_else(test_size);
            let (w, h) = preview::fit(w, h);
            let caps = capsfilter(&format!("video/x-raw,width={w},height={h},framerate=15/1"))?;
            if b.silent {
                // Selftest: a window that never draws previews nothing either.
                src_pad(&src)?
                    .add_probe(gst::PadProbeType::BUFFER, |_, _| gst::PadProbeReturn::Drop);
            }
            (vec![src, caps], Vec::new(), Scaler::Cpu)
        }
        VideoSource::Portal { fd, node } => {
            let (pre, scaler) = dmabuf(convert)?;
            (
                vec![
                    portal_source("src", Some(fd), node)?,
                    capsfilter(SOURCE_CAPS)?,
                ],
                pre,
                scaler,
            )
        }
        VideoSource::Camera { entry, mode } => {
            let (pre, scaler) = match mode.path {
                CameraPath::DmaBuf => dmabuf(convert)?,
                // Only kept frames are decoded.
                CameraPath::Jpeg => (
                    vec![make("jpegparse")?, make("jpegdec")?],
                    if convert == Some(GpuConvert::Va) {
                        Scaler::Va
                    } else {
                        Scaler::Cpu
                    },
                ),
                CameraPath::Raw if convert == Some(GpuConvert::Va) => (Vec::new(), Scaler::Va),
                CameraPath::Raw => (Vec::new(), Scaler::Cpu),
            };
            (
                vec![
                    entry.create_source("cam-src")?,
                    capsfilter(&mode.source_caps())?,
                ],
                pre,
                scaler,
            )
        }
    };
    link_chain(&pipeline, &chain)?;
    let from = src_pad(chain.last().expect("source chain"))?;
    preview::branch(&pipeline, &from, tap, pre, scaler, false)?;
    let bus = pipeline.bus().ok_or("pipeline has no bus")?;
    Ok((pipeline, bus))
}

/// Source-negotiation probe (§5.2): `pipewiresrc ! capsfilter ! <CONVERT> ! outcaps-format !
/// fakesink num-buffers=1`, on the capture's one fd number.
/// The probe also records the negotiated source caps: the physical size and the source's own
/// maximum rate (`max-framerate`, niri: the output refresh), see [`source_facts`].
pub fn build_probe(
    name: &str,
    fd: RawFd,
    node: u32,
    convert: GpuConvert,
    enc: Option<VideoEncoder>,
) -> Result<(gst::Pipeline, gst::Bus, CapsSlot), String> {
    let pipeline = gst::Pipeline::with_name(name);
    let filter = capsfilter(SOURCE_CAPS)?;
    let mut chain = vec![portal_source("src", Some(fd), node)?, filter.clone()];
    let fmt = enc.map(encoders::encoder_format).unwrap_or("I420");
    chain.extend(convert_block(Some(convert), fmt)?.0);
    let out = match (convert, enc) {
        (GpuConvert::Va, Some(e)) if e.is_gpu() => {
            format!("video/x-raw(memory:VAMemory),format={fmt}")
        }
        _ => format!("video/x-raw,format={fmt}"),
    };
    chain.push(capsfilter(&out)?);
    let sink = make("fakesink")?;
    set_prop(&sink, "num-buffers", "1");
    set_prop(&sink, "sync", "false");
    chain.push(sink);
    link_chain(&pipeline, &chain)?;
    let caps: CapsSlot = Arc::new(Mutex::new(None));
    let slot = caps.clone();
    src_pad(&filter)?.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::EventView::Caps(c)) = info.event().map(|e| e.view())
            && let Ok(mut s) = slot.lock()
        {
            *s = Some(c.caps_owned());
        }
        gst::PadProbeReturn::Ok
    });
    let bus = pipeline.bus().ok_or("pipeline has no bus")?;
    Ok((pipeline, bus, caps))
}

/// What negotiated portal source caps say about the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SourceFacts {
    pub size: Option<(i32, i32)>,
    /// `max-framerate` (niri: the output refresh).
    pub max_rate: Option<(i32, i32)>,
}

pub fn source_facts(caps: &gst::CapsRef) -> SourceFacts {
    let Some(s) = caps.structure(0) else {
        return SourceFacts::default();
    };
    let size = match (s.get::<i32>("width"), s.get::<i32>("height")) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => Some((w, h)),
        _ => None,
    };
    let rate = s
        .get::<gst::Fraction>("max-framerate")
        .ok()
        .filter(|f| f.numer() > 0 && f.denom() > 0)
        .map(|f| (f.numer(), f.denom()));
    SourceFacts {
        size,
        max_rate: rate,
    }
}

/// Where a probe leaves the negotiated source caps.
pub type CapsSlot = Arc<Mutex<Option<gst::Caps>>>;

/// Changes the test pattern's caps to force a renegotiation (selftest resize, §5.3).
pub fn renegotiate_test(filter: &gst::Element) {
    if let Ok(caps) = gst::Caps::from_str(TEST_RESIZE_CAPS) {
        filter.set_property("caps", caps);
    }
}

/// `CHINSTRAP_TEST_RESIZE_AFTER_MS` (hidden, selftest only).
pub fn test_resize_after() -> Option<Duration> {
    std::env::var("CHINSTRAP_TEST_RESIZE_AFTER_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
}

/// Result of [`check_targeting`].
#[derive(Clone, Debug)]
pub struct TargetCheck {
    /// Caps negotiated by the targeted stream.
    pub caps: String,
    /// Object ids of the nodes linked into our stream (from `pw-dump`), when available.
    pub linked_from: Option<Vec<u32>>,
}

/// Every object `pw-dump` reports, when it is available.
fn pw_dump() -> Option<Vec<serde_json::Value>> {
    let out = std::process::Command::new("pw-dump").output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Object ids of the nodes whose `node.name` is `name`.
fn nodes_named(objects: &[serde_json::Value], name: &str) -> Vec<u64> {
    objects
        .iter()
        .filter(|o| {
            o["type"] == "PipeWire:Interface:Node" && o["info"]["props"]["node.name"] == name
        })
        .filter_map(|o| o["id"].as_u64())
        .collect()
}

/// Object ids of the nodes linked into the stream node called `stream_name` (`pw-dump`).
fn linked_into(stream_name: &str) -> Option<Vec<u32>> {
    let objects = pw_dump()?;
    let ours = nodes_named(&objects, stream_name);
    let mut from: Vec<u32> = objects
        .iter()
        .filter(|o| o["type"] == "PipeWire:Interface:Link")
        .filter(|o| {
            o["info"]["input-node-id"]
                .as_u64()
                .is_some_and(|i| ours.contains(&i))
        })
        .filter_map(|o| o["info"]["output-node-id"].as_u64())
        .map(|i| i as u32)
        .collect();
    from.sort_unstable();
    from.dedup();
    Some(from)
}

/// A temporary PipeWire `Video/Source` node (`videotestsrc ! pipewiresink mode=provide`), so the
/// positive targeting check runs without a camera. Dropping it removes the node.
pub struct TestNode {
    pipeline: gst::Pipeline,
    /// The node's object id (what the portal would hand out).
    pub id: u32,
}

impl Drop for TestNode {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Creates a [`TestNode`] and waits (up to `timeout`) until `pw-dump` lists it.
pub fn provide_test_node(timeout: Duration) -> Result<TestNode, String> {
    let name = format!("chinstrap-target-node-{}", std::process::id());
    let pipeline = gst::Pipeline::with_name("target-node");
    let src = make("videotestsrc")?;
    set_prop(&src, "is-live", "true");
    let filter = capsfilter("video/x-raw,format=NV12,width=320,height=240,framerate=30/1")?;
    let sink = make("pipewiresink")?;
    if !set_prop(&sink, "mode", "provide") {
        return Err("pipewiresink has no 'mode' property".into());
    }
    set_prop(&sink, "client-name", &name);
    if sink.find_property("stream-properties").is_some() {
        let props = gst::Structure::builder("props")
            .field("media.class", "Video/Source")
            .field("node.name", name.as_str())
            .build();
        sink.set_property("stream-properties", props);
    }
    link_chain(&pipeline, &[src, filter, sink])?;
    let mut node = TestNode {
        pipeline: pipeline.clone(),
        id: 0,
    };
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("cannot start the test node: {e}"))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let objects = pw_dump().ok_or("pw-dump unavailable")?;
        if let Some(id) = nodes_named(&objects, &name).first() {
            node.id = *id as u32;
            return Ok(node);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("test node {name} did not appear in pw-dump"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Headless targeting check (regression for the portal `target-object` bug): connects the
/// portal source chain to the *normal* PipeWire daemon (no portal fd) targeting object id
/// `node`, waits for the first frame and reports what was negotiated and which node is linked.
/// An error means the stream could not be linked to `node`; it never falls back to another one.
pub fn check_targeting(node: u32, timeout: Duration) -> Result<TargetCheck, String> {
    let stream_name = format!("chinstrap-target-check-{}-{node}", std::process::id());
    let pipeline = gst::Pipeline::with_name("target-check");
    let src = portal_source("src", None, node)?;
    set_prop(&src, "client-name", &stream_name);
    // Video caps: without them the stream is "Stream/Input/Unknown" and is never linked.
    let filter = capsfilter("video/x-raw(memory:DMABuf); video/x-raw; image/jpeg")?;
    let sink = make("fakesink")?;
    set_prop(&sink, "sync", "false");
    link_chain(&pipeline, &[src, filter.clone(), sink.clone()])?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(1);
    let pad = sink_pad(&sink)?;
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, _| {
        let caps = pad
            .current_caps()
            .map(|c| c.to_string())
            .unwrap_or_default();
        let _ = tx.try_send(caps);
        gst::PadProbeReturn::Ok
    });
    let bus = pipeline.bus().ok_or("pipeline has no bus")?;
    let deadline = std::time::Instant::now() + timeout;
    // State changes run on their own thread: on a missing target pipewiresrc (1.6.9) posts the
    // error at once but keeps the Playing transition blocked for its 30 s connect timeout.
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let worker = pipeline.clone();
    std::thread::spawn(move || {
        let _ = worker.set_state(gst::State::Playing);
        let _ = stop_rx.recv();
        let _ = worker.set_state(gst::State::Null);
        let _ = done_tx.send(());
    });
    let result = loop {
        if let Ok(caps) = rx.try_recv() {
            break Ok(TargetCheck {
                caps,
                linked_from: linked_into(&stream_name),
            });
        }
        if std::time::Instant::now() >= deadline {
            break Err("timed out waiting for a frame".to_string());
        }
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(20),
            &[gst::MessageType::Error, gst::MessageType::Eos],
        ) {
            match msg.view() {
                gst::MessageView::Error(e) => break Err(e.error().to_string()),
                gst::MessageView::Eos(_) => break Err("EOS before a frame".to_string()),
                _ => {}
            }
        }
    };
    let _ = stop_tx.send(());
    if result.is_ok() {
        // Release the device before the caller records from it.
        let _ = done_rx.recv_timeout(Duration::from_secs(5));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeater_backend_selects_compositor_by_capability() {
        assert_eq!(RepeaterBackend::Va.factory(), "vacompositor");
        assert_eq!(RepeaterBackend::Cpu.factory(), "compositor");
    }

    #[test]
    fn video_input_spec_groups_named_pipeline_configuration() {
        gst::init().unwrap();
        let clock = gst::SystemClock::obtain();
        let spec = VideoInputSpec {
            name: "test-video",
            source: VideoSource::Test(TestBehaviour::default()),
            convert: None,
            encoder: VideoEncoder::X264,
            role: Role::Screen,
            target: VideoTarget {
                rate: Rate::Cfr((30, 1)),
                max: (1920, 1080),
            },
            clock: &clock,
            path: Path::new("test.mkv"),
            preview: None,
        };
        assert_eq!(spec.name, "test-video");
        assert_eq!(spec.encoder, VideoEncoder::X264);
        assert_eq!(spec.target.rate.cfr(), Some((30, 1)));
    }

    #[test]
    fn portal_target_uses_object_id_path_and_never_falls_back() {
        let t = portal_target(104);
        assert_eq!(t.path, "104");
        // target-object would be matched against object.serial / node.name: never set it.
        assert_eq!(t.target_object, None);
        let has = |k: &str, v: &str| t.stream_properties.iter().any(|(a, b)| *a == k && b == v);
        assert!(has("node.dont-fallback", "true"));
        assert!(has("node.dont-move", "true"));
        assert!(has("node.target", "104"));
    }

    #[test]
    fn portal_source_element_properties() {
        gst::init().unwrap();
        if !encoders::has_element("pipewiresrc") {
            eprintln!("pipewiresrc missing; skipped");
            return;
        }
        let src = portal_source("src", Some(42), 104).unwrap();
        assert_eq!(
            src.property::<Option<String>>("path").as_deref(),
            Some("104")
        );
        assert_eq!(src.property::<Option<String>>("target-object"), None);
        assert_eq!(src.property::<i32>("fd"), 42);
        let props = src
            .property::<Option<gst::Structure>>("stream-properties")
            .expect("stream-properties set");
        assert_eq!(props.get::<&str>("node.dont-fallback").unwrap(), "true");
        assert_eq!(props.get::<&str>("node.dont-move").unwrap(), "true");
        assert_eq!(props.get::<&str>("node.target").unwrap(), "104");
        // Without a portal fd the normal daemon is used (fd stays -1).
        let src = portal_source("src2", None, 7).unwrap();
        assert_eq!(src.property::<i32>("fd"), -1);
    }

    #[test]
    fn max_rate_is_an_enum_that_never_exceeds_the_cap() {
        gst::init().unwrap();
        let f = max_rate_field(60);
        assert!(
            f.starts_with("max-framerate=(fraction){ 60/1, 60000/1001, 5994/100, 50/1"),
            "{f}"
        );
        assert!(!f.contains("144/1"), "{f}");
        let caps = gst::Caps::from_str(&portal_source_caps(Some(60))).expect("caps parse");
        assert_eq!(caps.size(), 2);
        // A producer offering up to 143.999 Hz intersects at 60.
        let producer =
            gst::Caps::from_str("video/x-raw,max-framerate=(fraction)[1/1, 143999/1000]").unwrap();
        let mut fixed = caps.intersect(&producer);
        fixed.fixate();
        assert_eq!(source_facts(&fixed).max_rate, Some((60, 1)));
        // A 59.94 Hz output still negotiates.
        let ntsc =
            gst::Caps::from_str("video/x-raw,max-framerate=(fraction)[1/1, 59940/1000]").unwrap();
        let mut fixed = caps.intersect(&ntsc);
        fixed.fixate();
        let (n, d) = source_facts(&fixed).max_rate.unwrap();
        assert!((n as f64 / d as f64 - 59.94).abs() < 1e-9, "{n}/{d}");
        assert_eq!(portal_source_caps(None), SOURCE_CAPS);
    }

    #[test]
    fn test_size_parsing() {
        assert_eq!(parse_size("3840x2160"), Some((3840, 2160)));
        assert_eq!(parse_size(" 1920X1080 "), Some((1920, 1080)));
        assert_eq!(parse_size("1280×720"), Some((1280, 720)));
        assert_eq!(parse_size("0x720"), None);
        assert_eq!(parse_size("4k"), None);
        assert_eq!(parse_size("99999x10"), None);
    }

    /// Finds a PipeWire Video/Source device: `CHINSTRAP_SELFTEST_CAMERA` substring if set, else
    /// the first one that is not the default (so the test proves targeting beats the default).
    fn find_pipewire_camera() -> Option<(String, u32)> {
        let monitor = gst::DeviceMonitor::new();
        monitor.add_filter(Some("Video/Source"), None);
        monitor.start().ok()?;
        let want = std::env::var("CHINSTRAP_SELFTEST_CAMERA").ok();
        let found = monitor.devices().into_iter().find_map(|d| {
            let props = d.properties()?;
            let id = super::super::devices::prop_string(&props, "object.id")?
                .parse::<u32>()
                .ok()?;
            let name = d.display_name().to_string();
            let default = props.get::<bool>("is-default").unwrap_or(false);
            let ok = match &want {
                Some(w) => name.to_lowercase().contains(&w.to_lowercase()),
                None => !default,
            };
            ok.then_some((name, id))
        });
        monitor.stop();
        found
    }

    /// Live regression test (needs PipeWire; uses a non-default camera when one is attached,
    /// else a temporary Video/Source node): `cargo test -- --ignored targeting`.
    #[test]
    #[ignore = "needs a running PipeWire daemon"]
    fn targeting_links_the_requested_node() {
        gst::init().unwrap();
        // A real camera when one is attached, else a temporary Video/Source node.
        let mut _node = None;
        let (name, id) = match find_pipewire_camera() {
            Some(c) => c,
            None => {
                let n = provide_test_node(Duration::from_secs(5)).expect("test node");
                let id = n.id;
                _node = Some(n);
                ("test node".to_string(), id)
            }
        };
        let check = check_targeting(id, Duration::from_secs(10))
            .unwrap_or_else(|e| panic!("targeting {name} (object.id {id}) failed: {e}"));
        eprintln!("targeted {name} #{id}: {check:?}");
        let linked = check.linked_from.expect("pw-dump unavailable");
        assert_eq!(linked, vec![id], "linked to {linked:?}, wanted only #{id}");
    }

    #[test]
    #[ignore = "needs a running PipeWire daemon"]
    fn targeting_a_missing_node_fails_loudly() {
        gst::init().unwrap();
        match check_targeting(999_999_999, Duration::from_secs(10)) {
            Ok(c) => panic!("a missing node must not fall back, got {c:?}"),
            Err(e) => assert!(e.contains("target not found"), "{e}"),
        }
    }
}
