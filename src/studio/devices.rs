//! Device enumeration, hotplug, camera format classification and element creation (SPEC §5.4,
//! §5.5). Device ids are stable for the process lifetime, keyed by `object.serial` (or
//! `node.name`, else the display name).

use std::collections::HashMap;

use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

use super::types::{DeviceId, DeviceInfo, DeviceKind};

/// How a camera's frames are brought into the video chain.
#[derive(Clone, Debug, PartialEq)]
pub enum CameraClass {
    /// Plain raw video. `any_rate`: rule 3 (no 720p24 mode, use whatever it offers).
    Raw {
        any_rate: bool,
    },
    Jpeg,
    DmaBuf,
    Unsupported(String),
}

impl CameraClass {
    /// Capsfilter right after the camera source.
    pub fn source_caps(&self) -> Option<&'static str> {
        match self {
            CameraClass::Raw { any_rate: false } => {
                Some("video/x-raw,width=[1,1920],height=[1,1080],framerate=[24/1,30/1]")
            }
            CameraClass::Raw { any_rate: true } => {
                Some("video/x-raw,width=[1,1920],height=[1,1080],framerate=[1/1,30/1]")
            }
            CameraClass::Jpeg => Some("image/jpeg,width=[1,1920],height=[1,1080]"),
            CameraClass::DmaBuf => Some("video/x-raw(memory:DMABuf),format=DMA_DRM; video/x-raw"),
            CameraClass::Unsupported(_) => None,
        }
    }
}

fn int_max(v: &glib::SendValue) -> Option<i32> {
    if let Ok(i) = v.get::<i32>() {
        return Some(i);
    }
    if let Ok(r) = v.get::<gst::IntRange<i32>>() {
        return Some(r.max());
    }
    if let Ok(l) = v.get::<gst::List>() {
        return l.iter().filter_map(int_max).max();
    }
    None
}

fn frac_max(v: &glib::SendValue) -> Option<f64> {
    let f = |x: gst::Fraction| {
        if x.denom() == 0 {
            0.0
        } else {
            x.numer() as f64 / x.denom() as f64
        }
    };
    if let Ok(x) = v.get::<gst::Fraction>() {
        return Some(f(x));
    }
    if let Ok(r) = v.get::<gst::FractionRange>() {
        return Some(f(r.max()));
    }
    if let Ok(l) = v.get::<gst::List>() {
        return l.iter().filter_map(frac_max).reduce(f64::max);
    }
    None
}

fn is_system_memory(features: &gst::CapsFeaturesRef) -> bool {
    features.is_empty() || features.contains("memory:SystemMemory")
}

/// Max (width, height, fps) of one structure, where present.
fn structure_limits(s: &gst::StructureRef) -> (Option<i32>, Option<i32>, Option<f64>) {
    (
        s.value("width").ok().and_then(int_max),
        s.value("height").ok().and_then(int_max),
        s.value("framerate").ok().and_then(frac_max),
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
pub fn classify_camera(caps: &gst::CapsRef, gpu_convert: bool) -> CameraClass {
    let mut raw_good = false;
    let mut raw_any = false;
    let mut jpeg = false;
    let mut dmabuf = false;
    for (s, f) in caps.iter_with_features() {
        match s.name().as_str() {
            "video/x-raw" if f.contains("memory:DMABuf") => dmabuf = true,
            "video/x-raw" if is_system_memory(f) => {
                raw_any = true;
                let (w, h, fps) = structure_limits(s);
                if w.is_some_and(|w| w >= 1280)
                    && h.is_some_and(|h| h >= 720)
                    && fps.is_some_and(|r| r >= 24.0 - 1e-6)
                {
                    raw_good = true;
                }
            }
            "image/jpeg" => jpeg = true,
            _ => {}
        }
    }
    if raw_good {
        CameraClass::Raw { any_rate: false }
    } else if jpeg {
        CameraClass::Jpeg
    } else if raw_any {
        CameraClass::Raw { any_rate: true }
    } else if dmabuf && gpu_convert {
        CameraClass::DmaBuf
    } else {
        CameraClass::Unsupported(format!("no usable format: {}", caps_summary(caps)))
    }
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
    /// `object.serial` for pipewire devices.
    pub serial: Option<String>,
}

impl Entry {
    /// Creates the source element (`device.create_element`, else `pipewiresrc target-object`).
    pub fn create_source(&self, name: &str) -> Result<gst::Element, String> {
        match self.device.create_element(Some(name)) {
            Ok(e) => Ok(e),
            Err(err) => {
                let serial = self
                    .serial
                    .clone()
                    .ok_or_else(|| format!("cannot create source: {err}"))?;
                gst::ElementFactory::make("pipewiresrc")
                    .name(name)
                    .property_from_str("target-object", &serial)
                    .build()
                    .map_err(|e| format!("cannot create pipewiresrc: {e}"))
            }
        }
    }
}

pub struct Devices {
    monitor: gst::DeviceMonitor,
    bus: gst::Bus,
    ids: HashMap<String, DeviceId>,
    next_id: u32,
    pub cameras: Vec<Entry>,
    pub microphones: Vec<Entry>,
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
    pub fn start() -> Self {
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
    pub fn enumerate(&mut self, gpu_convert: bool) -> bool {
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
                let class = classify_camera(&caps, gpu_convert);
                let detail = match &class {
                    CameraClass::Jpeg => {
                        let (w, h) = max_size(&caps, "image/jpeg");
                        format!("{origin} · MJPEG ≤{w}×{h}")
                    }
                    CameraClass::Raw { .. } => {
                        let (w, h) = max_size(&caps, "video/x-raw");
                        format!("{origin} · raw ≤{w}×{h}")
                    }
                    CameraClass::DmaBuf => format!("{origin} · DMA-BUF"),
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
                    },
                    device,
                    class: Some(class),
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
                    },
                    device,
                    class: None,
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
    pub fn poll(&mut self, gpu_convert: bool) -> bool {
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
        dirty && self.enumerate(gpu_convert)
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

fn max_size(caps: &gst::CapsRef, name: &str) -> (i32, i32) {
    caps.iter()
        .filter(|s| s.name() == name)
        .map(|s| {
            let (w, h, _) = structure_limits(s);
            (w.unwrap_or(0).min(1920), h.unwrap_or(0).min(1080))
        })
        .max_by_key(|(w, h)| (*w as i64) * (*h as i64))
        .unwrap_or((0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn caps(s: &str) -> gst::Caps {
        gst::init().unwrap();
        gst::Caps::from_str(s).unwrap()
    }

    #[test]
    fn mjpeg_beats_slow_yuy2() {
        let c = caps(
            "video/x-raw,format=YUY2,width=1920,height=1080,framerate=5/1; \
             image/jpeg,width=1920,height=1080,framerate=30/1",
        );
        assert_eq!(classify_camera(&c, true), CameraClass::Jpeg);
    }

    #[test]
    fn raw_720p30() {
        let c = caps("video/x-raw,format=YUY2,width=1280,height=720,framerate=30/1");
        assert_eq!(
            classify_camera(&c, false),
            CameraClass::Raw { any_rate: false }
        );
        let c = caps(
            "video/x-raw,format=YUY2,width=[1,1280],height=[1,720],framerate={ (fraction)15/1, (fraction)30/1 }",
        );
        assert_eq!(
            classify_camera(&c, false),
            CameraClass::Raw { any_rate: false }
        );
    }

    #[test]
    fn slow_raw_only_is_raw_any_rate() {
        let c = caps("video/x-raw,format=YUY2,width=640,height=480,framerate=15/1");
        assert_eq!(
            classify_camera(&c, false),
            CameraClass::Raw { any_rate: true }
        );
    }

    #[test]
    fn dmabuf_only() {
        let c = caps(
            "video/x-raw(memory:DMABuf),drm-format=XR24,format=DMA_DRM,width=[2,8192],height=[1,8192]",
        );
        assert_eq!(classify_camera(&c, true), CameraClass::DmaBuf);
        match classify_camera(&c, false) {
            CameraClass::Unsupported(r) => assert!(r.starts_with("no usable format:"), "{r}"),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn nothing_usable() {
        let c = caps("video/x-bayer,format=rggb,width=640,height=480");
        assert!(matches!(
            classify_camera(&c, true),
            CameraClass::Unsupported(_)
        ));
        assert!(matches!(
            classify_camera(&gst::Caps::new_empty(), true),
            CameraClass::Unsupported(_)
        ));
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
