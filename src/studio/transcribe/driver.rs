//! Spawns and drives one `--transcribe-worker` child (§12.3). Shared by the GUI worker thread,
//! `--transcribe` and the selftest.
//!
//! The child runs in its own process group so pause (SIGSTOP), resume (SIGCONT) and kill reach
//! everything it started. The driver never blocks: `poll` drains whatever stdout lines arrived
//! and reports the exit once the child has gone.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::proto::{Job, STAGE_TRANSCRIBING, WorkerLine};

const STDERR_TAIL: usize = 4096;
const TERM_GRACE: Duration = Duration::from_millis(500);

/// How a child exit is treated (§12.3 policy 7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitClass {
    Ok,
    /// We sent TERM/KILL (cancel, shutdown). Never latches.
    Cancelled,
    /// A failure of this job; retry is the user's call. Never latches.
    JobFailed,
    /// Vulkan is the likely cause: retry this job on CPU.
    VulkanFault,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Exit {
    pub class: ExitClass,
    pub reason: String,
    pub done: Option<WorkerLine>,
}

/// What the child's exit looked like, for `classify`.
#[derive(Clone, Debug, Default)]
pub struct ExitFacts {
    pub killed_by_us: bool,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub error_line: Option<String>,
    pub saw_transcribing: bool,
    pub requested_cpu: bool,
    pub backend_cpu: bool,
    pub stderr_tail: String,
    pub done: bool,
}

const VK_MARKERS: &[&str] = &[
    "ggml_vulkan",
    "vk::",
    "VK_ERROR",
    "ErrorDeviceLost",
    "ErrorOutOfDeviceMemory",
];

pub fn classify(f: &ExitFacts) -> (ExitClass, String) {
    if f.killed_by_us {
        return (ExitClass::Cancelled, "cancelled".into());
    }
    if f.code == Some(0) && f.done {
        return (ExitClass::Ok, String::new());
    }
    if let Some(reason) = &f.error_line
        && f.signal.is_none()
    {
        // Includes "cpu-unsupported: …": never a Vulkan fault.
        return (ExitClass::JobFailed, reason.clone());
    }
    let tail = f.stderr_tail.trim();
    let how = match (f.signal, f.code) {
        (Some(s), _) => format!("worker crashed (signal {s})"),
        (None, Some(c)) => format!("worker exited with status {c}"),
        (None, None) => "worker ended abnormally".into(),
    };
    let on_gpu = !f.requested_cpu && !f.backend_cpu;
    let vk_tail = VK_MARKERS.iter().any(|m| tail.contains(m));
    if on_gpu && (!f.saw_transcribing || vk_tail) {
        return (ExitClass::VulkanFault, with_tail(how, tail));
    }
    (ExitClass::JobFailed, with_tail(how, tail))
}

fn with_tail(how: String, tail: &str) -> String {
    let last = tail.lines().rev().find(|l| !l.trim().is_empty());
    match last {
        Some(l) => format!("{how}: {}", l.trim()),
        None => how,
    }
}

/// One stdout event.
#[derive(Debug)]
pub enum DriverEvent {
    Line(WorkerLine),
    Exit(Exit),
}

pub struct Running {
    child: Child,
    pid: u32,
    media_dir: PathBuf,
    lines: Receiver<WorkerLine>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    stderr_tail: Arc<Mutex<VecDeque<u8>>>,
    killed_by_us: bool,
    term_at: Option<Instant>,
    paused: bool,
    saw_transcribing: bool,
    backend_cpu: bool,
    requested_cpu: bool,
    error_line: Option<String>,
    done: Option<WorkerLine>,
    exited: bool,
}

/// The real worker command: this executable with `--transcribe-worker`.
///
/// Spawned through `/proc/self/exe`, which names the running binary's inode even after the
/// file on disk was replaced or deleted (an upgrade or a rebuild while the app is open), so the
/// worker always speaks this process's protocol. `argv[0]` keeps the readable path.
pub fn worker_command() -> std::io::Result<Command> {
    let proc_exe = Path::new("/proc/self/exe");
    let mut cmd = if proc_exe.exists() {
        let mut c = Command::new(proc_exe);
        if let Ok(exe) = std::env::current_exe() {
            c.arg0(exe);
        }
        c
    } else {
        Command::new(std::env::current_exe()?)
    };
    cmd.arg("--transcribe-worker");
    Ok(cmd)
}

static IO_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Running {
    /// Spawns `cmd` as a worker for `job`. `stderr_prefix`: forward the child's stderr lines to
    /// ours with this prefix (None: keep only the tail).
    pub fn spawn(
        mut cmd: Command,
        job: &Job,
        stderr_prefix: Option<&'static str>,
    ) -> std::io::Result<Self> {
        cmd.env("CHINSTRAP_WORKER_PARENT", std::process::id().to_string())
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let mut stdin = child.stdin.take().expect("piped stdin");
        let mut body = serde_json::to_vec(job).map_err(std::io::Error::other)?;
        body.push(b'\n');
        // The child reads one line; a broken pipe means it already died (classified on exit).
        let _ = stdin.write_all(&body);
        drop(stdin);
        let n = IO_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, lines) = mpsc::channel();
        let stdout = child.stdout.take().expect("piped stdout");
        let stdout_thread = std::thread::Builder::new()
            .name(format!("chinstrap-transcribe-io-{n}"))
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    match serde_json::from_str::<WorkerLine>(&line) {
                        Ok(l) => {
                            if tx.send(l).is_err() {
                                break;
                            }
                        }
                        Err(_) if !line.trim().is_empty() => {
                            eprintln!("whisper: (unparsed) {line}");
                        }
                        Err(_) => {}
                    }
                }
            })?;
        let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL)));
        let tail = stderr_tail.clone();
        let mut stderr = child.stderr.take().expect("piped stderr");
        let stderr_thread = std::thread::Builder::new()
            .name(format!("chinstrap-transcribe-err-{n}"))
            .spawn(move || {
                let mut buf = [0u8; 4096];
                let mut pending = Vec::new();
                loop {
                    let n = match stderr.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    {
                        let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                        t.extend(&buf[..n]);
                        while t.len() > STDERR_TAIL {
                            t.pop_front();
                        }
                    }
                    if let Some(prefix) = stderr_prefix {
                        pending.extend_from_slice(&buf[..n]);
                        while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = pending.drain(..=nl).collect();
                            eprint!("{prefix}{}", String::from_utf8_lossy(&line));
                        }
                    }
                }
                if let Some(prefix) = stderr_prefix
                    && !pending.is_empty()
                {
                    eprintln!("{prefix}{}", String::from_utf8_lossy(&pending));
                }
            })?;
        Ok(Self {
            child,
            pid,
            media_dir: job
                .media
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            lines,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            stderr_tail,
            killed_by_us: false,
            term_at: None,
            paused: false,
            saw_transcribing: false,
            backend_cpu: false,
            requested_cpu: job.backend == super::proto::BackendReq::Cpu,
            error_line: None,
            done: None,
            exited: false,
        })
    }

    #[cfg(test)]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    fn signal_group(&self, sig: i32) {
        // SAFETY: plain kill(2) on the child's process group (pgid == pid).
        unsafe {
            libc::kill(-(self.pid as i32), sig);
        }
    }

    pub fn pause(&mut self) {
        if !self.paused && !self.exited {
            self.signal_group(libc::SIGSTOP);
            self.paused = true;
        }
    }

    pub fn resume(&mut self) {
        if self.paused && !self.exited {
            self.signal_group(libc::SIGCONT);
            self.paused = false;
        }
    }

    /// Cancel: SIGCONT, SIGTERM, then SIGKILL after 500 ms (escalated by `poll`).
    pub fn cancel(&mut self) {
        if self.exited {
            return;
        }
        self.killed_by_us = true;
        self.signal_group(libc::SIGCONT);
        self.paused = false;
        self.signal_group(libc::SIGTERM);
        self.term_at.get_or_insert_with(Instant::now);
    }

    /// Shutdown: SIGKILL now. Never waits.
    pub fn kill_now(&mut self) {
        if self.exited {
            return;
        }
        self.killed_by_us = true;
        self.signal_group(libc::SIGKILL);
        self.signal_group(libc::SIGCONT);
    }

    /// Drains stdout lines and, once the child has exited, returns its classified exit last.
    pub fn poll(&mut self) -> Vec<DriverEvent> {
        let mut out = Vec::new();
        if self.exited {
            return out;
        }
        if let Some(t) = self.term_at
            && t.elapsed() >= TERM_GRACE
        {
            self.signal_group(libc::SIGKILL);
            self.term_at = None;
        }
        self.drain_lines(&mut out);
        let status = match self.child.try_wait() {
            Ok(Some(s)) => s,
            Ok(None) => return out,
            Err(e) => {
                eprintln!("chinstrap: waiting for the transcription worker: {e}");
                return out;
            }
        };
        self.exited = true;
        // The child is gone: its pipes reach EOF promptly. Collect the remaining lines.
        if let Some(t) = self.stdout_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.stderr_thread.take() {
            let _ = t.join();
        }
        self.drain_lines(&mut out);
        if self.killed_by_us {
            remove_tmp_outputs(&self.media_dir, self.pid);
        }
        let tail = {
            let t = self.stderr_tail.lock().unwrap_or_else(|e| e.into_inner());
            let (a, b) = t.as_slices();
            String::from_utf8_lossy(&[a, b].concat()).to_string()
        };
        let facts = ExitFacts {
            killed_by_us: self.killed_by_us,
            code: status.code(),
            signal: status.signal(),
            error_line: self.error_line.clone(),
            saw_transcribing: self.saw_transcribing,
            requested_cpu: self.requested_cpu,
            backend_cpu: self.backend_cpu,
            stderr_tail: tail,
            done: self.done.is_some(),
        };
        let (class, reason) = classify(&facts);
        out.push(DriverEvent::Exit(Exit {
            class,
            reason,
            done: self.done.clone(),
        }));
        out
    }

    fn drain_lines(&mut self, out: &mut Vec<DriverEvent>) {
        while let Ok(line) = self.lines.try_recv() {
            match &line {
                WorkerLine::Backend { backend, .. } => self.backend_cpu = backend == "cpu",
                WorkerLine::Progress { stage, .. } if stage == STAGE_TRANSCRIBING => {
                    self.saw_transcribing = true
                }
                WorkerLine::Error { reason } => self.error_line = Some(reason.clone()),
                WorkerLine::Done { .. } => self.done = Some(line.clone()),
                _ => {}
            }
            out.push(DriverEvent::Line(line));
        }
    }

    /// Blocks until the child exits (CLI and selftest only), passing every event to `on`.
    pub fn wait(mut self, mut on: impl FnMut(&DriverEvent)) -> Exit {
        loop {
            for ev in self.poll() {
                on(&ev);
                if let DriverEvent::Exit(e) = ev {
                    return e;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if !self.exited {
            self.killed_by_us = true;
            self.signal_group(libc::SIGKILL);
            self.signal_group(libc::SIGCONT);
            let _ = self.child.wait();
            remove_tmp_outputs(&self.media_dir, self.pid);
        }
    }
}

/// Deletes the hidden `.*.tmp-<pid>` files `write_atomic` leaves when a writer is killed.
pub fn remove_tmp_outputs(dir: &Path, pid: u32) {
    let suffix = format!(".tmp-{pid}");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && name.ends_with(&suffix) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::proto::BackendReq;
    use super::*;

    fn job(dir: &Path, backend: BackendReq) -> Job {
        Job {
            media: dir.join("mic.mkv"),
            model: "/m.bin".into(),
            model_id: "m".into(),
            model_sha256: None,
            vad_model: None,
            prompt_hint: None,
            language: None,
            backend,
            gpu_device: 0,
        }
    }

    /// A fake worker: a shell script that reads the job line and then runs `body`.
    fn fake(body: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c").arg(format!("read -r job; {body}"));
        c
    }

    fn scratch() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "chinstrap-driver-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const BACKEND_VK: &str =
        r#"echo '{"type":"backend","backend":"vulkan","device":"AMD Radeon RX 7900 XT"}'"#;
    const TRANSCRIBING: &str = r#"echo '{"type":"progress","stage":"transcribing","percent":20}'"#;

    #[test]
    fn ok_run_reports_lines_and_done() {
        let dir = scratch();
        let r = Running::spawn(
            fake(&format!(
                r#"{BACKEND_VK}; {TRANSCRIBING}; echo '{{"type":"done","json":"/j","srt":"/s","vtt":"/v","language":"en","words":3,"audio_s":1.0,"rtf":0.1}}'"#
            )),
            &job(&dir, BackendReq::Auto),
            None,
        )
        .unwrap();
        let mut lines = 0;
        let exit = r.wait(|e| {
            if matches!(e, DriverEvent::Line(_)) {
                lines += 1
            }
        });
        assert_eq!(exit.class, ExitClass::Ok);
        assert_eq!(lines, 3);
        assert!(matches!(exit.done, Some(WorkerLine::Done { words: 3, .. })));
    }

    #[test]
    fn cancel_after_backend_is_cancelled_and_cleans_tmp() {
        let dir = scratch();
        let mut r = Running::spawn(
            fake(&format!("{BACKEND_VK}; trap '' TERM; sleep 30")),
            &job(&dir, BackendReq::Auto),
            None,
        )
        .unwrap();
        std::fs::write(dir.join(format!(".mic.srt.tmp-{}", r.pid())), b"x").unwrap();
        std::fs::write(dir.join(".keep.tmp-1"), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        r.pause();
        r.cancel();
        let t0 = Instant::now();
        let exit = r.wait(|_| {});
        assert_eq!(exit.class, ExitClass::Cancelled);
        assert!(
            t0.elapsed() < Duration::from_secs(3),
            "TERM ignored → KILL after 500 ms"
        );
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, vec![".keep.tmp-1".to_string()]);
    }

    #[test]
    fn signal_before_transcribing_is_vulkan_fault() {
        let dir = scratch();
        let r = Running::spawn(
            fake(&format!("{BACKEND_VK}; kill -KILL $$")),
            &job(&dir, BackendReq::Auto),
            None,
        )
        .unwrap();
        let exit = r.wait(|_| {});
        assert_eq!(exit.class, ExitClass::VulkanFault);
        assert!(exit.reason.contains("signal 9"), "{}", exit.reason);
    }

    #[test]
    fn signal_after_transcribing_without_vk_stderr_is_job_failure() {
        let dir = scratch();
        let r = Running::spawn(
            fake(&format!(
                "{BACKEND_VK}; {TRANSCRIBING}; echo 'oops' >&2; kill -KILL $$"
            )),
            &job(&dir, BackendReq::Auto),
            None,
        )
        .unwrap();
        let exit = r.wait(|_| {});
        assert_eq!(exit.class, ExitClass::JobFailed);
        assert!(exit.reason.contains("signal 9") && exit.reason.contains("oops"));
        // With a Vulkan marker in stderr it is a Vulkan fault even after transcribing started.
        let r = Running::spawn(
            fake(&format!(
                "{BACKEND_VK}; {TRANSCRIBING}; echo 'ggml_vulkan: ErrorDeviceLost' >&2; kill -KILL $$"
            )),
            &job(&dir, BackendReq::Auto),
            None,
        )
        .unwrap();
        assert_eq!(r.wait(|_| {}).class, ExitClass::VulkanFault);
    }

    #[test]
    fn cpu_unsupported_and_error_lines_are_job_failures() {
        let dir = scratch();
        let r = Running::spawn(
            fake(
                r#"echo '{"type":"error","reason":"cpu-unsupported: this build needs x86-64-v3 (AVX2/FMA/F16C)"}'; exit 1"#,
            ),
            &job(&dir, BackendReq::Auto),
            None,
        )
        .unwrap();
        let exit = r.wait(|_| {});
        assert_eq!(exit.class, ExitClass::JobFailed);
        assert!(exit.reason.starts_with("cpu-unsupported:"));
    }

    #[test]
    fn crash_on_cpu_is_never_a_vulkan_fault() {
        let dir = scratch();
        let r = Running::spawn(fake("kill -KILL $$"), &job(&dir, BackendReq::Cpu), None).unwrap();
        assert_eq!(r.wait(|_| {}).class, ExitClass::JobFailed);
    }

    #[test]
    fn classify_table() {
        let base = ExitFacts {
            signal: Some(11),
            ..Default::default()
        };
        assert_eq!(classify(&base).0, ExitClass::VulkanFault);
        assert_eq!(
            classify(&ExitFacts {
                killed_by_us: true,
                ..base.clone()
            })
            .0,
            ExitClass::Cancelled
        );
        assert_eq!(
            classify(&ExitFacts {
                backend_cpu: true,
                ..base.clone()
            })
            .0,
            ExitClass::JobFailed
        );
        // Non-zero exit without an Error line before transcribing: Vulkan fault.
        assert_eq!(
            classify(&ExitFacts {
                code: Some(134),
                signal: None,
                ..Default::default()
            })
            .0,
            ExitClass::VulkanFault
        );
        assert_eq!(
            classify(&ExitFacts {
                code: Some(1),
                signal: None,
                error_line: Some("decode failed".into()),
                ..Default::default()
            }),
            (ExitClass::JobFailed, "decode failed".into())
        );
    }
}
