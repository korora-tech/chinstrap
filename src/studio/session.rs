//! Session paths, naming and the manifest (SPEC §6).
//!
//! Layout: `<root>/<session>/session.json` and `<root>/<session>/segment-NNN/*.mkv`.
//! Session and segment directories are created non-recursively with mode 0700, so an existing
//! directory is never clobbered. Manifest writes are atomic: tmp → write → fsync → rename →
//! fsync(dir), via `write_atomic`, which the transcription outputs use too (§12.7).

use std::fs::{self, DirBuilder, File};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use gstreamer as gst;
use serde::{Deserialize, Serialize};

pub const MANIFEST_NAME: &str = "session.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub name: String,
    pub clock: String,
    pub created: String,
    pub segments: Vec<ManifestSegment>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestSegment {
    pub index: u32,
    pub dir: String,
    /// "recording" | "complete" | "incomplete"
    pub status: String,
    pub reason: Option<String>,
    pub base_time_ns: Option<u64>,
    pub stop_clock_ns: Option<u64>,
    pub duration_ns: Option<u64>,
    pub gap_before_ns: Option<u64>,
    pub started_wall: Option<String>,
    pub inputs: Vec<ManifestInput>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestInput {
    /// "camera" | "mic" | "source:<id>"
    pub key: String,
    /// "camera" | "microphone" | "monitor" | "window" | "virtual" | "test"
    pub kind: String,
    pub label: String,
    pub file: String,
    /// "h264" | "opus"
    pub codec: String,
    pub encoder: Option<String>,
    /// "va" | "gl" | null
    pub convert: Option<String>,
    pub capture: Option<u32>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    /// Recorded frame rate, e.g. "25/1" (video only); "variable" for Native.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framerate: Option<String>,
    /// Native (variable rate) only: the maximum rate requested from the source, e.g. "60/1";
    /// absent when the source's own maximum applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_framerate: Option<String>,
    /// Running time of the first real frame (video: its arrival; the file itself starts at
    /// zero, showing that frame until the next one).
    pub first_pts_ns: Option<u64>,
    pub last_pts_ns: Option<u64>,
    /// "starting" | "recording" | "finalizing" | "complete" | "failed"
    pub status: String,
    pub error: Option<String>,
    pub device: Option<String>,
    /// Mic only (§12.7). Absent in manifests written before transcription existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<ManifestTranscript>,
    /// Video only: frame accounting (P2-8). Absent in older manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<ManifestQuality>,
}

/// Frame accounting of one video input (P0-1, P2-8).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestQuality {
    pub frames_in: u64,
    pub dropped_corrupt: u64,
    pub dropped_queue: u64,
    pub duplicated: u64,
    pub padded: u64,
    pub encoded: u64,
    pub effective_fps: f64,
    pub repeat_pct: f64,
    pub bitrate_kbps: u32,
    pub start_offset_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestTranscript {
    /// "queued" | "running" | "done" | "failed" | "cancelled" | "interrupted"
    pub status: String,
    /// Relative to the segment directory.
    pub json: Option<String>,
    pub srt: Option<String>,
    pub vtt: Option<String>,
    pub model: Option<String>,
    pub backend: Option<String>,
    pub language: Option<String>,
    pub words: Option<u32>,
    pub error: Option<String>,
    /// `wall_now()` of the last change.
    pub updated: String,
}

impl ManifestTranscript {
    pub fn status_only(status: &str) -> Self {
        Self {
            status: status.into(),
            json: None,
            srt: None,
            vtt: None,
            model: None,
            backend: None,
            language: None,
            words: None,
            error: None,
            updated: wall_now(),
        }
    }

    /// §12.3 policy 10: a `queued`/`running` status with no live job behind it is a leftover
    /// of a process that ended mid-job, and reads as `interrupted`.
    pub fn effective_status(&self, live_job: bool) -> &str {
        match self.status.as_str() {
            "queued" | "running" if !live_job => "interrupted",
            s => s,
        }
    }
}

impl Manifest {
    pub fn new(name: &str, created: String) -> Self {
        Self {
            format: "chinstrap-session".into(),
            version: 1,
            name: name.into(),
            clock: "CLOCK_MONOTONIC".into(),
            created,
            segments: Vec::new(),
        }
    }
}

/// `YYYY-MM-DDTHH-MM-SS` in local time.
pub fn session_name_now() -> String {
    gst::glib::DateTime::now_local()
        .ok()
        .and_then(|d| d.format("%Y-%m-%dT%H-%M-%S").ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "session".into())
}

/// ISO-8601 local wall clock with milliseconds, e.g. `2026-10-21T14:03:22.123+01:00`.
pub fn wall_now() -> String {
    let Ok(d) = gst::glib::DateTime::now_local() else {
        return String::new();
    };
    let base = d
        .format("%Y-%m-%dT%H:%M:%S")
        .map(|s| s.to_string())
        .unwrap_or_default();
    let ms = d.microsecond() / 1000;
    let offset = d.format("%:z").map(|s| s.to_string()).unwrap_or_default();
    format!("{base}.{ms:03}{offset}")
}

pub fn segment_dir_name(index: u32) -> String {
    format!("segment-{index:03}")
}

fn mkdir_0700(path: &Path) -> io::Result<()> {
    DirBuilder::new().mode(0o700).create(path)
}

/// Creates `<root>/<name>` (root recursively; the session dir non-recursively, 0700), appending
/// `-2`, `-3`, ... on collision. Returns the final name and directory.
pub fn create_session_dir(root: &Path, name: &str) -> io::Result<(String, PathBuf)> {
    fs::create_dir_all(root)?;
    for n in 1u32..1000 {
        let candidate = if n == 1 {
            name.to_string()
        } else {
            format!("{name}-{n}")
        };
        let dir = root.join(&candidate);
        match mkdir_0700(&dir) {
            Ok(()) => return Ok((candidate, dir)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "too many sessions with the same name",
    ))
}

/// Creates `<session>/segment-NNN` (0700, fails if it exists).
pub fn create_segment_dir(session_dir: &Path, index: u32) -> io::Result<PathBuf> {
    let dir = session_dir.join(segment_dir_name(index));
    mkdir_0700(&dir)?;
    Ok(dir)
}

/// The hidden temporary name `write_atomic` uses for `path` in process `pid`.
pub fn atomic_tmp_path(path: &Path, pid: u32) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.tmp-{pid}"))
}

/// Atomic write: hidden `.<name>.tmp-<pid>` → write → fsync → rename → fsync(dir). The tmp file
/// is removed if any step before the rename fails.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = atomic_tmp_path(path, std::process::id());
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let written = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    File::open(&dir)?.sync_all()?;
    Ok(())
}

/// Atomic manifest write (pretty JSON plus a trailing newline) through `write_atomic`.
pub fn write_manifest(session_dir: &Path, manifest: &Manifest) -> io::Result<()> {
    let mut json = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    json.push(b'\n');
    write_atomic(&session_dir.join(MANIFEST_NAME), &json)
}

pub fn read_manifest(path: &Path) -> io::Result<Manifest> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chinstrap-session-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn naming_format() {
        let n = session_name_now();
        assert_eq!(n.len(), "2026-10-21T14-03-22".len(), "{n}");
        assert_eq!(&n[10..11], "T");
        assert!(!n.contains(':'));
        assert_eq!(segment_dir_name(3), "segment-003");
        let w = wall_now();
        assert!(w.contains('T') && w.contains('.'), "{w}");
    }

    #[test]
    fn collision_suffix_and_mode() {
        let root = scratch("collide").join("nested");
        let (a, da) = create_session_dir(&root, "2026-10-21T14-03-22").unwrap();
        let (b, _) = create_session_dir(&root, "2026-10-21T14-03-22").unwrap();
        let (c, _) = create_session_dir(&root, "2026-10-21T14-03-22").unwrap();
        assert_eq!(a, "2026-10-21T14-03-22");
        assert_eq!(b, "2026-10-21T14-03-22-2");
        assert_eq!(c, "2026-10-21T14-03-22-3");
        let mode = fs::metadata(&da).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let seg = create_segment_dir(&da, 1).unwrap();
        assert_eq!(
            fs::metadata(&seg).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(create_segment_dir(&da, 1).is_err(), "never clobbers");
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn manifest_round_trip_no_tmp() {
        let dir = scratch("manifest");
        let mut m = Manifest::new("s", wall_now());
        m.segments.push(ManifestSegment {
            index: 1,
            dir: segment_dir_name(1),
            status: "complete".into(),
            reason: None,
            base_time_ns: Some(123),
            stop_clock_ns: Some(456),
            duration_ns: Some(333),
            gap_before_ns: None,
            started_wall: Some(wall_now()),
            inputs: vec![ManifestInput {
                key: "source:3".into(),
                kind: "test".into(),
                label: "Test pattern 3".into(),
                file: "test-3.mkv".into(),
                codec: "h264".into(),
                encoder: Some("x264enc".into()),
                convert: Some("va".into()),
                capture: None,
                width: Some(1280),
                height: Some(720),
                framerate: Some("30/1".into()),
                max_framerate: None,
                first_pts_ns: Some(1),
                last_pts_ns: Some(2),
                status: "complete".into(),
                error: None,
                device: None,
                transcript: None,
                quality: None,
            }],
        });
        write_manifest(&dir, &m).unwrap();
        write_manifest(&dir, &m).unwrap();
        let back = read_manifest(&dir.join(MANIFEST_NAME)).unwrap();
        assert_eq!(back, m);
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join(MANIFEST_NAME)).unwrap()).unwrap();
        assert_eq!(json["format"], "chinstrap-session");
        assert_eq!(
            json["segments"][0]["gap_before_ns"],
            serde_json::Value::Null
        );
        assert!(no_tmp_files(&dir), "write_atomic leaves no tmp files");
        // Without a transcript the key is omitted entirely (additive, version stays 1).
        assert!(json["segments"][0]["inputs"][0].get("transcript").is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    fn no_tmp_files(dir: &Path) -> bool {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .all(|e| !e.file_name().to_string_lossy().contains(".tmp"))
    }

    #[test]
    fn manifest_transcript_round_trip_and_old_manifests() {
        let dir = scratch("transcript");
        let mut m = Manifest::new("s", wall_now());
        let mut input = ManifestInput {
            key: "mic".into(),
            kind: "microphone".into(),
            label: "Microphone".into(),
            file: "mic.mkv".into(),
            codec: "opus".into(),
            encoder: None,
            convert: None,
            capture: None,
            width: None,
            height: None,
            framerate: None,
            max_framerate: None,
            first_pts_ns: Some(0),
            last_pts_ns: Some(1),
            status: "complete".into(),
            error: None,
            device: None,
            transcript: None,
            quality: None,
        };
        input.transcript = Some(ManifestTranscript {
            status: "done".into(),
            json: Some("transcript.json".into()),
            srt: Some("mic.srt".into()),
            vtt: Some("mic.vtt".into()),
            model: Some("large-v3-turbo-q8_0".into()),
            backend: Some("vulkan".into()),
            language: Some("en".into()),
            words: Some(12),
            error: None,
            updated: wall_now(),
        });
        m.segments.push(ManifestSegment {
            index: 1,
            dir: segment_dir_name(1),
            status: "complete".into(),
            reason: None,
            base_time_ns: Some(1),
            stop_clock_ns: Some(2),
            duration_ns: Some(1),
            gap_before_ns: None,
            started_wall: None,
            inputs: vec![input],
        });
        write_manifest(&dir, &m).unwrap();
        let back = read_manifest(&dir.join(MANIFEST_NAME)).unwrap();
        assert_eq!(back, m);
        assert!(no_tmp_files(&dir));

        // A manifest written before transcription existed still deserializes.
        let mut old: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join(MANIFEST_NAME)).unwrap()).unwrap();
        old["segments"][0]["inputs"][0]
            .as_object_mut()
            .unwrap()
            .remove("transcript");
        let old: Manifest = serde_json::from_value(old).unwrap();
        assert_eq!(old.segments[0].inputs[0].transcript, None);

        // Leftover non-terminal states read as interrupted without a live job.
        let t = ManifestTranscript::status_only("running");
        assert_eq!(t.effective_status(false), "interrupted");
        assert_eq!(t.effective_status(true), "running");
        assert_eq!(
            ManifestTranscript::status_only("queued").effective_status(false),
            "interrupted"
        );
        assert_eq!(
            ManifestTranscript::status_only("done").effective_status(false),
            "done"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn write_atomic_replaces_and_cleans_up() {
        let dir = scratch("atomic");
        let p = dir.join("a.srt");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert!(no_tmp_files(&dir));
        // A failing write (missing directory) leaves nothing behind.
        assert!(write_atomic(&dir.join("missing").join("x"), b"x").is_err());
        assert!(no_tmp_files(&dir));
        assert_eq!(atomic_tmp_path(&p, 42), dir.join(".a.srt.tmp-42"));
        fs::remove_dir_all(dir).unwrap();
    }
}
