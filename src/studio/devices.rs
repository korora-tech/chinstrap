//! Device enumeration, hotplug, camera format classification and element creation (SPEC §5.4,
//! §5.5). Device ids are stable for the process lifetime, keyed by `object.serial` (or
//! `node.name`, else the display name).

use std::collections::HashMap;

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

use super::types::{CameraModeInfo, DeviceId, DeviceInfo, DeviceKind, GpuConvert};

/// Largest camera mode recorded without downscaling (DCI 4K width, UHD height).
pub const CAMERA_MAX_W: i32 = 4096;
pub const CAMERA_MAX_H: i32 = 2160;
/// Size picked from a width/height *range* (virtual/loopback sources that accept any size, so
/// there is no native size): a conservative 1080p rather than making the producer scale to 4K.
const RANGE_W: i32 = 1920;
const RANGE_H: i32 = 1080;
/// Rate picked from a framerate *range* (no native rate): at most 30 fps.
const RANGE_MAX_FPS: i32 = 30;
/// Highest frame rate recorded; faster modes are rated down to this.
pub const CAMERA_MAX_FPS: i32 = 60;
/// Modes at or above this rate are preferred over any slower mode, whatever its size.
const SMOOTH_FPS_MILLI: i64 = 23_900;

/// How a camera's frames reach the video chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CameraPath {
    /// DMA-BUF raw video: GPU convert block.
    DmaBuf,
    /// MJPEG: jpegparse ! jpegdec.
    Jpeg,
    /// System-memory raw video.
    Raw,
}

/// One concrete camera mode: what is recorded, at the camera's native size and rate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraMode {
    pub path: CameraPath,
    /// Raw `format` (Raw) or `drm-format` (DmaBuf), when the device fixes one.
    pub format: Option<String>,
    pub width: i32,
    pub height: i32,
    pub fps_n: i32,
    pub fps_d: i32,
}

impl CameraMode {
    fn fps_milli(&self) -> i64 {
        if self.fps_d <= 0 {
            return 0;
        }
        self.fps_n as i64 * 1000 / self.fps_d as i64
    }

    /// Capsfilter right after the camera source: exactly this mode.
    pub fn source_caps(&self) -> String {
        let mut geometry = format!("width={},height={}", self.width, self.height);
        if self.fps_n > 0 {
            geometry.push_str(&format!(",framerate={}/{}", self.fps_n, self.fps_d));
        }
        match self.path {
            CameraPath::Raw => match &self.format {
                Some(f) => format!("video/x-raw,format={f},{geometry}"),
                None => format!("video/x-raw,{geometry}"),
            },
            CameraPath::Jpeg => format!("image/jpeg,{geometry}"),
            CameraPath::DmaBuf => match &self.format {
                Some(f) => {
                    format!("video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={f},{geometry}")
                }
                None => format!("video/x-raw(memory:DMABuf),format=DMA_DRM,{geometry}"),
            },
        }
    }

    /// Recorded frame rate: native, capped at 60 fps (30 fps when the device gives no rate).
    pub fn output_fps(&self) -> (i32, i32) {
        if self.fps_n <= 0 {
            (30, 1)
        } else if self.fps_milli() > CAMERA_MAX_FPS as i64 * 1000 {
            (CAMERA_MAX_FPS, 1)
        } else {
            (self.fps_n, self.fps_d.max(1))
        }
    }

    /// Recorded size: native, fitted within 4096×2160 only when larger.
    pub fn output_size(&self) -> (i32, i32) {
        super::stamp::lock_size_within(self.width, self.height, CAMERA_MAX_W, CAMERA_MAX_H)
    }

    /// Contract view of this mode (`auto`: the automatic choice).
    pub fn info(&self, auto: bool) -> CameraModeInfo {
        let (w, h) = self.output_size();
        let (n, d) = self.output_fps();
        CameraModeInfo {
            label: self.label(),
            width: w,
            height: h,
            fps_n: n,
            fps_d: d,
            format: self.format_label(),
            auto,
        }
    }

    fn format_label(&self) -> String {
        match (self.path, &self.format) {
            (CameraPath::Raw, Some(f)) => f.clone(),
            (CameraPath::Raw, None) => "raw".into(),
            (CameraPath::Jpeg, _) => "MJPEG".into(),
            (CameraPath::DmaBuf, Some(f)) => format!("DMA-BUF {f}"),
            (CameraPath::DmaBuf, None) => "DMA-BUF".into(),
        }
    }

    /// UI label, e.g. "3840×2160 @25 · NV12" or "1920×1080 @30 · MJPEG".
    pub fn label(&self) -> String {
        let (w, h) = self.output_size();
        let (n, d) = self.output_fps();
        let fps = if d == 1 {
            format!("{n}")
        } else {
            format!("{:.2}", n as f64 / d as f64)
        };
        format!("{w}×{h} @{fps} · {}", self.format_label())
    }
}

/// How a camera is recorded: the chosen mode, or why it cannot be.
#[derive(Clone, Debug, PartialEq)]
pub enum CameraClass {
    Usable(CameraMode),
    Unsupported(String),
}

impl CameraClass {
    pub fn mode(&self) -> Option<&CameraMode> {
        match self {
            CameraClass::Usable(m) => Some(m),
            CameraClass::Unsupported(_) => None,
        }
    }
}

/// Whether JPEG parsing and decoding are available for MJPEG camera modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JpegDecodeSupport {
    Available,
    #[default]
    Unavailable,
}

/// Capabilities that determine which advertised camera modes can be recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraSupport {
    pub gpu_convert: Option<GpuConvert>,
    pub jpeg_decode: JpegDecodeSupport,
}

impl CameraSupport {
    fn supports(self, path: CameraPath) -> bool {
        match path {
            CameraPath::Raw => true,
            CameraPath::Jpeg => self.jpeg_decode == JpegDecodeSupport::Available,
            CameraPath::DmaBuf => self.gpu_convert.is_some(),
        }
    }
}

/// Candidate values of an int field: fixed values and list entries as-is; a range contributes
/// its largest value not above `target` (or its minimum when the whole range is above it).
fn int_candidates(v: &glib::SendValue, target: i32) -> Vec<i32> {
    if let Ok(i) = v.get::<i32>() {
        return vec![i];
    }
    if let Ok(r) = v.get::<gst::IntRange<i32>>() {
        let step = r.step().max(1);
        let pick = if r.max() <= target {
            r.max()
        } else if r.min() <= target {
            r.min() + (target - r.min()) / step * step
        } else {
            r.min()
        };
        return vec![pick];
    }
    if let Ok(l) = v.get::<gst::List>() {
        return l.iter().flat_map(|x| int_candidates(x, target)).collect();
    }
    Vec::new()
}

/// Candidate frame rates: fixed values and list entries as-is; a range (no native rate)
/// contributes its largest rate not above 30 fps (or its minimum when the whole range is faster).
fn fps_candidates(v: &glib::SendValue) -> Vec<(i32, i32)> {
    let as_pair = |f: gst::Fraction| (f.numer(), f.denom());
    if let Ok(f) = v.get::<gst::Fraction>() {
        return vec![as_pair(f)];
    }
    if let Ok(r) = v.get::<gst::FractionRange>() {
        let cap = gst::Fraction::new(RANGE_MAX_FPS, 1);
        let pick = if r.max() <= cap {
            r.max()
        } else if r.min() <= cap {
            cap
        } else {
            r.min()
        };
        return vec![as_pair(pick)];
    }
    if let Ok(l) = v.get::<gst::List>() {
        return l.iter().flat_map(fps_candidates).collect();
    }
    Vec::new()
}

fn is_system_memory(features: &gst::CapsFeaturesRef) -> bool {
    features.is_empty() || features.contains("memory:SystemMemory")
}

/// Raw formats that cannot be converted (placeholders some providers advertise).
fn bad_format(f: &str) -> bool {
    matches!(f, "UNKNOWN" | "ENCODED")
}

/// Every concrete mode the device caps offer (unusable structures skipped).
pub fn camera_modes(caps: &gst::CapsRef) -> Vec<CameraMode> {
    let mut out = Vec::new();
    for (s, f) in caps.iter_with_features() {
        let fixed_format = s.get::<&str>("format").ok();
        let (path, format) = match s.name().as_str() {
            "video/x-raw" if f.contains("memory:DMABuf") => {
                if fixed_format != Some("DMA_DRM") {
                    continue;
                }
                (
                    CameraPath::DmaBuf,
                    s.get::<&str>("drm-format").ok().map(String::from),
                )
            }
            "video/x-raw" if is_system_memory(f) => {
                if fixed_format.is_some_and(bad_format) {
                    continue;
                }
                (CameraPath::Raw, fixed_format.map(String::from))
            }
            "image/jpeg" if is_system_memory(f) => (CameraPath::Jpeg, None),
            _ => continue,
        };
        let widths = s
            .value("width")
            .map(|v| int_candidates(v, RANGE_W))
            .unwrap_or_default();
        let heights = s
            .value("height")
            .map(|v| int_candidates(v, RANGE_H))
            .unwrap_or_default();
        // No framerate field (some virtual sources): an unknown rate, recorded at 30 fps.
        let rates = match s.value("framerate") {
            Ok(v) => fps_candidates(v),
            Err(_) => vec![(0, 1)],
        };
        for &w in &widths {
            for &h in &heights {
                for &(n, d) in &rates {
                    if w <= 0 || h <= 0 || n < 0 || d <= 0 {
                        continue;
                    }
                    let m = CameraMode {
                        path,
                        format: format.clone(),
                        width: w,
                        height: h,
                        fps_n: n,
                        fps_d: d,
                    };
                    if !out.contains(&m) {
                        out.push(m);
                    }
                }
            }
        }
    }
    out
}

/// Preference key (larger is better), SPEC §5.4 as revised for 4K:
/// 1. modes within 4096×2160 over larger ones (larger ones are downscaled);
/// 2. modes of at least ~24 fps over slower ones;
/// 3. the highest resolution (within 4K), or the smallest one above 4K;
/// 4. the highest frame rate up to 60 fps, then a mode that needs no rate reduction;
/// 5. raw over MJPEG over DMA-BUF at an otherwise equal mode.
fn mode_rank(m: &CameraMode) -> (bool, bool, i64, i64, bool, CameraPath) {
    let within = m.width <= CAMERA_MAX_W && m.height <= CAMERA_MAX_H;
    let area = m.width as i64 * m.height as i64;
    let fps = m.fps_milli();
    let cap = CAMERA_MAX_FPS as i64 * 1000;
    (
        within,
        fps >= SMOOTH_FPS_MILLI,
        if within { area } else { -area },
        fps.min(cap),
        fps <= cap,
        m.path,
    )
}

/// Short human summary of caps: distinct `name(features)` entries.
pub fn caps_summary(caps: &gst::CapsRef) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (s, f) in caps.iter_with_features() {
        let mut p = s.name().to_string();
        if !is_system_memory(f) && !f.is_any() {
            p.push_str(&format!("({f})"));
        }
        if let Ok(fmt) = s.get::<&str>("format") {
            p.push_str(&format!(" {fmt}"));
        }
        if !parts.contains(&p) {
            parts.push(p);
        }
    }
    if parts.is_empty() {
        return "no caps".into();
    }
    let mut out = parts.into_iter().take(4).collect::<Vec<_>>().join(", ");
    if out.len() > 120 {
        out.truncate(120);
        out.push('…');
    }
    out
}

/// Camera classification (§5.4), a pure function over the device caps.
#[cfg(test)]
pub fn classify_camera(caps: &gst::CapsRef, support: CameraSupport) -> CameraClass {
    classify_camera_with(caps, support)
}

/// As [`classify_camera`], with the capabilities that determine usable camera modes.
pub fn classify_camera_with(caps: &gst::CapsRef, support: CameraSupport) -> CameraClass {
    let modes = camera_modes(caps);
    let best = usable_modes(caps, support).into_iter().next();
    let best = best.as_ref();
    match best {
        Some(m) => CameraClass::Usable(m.clone()),
        None if modes.iter().any(|m| m.path == CameraPath::Jpeg)
            && support.jpeg_decode == JpegDecodeSupport::Unavailable =>
        {
            CameraClass::Unsupported("MJPEG needs jpegparse and jpegdec, which are missing".into())
        }
        None => CameraClass::Unsupported(format!("no usable format: {}", caps_summary(caps))),
    }
}

/// Every mode that can be recorded, best (the automatic choice) first, one per label.
pub fn usable_modes(caps: &gst::CapsRef, support: CameraSupport) -> Vec<CameraMode> {
    let mut modes: Vec<CameraMode> = camera_modes(caps)
        .into_iter()
        .filter(|m| support.supports(m.path))
        .collect();
    modes.sort_by_key(|m| std::cmp::Reverse(mode_rank(m)));
    let mut seen = std::collections::HashSet::new();
    modes.retain(|m| seen.insert(m.label()));
    modes
}

/// Reads a device property as a string, whatever its GType.
pub fn prop_string(props: &gst::StructureRef, key: &str) -> Option<String> {
    let v = props.value(key).ok()?;
    if let Ok(s) = v.get::<String>() {
        return Some(s);
    }
    v.transform::<String>().ok()?.get::<String>().ok()
}

/// Sink monitors are not microphones (§5.5 filtering).
pub fn is_monitor(props: Option<&gst::StructureRef>, name: &str) -> bool {
    name.starts_with("Monitor of")
        || props
            .and_then(|p| prop_string(p, "device.class"))
            .is_some_and(|c| c == "monitor")
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub info: DeviceInfo,
    pub device: gst::Device,
    pub class: Option<CameraClass>,
    /// Cameras: every usable mode, best first (same order as `info.modes`).
    pub modes: Vec<CameraMode>,
    /// `object.serial` for pipewire devices.
    pub serial: Option<String>,
}

impl Entry {
    /// The mode labelled `label`, or the automatic one.
    pub fn mode(&self, label: Option<&str>) -> Option<&CameraMode> {
        label
            .and_then(|l| self.modes.iter().find(|m| m.label() == l))
            .or_else(|| self.class.as_ref().and_then(CameraClass::mode))
    }

    /// Creates the source element (`device.create_element`, else `pipewiresrc target-object`).
    /// The provider targets the node by `object.serial`, which is correct; the stream is also
    /// marked `node.dont-fallback` so a vanished device fails loudly instead of WirePlumber
    /// linking the default Video/Source in its place.
    pub fn create_source(&self, name: &str) -> Result<gst::Element, String> {
        let el = match self.device.create_element(Some(name)) {
            Ok(e) => e,
            Err(err) => {
                let serial = self
                    .serial
                    .clone()
                    .ok_or_else(|| format!("cannot create source: {err}"))?;
                gst::ElementFactory::make("pipewiresrc")
                    .name(name)
                    .property_from_str("target-object", &serial)
                    .build()
                    .map_err(|e| format!("cannot create pipewiresrc: {e}"))?
            }
        };
        if el.factory().is_some_and(|f| f.name() == "pipewiresrc") {
            super::recorder::set_no_fallback(&el);
        }
        Ok(el)
    }
}

pub struct Devices {
    monitor: gst::DeviceMonitor,
    bus: gst::Bus,
    ids: HashMap<String, DeviceId>,
    next_id: u32,
    pub cameras: Vec<Entry>,
    pub microphones: Vec<Entry>,
    camera_support: CameraSupport,
}

fn rate_label(rate: i32) -> String {
    if rate % 1000 == 0 {
        format!("{} kHz", rate / 1000)
    } else {
        format!("{:.1} kHz", rate as f64 / 1000.0)
    }
}

impl Devices {
    /// Creates and starts the monitor (Video/Source + Audio/Source). Blocks briefly while the
    /// providers probe; called on the engine thread before the first Snapshot.
    pub fn start(camera_support: CameraSupport) -> Self {
        let monitor = gst::DeviceMonitor::new();
        monitor.add_filter(Some("Video/Source"), None);
        monitor.add_filter(Some("Audio/Source"), None);
        if let Err(e) = monitor.start() {
            eprintln!("chinstrap: device monitor failed to start: {e}");
        }
        let bus = monitor.bus();
        Self {
            monitor,
            bus,
            ids: HashMap::new(),
            next_id: 1,
            cameras: Vec::new(),
            microphones: Vec::new(),
            camera_support,
        }
    }

    pub fn stop(&self) {
        self.monitor.stop();
    }

    fn id_for(&mut self, key: String) -> DeviceId {
        if let Some(id) = self.ids.get(&key) {
            return *id;
        }
        let id = DeviceId(self.next_id);
        self.next_id += 1;
        self.ids.insert(key, id);
        id
    }

    /// Re-reads the device list. Returns true if anything visible changed.
    pub fn enumerate(&mut self) -> bool {
        let mut cameras = Vec::new();
        let mut microphones = Vec::new();
        for device in self.monitor.devices() {
            let name = device.display_name().to_string();
            let props = device.properties();
            let serial = props.as_ref().and_then(|p| prop_string(p, "object.serial"));
            let key = serial
                .clone()
                .or_else(|| props.as_ref().and_then(|p| prop_string(p, "node.name")))
                .unwrap_or_else(|| format!("{}:{name}", device.device_class()));
            // object.serial is PipeWire's; other providers fall back to their device.api.
            let origin = match &serial {
                Some(s) => format!("pipewire #{s}"),
                None => props
                    .as_ref()
                    .and_then(|p| prop_string(p, "device.api"))
                    .unwrap_or_else(|| "device".into()),
            };
            let caps = device.caps().unwrap_or_else(gst::Caps::new_empty);
            if device.has_classes("Video/Source") {
                let class = classify_camera_with(&caps, self.camera_support);
                let modes = usable_modes(&caps, self.camera_support);
                let detail = match &class {
                    CameraClass::Usable(m) => format!("{origin} · {}", m.label()),
                    CameraClass::Unsupported(_) => origin.clone(),
                };
                let unsupported = match &class {
                    CameraClass::Unsupported(r) => Some(r.clone()),
                    _ => None,
                };
                let id = self.id_for(key);
                cameras.push(Entry {
                    info: DeviceInfo {
                        id,
                        kind: DeviceKind::Camera,
                        name,
                        detail,
                        unsupported,
                        modes: modes
                            .iter()
                            .enumerate()
                            .map(|(i, m)| m.info(i == 0))
                            .collect(),
                    },
                    device,
                    class: Some(class),
                    modes,
                    serial,
                });
            } else if device.has_classes("Audio/Source") {
                if is_monitor(props.as_deref(), &name) {
                    continue;
                }
                let mut detail = origin.clone();
                if let Some(s) = caps.structure(0) {
                    if let Ok(rate) = s.get::<i32>("rate") {
                        detail.push_str(&format!(" · {}", rate_label(rate)));
                    }
                    if let Ok(ch) = s.get::<i32>("channels") {
                        detail.push_str(&format!(" · {ch} ch"));
                    }
                }
                let id = self.id_for(key);
                microphones.push(Entry {
                    info: DeviceInfo {
                        id,
                        kind: DeviceKind::Microphone,
                        name,
                        detail,
                        unsupported: None,
                        modes: Vec::new(),
                    },
                    device,
                    class: None,
                    modes: Vec::new(),
                    serial,
                });
            }
        }
        cameras.sort_by_key(|e| e.info.id);
        microphones.sort_by_key(|e| e.info.id);
        let changed = !same(&self.cameras, &cameras) || !same(&self.microphones, &microphones);
        self.cameras = cameras;
        self.microphones = microphones;
        changed
    }

    /// Drains hotplug messages without blocking. Returns true if the device list changed.
    pub fn poll(&mut self) -> bool {
        let mut dirty = false;
        while let Some(msg) = self.bus.pop() {
            if matches!(
                msg.view(),
                gst::MessageView::DeviceAdded(_)
                    | gst::MessageView::DeviceRemoved(_)
                    | gst::MessageView::DeviceChanged(_)
            ) {
                dirty = true;
            }
        }
        dirty && self.enumerate()
    }

    pub fn camera(&self, id: DeviceId) -> Option<&Entry> {
        self.cameras.iter().find(|e| e.info.id == id)
    }

    pub fn microphone(&self, id: DeviceId) -> Option<&Entry> {
        self.microphones.iter().find(|e| e.info.id == id)
    }
}

fn same(a: &[Entry], b: &[Entry]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.info == y.info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn caps(s: &str) -> gst::Caps {
        gst::init().unwrap();
        gst::Caps::from_str(s).unwrap()
    }

    fn usable(c: CameraClass) -> CameraMode {
        match c {
            CameraClass::Usable(m) => m,
            other => panic!("expected a usable mode, got {other:?}"),
        }
    }

    const GPU_JPEG_SUPPORT: CameraSupport = CameraSupport {
        gpu_convert: Some(GpuConvert::Va),
        jpeg_decode: JpegDecodeSupport::Available,
    };
    const CPU_JPEG_SUPPORT: CameraSupport = CameraSupport {
        gpu_convert: None,
        jpeg_decode: JpegDecodeSupport::Available,
    };
    const GPU_NO_JPEG_SUPPORT: CameraSupport = CameraSupport {
        gpu_convert: Some(GpuConvert::Va),
        jpeg_decode: JpegDecodeSupport::Unavailable,
    };
    const CPU_NO_JPEG_SUPPORT: CameraSupport = CameraSupport {
        gpu_convert: None,
        jpeg_decode: JpegDecodeSupport::Unavailable,
    };

    /// The exact caps of the Sony ILCE-7CM2 (uvcvideo) as PipeWire reports them.
    const ILCE_7CM2: &str = "video/x-raw(memory:DMABuf), drm-format=NV12, format=DMA_DRM, \
        interlace-mode=progressive, width=3840, height=2160, framerate=25/1, colorimetry=bt709; \
        video/x-raw, format=NV12, interlace-mode=progressive, width=3840, height=2160, \
        framerate=25/1, colorimetry=bt709; \
        image/jpeg, width=3840, height=2160, framerate=25/1, colorimetry=bt709";

    #[test]
    fn ilce_7cm2_records_native_4k25_raw() {
        let c = caps(ILCE_7CM2);
        let m = usable(classify_camera(&c, GPU_JPEG_SUPPORT));
        assert_eq!(m.path, CameraPath::Raw);
        assert_eq!(m.format.as_deref(), Some("NV12"));
        assert_eq!((m.width, m.height), (3840, 2160));
        assert_eq!((m.fps_n, m.fps_d), (25, 1));
        assert_eq!(m.output_fps(), (25, 1), "25 fps must not be rated to 30");
        assert_eq!(m.output_size(), (3840, 2160));
        assert_eq!(
            m.source_caps(),
            "video/x-raw,format=NV12,width=3840,height=2160,framerate=25/1"
        );
        assert_eq!(m.label(), "3840×2160 @25 · NV12");
        // The source capsfilter admits exactly the advertised raw structure.
        let filter = gst::Caps::from_str(&m.source_caps()).unwrap();
        assert!(filter.can_intersect(&c));
        // Same result without a GPU path; without MJPEG support too.
        assert_eq!(usable(classify_camera_with(&c, CPU_NO_JPEG_SUPPORT)), m);
        // The mode list: the automatic choice first, then the other advertised paths.
        let modes = usable_modes(&c, GPU_JPEG_SUPPORT);
        assert_eq!(modes[0], m);
        let labels: Vec<String> = modes.iter().map(|m| m.label()).collect();
        assert!(labels.iter().any(|l| l.contains("MJPEG")), "{labels:?}");
        assert!(modes[0].info(true).auto && !modes[1].info(false).auto);
    }

    #[test]
    fn ilce_7cm2_fallbacks() {
        // Raw dropped: MJPEG at the same mode beats the DMA-BUF path.
        let c = caps(
            "video/x-raw(memory:DMABuf), drm-format=NV12, format=DMA_DRM, width=3840, height=2160, framerate=25/1; \
             image/jpeg, width=3840, height=2160, framerate=25/1",
        );
        let m = usable(classify_camera(&c, GPU_JPEG_SUPPORT));
        assert_eq!(m.path, CameraPath::Jpeg);
        assert_eq!(
            m.source_caps(),
            "image/jpeg,width=3840,height=2160,framerate=25/1"
        );
        // Only DMA-BUF works: use it when a GPU path exists.
        let c = caps(
            "video/x-raw(memory:DMABuf), drm-format=NV12, format=DMA_DRM, width=3840, height=2160, framerate=25/1",
        );
        let m = usable(classify_camera(&c, GPU_JPEG_SUPPORT));
        assert_eq!(m.path, CameraPath::DmaBuf);
        assert_eq!(
            m.source_caps(),
            "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=NV12,width=3840,height=2160,framerate=25/1"
        );
        assert!(matches!(
            classify_camera(&c, CPU_JPEG_SUPPORT),
            CameraClass::Unsupported(_)
        ));
    }

    #[test]
    fn typical_webcam_mjpeg_1080p30_beats_slow_yuy2() {
        // C920-style: YUY2 only at low rates for large sizes, MJPEG 1080p30.
        let c = caps(
            "video/x-raw,format=YUY2,width=1920,height=1080,framerate={ (fraction)5/1 }; \
             video/x-raw,format=YUY2,width=1280,height=720,framerate={ (fraction)10/1, (fraction)5/1 }; \
             video/x-raw,format=YUY2,width=640,height=480,framerate={ (fraction)30/1, (fraction)15/1 }; \
             image/jpeg,width=1920,height=1080,framerate={ (fraction)30/1, (fraction)24/1, (fraction)15/1 }; \
             image/jpeg,width=1280,height=720,framerate={ (fraction)30/1, (fraction)15/1 }",
        );
        let m = usable(classify_camera(&c, GPU_JPEG_SUPPORT));
        assert_eq!(m.path, CameraPath::Jpeg);
        assert_eq!((m.width, m.height, m.fps_n, m.fps_d), (1920, 1080, 30, 1));
        assert_eq!(m.label(), "1920×1080 @30 · MJPEG");
        // Without MJPEG decoders: the best smooth raw mode (640×480@30) wins over 1080p5.
        let m = usable(classify_camera_with(&c, GPU_NO_JPEG_SUPPORT));
        assert_eq!(m.path, CameraPath::Raw);
        assert_eq!((m.width, m.height, m.fps_n), (640, 480, 30));
    }

    #[test]
    fn raw_preferred_when_it_matches_mjpeg() {
        let c = caps(
            "image/jpeg,width=1920,height=1080,framerate=30/1; \
             video/x-raw,format=YUY2,width=1920,height=1080,framerate=30/1",
        );
        let m = usable(classify_camera(&c, GPU_JPEG_SUPPORT));
        assert_eq!(m.path, CameraPath::Raw);
        assert_eq!(m.format.as_deref(), Some("YUY2"));
    }

    #[test]
    fn highest_resolution_then_fps_up_to_4k() {
        let c = caps(
            "video/x-raw,format=YUY2,width=1280,height=720,framerate=60/1; \
             image/jpeg,width=3840,height=2160,framerate=30/1; \
             image/jpeg,width=1920,height=1080,framerate=60/1; \
             image/jpeg,width=4096,height=2160,framerate=30/1",
        );
        let m = usable(classify_camera(&c, GPU_JPEG_SUPPORT));
        assert_eq!((m.width, m.height, m.fps_n), (4096, 2160, 30));
        assert_eq!(m.output_size(), (4096, 2160), "DCI 4K is recorded unscaled");
        // A faster mode at the same size wins.
        let c =
            caps("image/jpeg,width=3840,height=2160,framerate={ (fraction)30/1, (fraction)60/1 }");
        assert_eq!(usable(classify_camera(&c, GPU_JPEG_SUPPORT)).fps_n, 60);
        // 120 fps is capped to 60, and a native 60 fps mode is preferred over it.
        let c = caps("image/jpeg,width=1920,height=1080,framerate={ (fraction)120/1 }");
        assert_eq!(
            usable(classify_camera(&c, GPU_JPEG_SUPPORT)).output_fps(),
            (60, 1)
        );
        let c =
            caps("image/jpeg,width=1920,height=1080,framerate={ (fraction)120/1, (fraction)60/1 }");
        assert_eq!(usable(classify_camera(&c, GPU_JPEG_SUPPORT)).fps_n, 60);
        // NTSC rates are kept exactly.
        let c = caps("video/x-raw,format=NV12,width=1920,height=1080,framerate=30000/1001");
        let m = usable(classify_camera(&c, CPU_JPEG_SUPPORT));
        assert_eq!(m.output_fps(), (30000, 1001));
        assert_eq!(m.label(), "1920×1080 @29.97 · NV12");
    }

    #[test]
    fn above_4k_is_downscaled_and_smaller_modes_win() {
        let c = caps("video/x-raw,format=NV12,width=6144,height=3456,framerate=30/1");
        let m = usable(classify_camera(&c, CPU_JPEG_SUPPORT));
        assert_eq!((m.width, m.height), (6144, 3456));
        let (w, h) = m.output_size();
        assert!(w <= CAMERA_MAX_W && h <= CAMERA_MAX_H, "{w}×{h}");
        assert_eq!((w, h), (3840, 2160));
        let c = caps(
            "video/x-raw,format=NV12,width=6144,height=3456,framerate=30/1; \
             video/x-raw,format=NV12,width=3840,height=2160,framerate=30/1",
        );
        assert_eq!(usable(classify_camera(&c, CPU_JPEG_SUPPORT)).width, 3840);
    }

    #[test]
    fn ranges_pick_conservative_1080p30() {
        // Any-size sources (v4l2loopback, virtual cameras) are not pushed to 4K60.
        let c =
            caps("video/x-raw,format=YUY2,width=[2,8192],height=[1,8192],framerate=[1/1,120/1]");
        let m = usable(classify_camera(&c, CPU_JPEG_SUPPORT));
        assert_eq!((m.width, m.height, m.fps_n, m.fps_d), (1920, 1080, 30, 1));
        assert_eq!(m.label(), "1920×1080 @30 · YUY2");
        // A range entirely above the defaults uses its minimum.
        let c = caps(
            "video/x-raw,format=YUY2,width=[2560,8192],height=[1440,8192],framerate=[50/1,120/1]",
        );
        let m = usable(classify_camera(&c, CPU_JPEG_SUPPORT));
        assert_eq!((m.width, m.height, m.fps_n), (2560, 1440, 50));
        let c = caps("video/x-raw,format=YUY2,width=[2,1280],height=[1,720],framerate=[1/1,30/1]");
        let m = usable(classify_camera(&c, CPU_JPEG_SUPPORT));
        assert_eq!((m.width, m.height, m.fps_n), (1280, 720, 30));
    }

    #[test]
    fn slow_raw_only_is_still_usable() {
        let c = caps("video/x-raw,format=YUY2,width=640,height=480,framerate=15/1");
        let m = usable(classify_camera(&c, CPU_JPEG_SUPPORT));
        assert_eq!((m.width, m.height, m.fps_n), (640, 480, 15));
    }

    #[test]
    fn mjpeg_without_decoder_is_unsupported() {
        let c = caps("image/jpeg,width=1920,height=1080,framerate=30/1");
        match classify_camera_with(&c, GPU_NO_JPEG_SUPPORT) {
            CameraClass::Unsupported(r) => assert!(r.contains("jpegparse"), "{r}"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn dmabuf_range_without_rate() {
        // The v4l2loopback dummy shape: ranges, no framerate field, placeholder formats.
        let c = caps(
            "video/x-raw(memory:DMABuf),drm-format=XR24,format=DMA_DRM,width=[2,8192],height=[1,8192]; \
             video/x-raw(memory:DMABuf),format=UNKNOWN,width=[2,8192],height=[1,8192]; \
             video/x-raw,format=UNKNOWN,width=[2,8192],height=[1,8192]",
        );
        let m = usable(classify_camera(&c, GPU_JPEG_SUPPORT));
        assert_eq!(m.path, CameraPath::DmaBuf);
        assert_eq!((m.width, m.height), (1920, 1080));
        assert_eq!(m.output_fps(), (30, 1));
        assert!(
            !m.source_caps().contains("framerate"),
            "{}",
            m.source_caps()
        );
        match classify_camera(&c, CPU_JPEG_SUPPORT) {
            CameraClass::Unsupported(r) => assert!(r.starts_with("no usable format:"), "{r}"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn nothing_usable() {
        let c = caps("video/x-bayer,format=rggb,width=640,height=480,framerate=30/1");
        assert!(matches!(
            classify_camera(&c, GPU_JPEG_SUPPORT),
            CameraClass::Unsupported(_)
        ));
        assert!(matches!(
            classify_camera(&gst::Caps::new_empty(), GPU_JPEG_SUPPORT),
            CameraClass::Unsupported(_)
        ));
    }

    #[test]
    fn camera_support_distinguishes_gpu_and_jpeg_paths() {
        let c = caps(
            "video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format=NV12,width=1920,height=1080,framerate=30/1; \
             image/jpeg,width=1920,height=1080,framerate=30/1",
        );
        let cpu_with_jpeg = CameraSupport {
            gpu_convert: None,
            jpeg_decode: JpegDecodeSupport::Available,
        };
        let gpu_without_jpeg = CameraSupport {
            gpu_convert: Some(super::super::types::GpuConvert::Va),
            jpeg_decode: JpegDecodeSupport::Unavailable,
        };

        assert_eq!(
            usable(classify_camera_with(&c, cpu_with_jpeg)).path,
            CameraPath::Jpeg
        );
        assert_eq!(
            usable(classify_camera_with(&c, gpu_without_jpeg)).path,
            CameraPath::DmaBuf
        );
        assert!(
            usable_modes(&c, cpu_with_jpeg)
                .iter()
                .all(|m| m.path == CameraPath::Jpeg)
        );
        assert!(
            usable_modes(&c, gpu_without_jpeg)
                .iter()
                .all(|m| m.path == CameraPath::DmaBuf)
        );
    }

    #[test]
    fn monitor_filter() {
        gst::init().unwrap();
        assert!(is_monitor(None, "Monitor of Built-in Audio"));
        assert!(!is_monitor(None, "RODECaster Duo Pro"));
        let p = gst::Structure::builder("props")
            .field("device.class", "monitor")
            .build();
        assert!(is_monitor(Some(&p), "Something"));
        let p = gst::Structure::builder("props")
            .field("device.class", "sound")
            .field("object.serial", 61i32)
            .build();
        assert!(!is_monitor(Some(&p), "Something"));
        assert_eq!(prop_string(&p, "object.serial").as_deref(), Some("61"));
    }
}
