//! Encoder capacity plan (P0-1, P1-5): which encoder each video input gets and at what rate,
//! from the throughput measured at startup.
//!
//! Model: a frame of `w`×`h` costs `1 / fps_at(w, h)` seconds of its encoder (the single-stream
//! measurement; on the RX 7900 XT one stream saturates one VCN encode instance). Every VA encode
//! (H.264 and HEVC) shares the GPU's video engine; the kernel places streams on instances and
//! may put all of them on one, so the plan assumes the worst case and keeps the summed cost at
//! or under `GPU_BUDGET` of one second per second. Software encoders share `CPU_BUDGET`.
//!
//! Measured on this host (4K, ref-frames=1): vah264enc ~98 fps, vah265enc ~136 fps per stream;
//! two streams on one instance get ~54 / ~77 fps each. Over budget, the plan in order:
//! 1. moves H.264 inputs to HEVC (largest load first);
//! 2. moves the camera (system memory) to x264, isolating it from GPU bursts;
//! 3. lowers the cap of Native (variable rate) inputs step by step down to 30 fps;
//!
//! and reports `over` when that is still not enough (a CFR selection beyond capacity).
//! Native inputs are never planned on H.264 above 60 fps at more than 1080p (level 5.2, the
//! highest editors and hardware decoders reliably take); a CFR selection beyond it is warned
//! about. Every lowered Native rate is a pre-record warning (`Plan::warnings`).

use super::encoders::EncoderCap;
use super::types::{InputKey, Size, VideoCodec, VideoEncoder};

/// Share of one second of encode time per second the GPU encodes may use (worst case: one
/// instance). The single-stream measure is ~12% below an instance's real capacity.
pub const GPU_BUDGET: f64 = 0.9;
/// Share of the measured software throughput (all cores) the software encodes may use.
pub const CPU_BUDGET: f64 = 0.7;
/// Native caps never go below this.
pub const MIN_NATIVE_FPS: f64 = 30.0;
/// Native caps, highest first (a cap is the highest step at or below the source's refresh).
pub const NATIVE_STEPS: [f64; 10] = [
    240.0, 165.0, 144.0, 120.0, 100.0, 90.0, 75.0, 60.0, 50.0, 30.0,
];

/// What one video input needs.
#[derive(Clone, Debug, PartialEq)]
pub struct Demand {
    pub key: InputKey,
    pub label: String,
    /// Size it records at (locked).
    pub size: Size,
    /// CFR: the rate. Native: the source's maximum (its refresh).
    pub fps: f64,
    pub native: bool,
    /// Takes the GPU convert path (DMA-BUF): only encoders that path was probed with.
    pub gpu_path: bool,
    /// The camera (may move to a software encoder).
    pub camera: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Assigned {
    pub key: InputKey,
    pub enc: VideoEncoder,
    /// Largest size that encoder was verified at (the input is downscaled beyond it).
    pub max: Size,
    /// Planned rate (Native: the cap; equal to the demand when not lowered).
    pub fps: f64,
    pub native: bool,
    /// Seconds of encode time per second.
    pub load: f64,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Plan {
    pub inputs: Vec<Assigned>,
    pub gpu_load: f64,
    pub cpu_load: f64,
    pub over: bool,
    pub notes: Vec<String>,
    /// Shown before recording even when `over` is false: a Native rate the plan had to lower,
    /// or an H.264 rate beyond level 5.2.
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn get(&self, key: InputKey) -> Option<&Assigned> {
        self.inputs.iter().find(|a| a.key == key)
    }

    pub fn warning(&self) -> Option<String> {
        let over = self.over.then(|| {
            format!(
                "The selected inputs need about {:.0}% of the measured video encoder capacity{}: \
                 expect dropped or repeated frames. Use 30 fps for windows/screens, fewer \
                 sources, or a lower camera mode.",
                self.gpu_load / GPU_BUDGET * 100.0,
                if self.cpu_load > CPU_BUDGET {
                    format!(
                        " (software encoders {:.0}%)",
                        self.cpu_load / CPU_BUDGET * 100.0
                    )
                } else {
                    String::new()
                }
            )
        });
        let all: Vec<String> = over.into_iter().chain(self.warnings.clone()).collect();
        (!all.is_empty()).then(|| all.join(" "))
    }
}

fn cost(cap: &EncoderCap, size: Size) -> f64 {
    let fps = cap.fps_at(size);
    if fps > 0.0 { 1.0 / fps } else { 1.0 }
}

/// H.264 above 60 fps at more than 1080p needs level 6+.
fn level_ok(enc: VideoEncoder, size: Size, fps: f64) -> bool {
    enc.codec() != VideoCodec::H264
        || fps <= 60.5
        || size.width as i64 * size.height as i64 <= 1920 * 1088
}

/// Encoders an input may use, in table (preference) order.
fn allowed<'a>(
    d: &Demand,
    fps: f64,
    table: &'a [EncoderCap],
    gpu_ok: &[VideoEncoder],
) -> Vec<&'a EncoderCap> {
    let usable = |c: &&EncoderCap| !d.gpu_path || gpu_ok.contains(&c.enc);
    let fitting: Vec<&EncoderCap> = table
        .iter()
        .filter(usable)
        .filter(|c| c.fits(d.size))
        .collect();
    let pool = if fitting.is_empty() {
        // Nothing does this size: the largest one (the input is downscaled).
        let max = table
            .iter()
            .filter(usable)
            .map(|c| c.max.width as i64 * c.max.height as i64)
            .max();
        table
            .iter()
            .filter(usable)
            .filter(|c| Some(c.max.width as i64 * c.max.height as i64) == max)
            .collect()
    } else {
        fitting
    };
    let level: Vec<&EncoderCap> = pool
        .iter()
        .copied()
        .filter(|c| level_ok(c.enc, d.size, fps))
        .collect();
    if level.is_empty() { pool } else { level }
}

/// The next lower native step below `fps`.
fn step_down(fps: f64) -> Option<f64> {
    NATIVE_STEPS
        .iter()
        .copied()
        .find(|s| *s < fps - 0.5)
        .filter(|s| *s >= MIN_NATIVE_FPS)
}

struct Row<'a> {
    demand: Demand,
    encoder: &'a EncoderCap,
    fps: f64,
}

/// Plans every input; see the module docs.
pub fn plan(demands: &[Demand], table: &[EncoderCap], gpu_ok: &[VideoEncoder]) -> Plan {
    let mut out = Plan::default();
    let find = |e: VideoEncoder| table.iter().find(|c| c.enc == e);
    // Initial: the preferred (first) allowed encoder at the demanded rate.
    let mut rows: Vec<Row<'_>> = Vec::new();
    let mut initial: Vec<VideoEncoder> = Vec::new();
    for d in demands {
        if let Some(c) = allowed(d, d.fps, table, gpu_ok).first() {
            // Only H.264 can take it (no HEVC encoder): a Native rate stays within level 5.2.
            let fps = if d.native && !level_ok(c.enc, d.size, d.fps) {
                d.fps.min(60.0)
            } else {
                d.fps
            };
            rows.push(Row {
                demand: d.clone(),
                encoder: c,
                fps,
            });
            initial.push(c.enc);
        }
    }
    let load = |rows: &[Row<'_>], gpu: bool| -> f64 {
        rows.iter()
            .filter(|row| row.encoder.enc.is_gpu() == gpu)
            .map(|row| row.fps * cost(row.encoder, row.demand.size))
            .sum()
    };
    for _ in 0..64 {
        let gpu = load(&rows, true);
        let cpu = load(&rows, false);
        if gpu <= GPU_BUDGET && cpu <= CPU_BUDGET {
            break;
        }
        if gpu > GPU_BUDGET {
            // 1. H.264 → HEVC, largest load first.
            let hevc = find(VideoEncoder::VaHevc);
            let pick = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| {
                    row.encoder.enc == VideoEncoder::Va
                        && hevc.is_some_and(|h| {
                            allowed(&row.demand, row.fps, table, gpu_ok)
                                .iter()
                                .any(|a| a.enc == h.enc)
                        })
                })
                .max_by(|(_, a), (_, b)| {
                    let la = a.fps * cost(a.encoder, a.demand.size);
                    let lb = b.fps * cost(b.encoder, b.demand.size);
                    la.total_cmp(&lb)
                })
                .map(|(i, _)| i);
            if let (Some(i), Some(h)) = (pick, hevc) {
                rows[i].encoder = h;
                continue;
            }
            // 2. The camera to a software encoder with room.
            let moved = rows.iter().position(|row| {
                row.demand.camera
                    && row.encoder.enc.is_gpu()
                    && allowed(&row.demand, row.fps, table, gpu_ok)
                        .iter()
                        .any(|a| {
                            !a.enc.is_gpu()
                                && cpu + row.fps * cost(a, row.demand.size) <= CPU_BUDGET
                        })
            });
            if let Some(i) = moved {
                let row = &rows[i];
                let sw = allowed(&row.demand, row.fps, table, gpu_ok)
                    .into_iter()
                    .find(|a| !a.enc.is_gpu())
                    .expect("checked above");
                rows[i].encoder = sw;
                continue;
            }
            // 3. Lower the highest native cap one step.
            let native = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.demand.native && step_down(row.fps).is_some())
                .max_by(|(_, a), (_, b)| a.fps.total_cmp(&b.fps))
                .map(|(i, _)| i);
            if let Some(i) = native {
                let next = step_down(rows[i].fps).expect("filtered");
                rows[i].fps = next;
                // A lower rate may make H.264 legal again; keep the codec (it was moved for load).
                continue;
            }
        }
        break;
    }
    for (row, first) in rows.iter().zip(&initial) {
        let d = &row.demand;
        let c = row.encoder;
        let f = row.fps;
        if c.enc != *first {
            out.notes.push(format!(
                "{}: {} instead of {} to fit the measured encoder capacity",
                d.label,
                c.enc.element_name(),
                first.element_name()
            ));
        }
        if d.native && f < d.fps - 0.5 {
            let why = if !level_ok(c.enc, d.size, d.fps) && f >= 59.5 {
                "H.264 level 5.2; no HEVC encoder is available"
            } else {
                "fit the measured encoder capacity"
            };
            out.warnings.push(format!(
                "{}: Native frame rate limited to {:.0} fps (source refresh {:.0}) to {why}.",
                d.label, f, d.fps
            ));
        }
        if !level_ok(c.enc, d.size, f) {
            out.warnings.push(format!(
                "{}: {:.0} fps at {}×{} on H.264 needs level 6, which many editors and decoders \
                 reject; choose 60 fps or less.",
                d.label, f, d.size.width, d.size.height
            ));
        }
    }
    out.gpu_load = load(&rows, true);
    out.cpu_load = load(&rows, false);
    out.over = out.gpu_load > GPU_BUDGET + 1e-9 || out.cpu_load > CPU_BUDGET + 1e-9;
    out.inputs = rows
        .into_iter()
        .map(|row| Assigned {
            key: row.demand.key,
            enc: row.encoder.enc,
            max: row.encoder.max,
            fps: row.fps,
            native: row.demand.native,
            load: row.fps * cost(row.encoder, row.demand.size),
        })
        .collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio::types::SourceId;

    const UHD: Size = Size {
        width: 3840,
        height: 2160,
    };

    fn cap(enc: VideoEncoder, fps: f64) -> EncoderCap {
        EncoderCap {
            enc,
            max: Size {
                width: 4096,
                height: 2160,
            },
            fps_at_max: fps,
        }
    }

    /// This host: VA H.264 98, VA HEVC 136, x264 118 (all cores) at 4K.
    fn host() -> Vec<EncoderCap> {
        vec![
            cap(VideoEncoder::Va, 98.0),
            cap(VideoEncoder::VaHevc, 136.0),
            cap(VideoEncoder::X264, 118.0),
        ]
    }

    const GPU_OK: [VideoEncoder; 3] = [VideoEncoder::Va, VideoEncoder::VaHevc, VideoEncoder::X264];

    fn camera() -> Demand {
        Demand {
            key: InputKey::Camera,
            label: "Camera".into(),
            size: UHD,
            fps: 25.0,
            native: false,
            gpu_path: false,
            camera: true,
        }
    }

    fn window(id: u32, fps: f64, native: bool) -> Demand {
        Demand {
            key: InputKey::Source(SourceId(id)),
            label: format!("Window {id}"),
            size: UHD,
            fps,
            native,
            gpu_path: true,
            camera: false,
        }
    }

    #[test]
    fn camera_and_two_4k30_windows_fit_on_h264() {
        let p = plan(
            &[camera(), window(1, 30.0, false), window(2, 30.0, false)],
            &host(),
            &GPU_OK,
        );
        assert!(!p.over, "{p:?}");
        assert!(p.inputs.iter().all(|a| a.enc == VideoEncoder::Va), "{p:?}");
        assert!((p.gpu_load - 85.0 / 98.0).abs() < 1e-6);
    }

    #[test]
    fn the_old_default_ref_frames_did_not_fit() {
        // ref-frames=3: ~46 fps per stream. The plan moves load off H.264 instead of
        // overloading it (the 2026-10-22 camera repeats).
        let mut t = host();
        t[0].fps_at_max = 46.0;
        let p = plan(
            &[camera(), window(1, 30.0, false), window(2, 30.0, false)],
            &t,
            &GPU_OK,
        );
        assert!(!p.over, "{p:?}");
        assert!(
            p.inputs.iter().any(|a| a.enc == VideoEncoder::VaHevc),
            "{p:?}"
        );
    }

    #[test]
    fn native_144_windows_go_hevc_camera_cpu_and_caps_drop() {
        let p = plan(
            &[camera(), window(1, 144.0, true), window(2, 144.0, true)],
            &host(),
            &GPU_OK,
        );
        assert!(!p.over, "{p:?}");
        let w1 = p.get(InputKey::Source(SourceId(1))).unwrap();
        // H.264 is never planned at 4K above 60 fps.
        assert_eq!(w1.enc, VideoEncoder::VaHevc);
        assert_eq!(p.get(InputKey::Camera).unwrap().enc, VideoEncoder::X264);
        assert!(w1.fps <= 60.0 && w1.fps >= 30.0, "{w1:?}");
        assert!(p.gpu_load <= GPU_BUDGET);
        assert!(
            p.warnings.iter().any(|n| n.contains("limited")),
            "{:?}",
            p.warnings
        );
        // Shown before recording although the plan fits.
        assert!(p.warning().unwrap().contains("limited"));
    }

    #[test]
    fn one_native_window_keeps_its_refresh() {
        let p = plan(&[window(1, 120.0, true)], &host(), &GPU_OK);
        let w = &p.inputs[0];
        assert_eq!((w.enc, w.fps), (VideoEncoder::VaHevc, 120.0));
        assert!(!p.over);
    }

    #[test]
    fn cfr_beyond_capacity_is_reported() {
        let ws: Vec<Demand> = (1..=4).map(|i| window(i, 60.0, false)).collect();
        let p = plan(&ws, &host(), &GPU_OK);
        assert!(p.over, "{p:?}");
        assert!(p.warning().unwrap().contains('%'));
        // DMA-BUF inputs only get encoders the GPU path was probed with.
        let p = plan(&[window(1, 30.0, false)], &host(), &[VideoEncoder::X264]);
        assert_eq!(p.inputs[0].enc, VideoEncoder::X264);
    }

    #[test]
    fn small_inputs_and_missing_encoders() {
        let mut w = window(1, 30.0, false);
        w.size = Size {
            width: 1920,
            height: 1080,
        };
        let p = plan(&[w], &[cap(VideoEncoder::Va, 98.0)], &GPU_OK);
        assert!(
            (p.inputs[0].load - 30.0 / (98.0 * 4.0)).abs() < 1e-3,
            "{p:?}"
        );
        assert!(plan(&[camera()], &[], &GPU_OK).inputs.is_empty());
        // Only a 1080p encoder: the 4K input is planned on it (downscaled).
        let small = EncoderCap {
            enc: VideoEncoder::Va,
            max: Size {
                width: 1920,
                height: 1080,
            },
            fps_at_max: 200.0,
        };
        let p = plan(&[camera()], &[small], &GPU_OK);
        assert_eq!(
            p.inputs[0].max,
            Size {
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn h264_only_keeps_level_5_2() {
        let t = [cap(VideoEncoder::Va, 98.0)];
        let p = plan(&[window(1, 144.0, true)], &t, &GPU_OK);
        assert_eq!((p.inputs[0].enc, p.inputs[0].fps), (VideoEncoder::Va, 60.0));
        assert!(p.warning().unwrap().contains("level 5.2"), "{p:?}");
        // CFR beyond level 5.2 is warned about, not silently planned.
        let p = plan(&[window(1, 120.0, false)], &t, &GPU_OK);
        assert!(p.warning().unwrap().contains("level 6"), "{p:?}");
        // A plan that fits without lowering anything warns about nothing.
        let p = plan(&[window(1, 30.0, false)], &host(), &GPU_OK);
        assert_eq!(p.warning(), None);
    }
}
