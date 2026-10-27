use std::fmt;
use std::io;

use gst::glib;
use gstreamer as gst;

/// Structured errors shared by the GStreamer pipeline-building modules.
#[derive(Debug)]
pub enum PipelineError {
    MissingElement {
        factory: String,
        message: String,
    },
    InvalidCaps {
        caps: String,
        message: String,
    },
    MissingPad {
        element: String,
        pad: String,
    },
    Link {
        message: String,
    },
    StateChange {
        element: String,
        message: String,
    },
    NoBus,
    NotPipeline,
    TimedOut,
    NoTimeToMeasure,
    TooFewFrames(u32),
    Io(io::Error),
    GStreamer {
        source: glib::Error,
        debug: Option<String>,
    },
    StorageFull {
        source: glib::Error,
        debug: Option<String>,
    },
    Other(String),
}

impl PipelineError {
    /// Converts a GStreamer bus error while retaining its domain and code.
    pub fn from_gst_error(source: &glib::Error, debug: Option<String>) -> Self {
        if source.matches(gst::ResourceError::NoSpaceLeft) {
            Self::StorageFull {
                source: source.clone(),
                debug,
            }
        } else {
            Self::GStreamer {
                source: source.clone(),
                debug,
            }
        }
    }

    /// True for operating-system or GStreamer errors that mean the destination is full.
    /// Kept public for the engine's recording-bus handling to classify write failures.
    pub fn is_disk_full(&self) -> bool {
        match self {
            Self::StorageFull { .. } => true,
            Self::Io(error) => {
                error.kind() == io::ErrorKind::StorageFull
                    || error.raw_os_error() == Some(libc::ENOSPC)
            }
            _ => false,
        }
    }
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingElement { factory, message } => {
                write!(f, "cannot create {factory}: {message}")
            }
            Self::InvalidCaps { caps, message } => write!(f, "bad caps {caps}: {message}"),
            Self::MissingPad { element, pad } => write!(f, "{element} has no {pad} pad"),
            Self::Link { message } | Self::Other(message) => f.write_str(message),
            Self::StateChange { element, message } => {
                let _ = element;
                f.write_str(message)
            }
            Self::NoBus => f.write_str("pipeline has no bus"),
            Self::NotPipeline => f.write_str("not a pipeline"),
            Self::TimedOut => f.write_str("timed out"),
            Self::NoTimeToMeasure => f.write_str("no time left to measure"),
            Self::TooFewFrames(frames) => write!(f, "only {frames} frames encoded"),
            Self::Io(error) => write!(f, "{error}"),
            Self::GStreamer { source, debug } | Self::StorageFull { source, debug } => {
                write!(f, "{source}")?;
                if let Some(debug) = debug {
                    write!(f, " ({debug})")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for PipelineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::GStreamer { source, .. } | Self::StorageFull { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for PipelineError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<String> for PipelineError {
    fn from(message: String) -> Self {
        Self::Other(message)
    }
}

impl From<&str> for PipelineError {
    fn from(message: &str) -> Self {
        Self::Other(message.to_owned())
    }
}
