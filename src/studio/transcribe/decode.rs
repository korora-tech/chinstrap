//! Decode a media file's first audio stream to 16 kHz mono f32 on the file timeline (§12.4).
//!
//! Sample `i` of the output is at `i / 16000` s of the file's presentation timeline (Matroska
//! timeline 0, i.e. the demuxed buffer PTS). Leading offsets and real gaps are zero-filled;
//! resampler jitter of up to 2 samples is absorbed by appending contiguously.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

pub const RATE: u32 = 16_000;
/// 4 h at 16 kHz (about 920 MB of f32).
pub const MAX_SAMPLES: usize = 4 * 3600 * RATE as usize;
const JITTER: i64 = 2;

pub struct Decoded {
    pub pcm: Vec<f32>,
    /// File-timeline time of the first decoded buffer.
    pub media_start_ns: Option<u64>,
}

/// Places decoded buffers by timestamp (§12.4 "Sample layout").
#[derive(Default)]
pub struct Placer {
    pub pcm: Vec<f32>,
    pub first_ns: Option<u64>,
}

impl Placer {
    /// `t_ns`: file-timeline time of the buffer's first sample (None: append contiguously).
    pub fn place(&mut self, t_ns: Option<u64>, samples: &[f32]) -> Result<(), String> {
        if self.first_ns.is_none() {
            self.first_ns = t_ns;
        }
        let write_pos = self.pcm.len() as i64;
        let idx = t_ns.map_or(write_pos, |t| {
            ((t as u128 * RATE as u128 + 500_000_000) / 1_000_000_000) as i64
        });
        let mut src = samples;
        if idx > write_pos + JITTER {
            if idx as usize > MAX_SAMPLES {
                return Err("audio is longer than 4 hours".into());
            }
            self.pcm.resize(idx as usize, 0.0);
        } else if idx < write_pos - JITTER {
            let overlap = ((write_pos - idx) as usize).min(src.len());
            src = &src[overlap..];
        }
        if self.pcm.len() + src.len() > MAX_SAMPLES {
            return Err("audio is longer than 4 hours".into());
        }
        self.pcm.extend_from_slice(src);
        Ok(())
    }
}

/// Decodes `media`. `progress` gets 0.0..=1.0 at most every 250 ms. `cancel` aborts.
pub fn decode(
    media: &Path,
    cancel: Option<&AtomicBool>,
    mut progress: impl FnMut(f64),
) -> Result<Decoded, String> {
    gst::init().map_err(|e| format!("GStreamer init: {e}"))?;
    if !media.is_file() {
        return Err(format!("{}: no such file", media.display()));
    }
    let make = |name: &str| {
        gst::ElementFactory::make(name)
            .build()
            .map_err(|_| format!("missing GStreamer element {name}"))
    };
    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", media.to_string_lossy().to_string())
        .build()
        .map_err(|_| "missing GStreamer element filesrc".to_string())?;
    let decodebin = make("decodebin")?;
    let convert = make("audioconvert")?;
    let resample = make("audioresample")?;
    let caps = gst::Caps::builder("audio/x-raw")
        .field("format", "F32LE")
        .field("rate", RATE as i32)
        .field("channels", 1i32)
        .field("layout", "interleaved")
        .build();
    let sink = gst_app::AppSink::builder()
        .caps(&caps)
        .sync(false)
        .max_buffers(64)
        .build();
    pipeline
        .add_many([&src, &decodebin, &convert, &resample, sink.upcast_ref()])
        .map_err(|e| e.to_string())?;
    src.link(&decodebin).map_err(|e| e.to_string())?;
    gst::Element::link_many([&convert, &resample, sink.upcast_ref()]).map_err(|e| e.to_string())?;

    let linked = Arc::new(AtomicBool::new(false));
    let no_more = Arc::new(AtomicBool::new(false));
    {
        let linked = linked.clone();
        let convert = convert.clone();
        let pipeline_weak = pipeline.downgrade();
        decodebin.connect_pad_added(move |_, pad| {
            let is_audio = pad
                .current_caps()
                .or_else(|| Some(pad.query_caps(None)))
                .and_then(|c| c.structure(0).map(|s| s.name().starts_with("audio/")))
                .unwrap_or(false);
            if is_audio && !linked.swap(true, Ordering::SeqCst) {
                if let Some(sinkpad) = convert.static_pad("sink") {
                    let _ = pad.link(&sinkpad);
                }
                return;
            }
            // Other streams (and later audio streams) are discarded.
            let Some(pipeline) = pipeline_weak.upgrade() else {
                return;
            };
            if let Ok(fake) = gst::ElementFactory::make("fakesink")
                .property("sync", false)
                .build()
                && pipeline.add(&fake).is_ok()
            {
                let _ = fake.sync_state_with_parent();
                if let Some(sp) = fake.static_pad("sink") {
                    let _ = pad.link(&sp);
                }
            }
        });
        let no_more = no_more.clone();
        decodebin.connect_no_more_pads(move |_| no_more.store(true, Ordering::SeqCst));
    }

    let bus = pipeline.bus().ok_or("pipeline has no bus")?;
    let result = (|| -> Result<Decoded, String> {
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| bus_error(&bus).unwrap_or_else(|| "cannot start decoding".into()))?;
        let mut placer = Placer::default();
        let mut last_progress = Instant::now();
        let mut duration: Option<gst::ClockTime> = None;
        loop {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err("cancelled".into());
            }
            if let Some(e) = bus_error(&bus) {
                return Err(e);
            }
            if no_more.load(Ordering::SeqCst) && !linked.load(Ordering::SeqCst) {
                return Err("the file has no audio stream".into());
            }
            match sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
                Some(sample) => {
                    let Some(buffer) = sample.buffer() else {
                        continue;
                    };
                    // Matroska timeline position. matroskademux emits block timestamps as PTS
                    // but sets segment.start to the first block, so `to_stream_time` would
                    // rebase a non-zero first PTS to 0; the PTS itself is the file timeline.
                    let t_ns = buffer.pts().map(|pts| pts.nseconds());
                    let map = buffer.map_readable().map_err(|e| e.to_string())?;
                    let samples: Vec<f32> = map
                        .as_slice()
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| f32::from_le_bytes(*c))
                        .collect();
                    placer.place(t_ns, &samples)?;
                }
                None => {
                    if sink.is_eos() {
                        break;
                    }
                }
            }
            if last_progress.elapsed() >= Duration::from_millis(250) {
                last_progress = Instant::now();
                if duration.is_none() {
                    duration = pipeline.query_duration::<gst::ClockTime>();
                }
                if let (Some(d), Some(p)) = (duration, pipeline.query_position::<gst::ClockTime>())
                    && d.nseconds() > 0
                {
                    progress((p.nseconds() as f64 / d.nseconds() as f64).clamp(0.0, 1.0));
                }
            }
        }
        if !linked.load(Ordering::SeqCst) {
            return Err("the file has no audio stream".into());
        }
        progress(1.0);
        Ok(Decoded {
            media_start_ns: placer.first_ns,
            pcm: placer.pcm,
        })
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}

fn bus_error(bus: &gst::Bus) -> Option<String> {
    while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error]) {
        if let gst::MessageView::Error(e) = msg.view() {
            return Some(match e.debug() {
                Some(d) => format!("decode failed: {} ({d})", e.error()),
                None => format!("decode failed: {}", e.error()),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn placer_layout() {
        let mut p = Placer::default();
        // Leading offset of 1.5 s is zero-filled.
        p.place(Some(1_500_000_000), &[1.0; 160]).unwrap();
        assert_eq!(p.pcm.len(), 24_000 + 160);
        assert_eq!(p.pcm[23_999], 0.0);
        assert_eq!(p.pcm[24_000], 1.0);
        assert_eq!(p.first_ns, Some(1_500_000_000));
        // Jitter of 2 samples (125 µs) is appended contiguously.
        let next = 1_500_000_000 + 10_000_000 + 125_000;
        p.place(Some(next), &[2.0; 160]).unwrap();
        assert_eq!(p.pcm.len(), 24_000 + 320);
        // A real 2 s gap is reproduced.
        p.place(Some(3_520_000_000), &[3.0; 10]).unwrap();
        assert_eq!(p.pcm.len(), 56_320 + 10);
        assert_eq!(p.pcm[56_320], 3.0);
        // Overlap: the overlapping prefix is dropped.
        p.place(Some(3_520_000_000), &[4.0; 20]).unwrap();
        assert_eq!(p.pcm.len(), 56_320 + 20);
        assert_eq!(p.pcm[56_330], 4.0);
        // No PTS: contiguous.
        p.place(None, &[5.0; 5]).unwrap();
        assert_eq!(p.pcm.len(), 56_345);
    }

    #[test]
    fn placer_repeated_small_gaps_do_not_drift() {
        // 60 s of 20 ms buffers, with a 10 ms gap after every 5th buffer.
        let mut p = Placer::default();
        let mut t = 0u64;
        let mut expected_last = 0usize;
        for i in 0..3000 {
            p.place(Some(t), &[1.0; 320]).unwrap();
            expected_last = (t * 16 / 1_000_000) as usize + 320;
            t += 20_000_000;
            if i % 5 == 4 {
                t += 10_000_000;
            }
        }
        assert!((p.pcm.len() as i64 - expected_last as i64).abs() <= 2);
    }

    #[test]
    fn placer_cap() {
        let mut p = Placer::default();
        assert!(
            p.place(Some(4 * 3600 * 1_000_000_000 + 1_000_000), &[0.0])
                .is_err()
        );
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "chinstrap-decode-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Writes a Matroska file of raw 16 kHz mono F32 audio: (pts_ns, n samples, value) buffers.
    fn write_mkv(path: &Path, buffers: &[(u64, usize, f32)], opus: bool) {
        gst::init().unwrap();
        let enc = if opus {
            "audioconvert ! audioresample ! audio/x-raw,rate=48000 ! opusenc bitrate=192000 ! "
        } else {
            "audioconvert ! audio/x-raw,format=S16LE ! "
        };
        let desc = format!(
            "appsrc name=src format=time caps=audio/x-raw,format=F32LE,rate=16000,channels=1,layout=interleaved ! {enc}matroskamux ! filesink location={}",
            path.display()
        );
        let pipeline = gst::parse::launch(&desc)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let src = pipeline
            .by_name("src")
            .unwrap()
            .downcast::<gst_app::AppSrc>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let mut last_end = None;
        for &(pts, n, v) in buffers {
            let bytes: Vec<u8> = std::iter::repeat_n(v, n)
                .flat_map(f32::to_le_bytes)
                .collect();
            let mut buf = gst::Buffer::from_slice(bytes);
            {
                let b = buf.get_mut().unwrap();
                b.set_pts(gst::ClockTime::from_nseconds(pts));
                b.set_duration(gst::ClockTime::from_nseconds(n as u64 * 62_500));
                if last_end.is_some_and(|e| e != pts) {
                    b.set_flags(gst::BufferFlags::DISCONT);
                }
            }
            last_end = Some(pts + n as u64 * 62_500);
            src.push_buffer(buf).unwrap();
        }
        src.end_of_stream().unwrap();
        let bus = pipeline.bus().unwrap();
        let msg = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(30),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("mux finished");
        assert!(matches!(msg.view(), gst::MessageView::Eos(_)), "{msg:?}");
        pipeline.set_state(gst::State::Null).unwrap();
    }

    fn first_nonzero(pcm: &[f32], from: usize) -> Option<usize> {
        pcm[from..]
            .iter()
            .position(|&x| x.abs() > 0.1)
            .map(|i| i + from)
    }

    #[test]
    fn decode_first_pts_and_discont_gap() {
        let dir = scratch("gap");
        let mkv = dir.join("gap.mkv");
        // 1.5 s first PTS; 1 s of audio; a 2 s DISCONT gap; 1 s more.
        let mut bufs = Vec::new();
        for i in 0..50u64 {
            bufs.push((1_500_000_000 + i * 20_000_000, 320, 0.5));
        }
        for i in 0..50u64 {
            bufs.push((4_500_000_000 + i * 20_000_000, 320, 0.5));
        }
        write_mkv(&mkv, &bufs, false);
        let d = decode(&mkv, None, |_| {}).unwrap();
        assert_eq!(d.media_start_ns, Some(1_500_000_000));
        let first = first_nonzero(&d.pcm, 0).unwrap();
        assert!(
            (first as i64 - 24_000).abs() <= 2,
            "first sample at {first}"
        );
        // The gap [2.5 s, 4.5 s) is silent and the second block starts at 4.5 s.
        assert!(d.pcm[40_000 + 10..72_000 - 10].iter().all(|&x| x == 0.0));
        let second = first_nonzero(&d.pcm, 40_100).unwrap();
        assert!(
            (second as i64 - 72_000).abs() <= 2,
            "second block at {second}"
        );
        assert!((d.pcm.len() as i64 - 88_000).abs() <= 2, "{}", d.pcm.len());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn decode_repeated_10ms_gaps_over_60s() {
        let dir = scratch("gaps");
        let mkv = dir.join("gaps.mkv");
        let mut bufs = Vec::new();
        let mut t = 0u64;
        for i in 0..2800 {
            // A marker (1.0) in the very last buffer, silence-level 0.2 elsewhere.
            let v = if i == 2799 { 1.0 } else { 0.2 };
            bufs.push((t, 320, v));
            t += 20_000_000;
            if i % 4 == 3 {
                t += 10_000_000;
            }
        }
        let last_pts = bufs.last().unwrap().0;
        write_mkv(&mkv, &bufs, false);
        let d = decode(&mkv, None, |_| {}).unwrap();
        let marker = d.pcm.iter().position(|&x| x > 0.9).unwrap();
        let want = (last_pts / 62_500) as i64;
        assert!(
            (marker as i64 - want).abs() <= 2,
            "final buffer at sample {marker}, expected {want}"
        );
        assert!(t > 60_000_000_000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn decode_opus_like_the_recorder() {
        let dir = scratch("opus");
        let mkv = dir.join("mic.mkv");
        let bufs: Vec<(u64, usize, f32)> = (0..100u64)
            .map(|i| (i * 20_000_000, 320, if i >= 50 { 0.5 } else { 0.0 }))
            .collect();
        write_mkv(&mkv, &bufs, true);
        let mut last = 0.0;
        let d = decode(&mkv, None, |p| last = p).unwrap();
        assert_eq!(last, 1.0);
        assert!((d.pcm.len() as i64 - 32_000).abs() < 400, "{}", d.pcm.len());
        let onset = first_nonzero(&d.pcm, 0).unwrap();
        assert!((onset as i64 - 16_000).abs() < 160, "onset {onset}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn decode_errors() {
        assert!(decode(Path::new("/nonexistent.mkv"), None, |_| {}).is_err());
        let dir = scratch("video");
        let mkv = dir.join("video.mkv");
        gst::init().unwrap();
        let p = gst::parse::launch(&format!(
            "videotestsrc num-buffers=10 ! video/x-raw,width=64,height=64 ! matroskamux ! filesink location={}",
            mkv.display()
        ))
        .unwrap();
        p.set_state(gst::State::Playing).unwrap();
        p.bus()
            .unwrap()
            .timed_pop_filtered(gst::ClockTime::from_seconds(10), &[gst::MessageType::Eos]);
        p.set_state(gst::State::Null).unwrap();
        let e = decode(&mkv, None, |_| {}).err().unwrap();
        assert!(e.contains("no audio"), "{e}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
