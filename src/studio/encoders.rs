//! Element checks (SPEC §5.7), the H.264 encoder probe (§5.6) and the GPU path probe (§5.2).
//!
//! The probes run on the engine thread before the first Snapshot; that is the only accepted
//! blocking period, bounded at 2 s per candidate.

use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;

use super::types::{GpuConvert, H264Encoder, InputClass, REQUIRED_ELEMENTS};

pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

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
) -> String {
    let feature = match (convert, enc) {
        (Some(GpuConvert::Va), H264Encoder::Va) => "(memory:VAMemory)",
        _ => "",
    };
    format!(
        "video/x-raw{feature},format={},width={width},height={height},framerate=30/1,pixel-aspect-ratio=1/1",
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

/// Creates and configures an encoder (§5.6 table).
pub fn make_encoder(enc: H264Encoder, role: Role) -> Result<gst::Element, String> {
    let el = gst::ElementFactory::make(enc.element_name())
        .build()
        .map_err(|e| format!("{}: {e}", enc.element_name()))?;
    let screen = role == Role::Screen;
    match enc {
        H264Encoder::Va => {
            set_prop(&el, "bitrate", if screen { "12000" } else { "8000" });
            set_prop(&el, "key-int-max", "60");
            set_prop(&el, "rate-control", "cbr");
        }
        H264Encoder::X264 => {
            set_prop(&el, "tune", "zerolatency");
            set_prop(&el, "speed-preset", "veryfast");
            set_prop(&el, "key-int-max", "60");
            set_prop(&el, "bitrate", if screen { "12000" } else { "8000" });
        }
        H264Encoder::OpenH264 => {
            set_prop(&el, "bitrate", if screen { "12000000" } else { "8000000" });
            set_prop(&el, "gop-size", "60");
            set_prop(&el, "complexity", "low");
        }
    }
    Ok(el)
}

/// Runs a launch-line pipeline to EOS, bounded by `timeout`. Blocking.
pub fn run_probe(desc: &str, timeout: Duration) -> Result<(), String> {
    let element = gst::parse::launch(desc).map_err(|e| e.to_string())?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_string())?;
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

/// Picks the first candidate whose factory exists and that reaches EOS. Returns the choice and
/// one Info line per failed candidate.
pub fn select_encoder(preferred: Option<H264Encoder>) -> (Option<H264Encoder>, Vec<String>) {
    let mut notes = Vec::new();
    for enc in candidates(preferred) {
        if !has_element(enc.element_name()) {
            notes.push(format!(
                "H.264 encoder {} is not installed",
                enc.element_name()
            ));
            continue;
        }
        match run_probe(&encoder_probe_launch(enc), PROBE_TIMEOUT) {
            Ok(()) => return (Some(enc), notes),
            Err(e) => notes.push(format!(
                "H.264 encoder {} failed its probe: {e}",
                enc.element_name()
            )),
        }
    }
    (None, notes)
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
        outcaps_string(enc, Some(convert), 640, 360),
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
            outcaps_string(H264Encoder::Va, Some(GpuConvert::Va), 1280, 720)
                .starts_with("video/x-raw(memory:VAMemory),format=NV12,width=1280,height=720")
        );
        assert!(
            outcaps_string(H264Encoder::X264, Some(GpuConvert::Va), 2, 2)
                .starts_with("video/x-raw,format=I420")
        );
        assert!(
            outcaps_string(H264Encoder::Va, Some(GpuConvert::Gl), 2, 2)
                .starts_with("video/x-raw,format=NV12")
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
