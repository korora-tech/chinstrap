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

use super::devices::{CameraClass, Entry};
use super::encoders::{self, Role, set_prop};
use super::stamp::{self, SizeSlot, UNSET};
use super::types::{GpuConvert, H264Encoder};

/// Caps accepted right after a pipewiresrc (D7): DMA-BUF with modifiers, or plain raw.
pub const SOURCE_CAPS: &str = "video/x-raw(memory:DMABuf),format=DMA_DRM; video/x-raw";
pub const TEST_CAPS: &str = "video/x-raw,width=1280,height=720,framerate=30/1";
pub const TEST_RESIZE_CAPS: &str = "video/x-raw,width=1000,height=700,framerate=30/1";
pub const H264_CAPS: &str = "video/x-h264,profile=(string){high,main,constrained-baseline}";
pub const MIC_CAPS: &str = "audio/x-raw,rate=48000,channels=[1,2]";

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

fn queue() -> Result<gst::Element, String> {
    make("queue")
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
            let scale = make("videoscale")?;
            let ok = set_prop(&scale, "add-borders", "true");
            Ok((
                vec![
                    make("glupload")?,
                    make("glcolorconvert")?,
                    capsfilter(&format!("video/x-raw(memory:GLMemory),format={format}"))?,
                    make("gldownload")?,
                    make("videoconvert")?,
                    scale,
                ],
                !ok,
            ))
        }
        None => {
            let scale = make("videoscale")?;
            let ok = set_prop(&scale, "add-borders", "true");
            Ok((vec![make("videoconvert")?, scale], !ok))
        }
    }
}

/// A pipewiresrc for a portal stream (§5.2). `on-disconnect` stays at its default (D9).
pub fn portal_source(name: &str, fd: RawFd, node: u32) -> Result<gst::Element, String> {
    let src = gst::ElementFactory::make("pipewiresrc")
        .name(name)
        .build()
        .map_err(|e| format!("cannot create pipewiresrc: {e}"))?;
    src.set_property_from_str("fd", &fd.to_string());
    src.set_property_from_str("target-object", &node.to_string());
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
pub fn build_video(
    name: &str,
    source: VideoSource<'_>,
    convert: Option<GpuConvert>,
    enc: H264Encoder,
    role: Role,
    clock: &gst::Clock,
    path: &Path,
) -> Result<InputPipeline, String> {
    let pipeline = new_pipeline(name, clock);
    let base = Arc::new(AtomicU64::new(UNSET));

    // Source section: `src_chain` ends at the element whose src pad carries the source caps.
    let mut src_chain: Vec<gst::Element> = Vec::new();
    let mut resize_filter = None;
    let mut camera_class = None;
    match &source {
        VideoSource::Test => {
            let src = make("videotestsrc")?;
            set_prop(&src, "is-live", "true");
            set_prop(&src, "pattern", "smpte");
            let caps = capsfilter(TEST_CAPS)?;
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
            src_chain.push(portal_source("src", *fd, *node)?);
        }
        VideoSource::Camera { entry, class } => {
            src_chain.push(entry.create_source("cam-src")?);
            camera_class = Some(class.clone());
        }
    }

    // Middle: source capsfilter, decode/convert, rate.
    let mut mid: Vec<gst::Element> = Vec::new();
    let (add_borders_missing, effective_convert) = match &camera_class {
        Some(CameraClass::Raw { .. }) | Some(CameraClass::Jpeg) => {
            let class = camera_class.as_ref().expect("camera class");
            mid.push(capsfilter(class.source_caps().expect("usable class"))?);
            if *class == CameraClass::Jpeg {
                mid.push(make("jpegparse")?);
                mid.push(make("jpegdec")?);
            }
            mid.push(queue()?);
            let (block, missing) = convert_block(None, encoders::encoder_format(enc))?;
            mid.extend(block);
            // skip-to-first: no frames are invented before the camera's first real frame, so
            // the muxer's first_pts_ns reports the camera's real start latency.
            let rate = make("videorate")?;
            set_prop(&rate, "skip-to-first", "true");
            mid.push(rate);
            (missing, None)
        }
        Some(CameraClass::Unsupported(r)) => return Err(r.clone()),
        _ => {
            // §5.2 chain (portal, test pattern, DMA-BUF camera).
            mid.push(capsfilter(SOURCE_CAPS)?);
            let (block, missing) = convert_block(convert, encoders::encoder_format(enc))?;
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
    mid.push(encoders::make_encoder(enc, role)?);
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
    stamp::install_video(&src_pad(&src_chain[0])?, clock.clone(), base.clone());
    let size = Arc::new(Mutex::new(None));
    stamp::install_size_lock(
        &src_out,
        outcaps,
        move |w, h| {
            gst::Caps::from_str(&encoders::outcaps_string(enc, effective_convert, w, h))
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
    let mut chain = vec![portal_source("src", fd, node)?, capsfilter(SOURCE_CAPS)?];
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
