//! Pipeline builders (SPEC §5.2-§5.5) and the source-negotiation probe (§5.2).
//!
//! Builders only construct pipelines in NULL; every state change is driven by the engine on
//! worker threads (D10). Each input gets its own `gst::Pipeline` on the shared clock (D1).

use std::os::fd::RawFd;
use std::path::Path;
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;

use super::devices::{CameraClass, CameraPath, Entry};
use super::encoders::{self, Role, set_prop};
use super::stamp::{self, SizeSlot, UNSET};
use super::types::{GpuConvert, H264Encoder};

/// Caps accepted right after a pipewiresrc (D7): DMA-BUF with modifiers, or plain raw.
pub const SOURCE_CAPS: &str = "video/x-raw(memory:DMABuf),format=DMA_DRM; video/x-raw";
/// Default test pattern size; `CHINSTRAP_TEST_SIZE=WxH` overrides it.
pub const TEST_SIZE: (i32, i32) = (1280, 720);
pub const TEST_RESIZE_CAPS: &str = "video/x-raw,width=1000,height=700,framerate=30/1";
pub const H264_CAPS: &str = "video/x-h264,profile=(string){high,main,constrained-baseline}";
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

pub fn test_caps() -> String {
    let (w, h) = test_size();
    format!("video/x-raw,width={w},height={h},framerate=30/1")
}

/// What one video input records: frame rate and the largest size before downscaling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoTarget {
    pub fps: (i32, i32),
    pub max: (i32, i32),
}

/// Where a video input's frames come from.
pub enum VideoSource<'a> {
    Test,
    Portal {
        fd: RawFd,
        node: u32,
    },
    Camera {
        entry: &'a Entry,
        class: CameraClass,
    },
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
    /// The H.264 encoder used (video only, manifest).
    pub encoder: Option<H264Encoder>,
    /// Recorded frame rate (video only, manifest).
    pub fps: Option<(i32, i32)>,
    /// Warning to surface once (e.g. `add-borders` missing).
    pub warning: Option<String>,
}

fn make(factory: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|e| format!("cannot create {factory}: {e}"))
}

fn capsfilter(caps: &str) -> Result<gst::Element, String> {
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
    set_prop(&src, "keepalive-time", "1000");
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

fn mux_probe(before_mux: &gst::Element) -> Result<(Arc<AtomicU64>, Arc<AtomicU64>), String> {
    let first = Arc::new(AtomicU64::new(UNSET));
    let last = Arc::new(AtomicU64::new(UNSET));
    let pad = src_pad(before_mux)?.peer().ok_or("muxer pad not linked")?;
    stamp::install_mux_probe(&pad, first.clone(), last.clone());
    Ok((first, last))
}

/// Builds a video input (§5.2 screen/window/test, §5.4 camera) in NULL.
#[allow(clippy::too_many_arguments)]
pub fn build_video(
    name: &str,
    source: VideoSource<'_>,
    convert: Option<GpuConvert>,
    enc: H264Encoder,
    role: Role,
    target: VideoTarget,
    clock: &gst::Clock,
    path: &Path,
) -> Result<InputPipeline, String> {
    let pipeline = new_pipeline(name, clock);
    let base = Arc::new(AtomicU64::new(UNSET));

    // Source section: `src_chain` ends at the element whose src pad carries the source caps.
    let mut src_chain: Vec<gst::Element> = Vec::new();
    let mut resize_filter = None;
    let mut camera_mode = None;
    match &source {
        VideoSource::Test => {
            let src = make("videotestsrc")?;
            set_prop(&src, "is-live", "true");
            set_prop(&src, "pattern", "smpte");
            let caps = capsfilter(&test_caps())?;
            resize_filter = Some(caps.clone());
            src_chain.push(src);
            src_chain.push(caps);
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
        VideoSource::Camera { entry, class } => {
            let mode = match class {
                CameraClass::Usable(m) => m.clone(),
                CameraClass::Unsupported(r) => return Err(r.clone()),
            };
            src_chain.push(entry.create_source("cam-src")?);
            camera_mode = Some(mode);
        }
    }

    // Middle: source capsfilter, decode/convert, rate.
    let mut mid: Vec<gst::Element> = Vec::new();
    let format = encoders::encoder_format(enc);
    let (add_borders_missing, effective_convert) = match &camera_mode {
        Some(mode) if mode.path != CameraPath::DmaBuf => {
            // §5.4: exactly the chosen native mode, decoded/converted on the CPU.
            mid.push(capsfilter(&mode.source_caps())?);
            if mode.path == CameraPath::Jpeg {
                mid.push(make("jpegparse")?);
                mid.push(make("jpegdec")?);
            }
            mid.push(queue()?);
            let (block, missing) = convert_block(None, format)?;
            mid.extend(block);
            // skip-to-first: no frames are invented before the camera's first real frame, so
            // the muxer's first_pts_ns reports the camera's real start latency.
            let rate = make("videorate")?;
            set_prop(&rate, "skip-to-first", "true");
            mid.push(rate);
            (missing, None)
        }
        _ => {
            // §5.2 chain (portal, test pattern, DMA-BUF camera).
            let caps = match &camera_mode {
                Some(mode) => mode.source_caps(),
                None => SOURCE_CAPS.to_string(),
            };
            mid.push(capsfilter(&caps)?);
            let (block, missing) = convert_block(convert, format)?;
            mid.extend(block);
            mid.push(leaky_queue()?);
            let rate = make("videorate")?;
            set_prop(&rate, "skip-to-first", "true");
            mid.push(rate);
            (missing, convert)
        }
    };
    let outcaps = gst::ElementFactory::make("capsfilter")
        .name("outcaps")
        .build()
        .map_err(|e| format!("cannot create capsfilter: {e}"))?;
    mid.push(outcaps.clone());
    mid.push(queue()?);
    let encoder = encoders::make_encoder(enc, role, target.fps)?;
    mid.push(encoder.clone());
    let parse = make("h264parse")?;
    set_prop(&parse, "config-interval", "-1");
    mid.push(parse);
    let h264caps = capsfilter(H264_CAPS)?;
    mid.push(h264caps.clone());
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
    stamp::install_video(
        &src_pad(&src_chain[0])?,
        clock.clone(),
        base.clone(),
        deep_copy,
    );
    let size = Arc::new(Mutex::new(None));
    let fps = target.fps;
    stamp::install_size_lock(
        &src_out,
        outcaps,
        target.max,
        move |w, h| {
            // The bitrate follows the locked size; the encoder has not seen caps yet.
            encoders::set_bitrate(&encoder, enc, encoders::bitrate_kbps(role, w, h, fps));
            gst::Caps::from_str(&encoders::outcaps_string(enc, effective_convert, w, h, fps))
                .unwrap_or_else(|_| gst::Caps::new_any())
        },
        size.clone(),
    );
    let (first_pts, last_pts) = mux_probe(&h264caps)?;
    let bus = pipeline.bus().ok_or("pipeline has no bus")?;
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
        fps: Some(fps),
        warning: add_borders_missing
            .then(|| "add-borders is unavailable: video will be stretched".to_string()),
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
    let (first_pts, last_pts) = mux_probe(&opus)?;
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
        fps: None,
        warning: None,
    })
}

/// Source-negotiation probe (§5.2): `pipewiresrc ! capsfilter ! <CONVERT> ! outcaps-format !
/// fakesink num-buffers=1`, on the capture's one fd number.
pub fn build_probe(
    name: &str,
    fd: RawFd,
    node: u32,
    convert: GpuConvert,
    enc: Option<H264Encoder>,
) -> Result<(gst::Pipeline, gst::Bus), String> {
    let pipeline = gst::Pipeline::with_name(name);
    let mut chain = vec![
        portal_source("src", Some(fd), node)?,
        capsfilter(SOURCE_CAPS)?,
    ];
    let fmt = enc.map(encoders::encoder_format).unwrap_or("I420");
    chain.extend(convert_block(Some(convert), fmt)?.0);
    let out = match (convert, enc) {
        (GpuConvert::Va, Some(H264Encoder::Va)) => {
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
    let bus = pipeline.bus().ok_or("pipeline has no bus")?;
    Ok((pipeline, bus))
}

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
