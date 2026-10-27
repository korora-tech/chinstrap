//! Crash recovery for incomplete Matroska files and their atomic session manifest.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_pbutils as pbutils;

use super::session::{self, ManifestInput};
use super::storage;

#[derive(Clone, Debug, Default)]
pub struct RecoveryResult {
    pub recovered_files: u32,
    pub failed_files: u32,
    pub mic_files: Vec<(u32, PathBuf)>,
}

fn active(status: &str) -> bool {
    matches!(status, "starting" | "recording" | "finalizing")
}

fn retryable_failure(input: &ManifestInput) -> bool {
    input.status == "failed"
        && input.error.as_deref().is_some_and(|error| {
            error.starts_with("recovery failed:") || error.starts_with("not finalized:")
        })
}

/// Remuxes active files without re-encoding, verifies each output, and atomically repairs the
/// manifest. The original file is retained as `.orig` until the replacement passes Discoverer.
pub fn recover_session(session_dir: &Path) -> Result<RecoveryResult, String> {
    gst::init().map_err(|e| format!("GStreamer initialization failed: {e}"))?;
    let _lock = session::SessionLock::acquire(session_dir)
        .map_err(|e| format!("session is locked or cannot be locked: {e}"))?;
    storage::release_emergency_reserve(&session_dir.join(".chinstrap-reserve"))
        .map_err(|e| format!("cannot release emergency stop reserve: {e}"))?;
    let manifest_path = session_dir.join(session::MANIFEST_NAME);
    let mut manifest = session::read_manifest(&manifest_path)
        .map_err(|e| format!("cannot read interrupted session manifest: {e}"))?;
    let mut result = RecoveryResult::default();
    let mut changed = false;

    for segment in &mut manifest.segments {
        let segment_active = active(&segment.status)
            || segment
                .inputs
                .iter()
                .any(|input| active(&input.status) || retryable_failure(input));
        if !segment_active {
            continue;
        }
        let mut max_duration_ns = 0u64;
        let expected_dir = session::segment_dir_name(segment.index);
        let segment_path = if segment.dir == expected_dir {
            session_dir.join(&segment.dir)
        } else {
            PathBuf::new()
        };
        for input in &mut segment.inputs {
            if !active(&input.status) && !retryable_failure(input) {
                continue;
            }
            let file_path = safe_input_path(&segment_path, input);
            let recovered = file_path.and_then(|path| recover_file(&path));
            match recovered {
                Ok(duration_ns) => {
                    input.status = "complete".into();
                    input.error = None;
                    input.last_pts_ns = Some(duration_ns);
                    max_duration_ns = max_duration_ns.max(duration_ns);
                    result.recovered_files += 1;
                    if input.key == "mic" || input.kind == "microphone" {
                        result
                            .mic_files
                            .push((segment.index, segment_path.join(&input.file)));
                    }
                }
                Err(error) => {
                    input.status = "failed".into();
                    input.error = Some(format!("recovery failed: {error}"));
                    result.failed_files += 1;
                }
            }
        }
        segment.status = "incomplete".into();
        segment.reason = Some("recovered after interruption".into());
        if max_duration_ns > 0 {
            let base = segment.base_time_ns.unwrap_or(0);
            segment.base_time_ns = Some(base);
            segment.duration_ns = Some(max_duration_ns);
            segment.stop_clock_ns = Some(base.saturating_add(max_duration_ns));
        }
        changed = true;
    }
    if changed {
        session::write_manifest(session_dir, &manifest)
            .map_err(|e| format!("cannot write recovered session manifest: {e}"))?;
    }
    Ok(result)
}

fn safe_input_path(segment_dir: &Path, input: &ManifestInput) -> Result<PathBuf, String> {
    let relative = Path::new(&input.file);
    if relative.components().count() != 1
        || relative.file_name().is_none()
        || relative.file_name().unwrap().as_bytes() != input.file.as_bytes()
        || relative.extension().is_none_or(|ext| ext != "mkv")
        || segment_dir.as_os_str().is_empty()
    {
        return Err("invalid manifest path".into());
    }
    Ok(segment_dir.join(relative))
}

fn unique_sibling(path: &Path, label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.{label}-{}-{nonce}", std::process::id()))
}

fn discover_duration(path: &Path) -> Result<u64, String> {
    let discoverer = pbutils::Discoverer::new(gst::ClockTime::from_seconds(30))
        .map_err(|e| format!("cannot create Discoverer: {e}"))?;
    let uri = gst::glib::filename_to_uri(path, None).map_err(|e| e.to_string())?;
    let info = discoverer
        .discover_uri(&uri)
        .map_err(|e| format!("Discoverer rejected the file: {e}"))?;
    let streams = info.video_streams().len() + info.audio_streams().len();
    if streams == 0 {
        return Err("no playable audio or video stream found".into());
    }
    info.duration()
        .filter(|duration| duration.nseconds() > 0)
        .map(|duration| duration.nseconds())
        .ok_or_else(|| "Discoverer found no duration".into())
}

#[derive(Clone, Copy)]
struct EbmlElement {
    id: u64,
    data_start: u64,
    data_end: u64,
    unknown_size: bool,
}

fn read_vint(file: &mut File, max_len: usize, keep_marker: bool) -> io::Result<(u64, usize)> {
    let mut bytes = [0u8; 8];
    file.read_exact(&mut bytes[..1])?;
    let length = bytes[0].leading_zeros() as usize + 1;
    if length > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid EBML vint length",
        ));
    }
    file.read_exact(&mut bytes[1..length])?;
    let raw = bytes[..length]
        .iter()
        .fold(0u64, |value, byte| (value << 8) | *byte as u64);
    let value = if keep_marker {
        raw
    } else {
        raw & (u64::MAX >> (64 - 7 * length))
    };
    Ok((value, length))
}

fn read_element(file: &mut File, parent_end: u64) -> io::Result<Option<EbmlElement>> {
    let offset = file.stream_position()?;
    if offset == parent_end {
        return Ok(None);
    }
    if offset > parent_end {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "EBML child exceeds parent",
        ));
    }
    let (id, _) = read_vint(file, 4, true)?;
    let (size, size_len) = read_vint(file, 8, false)?;
    let size_mask = u64::MAX >> (64 - 7 * size_len);
    let unknown_size = size == size_mask;
    let data_start = file.stream_position()?;
    let data_end = if unknown_size {
        parent_end
    } else {
        let end = data_start.checked_add(size).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "EBML element size overflow")
        })?;
        if end > parent_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "EBML child exceeds parent",
            ));
        }
        end
    };
    Ok(Some(EbmlElement {
        id,
        data_start,
        data_end,
        unknown_size,
    }))
}

fn has_cue_point(file: &mut File, cues: EbmlElement) -> io::Result<bool> {
    if cues.unknown_size {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(cues.data_start))?;
    while let Some(point) = read_element(file, cues.data_end)? {
        if point.id == 0xbb && point.data_start < point.data_end {
            let mut has_time = false;
            let mut has_track = false;
            let mut has_cluster_position = false;
            file.seek(SeekFrom::Start(point.data_start))?;
            while let Some(field) = read_element(file, point.data_end)? {
                match field.id {
                    0xb3 => has_time = field.data_start < field.data_end,
                    0xb7 => {
                        file.seek(SeekFrom::Start(field.data_start))?;
                        while let Some(position) = read_element(file, field.data_end)? {
                            match position.id {
                                0xf7 => has_track = position.data_start < position.data_end,
                                0xf1 => {
                                    has_cluster_position = position.data_start < position.data_end
                                }
                                _ => {}
                            }
                            if position.unknown_size {
                                break;
                            }
                            file.seek(SeekFrom::Start(position.data_end))?;
                        }
                    }
                    _ => {}
                }
                if field.unknown_size {
                    break;
                }
                file.seek(SeekFrom::Start(field.data_end))?;
            }
            if has_time && has_track && has_cluster_position {
                return Ok(true);
            }
        }
        if point.unknown_size {
            break;
        }
        file.seek(SeekFrom::Start(point.data_end))?;
    }
    Ok(false)
}

fn info_has_duration(file: &mut File, info: EbmlElement) -> io::Result<bool> {
    file.seek(SeekFrom::Start(info.data_start))?;
    while let Some(field) = read_element(file, info.data_end)? {
        if field.id == 0x4489 {
            let length = field.data_end.saturating_sub(field.data_start);
            file.seek(SeekFrom::Start(field.data_start))?;
            let duration = match length {
                4 => {
                    let mut raw = [0u8; 4];
                    file.read_exact(&mut raw)?;
                    f32::from_be_bytes(raw) as f64
                }
                8 => {
                    let mut raw = [0u8; 8];
                    file.read_exact(&mut raw)?;
                    f64::from_be_bytes(raw)
                }
                _ => return Ok(false),
            };
            return Ok(duration.is_finite() && duration > 0.0);
        }
        if field.unknown_size {
            break;
        }
        file.seek(SeekFrom::Start(field.data_end))?;
    }
    Ok(false)
}

/// Discoverer can infer duration from clusters; require the actual Matroska Cues and Duration.
pub(crate) fn verify_matroska_index(path: &Path) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open Matroska file: {e}"))?;
    let file_len = file
        .metadata()
        .map_err(|e| format!("cannot stat Matroska file: {e}"))?
        .len();
    let mut root_position = 0;
    let mut segment = None;
    while root_position < file_len {
        file.seek(SeekFrom::Start(root_position))
            .map_err(|e| format!("cannot seek Matroska file: {e}"))?;
        let element = read_element(&mut file, file_len)
            .map_err(|e| format!("invalid EBML header: {e}"))?
            .ok_or_else(|| "missing Matroska Segment".to_string())?;
        if element.id == 0x18538067 {
            segment = Some(element);
            break;
        }
        if element.unknown_size {
            break;
        }
        root_position = element.data_end;
    }
    let segment = segment.ok_or_else(|| "missing Matroska Segment".to_string())?;
    let mut has_cues = false;
    let mut has_duration = false;
    file.seek(SeekFrom::Start(segment.data_start))
        .map_err(|e| format!("cannot seek Matroska Segment: {e}"))?;
    while let Some(element) = read_element(&mut file, segment.data_end)
        .map_err(|e| format!("invalid Matroska Segment: {e}"))?
    {
        match element.id {
            0x1549a966 => {
                has_duration |= info_has_duration(&mut file, element)
                    .map_err(|e| format!("cannot read Matroska Info: {e}"))?;
            }
            0x1c53bb6b => {
                has_cues |= has_cue_point(&mut file, element)
                    .map_err(|e| format!("cannot read Matroska Cues: {e}"))?;
            }
            _ => {}
        }
        if element.unknown_size {
            break;
        }
        file.seek(SeekFrom::Start(element.data_end))
            .map_err(|e| format!("cannot seek Matroska element: {e}"))?;
    }
    if !has_cues {
        return Err("Matroska Cues index is missing or invalid".into());
    }
    if !has_duration {
        return Err("Matroska Duration is missing or zero".into());
    }
    Ok(())
}

pub(crate) fn verify_recovered_file(path: &Path) -> Result<u64, String> {
    let duration = discover_duration(path)?;
    verify_matroska_index(path)?;
    Ok(duration)
}

fn recover_file(path: &Path) -> Result<u64, String> {
    recover_file_with_sync(path, &sync_parent)
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    File::open(parent)?.sync_all()
}

fn sync_file(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn recover_file_with_sync(
    path: &Path,
    sync_directory: &impl Fn(&Path) -> io::Result<()>,
) -> Result<u64, String> {
    let backup = path.with_file_name(format!(
        "{}.orig",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    if path.exists()
        && let Ok(duration) = verify_recovered_file(path)
    {
        sync_file(path).map_err(|e| format!("cannot sync verified recovery file: {e}"))?;
        sync_directory(path).map_err(|e| format!("cannot sync recovered file directory: {e}"))?;
        if backup.exists() {
            fs::remove_file(&backup)
                .map_err(|e| format!("cannot remove verified original backup: {e}"))?;
            sync_directory(path)
                .map_err(|e| format!("cannot sync original-backup removal: {e}"))?;
        }
        return Ok(duration);
    }
    let source = if backup.exists() {
        backup.clone()
    } else if path.exists() {
        path.to_path_buf()
    } else {
        return Err("recording file is missing".into());
    };
    sync_file(&source).map_err(|e| format!("cannot sync original recovery source: {e}"))?;
    sync_directory(path).map_err(|e| format!("cannot sync original recovery directory: {e}"))?;
    let temp = unique_sibling(path, "recovering");
    remux_matroska(&source, &temp)?;
    let duration = match verify_recovered_file(&temp) {
        Ok(duration) => duration,
        Err(error) => {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
    };
    sync_file(&temp).map_err(|e| format!("cannot sync remuxed recovery file: {e}"))?;

    if path.exists() {
        let destination = if backup.exists() {
            unique_sibling(path, "previous")
        } else {
            backup.clone()
        };
        fs::rename(path, &destination)
            .map_err(|e| format!("cannot preserve the original file: {e}"))?;
        sync_directory(path)
            .map_err(|e| format!("cannot sync preserved original directory entry: {e}"))?;
    }
    fs::rename(&temp, path).map_err(|e| format!("cannot install verified recovery file: {e}"))?;
    sync_directory(path)
        .map_err(|e| format!("cannot sync installed recovery directory entry: {e}"))?;
    let installed_duration =
        verify_recovered_file(path).map_err(|e| format!("replacement verification failed: {e}"))?;
    sync_file(path).map_err(|e| format!("cannot sync verified replacement: {e}"))?;
    sync_directory(path).map_err(|e| format!("cannot sync verified replacement directory: {e}"))?;

    if backup.exists() {
        fs::remove_file(&backup)
            .map_err(|e| format!("cannot remove verified original backup: {e}"))?;
        sync_directory(path).map_err(|e| format!("cannot sync original-backup removal: {e}"))?;
    }
    Ok(installed_duration.max(duration))
}

fn remux_matroska(source: &Path, destination: &Path) -> Result<(), String> {
    let pipeline = gst::Pipeline::new();
    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", source.to_string_lossy().to_string())
        .build()
        .map_err(|e| format!("cannot create recovery filesrc: {e}"))?;
    let demux = gst::ElementFactory::make("matroskademux")
        .build()
        .map_err(|e| format!("matroskademux unavailable: {e}"))?;
    let mux = gst::ElementFactory::make("matroskamux")
        .property("writing-app", "Chinstrap recovery")
        .property("max-cluster-duration", 2_000_000_000i64)
        .build()
        .map_err(|e| format!("matroskamux unavailable: {e}"))?;
    let sink = gst::ElementFactory::make("filesink")
        .property("location", destination.to_string_lossy().to_string())
        .property("o-sync", true)
        .build()
        .map_err(|e| format!("cannot create recovery filesink: {e}"))?;
    sink.set_property_from_str("buffer-mode", "unbuffered");
    pipeline
        .add_many([&filesrc, &demux, &mux, &sink])
        .map_err(|e| format!("cannot assemble recovery pipeline: {e}"))?;
    filesrc
        .link(&demux)
        .map_err(|e| format!("cannot link recovery filesrc: {e}"))?;
    mux.link(&sink)
        .map_err(|e| format!("cannot link recovery filesink: {e}"))?;
    let link_error = Arc::new(Mutex::new(None::<String>));
    {
        let mux = mux.clone();
        let link_error = link_error.clone();
        demux.connect_pad_added(move |_, source_pad| {
            let media = source_pad
                .current_caps()
                .unwrap_or_else(|| source_pad.query_caps(None))
                .structure(0)
                .map(|structure| structure.name().to_string());
            let Some(media) = media else {
                return;
            };
            let template = if media.starts_with("video/") {
                "video_%u"
            } else if media.starts_with("audio/") {
                "audio_%u"
            } else {
                return;
            };
            let Some(mux_pad) = mux.request_pad_simple(template) else {
                *link_error.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(format!("matroskamux has no request pad for {media}"));
                return;
            };
            if let Err(error) = source_pad.link(&mux_pad) {
                *link_error.lock().unwrap_or_else(|p| p.into_inner()) =
                    Some(format!("cannot link demuxed {media} stream: {error}"));
            }
        });
    }
    let bus = pipeline.bus().ok_or("recovery pipeline has no bus")?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| format!("cannot start recovery pipeline: {e}"))?;
    let message = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(300),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    pipeline.set_state(gst::State::Null).ok();
    if let Some(error) = link_error.lock().unwrap_or_else(|p| p.into_inner()).take() {
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    match message {
        Some(message) => match message.view() {
            gst::MessageView::Eos(_) => Ok(()),
            gst::MessageView::Error(error) => {
                let _ = fs::remove_file(destination);
                Err(format!("GStreamer remux failed: {}", error.error()))
            }
            _ => {
                let _ = fs::remove_file(destination);
                Err("unexpected GStreamer message during remux".into())
            }
        },
        None => {
            let _ = fs::remove_file(destination);
            Err("GStreamer remux timed out".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::studio::session::{
        self, Manifest, ManifestInput, ManifestSegment, segment_dir_name,
    };
    use gstreamer as gst;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn scratch() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "chinstrap-recovery-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn recording_manifest(name: &str) -> Manifest {
        let mut manifest = Manifest::new(name, session::wall_now());
        manifest.segments.push(ManifestSegment {
            index: 1,
            dir: segment_dir_name(1),
            status: "recording".into(),
            reason: None,
            base_time_ns: Some(0),
            stop_clock_ns: None,
            duration_ns: None,
            gap_before_ns: None,
            started_wall: Some(session::wall_now()),
            inputs: vec![ManifestInput {
                key: "source:1".into(),
                kind: "test".into(),
                label: "Test pattern".into(),
                file: "video.mkv".into(),
                codec: "h264".into(),
                encoder: Some("x264enc".into()),
                convert: None,
                capture: None,
                width: Some(320),
                height: Some(240),
                framerate: Some("30/1".into()),
                max_framerate: None,
                first_pts_ns: Some(0),
                last_pts_ns: Some(0),
                status: "recording".into(),
                error: None,
                device: None,
                transcript: None,
                quality: None,
            }],
        });
        manifest
    }

    fn ebml_element(
        bytes: &[u8],
        offset: usize,
        limit: usize,
    ) -> Option<(u64, usize, usize, bool)> {
        let first = *bytes.get(offset)?;
        let id_len = first.leading_zeros() as usize + 1;
        if id_len > 4 || offset + id_len > limit {
            return None;
        }
        let id = u64::from_be_bytes({
            let mut raw = [0u8; 8];
            raw[8 - id_len..].copy_from_slice(&bytes[offset..offset + id_len]);
            raw
        });
        let size_at = offset + id_len;
        let size_first = *bytes.get(size_at)?;
        let size_len = size_first.leading_zeros() as usize + 1;
        if size_len > 8 || size_at + size_len > limit {
            return None;
        }
        let raw_size = u64::from_be_bytes({
            let mut raw = [0u8; 8];
            raw[8 - size_len..].copy_from_slice(&bytes[size_at..size_at + size_len]);
            raw
        });
        let size_mask = u64::MAX >> (64 - 7 * size_len);
        let size = raw_size & size_mask;
        let unknown = size == size_mask;
        let data_start = size_at + size_len;
        let data_end = if unknown {
            limit
        } else {
            data_start
                .checked_add(size as usize)
                .filter(|end| *end <= limit)?
        };
        Some((id, data_start, data_end, unknown))
    }

    fn matroska_has_cues_and_nonzero_duration(bytes: &[u8]) -> bool {
        let Some((ebml_id, _, ebml_end, _)) = ebml_element(bytes, 0, bytes.len()) else {
            return false;
        };
        if ebml_id != 0x1a45dfa3 {
            return false;
        }
        let Some((segment_id, segment_start, segment_end, _)) =
            ebml_element(bytes, ebml_end, bytes.len())
        else {
            return false;
        };
        if segment_id != 0x18538067 {
            return false;
        }
        let mut has_cues = false;
        let mut nonzero_duration = false;
        let mut offset = segment_start;
        while offset < segment_end {
            let Some((id, start, end, unknown)) = ebml_element(bytes, offset, segment_end) else {
                return false;
            };
            if id == 0x1c53bb6b && start < end {
                has_cues = true;
            } else if id == 0x1549a966 {
                let mut info_offset = start;
                while info_offset < end {
                    let Some((info_id, info_start, info_end, info_unknown)) =
                        ebml_element(bytes, info_offset, end)
                    else {
                        return false;
                    };
                    if info_id == 0x4489 {
                        let duration = match info_end - info_start {
                            4 => {
                                f32::from_be_bytes(bytes[info_start..info_end].try_into().unwrap())
                                    as f64
                            }
                            8 => {
                                f64::from_be_bytes(bytes[info_start..info_end].try_into().unwrap())
                            }
                            _ => 0.0,
                        };
                        nonzero_duration = duration.is_finite() && duration > 0.0;
                    }
                    if info_unknown {
                        break;
                    }
                    info_offset = info_end;
                }
            }
            if unknown {
                break;
            }
            offset = end;
        }
        has_cues && nonzero_duration
    }

    fn finalized_video_fixture(path: &Path) {
        gst::init().unwrap();
        let command = format!(
            "videotestsrc num-buffers=60 ! video/x-raw,format=I420,width=320,height=240,framerate=30/1 ! openh264enc gop-size=60 ! h264parse ! matroskamux ! filesink location=\"{}\"",
            path.display()
        );
        let pipeline = gst::parse::launch(&command)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let bus = pipeline.bus().unwrap();
        let message = bus
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(15),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .expect("finalized fixture pipeline EOS");
        assert!(matches!(message.view(), gst::MessageView::Eos(_)));
        pipeline.set_state(gst::State::Null).unwrap();
    }

    fn decoded_to_eos(path: &Path) -> Result<(), String> {
        let command = format!(
            "filesrc location=\"{}\" ! decodebin ! fakesink sync=false",
            path.display()
        );
        let pipeline = gst::parse::launch(&command)
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "decode check is not a pipeline")?;
        let bus = pipeline.bus().ok_or("decode check has no bus")?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| e.to_string())?;
        let message = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        let _ = pipeline.set_state(gst::State::Null);
        match message {
            Some(message) => match message.view() {
                gst::MessageView::Eos(_) => Ok(()),
                gst::MessageView::Error(_) => {
                    Err("GStreamer decoder rejected recovered file".into())
                }
                _ => Err("unexpected GStreamer decode result".into()),
            },
            None => Err("recovered file did not decode to EOS".into()),
        }
    }

    struct ChildGuard(std::process::Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn interrupted_fixture_child() {
        let Some(path) = std::env::var_os("CHINSTRAP_INTERRUPTED_FIXTURE") else {
            return;
        };
        gst::init().unwrap();
        let path = PathBuf::from(path);
        let command = format!(
            "videotestsrc is-live=true pattern=smpte ! video/x-raw,format=I420,width=320,height=240,framerate=30/1 ! openh264enc gop-size=30 ! h264parse ! matroskamux max-cluster-duration=2000000000 ! filesink location=\"{}\" buffer-mode=unbuffered o-sync=true",
            path.display()
        );
        let element = gst::parse::launch(&command).unwrap();
        let pipeline = element.downcast::<gst::Pipeline>().unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let ready = path.with_extension("ready");
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if fs::metadata(&path).is_ok_and(|meta| meta.len() > 64 * 1024) {
                fs::write(ready, b"ready").unwrap();
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("fixture did not write any Matroska clusters");
    }

    #[test]
    fn recovery_candidate_is_scannable_again_after_a_retryable_file_failure() {
        let root = scratch();
        let session_dir = root.join("interrupted");
        let segment_dir = session_dir.join(segment_dir_name(1));
        fs::create_dir_all(&segment_dir).unwrap();
        let mut manifest = recording_manifest("interrupted");
        manifest.segments[0].status = "incomplete".into();
        manifest.segments[0].inputs[0].status = "failed".into();
        manifest.segments[0].inputs[0].error =
            Some("recovery failed: temporary remux error".into());
        session::write_manifest(&session_dir, &manifest).unwrap();

        let candidates = session::scan_interrupted(&root).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].segments, vec![1]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finalization_timeout_is_retryable_for_recovery() {
        let mut input = recording_manifest("interrupted").segments[0].inputs[0].clone();
        input.status = "failed".into();
        input.error = Some("not finalized: file may lack index".into());
        assert!(retryable_failure(&input));
    }

    #[test]
    fn failed_parent_fsync_never_removes_the_original_backup() {
        let root = scratch();
        let path = root.join("video.mkv");
        let backup = root.join("video.mkv.orig");
        finalized_video_fixture(&path);
        fs::copy(&path, &backup).unwrap();
        let original = fs::read(&backup).unwrap();
        let fail_sync = |_: &Path| Err(io::Error::other("injected directory fsync failure"));

        let result = recover_file_with_sync(&path, &fail_sync);
        assert!(result.is_err(), "directory fsync failure must be reported");
        assert_eq!(fs::read(&backup).unwrap(), original);
        assert!(path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn killed_interrupted_matroska_is_remuxed_with_cues_and_manifest_repaired() {
        let root = scratch();
        let session_dir = root.join("2026-10-27T10-00-00");
        let segment_dir = session_dir.join(segment_dir_name(1));
        fs::create_dir_all(&segment_dir).unwrap();
        let video = segment_dir.join("video.mkv");

        let child_started = std::time::Instant::now();
        let mut child = ChildGuard(
            std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("studio::recovery::tests::interrupted_fixture_child")
                .arg("--nocapture")
                .env("CHINSTRAP_INTERRUPTED_FIXTURE", &video)
                .spawn()
                .unwrap(),
        );
        let ready = video.with_extension("ready");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !ready.exists() && std::time::Instant::now() < deadline {
            if let Some(status) = child.0.try_wait().unwrap() {
                panic!("interrupted fixture child exited early: {status}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            ready.exists(),
            "child did not start writing an interrupted file"
        );
        fs::remove_file(&ready).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let killed_file = fs::read(&video).unwrap();
        assert!(
            !matroska_has_cues_and_nonzero_duration(&killed_file),
            "the killed fixture unexpectedly has a durable index and duration"
        );
        child.0.kill().unwrap();
        let status = child.0.wait().unwrap();
        assert!(
            !status.success(),
            "fixture process should be killed without EOS"
        );
        let elapsed_at_kill_ns = child_started.elapsed().as_nanos() as u64;

        session::write_manifest(&session_dir, &recording_manifest("2026-10-27T10-00-00")).unwrap();
        let result = recover_session(&session_dir).unwrap();
        let manifest = session::read_manifest(&session_dir.join(session::MANIFEST_NAME)).unwrap();
        let recovered = fs::read(&video).unwrap();

        assert_eq!(result.recovered_files, 1);
        assert_eq!(manifest.segments[0].status, "incomplete");
        assert_eq!(
            manifest.segments[0].reason.as_deref(),
            Some("recovered after interruption")
        );
        assert_eq!(manifest.segments[0].inputs[0].status, "complete");
        assert!(manifest.segments[0].duration_ns.unwrap() > 0);
        assert!(
            manifest.segments[0]
                .duration_ns
                .unwrap()
                .abs_diff(elapsed_at_kill_ns)
                <= 2_000_000_000,
            "recovered duration is not within two seconds of the process kill"
        );
        assert!(manifest.segments[0].inputs[0].last_pts_ns.unwrap() > 0);
        assert!(
            matroska_has_cues_and_nonzero_duration(&recovered),
            "recovered file lacks Matroska Cues or nonzero Duration"
        );
        decoded_to_eos(&video).expect("recovered Matroska file must decode to EOS");
        assert!(video.exists());
        assert!(!video.with_file_name("video.mkv.orig").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
