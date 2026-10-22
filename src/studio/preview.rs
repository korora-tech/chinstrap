//! Live previews: the hub the UI reads frames from, and the appsink branch that feeds it.
//!
//! Exactly one PipeWire consumer per node: while idle each previewed input has its own small
//! pipeline (`PipeKind::Preview`, built by `recorder::build_preview`); while recording the
//! frames come from a tee branch inside the input's recording pipeline. Either way the branch
//! is `throttle ! queue leaky=1 ! scale ! BGRx (fits 480×300) ! appsink`.
//!
//! A preview never fails an input: the branch's flow returns are swallowed by the throttle
//! probe, its errors are dropped by a bus sync handler, and its appsink is not a sink as far as
//! the pipeline's EOS and preroll are concerned. Pixels never go through the Snapshot.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;

use super::encoders::set_prop;
use super::recorder::{capsfilter, make};
use super::types::{InputKey, PreviewFrame, PreviewOrigin};

/// Every preview frame fits in this box (aspect ratio kept).
pub const PREVIEW_MAX: (i32, i32) = (480, 300);
/// At most one preview frame per input this often (~15 fps).
pub const PREVIEW_EVERY: Duration = Duration::from_millis(66);
/// A frame this early for its slot still takes it.
const PREVIEW_SLACK: Duration = Duration::from_millis(16);
/// Element-name prefix of a recording pipeline's preview branch (its errors are dropped).
pub const BRANCH_PREFIX: &str = "preview-";

/// `CHINSTRAP_PREVIEW=0` turns previews off (no preview pipelines, no tee branches).
pub fn enabled_from_env() -> bool {
    std::env::var("CHINSTRAP_PREVIEW").map_or(true, |v| v.trim() != "0")
}

/// `w×h` scaled to fit [`PREVIEW_MAX`], aspect ratio kept, both sides even and at least 2.
pub fn fit(w: i32, h: i32) -> (i32, i32) {
    let (w, h) = (w.max(1) as f64, h.max(1) as f64);
    let s = (PREVIEW_MAX.0 as f64 / w)
        .min(PREVIEW_MAX.1 as f64 / h)
        .min(1.0);
    let even = |v: f64| ((v * s / 2.0).round() as i32 * 2).max(2);
    (even(w), even(h))
}

/// Caps the preview scaler fixates into: BGRx within the box, square pixels (both
/// `videoscale` and `vapostproc` then keep the display aspect ratio).
fn box_caps() -> String {
    format!(
        "video/x-raw,format=BGRx,width=[2,{}],height=[2,{}],pixel-aspect-ratio=1/1",
        PREVIEW_MAX.0, PREVIEW_MAX.1
    )
}

struct Slot {
    frame: Arc<PreviewFrame>,
    /// Frames published for this input so far (never reset while the input exists).
    generation: u64,
}

struct Hub {
    slots: Mutex<HashMap<InputKey, Slot>>,
    /// Woken (at most one pending wake) whenever a frame is published.
    wake_tx: async_channel::Sender<()>,
    wake_rx: async_channel::Receiver<()>,
}

/// The newest preview frame per input. Cheap to clone; shared by the engine (writer), the
/// streaming threads of the preview appsinks and the UI (reader).
#[derive(Clone)]
pub struct Previews(Arc<Hub>);

impl Default for Previews {
    fn default() -> Self {
        let (wake_tx, wake_rx) = async_channel::bounded(1);
        Self(Arc::new(Hub {
            slots: Mutex::new(HashMap::new()),
            wake_tx,
            wake_rx,
        }))
    }
}

impl Previews {
    /// The newest frame of `key` and its generation (bumped by every frame).
    pub fn latest(&self, key: InputKey) -> Option<(u64, Arc<PreviewFrame>)> {
        let slots = self.0.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.get(&key).map(|s| (s.generation, s.frame.clone()))
    }

    /// Frames published for `key` so far (0: none).
    pub fn generation(&self, key: InputKey) -> u64 {
        self.latest(key).map_or(0, |(g, _)| g)
    }

    /// Receives `()` after new frames were published (coalesced: at most one pending).
    pub fn changed(&self) -> async_channel::Receiver<()> {
        self.0.wake_rx.clone()
    }

    pub(super) fn publish(&self, key: InputKey, frame: PreviewFrame) {
        {
            let mut slots = self.0.slots.lock().unwrap_or_else(|e| e.into_inner());
            let generation = slots.get(&key).map_or(0, |s| s.generation) + 1;
            slots.insert(
                key,
                Slot {
                    frame: Arc::new(frame),
                    generation,
                },
            );
        }
        let _ = self.0.wake_tx.try_send(());
    }

    /// Forgets every input not in `keys` (a removed source, a deselected camera).
    pub(super) fn retain(&self, keys: &[InputKey]) {
        let mut slots = self.0.slots.lock().unwrap_or_else(|e| e.into_inner());
        let before = slots.len();
        slots.retain(|k, _| keys.contains(k));
        if slots.len() != before {
            let _ = self.0.wake_tx.try_send(());
        }
    }
}

/// Where one preview branch delivers its frames.
#[derive(Clone)]
pub struct Tap {
    pub hub: Previews,
    pub key: InputKey,
    pub origin: PreviewOrigin,
    /// Set once the branch failed; it then drops every frame (recording goes on).
    pub killed: Arc<AtomicBool>,
}

impl Tap {
    pub fn new(hub: &Previews, key: InputKey, origin: PreviewOrigin) -> Self {
        Self {
            hub: hub.clone(),
            key,
            origin,
            killed: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// How the branch downscales to BGRx.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scaler {
    /// `vapostproc`: on the GPU, from DMA-BUF, VA or system memory.
    Va,
    /// `videoscale ! videoconvert` on the CPU (system memory only).
    Cpu,
}

/// Builds the branch from `from` (a free src pad: a tee request pad, or a source's caps
/// filter): throttle probe, leaky queue of one, `pre` (e.g. a JPEG decoder, run only on kept
/// frames), the scaler, the BGRx box and an appsink publishing to `tap`.
///
/// `detached`: the appsink does not count as a sink of `pipeline` (a recording pipeline's EOS
/// and preroll never wait for it).
pub fn branch(
    pipeline: &gst::Pipeline,
    from: &gst::Pad,
    tap: &Tap,
    pre: Vec<gst::Element>,
    scaler: Scaler,
    detached: bool,
) -> Result<(), String> {
    let named = |factory: &str, name: &str| -> Result<gst::Element, String> {
        let el = make(factory)?;
        el.set_property("name", format!("{BRANCH_PREFIX}{name}"));
        Ok(el)
    };
    let queue = named("queue", "queue")?;
    set_prop(&queue, "max-size-buffers", "1");
    set_prop(&queue, "max-size-bytes", "0");
    set_prop(&queue, "max-size-time", "0");
    set_prop(&queue, "leaky", "downstream");
    let mut chain = vec![queue.clone()];
    for el in pre {
        let name = format!("{BRANCH_PREFIX}{}", el.name());
        el.set_property("name", name);
        chain.push(el);
    }
    match scaler {
        Scaler::Va => chain.push(named("vapostproc", "scale")?),
        Scaler::Cpu => {
            chain.push(named("videoscale", "scale")?);
            chain.push(named("videoconvert", "convert")?);
        }
    }
    let caps = capsfilter(&box_caps())?;
    caps.set_property("name", format!("{BRANCH_PREFIX}caps"));
    chain.push(caps);
    let sink = gst_app::AppSink::builder()
        .name(format!("{BRANCH_PREFIX}sink"))
        .sync(false)
        .max_buffers(1)
        .drop(true)
        .build();
    if detached {
        // Not a sink to the bin: the pipeline's EOS (finalize) and async state changes never
        // wait for the preview.
        sink.set_property("async", false);
        sink.unset_element_flags(gst::ElementFlags::SINK);
    }
    let out = tap.clone();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if !out.killed.load(Ordering::Acquire)
                    && let Some(frame) = frame_of(&sample, out.origin)
                {
                    out.hub.publish(out.key, frame);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    chain.push(sink.upcast());
    pipeline
        .add_many(&chain)
        .map_err(|e| format!("cannot add preview elements: {e}"))?;
    gst::Element::link_many(&chain).map_err(|e| format!("cannot link the preview: {e}"))?;
    let peer = queue
        .static_pad("sink")
        .ok_or("preview queue has no sink pad")?;
    from.link(&peer)
        .map_err(|e| format!("cannot link the preview branch: {e:?}"))?;
    gate(from, peer, tap.killed.clone());
    for el in &chain {
        let _ = el.sync_state_with_parent();
    }
    Ok(())
}

/// Throttles `pad` to one buffer per [`PREVIEW_EVERY`] slot and isolates the branch: kept buffers
/// are chained into `peer` here and the probe reports them handled, so a branch failure (e.g.
/// not-negotiated) never travels back up to a tee and the recording. After one, every buffer is
/// dropped.
fn gate(pad: &gst::Pad, peer: gst::Pad, killed: Arc<AtomicBool>) {
    let next = Mutex::new(None::<Instant>);
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if killed.load(Ordering::Acquire) {
            return gst::PadProbeReturn::Drop;
        }
        let now = Instant::now();
        if let Ok(mut next) = next.lock() {
            if next.is_some_and(|t| now + PREVIEW_SLACK < t) {
                return gst::PadProbeReturn::Drop;
            }
            // Slots follow each other (not this frame's arrival), so arrival jitter on a 30 fps
            // source still keeps every second frame instead of every third.
            *next = Some(match *next {
                Some(t) if now < t + PREVIEW_EVERY => t + PREVIEW_EVERY,
                _ => now + PREVIEW_EVERY,
            });
        }
        let Some(gst::PadProbeData::Buffer(buf)) = info.data.take() else {
            return gst::PadProbeReturn::Ok;
        };
        match peer.chain(buf) {
            Ok(_) | Err(gst::FlowError::Flushing) => {}
            Err(e) => {
                if !killed.swap(true, Ordering::AcqRel) {
                    eprintln!(
                        "chinstrap: preview of {} stopped ({e:?}); recording is unaffected",
                        pad.parent()
                            .and_then(|p| p.parent())
                            .map(|p| p.name().to_string())
                            .unwrap_or_default()
                    );
                }
            }
        }
        gst::PadProbeReturn::Handled
    });
}

/// Drops error and warning messages from the preview branch of a recording pipeline (they
/// would otherwise fail the input) and marks the branch killed.
pub fn isolate_errors(bus: &gst::Bus, killed: Arc<AtomicBool>) {
    bus.set_sync_handler(move |_, msg| {
        let from_branch = msg
            .src()
            .is_some_and(|s| s.name().starts_with(BRANCH_PREFIX));
        match msg.view() {
            gst::MessageView::Error(_) | gst::MessageView::Warning(_) if from_branch => {
                if let gst::MessageView::Error(e) = msg.view()
                    && !killed.swap(true, Ordering::AcqRel)
                {
                    eprintln!(
                        "chinstrap: preview branch error ignored: {} ({:?})",
                        e.error(),
                        e.debug()
                    );
                }
                gst::BusSyncReply::Drop
            }
            _ => gst::BusSyncReply::Pass,
        }
    });
}

/// The sample as a tightly packed BGRA frame (stride removed, alpha forced to 255).
fn frame_of(sample: &gst::Sample, origin: PreviewOrigin) -> Option<PreviewFrame> {
    let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
    let buffer = sample.buffer()?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    let (w, h) = (info.width(), info.height());
    let stride = *frame.plane_stride().first()? as usize;
    let data = frame.plane_data(0).ok()?;
    Some(PreviewFrame {
        width: w,
        height: h,
        bgra: pack_bgra(data, stride, w as usize, h as usize)?,
        origin,
        at: Instant::now(),
    })
}

/// Copies `h` rows of `w` BGRx pixels from `data` (rows `stride` bytes apart), alpha set to 255.
fn pack_bgra(data: &[u8], stride: usize, w: usize, h: usize) -> Option<Vec<u8>> {
    let row = w * 4;
    if stride < row || data.len() < stride * h.saturating_sub(1) + row {
        return None;
    }
    let mut out = Vec::with_capacity(row * h);
    for y in 0..h {
        out.extend_from_slice(&data[y * stride..y * stride + row]);
    }
    for px in out.as_chunks_mut::<4>().0 {
        px[3] = 255;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_aspect_within_the_box() {
        assert_eq!(fit(3840, 2160), (480, 270));
        assert_eq!(fit(1280, 720), (480, 270));
        assert_eq!(fit(3840, 2082), (480, 260));
        assert_eq!(fit(1080, 1920), (168, 300));
        assert_eq!(fit(1600, 1000), (480, 300));
        // Never upscaled.
        assert_eq!(fit(320, 240), (320, 240));
        assert_eq!(fit(0, 0), (2, 2));
    }

    #[test]
    fn pack_strips_stride_and_sets_alpha() {
        // 2×2 BGRx, stride 12 (4 bytes of padding per row).
        let data = [
            1, 2, 3, 0, 4, 5, 6, 0, 9, 9, 9, 9, //
            7, 8, 9, 0, 10, 11, 12, 0, 9, 9, 9, 9,
        ];
        let out = pack_bgra(&data, 12, 2, 2).unwrap();
        assert_eq!(
            out,
            vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
        // The last row may stop at its pixels.
        assert!(pack_bgra(&data[..20], 12, 2, 2).is_some());
        assert!(pack_bgra(&data[..19], 12, 2, 2).is_none());
        assert!(pack_bgra(&data, 4, 2, 2).is_none());
    }

    #[test]
    fn hub_counts_generations_and_forgets_removed_inputs() {
        let hub = Previews::default();
        let wake = hub.changed();
        let frame = |origin| PreviewFrame {
            width: 2,
            height: 2,
            bgra: vec![0; 16],
            origin,
            at: Instant::now(),
        };
        let key = InputKey::Camera;
        assert_eq!(hub.generation(key), 0);
        hub.publish(key, frame(PreviewOrigin::Idle));
        hub.publish(key, frame(PreviewOrigin::Recording { segment: 1 }));
        let (g, f) = hub.latest(key).unwrap();
        assert_eq!(g, 2);
        assert_eq!(f.origin, PreviewOrigin::Recording { segment: 1 });
        // Wakes coalesce.
        assert!(wake.try_recv().is_ok());
        assert!(wake.try_recv().is_err());
        hub.retain(&[]);
        assert!(hub.latest(key).is_none());
    }

    /// The branch behind a tee: frames reach the hub at the throttled rate, and a branch that
    /// fails leaves the main path running without an error on the bus.
    #[test]
    fn tee_branch_publishes_and_never_fails_the_main_path() {
        gst::init().unwrap();
        for broken in [false, true] {
            let pipeline = gst::Pipeline::new();
            let src = make("videotestsrc").unwrap();
            set_prop(&src, "is-live", "true");
            let caps = capsfilter("video/x-raw,width=1280,height=720,framerate=60/1").unwrap();
            let tee = make("tee").unwrap();
            let sink = make("fakesink").unwrap();
            set_prop(&sink, "sync", "false");
            let counted = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let c = counted.clone();
            sink.static_pad("sink")
                .unwrap()
                .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                    c.fetch_add(1, Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                });
            pipeline.add_many([&src, &caps, &tee, &sink]).unwrap();
            gst::Element::link_many([&src, &caps, &tee, &sink]).unwrap();
            let hub = Previews::default();
            let tap = Tap::new(
                &hub,
                InputKey::Camera,
                PreviewOrigin::Recording { segment: 1 },
            );
            // `broken`: a branch that fails at run time, after negotiation.
            let pre = if broken {
                let failing = make("identity").unwrap();
                set_prop(&failing, "error-after", "2");
                vec![failing]
            } else {
                Vec::new()
            };
            let pad = tee.request_pad_simple("src_%u").unwrap();
            branch(&pipeline, &pad, &tap, pre, Scaler::Cpu, true).unwrap();
            isolate_errors(&pipeline.bus().unwrap(), tap.killed.clone());
            pipeline.set_state(gst::State::Playing).unwrap();
            std::thread::sleep(Duration::from_millis(1000));
            let bus = pipeline.bus().unwrap();
            let error = bus.pop_filtered(&[gst::MessageType::Error]);
            pipeline.set_state(gst::State::Null).unwrap();
            assert!(error.is_none(), "broken={broken}: {error:?}");
            assert!(counted.load(Ordering::Relaxed) >= 40, "main path stalled");
            if broken {
                assert!(tap.killed.load(Ordering::Acquire));
                // At most the frame before the failure.
                assert!(hub.generation(InputKey::Camera) <= 1);
            } else {
                let g = hub.generation(InputKey::Camera);
                // 1 s at one frame per 66 ms.
                assert!((8..=17).contains(&g), "{g} frames");
                let (_, f) = hub.latest(InputKey::Camera).unwrap();
                assert_eq!((f.width, f.height), (480, 270));
                assert!(f.bgra.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
            }
        }
    }
}
