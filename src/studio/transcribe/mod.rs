//! On-device transcription with Whisper (SPEC §12).
//!
//! All inference runs in a `chinstrap --transcribe-worker` child (one per job), driven by
//! `driver.rs` from the GUI's `chinstrap-transcribe` thread, from `--transcribe`, and from the
//! selftest. The GUI process links ggml but never calls it.

mod cli;
pub mod control;
mod decode;
pub mod driver;
pub mod model;
mod output;
pub mod prompt;
pub mod proto;
pub mod vocab;
pub mod wer;
mod whisper;
pub mod worker;

// Keep these public result types available alongside the relocated CLI entry points.
#[allow(unused_imports)]
pub use cli::{CliError, FileResult};
pub use cli::{download_cli, run_cli, transcribe_file};
pub use whisper::run_worker;
