//! Model catalog, resolution, verification and the explicit download (§12.5).
//!
//! Nothing here touches the network except `download`, which only runs on an explicit
//! `DownloadModel` command or `--download-model`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gstreamer as gst;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelSpec {
    pub name: &'static str,
    pub file: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

const HF_WHISPER: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/";
const HF_VAD: &str =
    "https://huggingface.co/ggml-org/whisper-vad/resolve/9ffd54a1e1ee413ddf265af9913beaf518d1639b/";

/// The first entry is the default (T4).
pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        name: "large-v3-turbo-q8_0",
        file: "ggml-large-v3-turbo-q8_0.bin",
        bytes: 874_188_075,
        sha256: "317eb69c11673c9de1e1f0d459b253999804ec71ac4c23c17ecf5fbe24e259a1",
    },
    ModelSpec {
        name: "large-v3-turbo-q5_0",
        file: "ggml-large-v3-turbo-q5_0.bin",
        bytes: 574_041_195,
        sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
    },
    ModelSpec {
        name: "large-v3-turbo",
        file: "ggml-large-v3-turbo.bin",
        bytes: 1_624_555_275,
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
    },
];

pub const VAD: ModelSpec = ModelSpec {
    name: "silero-v6.2.0",
    file: "ggml-silero-v6.2.0.bin",
    bytes: 885_098,
    sha256: "2aa269b785eeb53a82983a20501ddf7c1d9c48e33ab63a41391ac6c9f7fb6987",
};

pub fn default_model() -> &'static ModelSpec {
    &MODELS[0]
}

pub fn url_for(spec: &ModelSpec) -> String {
    let base = if *spec == VAD { HF_VAD } else { HF_WHISPER };
    format!("{base}{}", spec.file)
}

/// `$XDG_DATA_HOME/chinstrap/models` (default `~/.local/share/chinstrap/models`).
pub fn models_dir() -> PathBuf {
    gst::glib::user_data_dir().join("chinstrap").join("models")
}

/// Creates the models directory (and missing parents) with mode 0700.
pub fn ensure_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Resolved {
    Catalog(&'static ModelSpec),
    /// `CHINSTRAP_WHISPER_MODEL=<path>`: no hash check, no download.
    External {
        id: String,
        path: PathBuf,
    },
}

impl Resolved {
    pub fn id(&self) -> String {
        match self {
            Self::Catalog(s) => s.name.to_string(),
            Self::External { id, .. } => id.clone(),
        }
    }
}

/// Resolves `CHINSTRAP_WHISPER_MODEL` (or a `--model` value). Returns a warning for an unknown
/// catalog name, which falls back to the default.
pub fn resolve(value: Option<&str>) -> (Resolved, Option<String>) {
    let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return (Resolved::Catalog(default_model()), None);
    };
    if v.contains('/') || v.ends_with(".bin") {
        let path = std::path::absolute(v).unwrap_or_else(|_| PathBuf::from(v));
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().trim_start_matches("ggml-").to_string())
            .unwrap_or_else(|| "custom".into());
        return (Resolved::External { id, path }, None);
    }
    match MODELS.iter().find(|m| m.name == v) {
        Some(m) => (Resolved::Catalog(m), None),
        None => (
            Resolved::Catalog(default_model()),
            Some(format!(
                "Unknown Whisper model {v:?}; using {} (known: {})",
                default_model().name,
                MODELS.iter().map(|m| m.name).collect::<Vec<_>>().join(", ")
            )),
        ),
    }
}

pub fn resolve_env() -> (Resolved, Option<String>) {
    resolve(std::env::var("CHINSTRAP_WHISPER_MODEL").ok().as_deref())
}

fn sidecar_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".sha256ok");
    PathBuf::from(s)
}

fn mtime_ns(meta: &fs::Metadata) -> i128 {
    meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128
}

fn sidecar_line(spec: &ModelSpec, meta: &fs::Metadata) -> String {
    format!("{} {} {}", spec.sha256, meta.len(), mtime_ns(meta))
}

fn write_sidecar(path: &Path, spec: &ModelSpec) -> io::Result<()> {
    let meta = fs::metadata(path)?;
    crate::studio::session::write_atomic(&sidecar_path(path), sidecar_line(spec, &meta).as_bytes())
}

pub fn sha256_file(path: &Path, cancel: Option<&AtomicBool>) -> io::Result<Option<String>> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Ok(None);
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(Some(hex(&h.finalize())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileCheck {
    Ok,
    Missing,
    /// The file had the wrong size or checksum and was moved to this `.bad` path (replacing
    /// any older one); it is removed once a good copy verifies.
    Quarantined(PathBuf),
}

impl FileCheck {
    /// User-facing explanation for a file that is not usable.
    pub fn describe(&self, spec: &ModelSpec) -> String {
        match self {
            FileCheck::Ok => format!("{} is verified", spec.file),
            FileCheck::Missing => format!("{} is not downloaded", spec.file),
            FileCheck::Quarantined(bad) => format!(
                "{} failed its size/checksum check and was moved to {}",
                spec.file,
                bad.display()
            ),
        }
    }
}

fn bad_path(path: &Path) -> PathBuf {
    let mut bad = path.as_os_str().to_owned();
    bad.push(".bad");
    PathBuf::from(bad)
}

/// Startup verification of one file: size + sidecar match is OK without hashing; otherwise the
/// file is hashed, a match writes the sidecar and a mismatch renames the file to `.bad`.
pub fn check_file(dir: &Path, spec: &ModelSpec) -> io::Result<FileCheck> {
    let path = dir.join(spec.file);
    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(FileCheck::Missing),
        Err(e) => return Err(e),
    };
    if meta.len() != spec.bytes {
        return Ok(quarantine(&path));
    }
    if fs::read_to_string(sidecar_path(&path)).is_ok_and(|s| s.trim() == sidecar_line(spec, &meta))
    {
        remove_stale_bad(&path);
        return Ok(FileCheck::Ok);
    }
    match sha256_file(&path, None)? {
        Some(h) if h == spec.sha256 => {
            let _ = write_sidecar(&path, spec);
            remove_stale_bad(&path);
            Ok(FileCheck::Ok)
        }
        _ => Ok(quarantine(&path)),
    }
}

/// Moves `path` to `<path>.bad` (rename replaces an older `.bad`) and drops its sidecar.
fn quarantine(path: &Path) -> FileCheck {
    let bad = bad_path(path);
    let _ = fs::remove_file(sidecar_path(path));
    match fs::rename(path, &bad) {
        Ok(()) => {
            eprintln!(
                "chinstrap: {} failed its size/checksum check; moved to {}",
                path.display(),
                bad.display()
            );
            FileCheck::Quarantined(bad)
        }
        Err(e) => {
            eprintln!(
                "chinstrap: {} failed its size/checksum check and cannot be moved aside: {e}",
                path.display()
            );
            FileCheck::Missing
        }
    }
}

/// A good copy is in place: the quarantined one is only wasted disk.
fn remove_stale_bad(path: &Path) {
    let _ = fs::remove_file(bad_path(path));
}

/// Cheap check (no hashing): the file exists at the right size with a matching sidecar.
pub fn verified_quick(dir: &Path, spec: &ModelSpec) -> bool {
    let path = dir.join(spec.file);
    fs::metadata(&path).is_ok_and(|meta| {
        meta.len() == spec.bytes
            && fs::read_to_string(sidecar_path(&path))
                .is_ok_and(|s| s.trim() == sidecar_line(spec, &meta))
    })
}

/// The model state from what is on disk, without hashing (sidecars only). Used after a download
/// or a cancel, where every verified file has a fresh sidecar.
pub fn state_quick(resolved: &Resolved, dir: &Path) -> crate::studio::types::ModelState {
    use crate::studio::types::ModelState;
    let vad_ok = verified_quick(dir, &VAD);
    match resolved {
        Resolved::External { id, path } => {
            if path.is_file() {
                ModelState::External {
                    id: id.clone(),
                    path: path.clone(),
                }
            } else {
                ModelState::Failed {
                    reason: format!("CHINSTRAP_WHISPER_MODEL: {} not found", path.display()),
                    download_bytes: 0,
                }
            }
        }
        Resolved::Catalog(spec) => {
            if verified_quick(dir, spec) {
                ModelState::Ready {
                    id: spec.name.into(),
                    path: dir.join(spec.file),
                    vad_download: (!vad_ok).then_some(VAD.bytes),
                }
            } else {
                ModelState::Missing {
                    download_bytes: spec.bytes + if vad_ok { 0 } else { VAD.bytes },
                }
            }
        }
    }
}

/// The startup check (§12.5): hashes files without a valid sidecar. Never touches the network.
pub fn state_checked(resolved: &Resolved, dir: &Path) -> crate::studio::types::ModelState {
    if let Resolved::Catalog(spec) = resolved {
        for s in [*spec, &VAD] {
            if let Err(e) = check_file(dir, s) {
                eprintln!(
                    "chinstrap: cannot check {}: {e}",
                    dir.join(s.file).display()
                );
            }
        }
    } else if let Err(e) = check_file(dir, &VAD) {
        eprintln!(
            "chinstrap: cannot check {}: {e}",
            dir.join(VAD.file).display()
        );
    }
    state_quick(resolved, dir)
}

/// Files of `spec` (whisper first, then VAD) that do not verify, for `download`.
pub fn needed_files(dir: &Path, spec: &ModelSpec) -> Vec<DlFile> {
    [*spec, VAD]
        .into_iter()
        .filter(|s| !matches!(check_file(dir, s), Ok(FileCheck::Ok)))
        .map(|s| DlFile {
            url: url_for(&s),
            spec: s,
        })
        .collect()
}

/// The VAD model, if present and verified (sidecar).
pub fn vad_path(dir: &Path) -> Option<PathBuf> {
    verified_quick(dir, &VAD).then(|| dir.join(VAD.file))
}

// ---- download ------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct DlFile {
    pub spec: ModelSpec,
    pub url: String,
}

#[derive(Clone, Debug)]
pub struct DlOpts {
    pub chunk: u64,
    pub backoff: [Duration; 4],
    pub connect: Duration,
    pub recv_response: Duration,
    pub recv_body: Duration,
    pub progress_every: Duration,
}

impl Default for DlOpts {
    fn default() -> Self {
        Self {
            chunk: 8 << 20,
            backoff: [1, 2, 4, 8].map(Duration::from_secs),
            connect: Duration::from_secs(15),
            recv_response: Duration::from_secs(30),
            recv_body: Duration::from_secs(120),
            progress_every: Duration::from_millis(250),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DlEvent {
    Progress { received: u64, total: u64 },
    Verifying,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DlError {
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for DlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cancelled"),
            Self::Failed(r) => write!(f, "{r}"),
        }
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn io_fail(e: io::Error, remaining: u64) -> DlError {
    if e.raw_os_error() == Some(libc::ENOSPC) {
        DlError::Failed(format!(
            "disk full: need {} MiB",
            remaining.div_ceil(1 << 20)
        ))
    } else {
        DlError::Failed(e.to_string())
    }
}

/// Parses `bytes a-b/total`.
fn content_range(v: &str) -> Option<(u64, u64, u64)> {
    let rest = v.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    Some((
        a.trim().parse().ok()?,
        b.trim().parse().ok()?,
        total.trim().parse().ok()?,
    ))
}

enum Attempt {
    /// The requested range was fully written.
    Done,
    /// Failed after writing `advanced` bytes (0 = no progress).
    Retry {
        advanced: u64,
        reason: String,
        was_200: bool,
    },
}

struct Progress<'a> {
    base: u64,
    total: u64,
    last: Instant,
    every: Duration,
    on: &'a mut dyn FnMut(DlEvent),
}

impl Progress<'_> {
    fn report(&mut self, file_pos: u64, force: bool) {
        if force || self.last.elapsed() >= self.every {
            self.last = Instant::now();
            (self.on)(DlEvent::Progress {
                received: self.base + file_pos,
                total: self.total,
            });
        }
    }
}

/// Downloads `files` into `dir` (only call it with files that do not verify), resuming any
/// `.part`. See §12.5 for the state machine.
pub fn download(
    dir: &Path,
    files: &[DlFile],
    opts: &DlOpts,
    cancel: &AtomicBool,
    on: &mut dyn FnMut(DlEvent),
) -> Result<(), DlError> {
    ensure_dir(dir)
        .map_err(|e| DlError::Failed(format!("cannot create {}: {e}", dir.display())))?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(opts.connect))
        .timeout_recv_response(Some(opts.recv_response))
        .timeout_recv_body(Some(opts.recv_body))
        .max_redirects(10)
        .http_status_as_error(false)
        .user_agent(format!("chinstrap/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let total: u64 = files.iter().map(|f| f.spec.bytes).sum();
    let mut progress = Progress {
        base: 0,
        total,
        last: Instant::now() - opts.progress_every,
        every: opts.progress_every,
        on,
    };
    for f in files {
        download_one(&agent, dir, f, opts, cancel, &mut progress)?;
        progress.base += f.spec.bytes;
    }
    Ok(())
}

fn sleep_cancellable(d: Duration, cancel: &AtomicBool) -> bool {
    let end = Instant::now() + d;
    while Instant::now() < end {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20).min(end - Instant::now()));
    }
    !cancel.load(Ordering::Relaxed)
}

fn download_one(
    agent: &ureq::Agent,
    dir: &Path,
    f: &DlFile,
    opts: &DlOpts,
    cancel: &AtomicBool,
    progress: &mut Progress,
) -> Result<(), DlError> {
    let spec = f.spec;
    let dest = dir.join(spec.file);
    let part = part_path(&dest);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&part)
        .map_err(|e| io_fail(e, spec.bytes))?;
    // SAFETY: flock on an fd we own; released when `file` is closed.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(DlError::Failed("another download is running".into()));
    }
    let mut len = file.metadata().map_err(|e| io_fail(e, spec.bytes))?.len();
    if len > spec.bytes {
        file.set_len(0).map_err(|e| io_fail(e, spec.bytes))?;
        len = 0;
    }
    // Resume: hash the existing prefix.
    let mut hasher = Sha256::new();
    {
        file.seek(SeekFrom::Start(0))
            .map_err(|e| io_fail(e, spec.bytes))?;
        let mut left = len;
        let mut buf = vec![0u8; 1 << 20];
        while left > 0 {
            if cancel.load(Ordering::Relaxed) {
                return Err(DlError::Cancelled);
            }
            let want = left.min(buf.len() as u64) as usize;
            let n = file
                .read(&mut buf[..want])
                .map_err(|e| io_fail(e, spec.bytes))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            left -= n as u64;
        }
    }
    progress.report(len, true);
    let mut zero_fails = 0usize;
    let mut fails_200 = 0usize;
    while len < spec.bytes {
        if cancel.load(Ordering::Relaxed) {
            return Err(DlError::Cancelled);
        }
        let end = (len + opts.chunk).min(spec.bytes) - 1;
        let before = len;
        match fetch_range(
            agent,
            &f.url,
            spec,
            len,
            end,
            &mut file,
            &mut hasher,
            &mut len,
            cancel,
            progress,
        )? {
            Attempt::Done => {
                zero_fails = 0;
                fails_200 = 0;
            }
            Attempt::Retry {
                advanced,
                reason,
                was_200,
            } => {
                debug_assert_eq!(advanced, len - before);
                if was_200 {
                    fails_200 += 1;
                    if fails_200 >= 4 {
                        return Err(DlError::Failed(
                            "server does not support resumable download".into(),
                        ));
                    }
                } else {
                    fails_200 = 0;
                }
                if advanced > 0 {
                    zero_fails = 0;
                } else {
                    zero_fails += 1;
                    if zero_fails >= 4 && !was_200 {
                        return Err(DlError::Failed(reason));
                    }
                }
                let wait = opts.backoff[(zero_fails.max(fails_200)).min(4).saturating_sub(1)];
                eprintln!(
                    "chinstrap: model download: {reason}; retrying in {:.0?}",
                    wait
                );
                if !sleep_cancellable(wait, cancel) {
                    return Err(DlError::Cancelled);
                }
            }
        }
    }
    progress.report(len, true);
    (progress.on)(DlEvent::Verifying);
    let digest = hex(&hasher.finalize());
    if digest != spec.sha256 {
        drop(file);
        let _ = fs::remove_file(&part);
        return Err(DlError::Failed("checksum mismatch".into()));
    }
    file.sync_all().map_err(|e| io_fail(e, 0))?;
    fs::rename(&part, &dest).map_err(|e| io_fail(e, 0))?;
    drop(file);
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| io_fail(e, 0))?;
    write_sidecar(&dest, &spec).map_err(|e| io_fail(e, 0))?;
    remove_stale_bad(&dest);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn fetch_range(
    agent: &ureq::Agent,
    url: &str,
    spec: ModelSpec,
    start: u64,
    end: u64,
    file: &mut File,
    hasher: &mut Sha256,
    len: &mut u64,
    cancel: &AtomicBool,
    progress: &mut Progress,
) -> Result<Attempt, DlError> {
    let retry = |advanced: u64, reason: String| Attempt::Retry {
        advanced,
        reason,
        was_200: false,
    };
    let mut resp = match agent
        .get(url)
        .header("Range", format!("bytes={start}-{end}"))
        .call()
    {
        Ok(r) => r,
        Err(e) => return Ok(retry(0, format!("request failed: {e}"))),
    };
    let status = resp.status().as_u16();
    match status {
        206 => {}
        200 => {
            // Range ignored: never stream a full body; close and count the attempt.
            return Ok(Attempt::Retry {
                advanced: 0,
                reason: "server ignored the Range request (HTTP 200)".into(),
                was_200: true,
            });
        }
        s if (500..600).contains(&s) || s == 429 || s == 408 => {
            return Ok(retry(0, format!("HTTP {s}")));
        }
        s => return Err(DlError::Failed(format!("download failed: HTTP {s}"))),
    }
    let range = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(content_range);
    match range {
        Some((a, _, total)) if a == start && total == spec.bytes => {}
        Some((a, b, total)) => {
            return Err(DlError::Failed(format!(
                "unexpected Content-Range bytes {a}-{b}/{total} (wanted {start}-…/{})",
                spec.bytes
            )));
        }
        None => {
            return Err(DlError::Failed(
                "206 response without a valid Content-Range".into(),
            ));
        }
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|e| io_fail(e, spec.bytes - start))?;
    let want = end + 1 - start;
    let mut got = 0u64;
    let mut buf = vec![0u8; 256 << 10];
    let mut reader = resp.body_mut().as_reader();
    while got < want {
        if cancel.load(Ordering::Relaxed) {
            let _ = file.flush();
            return Err(DlError::Cancelled);
        }
        let cap = (want - got).min(buf.len() as u64) as usize;
        let n = match reader.read(&mut buf[..cap]) {
            Ok(0) => {
                return Ok(retry(
                    got,
                    format!("connection closed after {got} of {want} bytes"),
                ));
            }
            Ok(n) => n,
            Err(e) => return Ok(retry(got, format!("read failed: {e}"))),
        };
        file.write_all(&buf[..n])
            .map_err(|e| io_fail(e, spec.bytes - *len))?;
        hasher.update(&buf[..n]);
        got += n as u64;
        *len += n as u64;
        progress.report(*len, false);
    }
    Ok(Attempt::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "chinstrap-model-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn payload(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn sha(b: &[u8]) -> &'static str {
        Box::leak(hex(&Sha256::digest(b)).into_boxed_str())
    }

    fn spec_for(data: &[u8], file: &'static str) -> ModelSpec {
        ModelSpec {
            name: "test",
            file,
            bytes: data.len() as u64,
            sha256: sha(data),
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Range,
        IgnoreRange,
        Status416,
        WrongTotal,
        /// Sends at most this many bytes per response, slowly, then drops the connection.
        Trickle(usize),
        /// Sends 256 KiB per response, slowly, so a cancel lands mid-chunk.
        Slow,
    }

    /// Minimal HTTP/1.1 server: one request per connection.
    fn serve(data: Vec<u8>, mode: Mode) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model.bin", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                h.fetch_add(1, Ordering::SeqCst);
                let data = data.clone();
                std::thread::spawn(move || handle(conn, &data, mode));
            }
        });
        (url, hits)
    }

    fn handle(mut conn: TcpStream, data: &[u8], mode: Mode) {
        let mut reader = BufReader::new(conn.try_clone().unwrap());
        let mut range = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            if let Some(v) = l.to_ascii_lowercase().strip_prefix("range: bytes=") {
                let (a, b) = v.split_once('-').unwrap();
                range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()));
            }
        }
        let total = data.len();
        let (a, b) = range.unwrap_or((0, total - 1));
        let b = b.min(total - 1);
        let w = |conn: &mut TcpStream, head: String, body: &[u8]| {
            let _ = conn.write_all(head.as_bytes());
            let _ = conn.write_all(body);
        };
        match mode {
            Mode::Status416 => w(
                &mut conn,
                "HTTP/1.1 416 Range Not Satisfiable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    .into(),
                &[],
            ),
            Mode::IgnoreRange => w(
                &mut conn,
                format!("HTTP/1.1 200 OK\r\ncontent-length: {total}\r\nconnection: close\r\n\r\n"),
                data,
            ),
            Mode::Range | Mode::WrongTotal => {
                let t = if mode == Mode::WrongTotal { total + 1 } else { total };
                w(
                    &mut conn,
                    format!(
                        "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {a}-{b}/{t}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        b + 1 - a
                    ),
                    &data[a..=b],
                )
            }
            Mode::Trickle(_) | Mode::Slow => {
                let max = if let Mode::Trickle(m) = mode { m } else { 256 << 10 };
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {a}-{b}/{total}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    b + 1 - a
                );
                let _ = conn.write_all(head.as_bytes());
                let end = (a + max).min(b + 1);
                for chunk in data[a..end].chunks(1024) {
                    if conn.write_all(chunk).is_err() {
                        return;
                    }
                    let _ = conn.flush();
                    std::thread::sleep(Duration::from_millis(if mode == Mode::Slow {
                        2
                    } else {
                        20
                    }));
                }
                if mode != Mode::Slow {
                    // Stall past the client's recv_body budget, then drop: a short body.
                    std::thread::sleep(Duration::from_millis(1200));
                }
            }
        }
    }

    fn fast_opts(chunk: u64) -> DlOpts {
        DlOpts {
            chunk,
            backoff: [Duration::from_millis(10); 4],
            connect: Duration::from_secs(5),
            recv_response: Duration::from_secs(5),
            recv_body: Duration::from_secs(5),
            progress_every: Duration::ZERO,
        }
    }

    fn run(
        dir: &Path,
        spec: ModelSpec,
        url: &str,
        opts: &DlOpts,
        cancel: &AtomicBool,
    ) -> (Result<(), DlError>, Vec<DlEvent>) {
        let mut events = Vec::new();
        let r = download(
            dir,
            &[DlFile {
                spec,
                url: url.into(),
            }],
            opts,
            cancel,
            &mut |e| events.push(e),
        );
        (r, events)
    }

    #[test]
    fn range_download_verifies_and_writes_sidecar() {
        let data = payload(300_000);
        let spec = spec_for(&data, "a.bin");
        let (url, hits) = serve(data.clone(), Mode::Range);
        let dir = scratch("range");
        let (r, ev) = run(
            &dir,
            spec,
            &url,
            &fast_opts(64 << 10),
            &AtomicBool::new(false),
        );
        r.unwrap();
        assert_eq!(fs::read(dir.join("a.bin")).unwrap(), data);
        assert!(!dir.join("a.bin.part").exists());
        assert!(verified_quick(&dir, &spec));
        assert_eq!(check_file(&dir, &spec).unwrap(), FileCheck::Ok);
        assert!(
            hits.load(Ordering::SeqCst) >= 5,
            "chunked into 64 KiB ranges"
        );
        assert!(ev.contains(&DlEvent::Verifying));
        assert!(matches!(ev.last(), Some(DlEvent::Verifying)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ignored_range_416_and_wrong_total_fail() {
        let data = payload(10_000);
        let spec = spec_for(&data, "b.bin");
        let dir = scratch("errors");
        let no = AtomicBool::new(false);
        let (url, hits) = serve(data.clone(), Mode::IgnoreRange);
        let (r, _) = run(&dir, spec, &url, &fast_opts(4096), &no);
        assert_eq!(
            r,
            Err(DlError::Failed(
                "server does not support resumable download".into()
            ))
        );
        assert_eq!(hits.load(Ordering::SeqCst), 4);
        let (url, _) = serve(data.clone(), Mode::Status416);
        let (r, _) = run(&dir, spec, &url, &fast_opts(4096), &no);
        assert!(matches!(r, Err(DlError::Failed(m)) if m.contains("416")));
        let (url, _) = serve(data.clone(), Mode::WrongTotal);
        let (r, _) = run(&dir, spec, &url, &fast_opts(4096), &no);
        assert!(matches!(r, Err(DlError::Failed(m)) if m.contains("Content-Range")));
        assert!(!dir.join("b.bin").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hash_mismatch_deletes_part() {
        let data = payload(20_000);
        let mut spec = spec_for(&data, "c.bin");
        spec.sha256 = "00";
        let (url, _) = serve(data, Mode::Range);
        let dir = scratch("mismatch");
        let (r, _) = run(&dir, spec, &url, &fast_opts(8192), &AtomicBool::new(false));
        assert_eq!(r, Err(DlError::Failed("checksum mismatch".into())));
        assert!(!dir.join("c.bin.part").exists() && !dir.join("c.bin").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resumes_from_partial_part() {
        let data = payload(100_000);
        let spec = spec_for(&data, "d.bin");
        let dir = scratch("resume");
        fs::write(dir.join("d.bin.part"), &data[..40_000]).unwrap();
        let (url, _) = serve(data.clone(), Mode::Range);
        let (r, ev) = run(
            &dir,
            spec,
            &url,
            &fast_opts(1 << 20),
            &AtomicBool::new(false),
        );
        r.unwrap();
        assert_eq!(
            ev.first(),
            Some(&DlEvent::Progress {
                received: 40_000,
                total: 100_000
            }),
            "the existing prefix counts"
        );
        assert_eq!(fs::read(dir.join("d.bin")).unwrap(), data);
        // An oversized .part is discarded.
        fs::remove_file(dir.join("d.bin")).unwrap();
        fs::write(dir.join("d.bin.part"), payload(200_000)).unwrap();
        let (r, _) = run(
            &dir,
            spec,
            &url,
            &fast_opts(1 << 20),
            &AtomicBool::new(false),
        );
        r.unwrap();
        assert_eq!(fs::read(dir.join("d.bin")).unwrap(), data);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancel_mid_chunk_then_immediate_redownload() {
        let data = payload(4 << 20);
        let spec = spec_for(&data, "e.bin");
        let (url, _) = serve(data.clone(), Mode::Slow);
        let dir = scratch("cancel");
        let cancel = Arc::new(AtomicBool::new(false));
        let (d2, u2, c2) = (dir.clone(), url.clone(), cancel.clone());
        let t = std::thread::spawn(move || run(&d2, spec, &u2, &fast_opts(4 << 20), &c2).0);
        std::thread::sleep(Duration::from_millis(300));
        cancel.store(true, Ordering::SeqCst);
        let t0 = Instant::now();
        assert_eq!(t.join().unwrap(), Err(DlError::Cancelled));
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "exits at the next buffer"
        );
        let kept = fs::metadata(dir.join("e.bin.part")).unwrap().len();
        assert!(kept > 0 && kept < data.len() as u64, ".part kept: {kept}");
        // The thread has exited (the engine waits for ModelEvent::Exited): no lock conflict.
        let (url2, _) = serve(data.clone(), Mode::Range);
        let (r, ev) = run(
            &dir,
            spec,
            &url2,
            &fast_opts(4 << 20),
            &AtomicBool::new(false),
        );
        r.unwrap();
        assert!(
            matches!(ev.first(), Some(DlEvent::Progress { received, .. }) if *received == kept)
        );
        assert_eq!(fs::read(dir.join("e.bin")).unwrap(), data);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trickle_with_stalls_completes() {
        // Every response trickles 2 KiB at ~50 KiB/s and then stalls past recv_body (1 s): each
        // attempt times out but makes progress, so the retry budget keeps resetting and the
        // download completes (more than 4 failed attempts in a row).
        let data = payload(12 << 10);
        let spec = spec_for(&data, "f.bin");
        let (url, hits) = serve(data.clone(), Mode::Trickle(2048));
        let dir = scratch("trickle");
        let mut opts = fast_opts(8 << 10);
        opts.recv_body = Duration::from_secs(1);
        let (r, _) = run(&dir, spec, &url, &opts, &AtomicBool::new(false));
        r.unwrap();
        assert_eq!(fs::read(dir.join("f.bin")).unwrap(), data);
        assert!(hits.load(Ordering::SeqCst) >= 6);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn busy_and_stale_locks() {
        let data = payload(10_000);
        let spec = spec_for(&data, "g.bin");
        let dir = scratch("lock");
        let (url, _) = serve(data.clone(), Mode::Range);
        // A live lock holder (another download) → Failed.
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("g.bin.part"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let (r, _) = run(&dir, spec, &url, &fast_opts(4096), &AtomicBool::new(false));
        assert_eq!(
            r,
            Err(DlError::Failed("another download is running".into()))
        );
        // The holder goes away (a crashed process's flock dies with it): a stale .part with no
        // lock holder is simply resumed.
        drop(holder);
        let (r, _) = run(&dir, spec, &url, &fast_opts(4096), &AtomicBool::new(false));
        r.unwrap();
        assert_eq!(fs::read(dir.join("g.bin")).unwrap(), data);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn check_file_sidecar_and_quarantine() {
        let data = payload(5000);
        let spec = spec_for(&data, "h.bin");
        let dir = scratch("check");
        assert_eq!(check_file(&dir, &spec).unwrap(), FileCheck::Missing);
        fs::write(dir.join("h.bin"), &data).unwrap();
        assert!(!verified_quick(&dir, &spec));
        assert_eq!(check_file(&dir, &spec).unwrap(), FileCheck::Ok);
        assert!(verified_quick(&dir, &spec), "sidecar written");
        // Same size, wrong content, stale sidecar (mtime changes) → .bad.
        std::thread::sleep(Duration::from_millis(10));
        let mut bad = data.clone();
        bad[0] ^= 1;
        fs::write(dir.join("h.bin"), &bad).unwrap();
        assert_eq!(
            check_file(&dir, &spec).unwrap(),
            FileCheck::Quarantined(dir.join("h.bin.bad"))
        );
        assert!(dir.join("h.bin.bad").exists() && !dir.join("h.bin").exists());
        assert!(
            FileCheck::Quarantined(dir.join("h.bin.bad"))
                .describe(&spec)
                .contains("checksum")
        );
        // A second bad copy replaces the first .bad; a good copy removes it.
        fs::write(dir.join("h.bin"), &bad).unwrap();
        assert!(matches!(
            check_file(&dir, &spec).unwrap(),
            FileCheck::Quarantined(_)
        ));
        fs::write(dir.join("h.bin"), &data).unwrap();
        assert_eq!(check_file(&dir, &spec).unwrap(), FileCheck::Ok);
        assert!(!dir.join("h.bin.bad").exists(), "stale .bad removed");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolution() {
        assert_eq!(resolve(None).0, Resolved::Catalog(&MODELS[0]));
        assert_eq!(
            resolve(Some("large-v3-turbo")).0,
            Resolved::Catalog(&MODELS[2])
        );
        let (r, w) = resolve(Some("tiny"));
        assert_eq!(r, Resolved::Catalog(&MODELS[0]));
        assert!(w.is_some());
        match resolve(Some("/m/ggml-small.en.bin")).0 {
            Resolved::External { id, path } => {
                assert_eq!(id, "small.en");
                assert_eq!(path, Path::new("/m/ggml-small.en.bin"));
            }
            r => panic!("{r:?}"),
        }
        assert_eq!(
            content_range("bytes 100-199/874188075"),
            Some((100, 199, 874_188_075))
        );
        assert!(url_for(&VAD).starts_with(HF_VAD));
        assert!(url_for(&MODELS[0]).ends_with("ggml-large-v3-turbo-q8_0.bin"));
    }
}
