//! NDJSON protocol between the parent and `chinstrap --transcribe-worker` (§12.3).
//!
//! The parent writes exactly one `Job` (one JSON line) to the child's stdin and closes it. The
//! child writes `WorkerLine`s to stdout, one JSON object per line, and exits 0 after `Done` or 1
//! after `Error`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::studio::types::{TranscribeStage, WhisperBackend};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendReq {
    Auto,
    Cpu,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub media: PathBuf,
    pub model: PathBuf,
    pub model_id: String,
    pub model_sha256: Option<String>,
    pub vad_model: Option<PathBuf>,
    /// Session hint (§12.6), already filtered and capped by the parent; the glossary is read
    /// by the child.
    pub prompt_hint: Option<String>,
    /// None = auto-detect.
    pub language: Option<String>,
    pub backend: BackendReq,
    pub gpu_device: i32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerLine {
    /// `backend` is "vulkan" or "cpu".
    Backend {
        backend: String,
        device: String,
    },
    /// decode 0–10, vad 10–12, whisper 12–98, write 98–100.
    Progress {
        stage: String,
        percent: u8,
    },
    Done {
        json: PathBuf,
        srt: PathBuf,
        vtt: PathBuf,
        language: String,
        words: u32,
        audio_s: f64,
        rtf: f64,
    },
    Error {
        reason: String,
    },
}

pub const STAGE_STARTING: &str = "starting";
pub const STAGE_DECODING: &str = "decoding";
pub const STAGE_DETECTING: &str = "detecting";
pub const STAGE_TRANSCRIBING: &str = "transcribing";
pub const STAGE_WRITING: &str = "writing";

pub fn stage_from_str(s: &str) -> TranscribeStage {
    match s {
        STAGE_DECODING => TranscribeStage::Decoding,
        STAGE_DETECTING => TranscribeStage::Detecting,
        STAGE_TRANSCRIBING => TranscribeStage::Transcribing,
        STAGE_WRITING => TranscribeStage::Writing,
        _ => TranscribeStage::Starting,
    }
}

pub fn backend_from_str(s: &str) -> WhisperBackend {
    if s == "vulkan" {
        WhisperBackend::Vulkan
    } else {
        WhisperBackend::Cpu
    }
}

pub fn backend_name(b: WhisperBackend) -> &'static str {
    match b {
        WhisperBackend::Vulkan => "vulkan",
        WhisperBackend::Cpu => "cpu",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip() {
        let lines = [
            WorkerLine::Backend {
                backend: "vulkan".into(),
                device: "AMD Radeon RX 7900 XT (RADV NAVI31)".into(),
            },
            WorkerLine::Progress {
                stage: STAGE_TRANSCRIBING.into(),
                percent: 42,
            },
            WorkerLine::Error {
                reason: "cpu-unsupported: x".into(),
            },
        ];
        for l in lines {
            let s = serde_json::to_string(&l).unwrap();
            assert!(!s.contains('\n'));
            assert_eq!(serde_json::from_str::<WorkerLine>(&s).unwrap(), l);
        }
        let s = serde_json::to_string(&WorkerLine::Progress {
            stage: "decoding".into(),
            percent: 3,
        })
        .unwrap();
        assert_eq!(s, r#"{"type":"progress","stage":"decoding","percent":3}"#);
        assert_eq!(stage_from_str("detecting"), TranscribeStage::Detecting);
    }
}
