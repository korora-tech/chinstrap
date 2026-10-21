//! §12.12: a `--transcribe-worker` child dies with the thread that spawned it, including while
//! it is SIGSTOPped, so a crashed parent never leaves an orphan running ggml.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Spawns the worker from a short-lived helper thread, as the app's `chinstrap-transcribe`
/// thread would, and lets that thread exit. The worker is blocked reading its job from stdin.
fn spawn_from_helper_thread(stop_child: bool) -> (Child, std::process::ChildStdin) {
    std::thread::spawn(move || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_chinstrap"))
            .arg("--transcribe-worker")
            .env("CHINSTRAP_WORKER_PARENT", std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn worker");
        let stdin = child.stdin.take().unwrap();
        // Let the child reach prctl(PR_SET_PDEATHSIG) before its parent thread exits.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            child.try_wait().unwrap().is_none(),
            "worker waits for its job"
        );
        if stop_child {
            unsafe {
                libc_kill(child.id() as i32, 19 /* SIGSTOP */);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        (child, stdin)
    })
    .join()
    .unwrap()
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

fn gone_within(child: &mut Child, limit: Duration) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn worker_dies_with_its_spawning_thread() {
    let (mut child, _stdin) = spawn_from_helper_thread(false);
    assert!(gone_within(&mut child, Duration::from_secs(1)));
}

#[test]
fn stopped_worker_dies_with_its_spawning_thread() {
    let (mut child, _stdin) = spawn_from_helper_thread(true);
    assert!(gone_within(&mut child, Duration::from_secs(1)));
}

#[test]
fn worker_refuses_a_foreign_parent() {
    let out = Command::new(env!("CARGO_BIN_EXE_chinstrap"))
        .arg("--transcribe-worker")
        .env("CHINSTRAP_WORKER_PARENT", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}
