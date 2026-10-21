//! Timestamp probes (SPEC §5.0) and size-lock math (§5.2).
//!
//! pipewiresrc's own PTS cannot be trusted (niri never writes header pts; camera/mic headers
//! carry the producer's clock domain), so every pipewiresrc src pad gets a buffer probe that
//! stamps buffers in pipeline running time on the engine's shared clock and base time.
//! Probes only touch atomics and their own small state.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gstreamer as gst;
use gstreamer::prelude::*;

/// The size locked from the first CAPS event, shared with the engine.
pub type SizeSlot = Arc<Mutex<Option<(i32, i32)>>>;

/// Sentinel for "not set yet" in the shared `AtomicU64`s.
pub const UNSET: u64 = u64::MAX;

/// Maximum size the encoders are fed (fit within, keeping aspect ratio).
pub const MAX_W: i32 = 3840;
pub const MAX_H: i32 = 2160;
/// vah264enc's sink range starts at 128.
pub const MIN_SIDE: i32 = 128;

/// Audio re-anchor threshold.
pub const REANCHOR_NS: u64 = 40_000_000;

fn even_down(v: i32) -> i32 {
    (v.max(2)) & !1
}

fn even_up(v: f64) -> i32 {
    let v = v.ceil() as i32;
    (v + 1) & !1
}

/// Size lock (§5.2): round down to even, fit within 3840×2160 keeping the aspect ratio, then
/// scale up (keeping the aspect ratio) so that both sides are at least 128. Always even.
pub fn lock_size(width: i32, height: i32) -> (i32, i32) {
    lock_size_within(width, height, MAX_W, MAX_H)
}

/// As [`lock_size`], fitting within `max_w`×`max_h` instead of 3840×2160 (cameras allow DCI
/// 4096×2160; an encoder that cannot do 4K lowers it).
pub fn lock_size_within(width: i32, height: i32, max_w: i32, max_h: i32) -> (i32, i32) {
    let (max_w, max_h) = (
        even_down(max_w.max(MIN_SIDE)),
        even_down(max_h.max(MIN_SIDE)),
    );
    let (mut w, mut h) = (even_down(width) as f64, even_down(height) as f64);
    let aspect = w / h;
    if w > max_w as f64 || h > max_h as f64 {
        let scale = (max_w as f64 / w).min(max_h as f64 / h);
        w *= scale;
        h *= scale;
    }
    let (mut w, mut h) = (even_down(w.floor() as i32), even_down(h.floor() as i32));
    if w < MIN_SIDE || h < MIN_SIDE {
        // Scale up so the smaller side reaches the floor, keeping the aspect ratio.
        if w < h {
            w = MIN_SIDE;
            h = even_up(MIN_SIDE as f64 / aspect).max(MIN_SIDE);
        } else {
            h = MIN_SIDE;
            w = even_up(MIN_SIDE as f64 * aspect).max(MIN_SIDE);
        }
        // An extreme aspect ratio could exceed the cap again; clamp (letterboxing absorbs it).
        w = w.min(max_w);
        h = h.min(max_h);
    }
    (w, h)
}

/// Video stamping state: arrival running time with a monotonic clamp.
#[derive(Debug, Default)]
pub struct VideoStamp {
    last: Option<u64>,
}

impl VideoStamp {
    pub fn next(&mut self, running_ns: u64) -> u64 {
        let pts = match self.last {
            Some(last) => running_ns.max(last + 1),
            None => running_ns,
        };
        self.last = Some(pts);
        pts
    }
}

/// Audio stamping state: sample-counted time anchored to arrival, with re-anchoring.
#[derive(Debug, Default)]
pub struct AudioStamp {
    t0: Option<u64>,
    samples: u64,
    /// A buffer was dropped; the next kept buffer carries DISCONT.
    dropped: bool,
}

/// Result of stamping one audio buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioStampOut {
    pub pts: u64,
    pub duration: u64,
    pub discont: bool,
    /// Drop this buffer: the sample-counted timeline is more than 40 ms ahead of arrival.
    pub drop: bool,
}

fn samples_to_ns(samples: u64, rate: u64) -> u64 {
    ((samples as u128 * 1_000_000_000u128) / rate.max(1) as u128) as u64
}

impl AudioStamp {
    /// `arrival_ns` is the running time at which the buffer arrived (its end); `frames` its
    /// sample count; `rate` the sample rate.
    ///
    /// Output PTS never goes backwards. Falling more than 40 ms behind arrival re-anchors forward
    /// (a gap, DISCONT). Running more than 40 ms ahead (a fast device clock, or a burst after a
    /// stall) drops whole buffers until the timeline is back within 40 ms, instead of re-anchoring
    /// backwards into overlapping timestamps; the next kept buffer carries DISCONT.
    pub fn next(&mut self, arrival_ns: u64, frames: u64, rate: u64) -> AudioStampOut {
        let duration = samples_to_ns(frames, rate);
        let estimate = arrival_ns.saturating_sub(duration);
        let mut discont = std::mem::take(&mut self.dropped);
        let t0 = match self.t0 {
            None => {
                self.samples = 0;
                estimate
            }
            Some(t0) => {
                let pts = t0 + samples_to_ns(self.samples, rate);
                if pts > estimate + REANCHOR_NS {
                    self.dropped = true;
                    return AudioStampOut {
                        pts,
                        duration,
                        discont: false,
                        drop: true,
                    };
                }
                if estimate > pts + REANCHOR_NS {
                    discont = true;
                    self.samples = 0;
                    estimate
                } else {
                    t0
                }
            }
        };
        self.t0 = Some(t0);
        let pts = t0 + samples_to_ns(self.samples, rate);
        self.samples += frames;
        AudioStampOut {
            pts,
            duration,
            discont,
            drop: false,
        }
    }
}

/// Running time "now" for a pipeline whose base time is in `base`, or None when the base time is
/// unset or still in the future (those buffers are dropped).
fn running_now(clock: &gst::Clock, base: &AtomicU64) -> Option<u64> {
    let base = base.load(Ordering::Acquire);
    if base == UNSET {
        return None;
    }
    let now = clock.time().nseconds();
    now.checked_sub(base)
}

/// Installs the video stamp probe (§5.0) on a pipewiresrc src pad.
///
/// Buffers flagged CORRUPTED are dropped: pipewiresrc sets the flag for truncated frames (a UVC
/// camera's first 4K NV12 frames arrive as ~98 KB of a 12.4 MB frame), and vah264enc fails
/// fatally ("Failed to import the input frame") on them. videorate fills the gap.
///
/// `deep_copy` (system-memory camera frames): each frame is copied out of PipeWire's buffer
/// right here, so the camera's few capture buffers are returned at once. Holding them in the
/// chain's queues starves a UVC camera at 4K (it then delivers only truncated frames).
///
/// `hold_early` (portal sources): a frame arriving after the base time is set but before it is
/// reached is stamped 0 instead of dropped. Compositors render a window once when the cast
/// starts and then only on damage, so dropping that frame leaves a static window with none.
pub fn install_video(
    pad: &gst::Pad,
    clock: gst::Clock,
    base: Arc<AtomicU64>,
    deep_copy: bool,
    hold_early: bool,
) {
    let state = Mutex::new(VideoStamp::default());
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if info
            .buffer()
            .is_some_and(|b| b.flags().contains(gst::BufferFlags::CORRUPTED))
        {
            return gst::PadProbeReturn::Drop;
        }
        if deep_copy && let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_mut() {
            *buffer = buffer.copy_deep().unwrap_or_else(|_| buffer.clone());
        }
        let running = match running_now(&clock, &base) {
            Some(running) => running,
            None if hold_early && base.load(Ordering::Acquire) != UNSET => 0,
            None => return gst::PadProbeReturn::Drop,
        };
        let pts = state.lock().map(|mut s| s.next(running)).unwrap_or(running);
        if let Some(buffer) = info.buffer_mut() {
            let b = buffer.make_mut();
            b.set_pts(gst::ClockTime::from_nseconds(pts));
            b.set_dts(gst::ClockTime::NONE);
            b.set_duration(gst::ClockTime::NONE);
        }
        gst::PadProbeReturn::Ok
    });
}

/// Bytes per sample for a raw audio format name (S16LE, F32LE, S24_32LE, ...).
pub fn audio_format_width(format: &str) -> Option<u64> {
    let f = format.to_ascii_uppercase();
    if f.starts_with("S24_32") || f.starts_with("U24_32") || f.starts_with("S20_32") {
        return Some(4);
    }
    let digits: String = f
        .chars()
        .skip(1)
        .take_while(|c| c.is_ascii_digit())
        .collect();
    match digits.parse::<u64>().ok()? {
        8 => Some(1),
        16 => Some(2),
        18 | 20 | 24 => Some(3),
        32 => Some(4),
        64 => Some(8),
        _ => None,
    }
}

#[derive(Default, Clone, Copy)]
struct AudioFormat {
    rate: u64,
    bytes_per_frame: u64,
}

/// Installs the audio stamp probe (§5.0) on a pipewiresrc src pad. The sample rate and frame
/// size are read from the CAPS event passing the same pad.
pub fn install_audio(pad: &gst::Pad, clock: gst::Clock, base: Arc<AtomicU64>) {
    let format = Arc::new(Mutex::new(AudioFormat::default()));
    let fmt_events = format.clone();
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::EventView::Caps(c)) = info.event().map(|e| e.view())
            && let Some(s) = c.caps().structure(0)
        {
            let rate = s.get::<i32>("rate").unwrap_or(0).max(0) as u64;
            let channels = s.get::<i32>("channels").unwrap_or(1).max(1) as u64;
            let width = s
                .get::<&str>("format")
                .ok()
                .and_then(audio_format_width)
                .unwrap_or(0);
            if let Ok(mut f) = fmt_events.lock() {
                *f = AudioFormat {
                    rate,
                    bytes_per_frame: width * channels,
                };
            }
        }
        gst::PadProbeReturn::Ok
    });
    let state = Mutex::new(AudioStamp::default());
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(arrival) = running_now(&clock, &base) else {
            return gst::PadProbeReturn::Drop;
        };
        let fmt = format.lock().map(|f| *f).unwrap_or_default();
        let Some(buffer) = info.buffer_mut() else {
            return gst::PadProbeReturn::Ok;
        };
        if fmt.rate == 0 || fmt.bytes_per_frame == 0 {
            // Unknown format: fall back to arrival stamping.
            let b = buffer.make_mut();
            b.set_pts(gst::ClockTime::from_nseconds(arrival));
            b.set_dts(gst::ClockTime::NONE);
            return gst::PadProbeReturn::Ok;
        }
        let frames = buffer.size() as u64 / fmt.bytes_per_frame;
        let out = match state.lock() {
            Ok(mut s) => s.next(arrival, frames, fmt.rate),
            Err(_) => return gst::PadProbeReturn::Ok,
        };
        if out.drop {
            return gst::PadProbeReturn::Drop;
        }
        let b = buffer.make_mut();
        b.set_pts(gst::ClockTime::from_nseconds(out.pts));
        b.set_dts(gst::ClockTime::NONE);
        b.set_duration(gst::ClockTime::from_nseconds(out.duration));
        if out.discont {
            b.set_flags(gst::BufferFlags::DISCONT);
        }
        gst::PadProbeReturn::Ok
    });
}

/// Installs the size lock (§5.2) on `pad`: the first CAPS event's width/height are locked
/// (see [`lock_size`]) and written into `outcaps` (a capsfilter) via `make_caps`. Later CAPS
/// events pass through unchanged. The locked size is also stored in `locked`.
pub fn install_size_lock(
    pad: &gst::Pad,
    outcaps: gst::Element,
    max: (i32, i32),
    make_caps: impl Fn(i32, i32) -> gst::Caps + Send + Sync + 'static,
    locked: SizeSlot,
) {
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::EventView::Caps(c)) = info.event().map(|e| e.view()) {
            let Ok(mut slot) = locked.lock() else {
                return gst::PadProbeReturn::Ok;
            };
            if slot.is_none()
                && let Some(s) = c.caps().structure(0)
                && let (Ok(w), Ok(h)) = (s.get::<i32>("width"), s.get::<i32>("height"))
            {
                let (lw, lh) = lock_size_within(w, h, max.0, max.1);
                outcaps.set_property("caps", make_caps(lw, lh));
                *slot = Some((lw, lh));
            }
        }
        gst::PadProbeReturn::Ok
    });
}

/// Records the first and last buffer PTS reaching a muxer sink pad (§5.1), as running time
/// (encoders may offset PTS, e.g. a 1000 h min-pts, compensated by their segment).
pub fn install_mux_probe(pad: &gst::Pad, first: Arc<AtomicU64>, last: Arc<AtomicU64>) {
    let segment: Arc<Mutex<Option<gst::FormattedSegment<gst::ClockTime>>>> =
        Arc::new(Mutex::new(None));
    let seg_events = segment.clone();
    pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::EventView::Segment(e)) = info.event().map(|e| e.view())
            && let Some(seg) = e.segment().downcast_ref::<gst::ClockTime>()
            && let Ok(mut slot) = seg_events.lock()
        {
            *slot = Some(seg.clone());
        }
        gst::PadProbeReturn::Ok
    });
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(pts) = info.buffer().and_then(|b| b.pts()) else {
            return gst::PadProbeReturn::Ok;
        };
        let running = segment
            .lock()
            .ok()
            .and_then(|s| s.as_ref().and_then(|s| s.to_running_time(pts)))
            .unwrap_or(pts);
        let ns = running.nseconds();
        let _ = first.compare_exchange(UNSET, ns, Ordering::AcqRel, Ordering::Acquire);
        let mut cur = last.load(Ordering::Acquire);
        while cur == UNSET || ns > cur {
            match last.compare_exchange_weak(cur, ns, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        gst::PadProbeReturn::Ok
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_lock_even_rounding() {
        assert_eq!(lock_size(1921, 1081), (1920, 1080));
        assert_eq!(lock_size(1280, 720), (1280, 720));
    }

    #[test]
    fn size_lock_caps_4k() {
        assert_eq!(lock_size(7680, 4320), (3840, 2160));
        let (w, h) = lock_size(5000, 1000);
        assert!(w <= MAX_W && h <= MAX_H && w % 2 == 0 && h % 2 == 0);
        assert_eq!(w, 3840);
    }

    #[test]
    fn size_lock_within_custom_max() {
        assert_eq!(lock_size_within(4096, 2160, 4096, 2160), (4096, 2160));
        assert_eq!(lock_size_within(4096, 2160, 3840, 2160), (3840, 2024));
        assert_eq!(lock_size_within(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(lock_size_within(6144, 3456, 4096, 2160), (3840, 2160));
    }

    #[test]
    fn size_lock_floor_keeps_aspect() {
        let (w, h) = lock_size(100, 60);
        assert_eq!(h, 128);
        assert!(w >= 128 && w % 2 == 0);
        // 100/60 = 1.667 → 128 * 1.667 = 213.3 → 214
        assert_eq!(w, 214);
        let (w, h) = lock_size(60, 100);
        assert_eq!((w, h), (128, 214));
    }

    #[test]
    fn video_monotonic_clamp() {
        let mut s = VideoStamp::default();
        assert_eq!(s.next(100), 100);
        assert_eq!(s.next(100), 101);
        assert_eq!(s.next(50), 102);
        assert_eq!(s.next(1_000), 1_000);
    }

    #[test]
    fn audio_reanchor_threshold() {
        let rate = 48_000;
        let mut s = AudioStamp::default();
        // 10 ms buffers (480 frames) arriving on time.
        let a = s.next(20_000_000, 480, rate);
        assert_eq!(a.pts, 10_000_000);
        assert_eq!(a.duration, 10_000_000);
        assert!(!a.discont);
        // Jitter below 40 ms keeps the sample-counted timeline.
        let b = s.next(60_000_000, 480, rate);
        assert_eq!(b.pts, 20_000_000);
        assert!(!b.discont);
        // A gap above 40 ms re-anchors to the arrival estimate.
        let c = s.next(200_000_000, 480, rate);
        assert!(c.discont);
        assert_eq!(c.pts, 190_000_000);
        let d = s.next(200_000_000 + 10_000_000, 480, rate);
        assert_eq!(d.pts, 200_000_000);
        assert!(!d.discont);
    }

    #[test]
    fn audio_never_goes_backwards() {
        let rate = 48_000;
        let mut s = AudioStamp::default();
        // A burst: buffers arrive faster than real time, so the sample count runs ahead.
        let mut end = 0u64;
        let mut dropped = 0;
        let mut saw_discont = false;
        for i in 0..20u64 {
            let o = s.next(20_000_000 + i * 1_000_000, 480, rate);
            if o.drop {
                dropped += 1;
                continue;
            }
            assert!(o.pts >= end, "pts {} before previous end {end}", o.pts);
            assert!(o.pts <= 20_000_000 + i * 1_000_000 + REANCHOR_NS);
            saw_discont |= o.discont;
            end = o.pts + o.duration;
        }
        assert!(dropped > 0);
        // Arrival catches up: buffers are kept again, the first one marked DISCONT.
        let o = s.next(400_000_000, 480, rate);
        assert!(!o.drop);
        assert!(o.discont || saw_discont);
        assert!(o.pts >= end);
    }

    #[test]
    fn audio_widths() {
        assert_eq!(audio_format_width("S32LE"), Some(4));
        assert_eq!(audio_format_width("S16LE"), Some(2));
        assert_eq!(audio_format_width("F32LE"), Some(4));
        assert_eq!(audio_format_width("S24_32LE"), Some(4));
        assert_eq!(audio_format_width("S24LE"), Some(3));
        assert_eq!(audio_format_width("F64LE"), Some(8));
        assert_eq!(audio_format_width("U8"), Some(1));
    }
}
