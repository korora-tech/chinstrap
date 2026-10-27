//! Routes termination signals through the engine's bounded finalization path.

use std::io;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use crate::studio::EngineHandle;

static RECEIVED_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn record_signal(signal: libc::c_int) {
    RECEIVED_SIGNAL.store(signal, Ordering::Release);
}

/// Installs async-signal-safe handlers for the signals used by desktop-session shutdown.
pub fn install_handlers() -> io::Result<()> {
    // SAFETY: `sigaction` is initialized before use, and the handler only stores an atomic int.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = record_signal as *const () as usize;
        action.sa_flags = 0;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            return Err(io::Error::last_os_error());
        }
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// A normal thread observes the signal and runs exactly the same shutdown API as Quit.
pub fn spawn_shutdown_watcher(engine: EngineHandle) -> io::Result<()> {
    std::thread::Builder::new()
        .name("chinstrap-signal-shutdown".into())
        .spawn(move || {
            loop {
                let signal = RECEIVED_SIGNAL.swap(0, Ordering::AcqRel);
                if signal != 0 {
                    eprintln!("chinstrap: received signal {signal}; finalizing active recording");
                    if !engine.shutdown_blocking(Duration::from_secs(8)) {
                        eprintln!(
                            "chinstrap: signal shutdown timed out; one or more files may lack Cues"
                        );
                    }
                    std::process::exit(128 + signal);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
        .map(|_| ())
}
