//! Element checks (SPEC §5.7), the video encoder probe (§5.6) and the GPU path probe (§5.2).
//!
//! The probes run on the engine thread before the first Snapshot; that is the only accepted
//! blocking period. Per encoder candidate: the small probe (≤2 s) plus the throughput
//! measurement (4K, and 1080p when 4K fails, sharing one ≤2 s budget), so ≤4 s per candidate.
//! Both VA encoders (H.264 and HEVC) and x264 are measured, so the capacity plan (`plan.rs`)
//! can spread the inputs over them; openh264 is only probed when none of those works.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

use super::types::{GpuConvert, InputClass, REQUIRED_ELEMENTS, VideoCodec, VideoEncoder};

pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Screen/window/test frame rate unless `CHINSTRAP_SCREEN_FPS` (1..=60) overrides it.
pub const DEFAULT_SCREEN_FPS: i32 = 30;
pub const MAX_FPS: i32 = 60;
/// Size the encoder probe checks: UHD.
pub const UHD: (i32, i32) = (3840, 2160);
/// Size assumed when an encoder cannot do UHD.
pub const FHD: (i32, i32) = (1920, 1080);

/// Bits per pixel per frame: screens carry sharp text, cameras are noisier but smoother.
pub const SCREEN_BPP: f64 = 0.18;
pub const CAMERA_BPP: f64 = 0.12;
/// HEVC reaches the same quality with about 0.65 times the H.264 bits.
pub const HEVC_FACTOR: f64 = 0.65;
/// Floor for any computed bitrate's bits per pixel.
pub const MIN_BPP: f64 = 0.1;
pub const MIN_KBPS: u32 = 2_000;
pub const MAX_KBPS: u32 = 400_000;

/// `CHINSTRAP_SCREEN_FPS` parsing: 1..=60, else the default.
pub fn parse_screen_fps(v: Option<&str>) -> i32 {
    v.and_then(|v| v.trim().parse::<i32>().ok())
        .filter(|f| (1..=MAX_FPS).contains(f))
        .unwrap_or(DEFAULT_SCREEN_FPS)
}

pub fn screen_fps() -> i32 {
    parse_screen_fps(std::env::var("CHINSTRAP_SCREEN_FPS").ok().as_deref())
}

/// Bitrate in kbit/s for `w`×`h` at `fps`: `override_kbps` when given, else
/// `bpp × pixels × fps` (bpp ≥ 0.1), clamped to 2..400 Mbit/s.
/// Screen 1080p30 ≈ 11 Mbit/s, 4K30 ≈ 45 Mbit/s; camera 4K25 ≈ 25 Mbit/s.
#[cfg(test)]
pub fn bitrate_for(role: Role, override_kbps: Option<u32>, w: i32, h: i32, fps: (i32, i32)) -> u32 {
    bitrate_for_codec(role, VideoCodec::H264, override_kbps, w, h, fps)
}

/// [`bitrate_for`] for `codec` (HEVC: [`HEVC_FACTOR`] of the H.264 rate, same floor).
pub fn bitrate_for_codec(
    role: Role,
    codec: VideoCodec,
    override_kbps: Option<u32>,
    w: i32,
    h: i32,
    fps: (i32, i32),
) -> u32 {
    if let Some(k) = override_kbps.filter(|k| *k > 0) {
        return k.min(MAX_KBPS);
    }
    let factor = match codec {
        VideoCodec::H264 => 1.0,
        VideoCodec::Hevc => HEVC_FACTOR,
    };
    let bpp = (match role {
        Role::Screen => SCREEN_BPP,
        Role::Camera => CAMERA_BPP,
    } * factor)
        .max(MIN_BPP);
    let rate = fps.0.max(1) as f64 / fps.1.max(1) as f64;
    let kbps = (w.max(1) as f64 * h.max(1) as f64 * rate * bpp / 1000.0).round() as u32;
    kbps.clamp(MIN_KBPS, MAX_KBPS)
}

/// [`bitrate_for_codec`] with `CHINSTRAP_SCREEN_BITRATE_KBPS` / `CHINSTRAP_CAMERA_BITRATE_KBPS`.
pub fn bitrate_kbps(role: Role, codec: VideoCodec, w: i32, h: i32, fps: (i32, i32)) -> u32 {
    let key = match role {
        Role::Screen => "CHINSTRAP_SCREEN_BITRATE_KBPS",
        Role::Camera => "CHINSTRAP_CAMERA_BITRATE_KBPS",
    };
    let over = std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok());
    bitrate_for_codec(role, codec, over, w, h, fps)
}

/// Sets an encoder's bitrate (`kbps` kbit/s; openh264enc takes bit/s).
pub fn set_bitrate(el: &gst::Element, enc: VideoEncoder, kbps: u32) {
    let value = match enc {
        VideoEncoder::OpenH264 => (kbps as u64 * 1000).to_string(),
        _ => kbps.to_string(),
    };
    set_prop(el, "bitrate", &value);
}

pub fn has_element(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// Every entry of `REQUIRED_ELEMENTS` whose factory is missing.
pub fn check_elements() -> Vec<(&'static str, InputClass, &'static str)> {
    REQUIRED_ELEMENTS
        .iter()
        .copied()
        .filter(|(name, _, _)| !has_element(name))
        .collect()
}

/// True when no *hard* element of `class` (or of `Any`) is missing.
pub fn class_ok(missing: &[String], class: InputClass) -> bool {
    !REQUIRED_ELEMENTS.iter().any(|(name, c, _)| {
        (*c == class || *c == InputClass::Any) && missing.iter().any(|m| m == name)
    })
}

/// Elements a GPU conversion path needs.
pub fn convert_elements(convert: GpuConvert) -> &'static [&'static str] {
    match convert {
        GpuConvert::Va => &["vapostproc"],
        GpuConvert::Gl => &["glupload", "glcolorconvert", "gldownload"],
    }
}

/// Encoder candidates: preferred first, then `FALLBACK_ORDER`, deduplicated.
pub fn candidates(preferred: Option<VideoEncoder>) -> Vec<VideoEncoder> {
    let mut out = Vec::new();
    for e in preferred.into_iter().chain(VideoEncoder::FALLBACK_ORDER) {
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// GPU convert candidates: preferred first, then Va, then Gl, deduplicated.
pub fn convert_candidates(preferred: Option<GpuConvert>) -> Vec<GpuConvert> {
    let mut out = Vec::new();
    for c in preferred
        .into_iter()
        .chain([GpuConvert::Va, GpuConvert::Gl])
    {
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// The raw format fed to an encoder.
pub fn encoder_format(enc: VideoEncoder) -> &'static str {
    match enc {
        VideoEncoder::Va | VideoEncoder::VaHevc => "NV12",
        _ => "I420",
    }
}

/// The parser after an encoder.
pub fn parser_name(enc: VideoEncoder) -> &'static str {
    match enc.codec() {
        VideoCodec::H264 => "h264parse",
        VideoCodec::Hevc => "h265parse",
    }
}

/// Caps after the parser: 8-bit 4:2:0 profiles the muxer and editors take.
pub fn coded_caps(enc: VideoEncoder) -> &'static str {
    match enc.codec() {
        VideoCodec::H264 => "video/x-h264,profile=(string){high,main,constrained-baseline}",
        VideoCodec::Hevc => "video/x-h265,profile=(string)main",
    }
}

/// `outcaps` for the video chain (§5.2 table). `convert == None` means plain system memory.
/// `fps == None` (variable frame rate) leaves the rate to the source.
pub fn outcaps_string(
    enc: VideoEncoder,
    convert: Option<GpuConvert>,
    width: i32,
    height: i32,
    fps: Option<(i32, i32)>,
) -> String {
    let feature = if convert == Some(GpuConvert::Va) && enc.is_gpu() {
        "(memory:VAMemory)"
    } else {
        ""
    };
    let rate = fps
        .map(|(n, d)| format!(",framerate={n}/{}", d.max(1)))
        .unwrap_or_default();
    format!(
        "video/x-raw{feature},format={},width={width},height={height}{rate},pixel-aspect-ratio=1/1",
        encoder_format(enc)
    )
}

/// gst-launch description of a `<CONVERT>` block (used by probes). `format` is the raw format
/// the GL path converts to on the GPU before `gldownload` (see `recorder::convert_block`).
pub fn convert_launch(convert: Option<GpuConvert>, format: &str) -> String {
    match convert {
        Some(GpuConvert::Va) => "vapostproc add-borders=true".into(),
        Some(GpuConvert::Gl) => format!(
            "glupload ! glcolorconvert ! video/x-raw(memory:GLMemory),format={format} ! gldownload ! videoconvert ! videoscale add-borders=true"
        ),
        None => "videoconvert ! videoscale add-borders=true".into(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Screen,
    Camera,
}

/// Sets a property from a string, only if the element has it. Returns whether it was set.
pub fn set_prop(el: &gst::Element, name: &str, value: &str) -> bool {
    if el.find_property(name).is_some() {
        el.set_property_from_str(name, value);
        true
    } else {
        false
    }
}

/// Configures an encoder (§5.6 table): a keyframe every 2 s at `fps`, and a 1080p bitrate
/// until the size lock sets the real one ([`set_bitrate`]).
///
/// VA: one reference frame. On radeonsi (RX 7900 XT, Mesa 26.2) vah264enc's default of 3
/// halves throughput: one 4K stream 46 fps vs 98 fps, and the encode engine then falls behind a
/// 4K25 camera plus two 4K30 windows (the camera's repeated frames in the 2026-10-22 recording).
pub fn configure_encoder(el: &gst::Element, enc: VideoEncoder, role: Role, fps: (i32, i32)) {
    let rate = fps.0.max(1) as f64 / fps.1.max(1) as f64;
    let gop = ((rate * 2.0).round() as i64).clamp(1, 1024).to_string();
    match enc {
        VideoEncoder::Va | VideoEncoder::VaHevc => {
            set_prop(el, "key-int-max", &gop);
            set_prop(el, "rate-control", "cbr");
            set_prop(el, "ref-frames", "1");
        }
        VideoEncoder::X264 => {
            set_prop(el, "tune", "zerolatency");
            set_prop(el, "speed-preset", "veryfast");
            set_prop(el, "key-int-max", &gop);
        }
        VideoEncoder::OpenH264 => {
            set_prop(el, "gop-size", &gop);
            set_prop(el, "complexity", "low");
        }
    }
    set_bitrate(
        el,
        enc,
        bitrate_for_codec(role, enc.codec(), None, FHD.0, FHD.1, fps),
    );
}

/// Creates and configures an encoder (§5.6 table).
pub fn make_encoder(
    enc: VideoEncoder,
    role: Role,
    fps: (i32, i32),
) -> Result<gst::Element, String> {
    let el = gst::ElementFactory::make(enc.element_name())
        .build()
        .map_err(|e| format!("{}: {e}", enc.element_name()))?;
    configure_encoder(&el, enc, role, fps);
    Ok(el)
}

/// Runs a launch-line pipeline to EOS, bounded by `timeout`. Blocking.
pub fn run_probe(desc: &str, timeout: Duration) -> Result<(), String> {
    let element = gst::parse::launch(desc).map_err(|e| e.to_string())?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;
    run_pipeline(&pipeline, timeout)
}

/// Runs `pipeline` to EOS, bounded by `timeout`, then sets it to Null. Blocking.
fn run_pipeline(pipeline: &gst::Pipeline, timeout: Duration) -> Result<(), String> {
    let bus = pipeline.bus().ok_or("pipeline has no bus")?;
    let deadline = Instant::now() + timeout;
    let result = (|| {
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| format!("failed to start: {e}"))?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out".to_string());
            }
            let Some(msg) = bus.timed_pop_filtered(
                gst::ClockTime::from_nseconds(remaining.as_nanos() as u64),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            ) else {
                return Err("timed out".to_string());
            };
            match msg.view() {
                gst::MessageView::Eos(_) => return Ok(()),
                gst::MessageView::Error(e) => {
                    return Err(format!(
                        "{}{}",
                        e.error(),
                        e.debug().map(|d| format!(" ({d})")).unwrap_or_default()
                    ));
                }
                _ => {}
            }
        }
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// Probe description for one encoder (§5.6).
pub fn encoder_probe_launch(enc: VideoEncoder) -> String {
    format!(
        "videotestsrc num-buffers=5 ! video/x-raw,width=320,height=240,framerate=30/1 ! videoconvert ! {} ! {} ! fakesink",
        enc.element_name(),
        parser_name(enc)
    )
}

/// What an encoder was verified to do (§5.6 as revised for 4K).
#[derive(Clone, Debug, PartialEq)]
pub struct EncoderCap {
    pub enc: VideoEncoder,
    /// Largest verified size (UHD/DCI 4K, or 1080p when 4K failed).
    pub max: (i32, i32),
    /// Measured encode throughput (frames per second) at 3840×2160, or at 1920×1080 when
    /// `max` is 1080p.
    pub fps_at_max: f64,
}

impl EncoderCap {
    pub fn fits(&self, w: i32, h: i32) -> bool {
        w <= self.max.0 && h <= self.max.1
    }

    /// Estimated throughput at `w`×`h` (scales with pixel count, never above 4× the measure).
    pub fn fps_at(&self, w: i32, h: i32) -> f64 {
        // `fps_at_max` is measured at 3840×2160 (1920×1080 when 4K failed), even when `max`
        // is DCI 4096×2160.
        let measured = if self.max == FHD { FHD } else { UHD };
        let ref_area = measured.0 as f64 * measured.1 as f64;
        let max_area = self.max.0 as f64 * self.max.1 as f64;
        let area = (w.max(1) as f64 * h.max(1) as f64).min(max_area);
        self.fps_at_max * (ref_area / area).min(4.0)
    }
}

/// True when the encoder's sink template admits raw video of `w`×`h`.
pub fn template_accepts(enc: VideoEncoder, w: i32, h: i32) -> bool {
    let Some(factory) = gst::ElementFactory::find(enc.element_name()) else {
        return false;
    };
    let want = gst::Caps::builder("video/x-raw")
        .field("width", w)
        .field("height", h)
        .build();
    factory
        .static_pad_templates()
        .iter()
        .filter(|t| t.direction() == gst::PadDirection::Sink)
        .any(|t| t.caps().can_intersect(&want))
}

/// Encodes a moving 4:2:0 test pattern of `w`×`h` as fast as possible and returns the measured
/// throughput (frames/s, first to last encoded frame, so start-up is excluded). Bounded by
/// `timeout`; a timeout after at least 5 frames still yields a (low) measurement.
/// videotestsrc renders 4K SMPTE at ~150 fps on its own thread (behind the queue), well above
/// the encoders measured, so the source does not cap the result.
pub fn measure_encoder(
    enc: VideoEncoder,
    w: i32,
    h: i32,
    timeout: Duration,
) -> Result<f64, String> {
    if timeout < Duration::from_millis(200) {
        return Err("no time left to measure".into());
    }
    const FRAMES: u32 = 40;
    let desc = format!(
        "videotestsrc num-buffers={FRAMES} pattern=smpte horizontal-speed=8 ! \
         video/x-raw,format={},width={w},height={h},framerate=60/1 ! \
         queue max-size-buffers=4 max-size-bytes=0 max-size-time=0 ! {} name=enc ! \
         {} ! fakesink name=sink sync=false",
        encoder_format(enc),
        enc.element_name(),
        parser_name(enc)
    );
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| e.to_string())?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;
    let el = pipeline.by_name("enc").ok_or("no encoder")?;
    configure_encoder(&el, enc, Role::Screen, (60, 1));
    set_bitrate(
        &el,
        enc,
        bitrate_for_codec(Role::Screen, enc.codec(), None, w, h, (30, 1)),
    );
    // (frames, first, last) at the sink.
    type Stats = (u32, Option<Instant>, Option<Instant>);
    let stats: Arc<Mutex<Stats>> = Arc::new(Mutex::new((0, None, None)));
    let probe_stats = stats.clone();
    pipeline
        .by_name("sink")
        .and_then(|s| s.static_pad("sink"))
        .ok_or("no sink pad")?
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            if let Ok(mut s) = probe_stats.lock() {
                let now = Instant::now();
                s.0 += 1;
                s.1.get_or_insert(now);
                s.2 = Some(now);
            }
            gst::PadProbeReturn::Ok
        });
    let run = run_pipeline(&pipeline, timeout);
    let (n, first, last) = *stats.lock().map_err(|_| "poisoned")?;
    let measured = match (first, last) {
        (Some(f), Some(l)) if n >= 5 && l > f => Some((n - 1) as f64 / (l - f).as_secs_f64()),
        _ => None,
    };
    match (run, measured) {
        (Ok(()), Some(fps)) => Ok(fps),
        (Err(e), Some(fps)) if e == "timed out" => Ok(fps),
        (Ok(()), None) => Err(format!("only {n} frames encoded")),
        (Err(e), _) => Err(e),
    }
}

/// Probes the candidates in preference order (§5.6): each must exist, pass the small probe, and
/// is then measured at 3840×2160 (or 1920×1080 when 4K fails), both measurements sharing one
/// `PROBE_TIMEOUT` budget. Probing stops at the first encoder that encodes 4K at `need_fps`.
/// The first entry is the default encoder. Returns the verified table and one Info line per
/// candidate.
pub fn select_encoders(preferred: Option<VideoEncoder>) -> (Vec<EncoderCap>, Vec<String>) {
    let mut notes = Vec::new();
    let mut table: Vec<EncoderCap> = Vec::new();
    for enc in candidates(preferred) {
        let name = enc.element_name();
        // openh264 (software, slow) only when nothing better works.
        if enc == VideoEncoder::OpenH264 && !table.is_empty() {
            continue;
        }
        if !has_element(name) || !has_element(parser_name(enc)) {
            notes.push(format!(
                "Video encoder {name} (or {}) is not installed",
                parser_name(enc)
            ));
            continue;
        }
        if let Err(e) = run_probe(&encoder_probe_launch(enc), PROBE_TIMEOUT) {
            notes.push(format!("Video encoder {name} failed its probe: {e}"));
            continue;
        }
        let budget = Instant::now() + PROBE_TIMEOUT;
        let left = || budget.saturating_duration_since(Instant::now());
        let uhd = if template_accepts(enc, UHD.0, UHD.1) {
            measure_encoder(enc, UHD.0, UHD.1, left())
        } else {
            Err("its caps stop below 3840×2160".into())
        };
        let cap = match uhd {
            Ok(fps) => {
                let dci = template_accepts(enc, 4096, 2160);
                EncoderCap {
                    enc,
                    max: if dci { (4096, 2160) } else { UHD },
                    fps_at_max: fps,
                }
            }
            Err(e) => {
                notes.push(format!("Video encoder {name} cannot encode 3840×2160: {e}"));
                let fps = measure_encoder(enc, FHD.0, FHD.1, left()).unwrap_or(0.0);
                EncoderCap {
                    enc,
                    max: FHD,
                    fps_at_max: fps,
                }
            }
        };
        notes.push(format!(
            "Video encoder {name}: up to {}×{}, ~{:.0} fps measured at {}",
            cap.max.0,
            cap.max.1,
            cap.fps_at_max,
            if cap.max == FHD {
                "1920×1080"
            } else {
                "3840×2160"
            }
        ));
        table.push(cap);
    }
    (table, notes)
}

/// Probe description for a GPU path (§5.2): upload to GPU memory, then the CONVERT block.
pub fn gpu_probe_launch(convert: GpuConvert, enc: VideoEncoder) -> String {
    let upload = match convert {
        GpuConvert::Va => "vapostproc ! video/x-raw(memory:DMABuf)",
        GpuConvert::Gl => "glupload",
    };
    format!(
        "videotestsrc num-buffers=5 ! video/x-raw,width=320,height=240,framerate=30/1 ! {upload} ! {} ! {} ! {} ! fakesink",
        convert_launch(Some(convert), encoder_format(enc)),
        outcaps_string(enc, Some(convert), 640, 360, Some((30, 1))),
        enc.element_name()
    )
}

/// Picks the first GPU conversion path (preferred, Va, Gl) whose elements exist and whose probe
/// reaches EOS with `enc`.
pub fn select_convert(
    preferred: Option<GpuConvert>,
    enc: Option<VideoEncoder>,
) -> (Option<GpuConvert>, Vec<String>) {
    let mut notes = Vec::new();
    let Some(enc) = enc else {
        return (None, notes);
    };
    for convert in convert_candidates(preferred) {
        if !convert_elements(convert).iter().all(|e| has_element(e)) {
            notes.push(format!("GPU convert {convert:?}: elements missing"));
            continue;
        }
        match run_probe(&gpu_probe_launch(convert, enc), PROBE_TIMEOUT) {
            Ok(()) => return (Some(convert), notes),
            Err(e) => notes.push(format!("GPU convert {convert:?} failed its probe: {e}")),
        }
    }
    (None, notes)
}

pub fn convert_name(convert: Option<GpuConvert>) -> Option<String> {
    convert.map(|c| match c {
        GpuConvert::Va => "va".to_string(),
        GpuConvert::Gl => "gl".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_element_name_round_trip() {
        for e in VideoEncoder::FALLBACK_ORDER {
            assert_eq!(VideoEncoder::from_element_name(e.element_name()), Some(e));
        }
        assert_eq!(VideoEncoder::from_element_name("nvh264enc"), None);
    }

    #[test]
    fn candidate_order() {
        assert_eq!(candidates(None), VideoEncoder::FALLBACK_ORDER.to_vec());
        assert_eq!(
            candidates(Some(VideoEncoder::OpenH264)),
            vec![
                VideoEncoder::OpenH264,
                VideoEncoder::Va,
                VideoEncoder::VaHevc,
                VideoEncoder::X264
            ]
        );
        assert_eq!(
            convert_candidates(Some(GpuConvert::Gl)),
            vec![GpuConvert::Gl, GpuConvert::Va]
        );
    }

    #[test]
    fn outcaps_table() {
        assert!(
            outcaps_string(
                VideoEncoder::Va,
                Some(GpuConvert::Va),
                1280,
                720,
                Some((30, 1))
            )
            .starts_with("video/x-raw(memory:VAMemory),format=NV12,width=1280,height=720")
        );
        assert!(
            outcaps_string(
                VideoEncoder::X264,
                Some(GpuConvert::Va),
                2,
                2,
                Some((30, 1))
            )
            .starts_with("video/x-raw,format=I420")
        );
        assert!(
            outcaps_string(VideoEncoder::Va, Some(GpuConvert::Gl), 2, 2, Some((30, 1)))
                .starts_with("video/x-raw,format=NV12")
        );
    }

    #[test]
    fn outcaps_carry_native_rate() {
        let c = outcaps_string(VideoEncoder::X264, None, 3840, 2160, Some((25, 1)));
        assert!(c.contains("width=3840,height=2160,framerate=25/1"), "{c}");
        let c = outcaps_string(VideoEncoder::X264, None, 1920, 1080, Some((30000, 1001)));
        assert!(c.contains("framerate=30000/1001"), "{c}");
        // Variable frame rate: no rate, HEVC on VA memory.
        let c = outcaps_string(VideoEncoder::VaHevc, Some(GpuConvert::Va), 3840, 2160, None);
        assert!(!c.contains("framerate"), "{c}");
        assert!(
            c.starts_with("video/x-raw(memory:VAMemory),format=NV12"),
            "{c}"
        );
    }

    #[test]
    fn screen_fps_env_parsing() {
        assert_eq!(parse_screen_fps(None), 30);
        assert_eq!(parse_screen_fps(Some("60")), 60);
        assert_eq!(parse_screen_fps(Some(" 24 ")), 24);
        assert_eq!(parse_screen_fps(Some("120")), 30);
        assert_eq!(parse_screen_fps(Some("0")), 30);
        assert_eq!(parse_screen_fps(Some("fast")), 30);
    }

    #[test]
    fn bitrate_scales_with_pixel_rate() {
        let s1080 = bitrate_for(Role::Screen, None, 1920, 1080, (30, 1));
        let s4k = bitrate_for(Role::Screen, None, 3840, 2160, (30, 1));
        assert!((10_000..=13_000).contains(&s1080), "{s1080}");
        assert!((40_000..=50_000).contains(&s4k), "{s4k}");
        assert!(s4k.abs_diff(s1080 * 4) <= 4, "{s4k} vs 4×{s1080}");
        let c4k25 = bitrate_for(Role::Camera, None, 3840, 2160, (25, 1));
        // At least 0.1 bits per pixel.
        assert!(
            c4k25 as f64 * 1000.0 >= 3840.0 * 2160.0 * 25.0 * MIN_BPP,
            "{c4k25}"
        );
        assert!(bitrate_for(Role::Screen, None, 3840, 2160, (60, 1)) > s4k);
        assert_eq!(bitrate_for(Role::Camera, None, 160, 120, (5, 1)), MIN_KBPS);
        assert_eq!(
            bitrate_for(Role::Screen, Some(30_000), 3840, 2160, (30, 1)),
            30_000
        );
        assert_eq!(
            bitrate_for(Role::Screen, Some(0), 1920, 1080, (30, 1)),
            s1080
        );
        let h4k = bitrate_for_codec(Role::Screen, VideoCodec::Hevc, None, 3840, 2160, (30, 1));
        assert!(
            h4k < s4k && h4k as f64 >= s4k as f64 * 0.6,
            "{h4k} vs {s4k}"
        );
    }

    #[test]
    fn class_ok_checks_any_and_class() {
        assert!(class_ok(&[], InputClass::Mic));
        assert!(!class_ok(&["opusenc".into()], InputClass::Mic));
        assert!(class_ok(&["opusenc".into()], InputClass::Video));
        assert!(!class_ok(&["matroskamux".into()], InputClass::Video));
    }
}
