pub mod engine;
mod fake;
// Frozen contract, kept verbatim from SPEC §3.1; rustfmt must not reflow it.
#[rustfmt::skip]
pub mod types;

mod devices;
mod encoders;
mod portal;
mod recorder;
mod session;
mod stamp;
pub mod transcribe;

pub use engine::{EngineConfig, EngineError, EngineHandle, start};
pub use recorder::{check_targeting, provide_test_node, test_size};
pub use stamp::lock_size;
pub use types::*;
