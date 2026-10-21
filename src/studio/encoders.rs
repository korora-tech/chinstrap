//! Element checks (SPEC §5.7), the H.264 encoder probe (§5.6) and the GPU path probe (§5.2).
//!
//! The probes run on the engine thread before the first Snapshot; that is the only accepted
//! blocking period. Per encoder candidate: the small probe (≤2 s) plus the throughput
//! measurement (4K, and 1080p when 4K fails, sharing one ≤2 s budget), so ≤4 s per candidate;
//! probing stops at the first candidate that keeps up at 4K.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

use super::types::{GpuConvert, H264Encoder, InputClass, REQUIRED_ELEMENTS};

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
pub fn bitrate_for(role: Role, override_kbps: Option<u32>, w: i32, h: i32, fps: (i32, i32)) -> u32 {
    if let Some(k) = override_kbps.filter(|k| *k > 0) {
        return k.min(MAX_KBPS);
    }
    let bpp = match role {
        Role::Screen => SCREEN_BPP,
        Role::Camera => CAMERA_BPP,
    }
    .max(MIN_BPP);
    let rate = fps.0.max(1) as f64 / fps.1.max(1) as f64;
    let kbps = (w.max(1) as f64 * h.max(1) as f64 * rate * bpp / 1000.0).round() as u32;
    kbps.clamp(MIN_KBPS, MAX_KBPS)
}

/// [`bitrate_for`] with `CHINSTRAP_SCREEN_BITRATE_KBPS` / `CHINSTRAP_CAMERA_BITRATE_KBPS`.
pub fn bitrate_kbps(role: Role, w: i32, h: i32, fps: (i32, i32)) -> u32 {
    let key = match role {
        Role::Screen => "CHINSTRAP_SCREEN_BITRATE_KBPS",
        Role::Camera => "CHINSTRAP_CAMERA_BITRATE_KBPS",
    };
    let over = std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok());
    bitrate_for(role, over, w, h, fps)
}

/// Sets an encoder's bitrate (`kbps` kbit/s; openh264enc takes bit/s).
pub fn set_bitrate(el: &gst::Element, enc: H264Encoder, kbps: u32) {
    let value = match enc {
        H264Encoder::OpenH264 => (kbps as u64 * 1000).to_string(),
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
pub fn candidates(preferred: Option<H264Encoder>) -> Vec<H264Encoder> {
    let mut out = Vec::new();
    for e in preferred.into_iter().chain(H264Encoder::FALLBACK_ORDER) {
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
pub fn encoder_format(enc: H264Encoder) -> &'static str {
    match enc {
        H264Encoder::Va => "NV12",
        _ => "I420",
    }
}

/// `outcaps` for the video chain (§5.2 table). `convert == None` means plain system memory.
pub fn outcaps_string(
    enc: H264Encoder,
    convert: Option<GpuConvert>,
    width: i32,
    height: i32,
    fps: (i32, i32),
) -> String {
    let feature = match (convert, enc) {
        (Some(GpuConvert::Va), H264Encoder::Va) => "(memory:VAMemory)",
        _ => "",
    };
    format!(
        "video/x-raw{feature},format={},width={width},height={height},framerate={}/{},pixel-aspect-ratio=1/1",
        encoder_format(enc),
        fps.0,
        fps.1.max(1)
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
pub fn configure_encoder(el: &gst::Element, enc: H264Encoder, role: Role, fps: (i32, i32)) {
    let rate = fps.0.max(1) as f64 / fps.1.max(1) as f64;
    let gop = ((rate * 2.0).round() as i64).clamp(1, 1024).to_string();
    match enc {
        H264Encoder::Va => {
            set_prop(el, "key-int-max", &gop);
            set_prop(el, "rate-control", "cbr");
        }
        H264Encoder::X264 => {
            set_prop(el, "tune", "zerolatency");
            set_prop(el, "speed-preset", "veryfast");
            set_prop(el, "key-int-max", &gop);
        }
        H264Encoder::OpenH264 => {
            set_prop(el, "gop-size", &gop);
            set_prop(el, "complexity", "low");
        }
    }
    set_bitrate(el, enc, bitrate_for(role, None, FHD.0, FHD.1, fps));
}

/// Creates and configures an encoder (§5.6 table).
pub fn make_encoder(enc: H264Encoder, role: Role, fps: (i32, i32)) -> Result<gst::Element, String> {
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
pub fn encoder_probe_launch(enc: H264Encoder) -> String {
    format!(
        "videotestsrc num-buffers=5 ! video/x-raw,width=320,height=240,framerate=30/1 ! videoconvert ! {} ! h264parse ! fakesink",
        enc.element_name()
    )
}

/// What an encoder was verified to do (§5.6 as revised for 4K).
#[derive(Clone, Debug, PartialEq)]
pub struct EncoderCap {
    pub enc: H264Encoder,
    /// Largest verified size (UHD/DCI 4K, or 1080p when 4K failed).
    pub max: (i32, i32),
    /// Measured encode throughput at `max` (frames per second).
    pub fps_at_max: f64,
}

impl EncoderCap {
    fn fits(&self, w: i32, h: i32) -> bool {
        w <= self.max.0 && h <= self.max.1
    }

    /// Estimated throughput at `w`×`h` (scales with pixel count, never above 4× the measure).
    fn fps_at(&self, w: i32, h: i32) -> f64 {
        let max_area = self.max.0 as f64 * self.max.1 as f64;
        let area = (w.max(1) as f64 * h.max(1) as f64).min(max_area);
        self.fps_at_max * (max_area / area).min(4.0)
    }
}

/// Picks the encoder for one input that needs `w`×`h` at `fps` frames per second: the first
/// verified encoder (preference order) that fits and keeps up (5% probe slack); else the
/// fastest one that fits; else the one with the largest size (the input is downscaled).
/// Returns the encoder and the size the input may record at most.
///
/// Each input is judged on its own against a *single-stream* measurement; the load of other
/// inputs on the same encoder is deliberately not summed. On the VA host this was checked on,
/// two concurrent 4K vah264enc encodes (30 + 25 fps) each still ran at ~46-47 fps, the same as
/// one alone (the measure is latency-bound), so summing would wrongly push a 4K camera onto
/// x264enc (~9 cores). On hardware whose engine does saturate, a shortfall shows up as frames
/// repeated by videorate after the leaky queue drops.
pub fn pick_encoder(
    table: &[EncoderCap],
    w: i32,
    h: i32,
    fps: f64,
) -> Option<(H264Encoder, (i32, i32))> {
    let fitting = || table.iter().filter(|c| c.fits(w, h));
    let choice = fitting()
        .find(|c| c.fps_at(w, h) >= fps * 0.95)
        .or_else(|| fitting().max_by(|a, b| a.fps_at(w, h).total_cmp(&b.fps_at(w, h))))
        .or_else(|| table.iter().max_by_key(|c| c.max.0 as i64 * c.max.1 as i64))?;
    Some((choice.enc, choice.max))
}

/// True when the encoder's sink template admits raw video of `w`×`h`.
pub fn template_accepts(enc: H264Encoder, w: i32, h: i32) -> bool {
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
pub fn measure_encoder(enc: H264Encoder, w: i32, h: i32, timeout: Duration) -> Result<f64, String> {
    if timeout < Duration::from_millis(200) {
        return Err("no time left to measure".into());
    }
    const FRAMES: u32 = 40;
    let desc = format!(
        "videotestsrc num-buffers={FRAMES} pattern=smpte horizontal-speed=8 ! \
         video/x-raw,format={},width={w},height={h},framerate=60/1 ! \
         queue max-size-buffers=4 max-size-bytes=0 max-size-time=0 ! {} name=enc ! \
         h264parse ! fakesink name=sink sync=false",
        encoder_format(enc),
        enc.element_name()
    );
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| e.to_string())?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;
    let el = pipeline.by_name("enc").ok_or("no encoder")?;
    configure_encoder(&el, enc, Role::Screen, (60, 1));
    set_bitrate(&el, enc, bitrate_for(Role::Screen, None, w, h, (30, 1)));
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
pub fn select_encoders(
    preferred: Option<H264Encoder>,
    need_fps: i32,
) -> (Vec<EncoderCap>, Vec<String>) {
    let mut notes = Vec::new();
    let mut table = Vec::new();
    for enc in candidates(preferred) {
        let name = enc.element_name();
        if !has_element(name) {
            notes.push(format!("H.264 encoder {name} is not installed"));
            continue;
        }
        if let Err(e) = run_probe(&encoder_probe_launch(enc), PROBE_TIMEOUT) {
            notes.push(format!("H.264 encoder {name} failed its probe: {e}"));
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
                notes.push(format!("H.264 encoder {name} cannot encode 3840×2160: {e}"));
                let fps = measure_encoder(enc, FHD.0, FHD.1, left()).unwrap_or(0.0);
                EncoderCap {
                    enc,
                    max: FHD,
                    fps_at_max: fps,
                }
            }
        };
        notes.push(format!(
            "H.264 encoder {name}: up to {}×{}, ~{:.0} fps measured at {}",
            cap.max.0,
            cap.max.1,
            cap.fps_at_max,
            if cap.max == FHD {
                "1920×1080"
            } else {
                "3840×2160"
            }
        ));
        let enough = cap.max != FHD && cap.fps_at_max >= need_fps as f64 * 0.95;
        table.push(cap);
        if enough {
            break;
        }
    }
    (table, notes)
}

/// Probe description for a GPU path (§5.2): upload to GPU memory, then the CONVERT block.
pub fn gpu_probe_launch(convert: GpuConvert, enc: H264Encoder) -> String {
    let upload = match convert {
        GpuConvert::Va => "vapostproc ! video/x-raw(memory:DMABuf)",
        GpuConvert::Gl => "glupload",
    };
    format!(
        "videotestsrc num-buffers=5 ! video/x-raw,width=320,height=240,framerate=30/1 ! {upload} ! {} ! {} ! {} ! fakesink",
        convert_launch(Some(convert), encoder_format(enc)),
        outcaps_string(enc, Some(convert), 640, 360, (30, 1)),
        enc.element_name()
    )
}

/// Picks the first GPU conversion path (preferred, Va, Gl) whose elements exist and whose probe
/// reaches EOS with `enc`.
pub fn select_convert(
    preferred: Option<GpuConvert>,
    enc: Option<H264Encoder>,
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
        for e in H264Encoder::FALLBACK_ORDER {
            assert_eq!(H264Encoder::from_element_name(e.element_name()), Some(e));
        }
        assert_eq!(H264Encoder::from_element_name("nvh264enc"), None);
    }

    #[test]
    fn candidate_order() {
        assert_eq!(candidates(None), H264Encoder::FALLBACK_ORDER.to_vec());
        assert_eq!(
            candidates(Some(H264Encoder::OpenH264)),
            vec![H264Encoder::OpenH264, H264Encoder::Va, H264Encoder::X264]
        );
        assert_eq!(
            convert_candidates(Some(GpuConvert::Gl)),
            vec![GpuConvert::Gl, GpuConvert::Va]
        );
    }

    #[test]
    fn outcaps_table() {
        assert!(
            outcaps_string(H264Encoder::Va, Some(GpuConvert::Va), 1280, 720, (30, 1))
                .starts_with("video/x-raw(memory:VAMemory),format=NV12,width=1280,height=720")
        );
        assert!(
            outcaps_string(H264Encoder::X264, Some(GpuConvert::Va), 2, 2, (30, 1))
                .starts_with("video/x-raw,format=I420")
        );
        assert!(
            outcaps_string(H264Encoder::Va, Some(GpuConvert::Gl), 2, 2, (30, 1))
                .starts_with("video/x-raw,format=NV12")
        );
    }

    #[test]
    fn outcaps_carry_native_rate() {
        let c = outcaps_string(H264Encoder::X264, None, 3840, 2160, (25, 1));
        assert!(c.contains("width=3840,height=2160,framerate=25/1"), "{c}");
        let c = outcaps_string(H264Encoder::X264, None, 1920, 1080, (30000, 1001));
        assert!(c.contains("framerate=30000/1001"), "{c}");
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
    }

    #[test]
    fn per_input_encoder_pick() {
        let va = EncoderCap {
            enc: H264Encoder::Va,
            max: (4096, 2160),
            fps_at_max: 70.0,
        };
        let x264 = EncoderCap {
            enc: H264Encoder::X264,
            max: (4096, 2160),
            fps_at_max: 40.0,
        };
        let slow_va = EncoderCap {
            enc: H264Encoder::Va,
            max: (4096, 2160),
            fps_at_max: 20.0,
        };
        let va_1080 = EncoderCap {
            enc: H264Encoder::Va,
            max: FHD,
            fps_at_max: 200.0,
        };
        // First encoder fits and keeps up.
        assert_eq!(
            pick_encoder(&[va.clone(), x264.clone()], 3840, 2160, 30.0),
            Some((H264Encoder::Va, (4096, 2160)))
        );
        // VA too slow for 4K30: fall back to x264 for that input only...
        assert_eq!(
            pick_encoder(&[slow_va.clone(), x264.clone()], 3840, 2160, 30.0),
            Some((H264Encoder::X264, (4096, 2160)))
        );
        // ...while 1080p30 (4× fewer pixels) stays on VA.
        assert_eq!(
            pick_encoder(&[slow_va.clone(), x264.clone()], 1920, 1080, 30.0),
            Some((H264Encoder::Va, (4096, 2160)))
        );
        // VA cannot do 4K at all: 4K inputs go to x264.
        assert_eq!(
            pick_encoder(&[va_1080.clone(), x264.clone()], 3840, 2160, 25.0),
            Some((H264Encoder::X264, (4096, 2160)))
        );
        // Nothing does 4K: the input is downscaled to 1080p.
        assert_eq!(
            pick_encoder(std::slice::from_ref(&va_1080), 3840, 2160, 30.0),
            Some((H264Encoder::Va, FHD))
        );
        // Nobody keeps up: the fastest one that fits.
        let slow_x264 = EncoderCap {
            fps_at_max: 25.0,
            ..x264
        };
        assert_eq!(
            pick_encoder(&[slow_va, slow_x264], 3840, 2160, 60.0),
            Some((H264Encoder::X264, (4096, 2160)))
        );
        assert_eq!(pick_encoder(&[], 1920, 1080, 30.0), None);
    }

    #[test]
    fn class_ok_checks_any_and_class() {
        assert!(class_ok(&[], InputClass::Mic));
        assert!(!class_ok(&["opusenc".into()], InputClass::Mic));
        assert!(class_ok(&["opusenc".into()], InputClass::Video));
        assert!(!class_ok(&["matroskamux".into()], InputClass::Video));
    }
}
