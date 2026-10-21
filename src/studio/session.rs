//! Session paths, naming and the manifest (SPEC §6).
//!
//! Layout: `<root>/<session>/session.json` and `<root>/<session>/segment-NNN/*.mkv`.
//! Session and segment directories are created non-recursively with mode 0700, so an existing
//! directory is never clobbered. Manifest writes are atomic: tmp → write → fsync → rename →
//! fsync(dir).

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
    /// Recorded frame rate, e.g. "25/1" (video only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framerate: Option<String>,
    pub first_pts_ns: Option<u64>,
    pub last_pts_ns: Option<u64>,
    /// "starting" | "recording" | "finalizing" | "complete" | "failed"
    pub status: String,
    pub error: Option<String>,
    pub device: Option<String>,
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

/// Atomic manifest write: `session.json.tmp` → write → fsync → rename → fsync(dir).
pub fn write_manifest(session_dir: &Path, manifest: &Manifest) -> io::Result<()> {
    let tmp = session_dir.join(format!("{MANIFEST_NAME}.tmp"));
    let dst = session_dir.join(MANIFEST_NAME);
    let json = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
    {
        let mut f = File::create(&tmp)?;
        f.write_all(&json)?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &dst)?;
    File::open(session_dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
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
                first_pts_ns: Some(1),
                last_pts_ns: Some(2),
                status: "complete".into(),
                error: None,
                device: None,
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
        assert!(!dir.join("session.json.tmp").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}
