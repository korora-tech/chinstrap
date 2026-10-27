//! Disk-space planning and filesystem queries for recording preflight and monitoring.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DEFAULT_MIN_FREE_MB: u64 = 1024;
pub const EMERGENCY_RESERVE_BYTES: u64 = 64 * 1024 * 1024;

/// Syncs a directory entry so creates, renames, and unlinks survive a crash.
pub(crate) fn sync_directory(directory: &Path) -> io::Result<()> {
    File::open(directory)?.sync_all()
}

/// Creates a physically allocated emergency file which can be removed to free space for EOS.
pub fn create_emergency_reserve(directory: &Path, bytes: u64) -> io::Result<PathBuf> {
    create_emergency_reserve_with_sync(directory, bytes, &sync_directory)
}

fn create_emergency_reserve_with_sync(
    directory: &Path,
    bytes: u64,
    sync_dir: &impl Fn(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let path = directory.join(".chinstrap-reserve");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let result = (|| {
        let result = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, bytes as libc::off_t) };
        if result != 0 {
            if matches!(result, libc::EOPNOTSUPP | libc::ENOSYS | libc::EINVAL) {
                file.set_len(0)?;
                let block = vec![0u8; 1024 * 1024];
                let mut remaining = bytes;
                while remaining > 0 {
                    let len = remaining.min(block.len() as u64) as usize;
                    file.write_all(&block[..len])?;
                    remaining -= len as u64;
                }
            } else {
                return Err(io::Error::from_raw_os_error(result));
            }
        }
        file.sync_all()?;
        sync_dir(directory)?;
        Ok(())
    })();
    if let Err(error) = result {
        drop(file);
        let _ = fs::remove_file(&path);
        let _ = sync_dir(directory);
        return Err(error);
    }
    Ok(path)
}

pub fn release_emergency_reserve(path: &Path) -> io::Result<()> {
    release_emergency_reserve_with_sync(path, &sync_directory)
}

fn release_emergency_reserve_with_sync(
    path: &Path,
    sync_dir: &impl Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Parses `CHINSTRAP_MIN_FREE_MB`, defaulting to 1 GiB for missing, zero, or invalid values.
pub fn parse_minimum_free_mb(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MIN_FREE_MB)
}

/// Returns the stop reserve: the configured minimum or one minute at the estimated rate,
/// whichever is larger. `minimum_mb` is in MiB.
pub fn stop_reserve_bytes(bytes_per_second: u64, minimum_mb: u64) -> u64 {
    let minimum = minimum_mb.saturating_mul(1024 * 1024);
    minimum.max(bytes_per_second.saturating_mul(60))
}

/// Worst-case planned output rate plus the 30% encoder headroom, in bytes per second.
pub fn estimate_bytes_per_second(video_kbps: &[u32], microphone_selected: bool) -> u64 {
    let video_rate = video_kbps
        .iter()
        .fold(0u64, |sum, rate| sum.saturating_add(*rate as u64));
    let total_kbps = video_rate.saturating_add(if microphone_selected { 192 } else { 0 });
    let numerator = total_kbps.saturating_mul(1000).saturating_mul(130);
    numerator.saturating_add(799) / 800
}

/// Remaining whole seconds at `bytes_per_second`, or `None` if the rate is zero.
pub fn remaining_seconds(free_bytes: u64, bytes_per_second: u64) -> Option<u64> {
    (bytes_per_second > 0).then(|| free_bytes / bytes_per_second)
}

/// Warning band based on time remaining until the stop reserve: 0 normal, 1 under 10 min,
/// 2 under 3 min.
pub fn warning_level(remaining_seconds: u64) -> u8 {
    if remaining_seconds <= 180 {
        2
    } else if remaining_seconds <= 600 {
        1
    } else {
        0
    }
}

pub fn stop_threshold_reached(free_bytes: u64, reserve_bytes: u64) -> bool {
    free_bytes <= reserve_bytes
}

/// Tracks recent on-disk growth for one active segment, sampled at most once per second.
/// Keeping a short rolling window avoids treating a single burst or a quiet static scene as a
/// permanent bitrate while still adapting storage warnings to real muxed output.
#[derive(Default)]
pub struct StorageRateTracker {
    segment: Option<u32>,
    samples: VecDeque<(Instant, u64)>,
}

impl StorageRateTracker {
    pub fn observe(&mut self, segment: u32, bytes: u64, at: Instant) -> Option<u64> {
        if self.segment != Some(segment) {
            self.segment = Some(segment);
            self.samples.clear();
        }
        if let Some((last_at, last_bytes)) = self.samples.back().copied() {
            if at.saturating_duration_since(last_at) < Duration::from_secs(1) {
                return self.rate();
            }
            if bytes < last_bytes {
                self.samples.clear();
            }
        }
        self.samples.push_back((at, bytes));
        while self.samples.len() > 2
            && self.samples.front().is_some_and(|(sample_at, _)| {
                at.saturating_duration_since(*sample_at) > Duration::from_secs(10)
            })
        {
            self.samples.pop_front();
        }
        self.rate()
    }

    fn rate(&self) -> Option<u64> {
        let (start_at, start_bytes) = self.samples.front().copied()?;
        let (end_at, end_bytes) = self.samples.back().copied()?;
        let elapsed = end_at.saturating_duration_since(start_at).as_nanos();
        if elapsed < 1_000_000_000 {
            return None;
        }
        let growth = end_bytes.saturating_sub(start_bytes) as u128;
        Some((growth.saturating_mul(1_000_000_000) / elapsed).min(u64::MAX as u128) as u64)
    }
}

/// Available bytes on the filesystem containing `path`. If `path` does not exist, uses its
/// closest existing directory ancestor.
pub fn available_bytes(path: &Path) -> io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let mut dir = path.to_path_buf();
    while !dir.is_dir() {
        let Some(parent) = dir.parent() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no existing directory ancestor for output path",
            ));
        };
        if parent == dir {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no existing directory ancestor for output path",
            ));
        }
        dir = parent.to_path_buf();
    }
    let path = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stats` points to writable storage for statvfs.
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statvfs initialized `stats` on success.
    let stats = unsafe { stats.assume_init() };
    let bytes = (stats.f_bavail as u128).saturating_mul(stats.f_frsize as u128);
    Ok(bytes.min(u64::MAX as u128) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_storage_rate_uses_real_growth_and_resets_for_each_segment() {
        let start = Instant::now();
        let mut tracker = StorageRateTracker::default();
        assert_eq!(tracker.observe(1, 0, start), None);
        assert_eq!(
            tracker.observe(1, 1_000, start + Duration::from_secs(1)),
            Some(1_000)
        );
        assert_eq!(
            tracker.observe(1, 2_000, start + Duration::from_secs(2)),
            Some(1_000)
        );
        assert_eq!(tracker.observe(2, 0, start + Duration::from_secs(3)), None);
        assert_eq!(
            tracker.observe(2, 4_000, start + Duration::from_secs(5)),
            Some(2_000)
        );
    }

    #[test]
    fn invalid_or_zero_free_space_override_uses_safe_default() {
        assert_eq!(parse_minimum_free_mb(Some("64")), 64);
        assert_eq!(parse_minimum_free_mb(Some("0")), DEFAULT_MIN_FREE_MB);
        assert_eq!(
            parse_minimum_free_mb(Some("not-a-number")),
            DEFAULT_MIN_FREE_MB
        );
        assert_eq!(parse_minimum_free_mb(None), DEFAULT_MIN_FREE_MB);
    }

    #[test]
    fn estimate_includes_microphone_and_thirty_percent_margin() {
        assert_eq!(estimate_bytes_per_second(&[8_000], false), 1_300_000);
        assert_eq!(estimate_bytes_per_second(&[8_000], true), 1_331_200);
        assert_eq!(estimate_bytes_per_second(&[], false), 0);
    }

    #[test]
    fn stop_reserve_is_at_least_one_minute_of_recording_or_configured_minimum() {
        let mib = 1024 * 1024;
        assert_eq!(stop_reserve_bytes(1_000_000, 1024), 1024 * mib);
        assert_eq!(stop_reserve_bytes(20 * mib, 1024), 1200 * mib);
    }

    #[test]
    fn storage_warning_bands_use_ten_and_three_minutes() {
        assert_eq!(warning_level(601), 0);
        assert_eq!(warning_level(600), 1);
        assert_eq!(warning_level(181), 1);
        assert_eq!(warning_level(180), 2);
    }

    #[test]
    fn stop_threshold_includes_exact_reserve_boundary() {
        assert!(!stop_threshold_reached(1025, 1024));
        assert!(stop_threshold_reached(1024, 1024));
        assert!(stop_threshold_reached(1000, 1024));
    }

    #[test]
    fn remaining_space_uses_whole_seconds_and_zero_rate_is_unknown() {
        assert_eq!(remaining_seconds(7, 3), Some(2));
        assert_eq!(remaining_seconds(2, 3), Some(0));
        assert_eq!(remaining_seconds(100, 0), None);
    }

    #[test]
    fn reserve_operations_report_injected_directory_sync_failures() {
        let dir = std::env::temp_dir().join(format!(
            "chinstrap-reserve-sync-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fail_sync = |_: &Path| Err(io::Error::other("injected directory fsync failure"));

        assert!(create_emergency_reserve_with_sync(&dir, 1024, &fail_sync).is_err());
        let reserve = dir.join(".chinstrap-reserve");
        assert!(
            !reserve.exists(),
            "failed reserve creation must be cleaned up"
        );
        std::fs::write(&reserve, b"reserved").unwrap();
        assert!(release_emergency_reserve_with_sync(&reserve, &fail_sync).is_err());
        assert!(
            !reserve.exists(),
            "unlink must report the failed durability barrier"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn emergency_reserve_file_is_allocated_and_released_safely() {
        let dir = std::env::temp_dir().join(format!("chinstrap-reserve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = create_emergency_reserve(&dir, 1024 * 1024).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1024 * 1024);
        release_emergency_reserve(&path).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_output_path_uses_existing_filesystem_ancestor() {
        let root = std::env::current_dir().unwrap();
        let missing = root.join(format!("chinstrap-missing-output-{}", std::process::id()));
        assert!(available_bytes(&missing).unwrap() > 0);
        assert!(!missing.exists());
    }
}
