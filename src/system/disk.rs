use std::{ffi::CString, os::unix::ffi::OsStrExt, path::Path};

use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskStatus {
    Normal,
    Warning,
    Critical,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiskSnapshot {
    pub status: DiskStatus,
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    pub used_percent: Option<f64>,
}

impl DiskSnapshot {
    pub fn unknown() -> Self {
        Self {
            status: DiskStatus::Unknown,
            total_bytes: None,
            available_bytes: None,
            used_percent: None,
        }
    }
}

pub fn probe(path: &Path) -> DiskSnapshot {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return DiskSnapshot::unknown();
    };
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid NUL-terminated byte string and `stats` points to writable memory.
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return DiskSnapshot::unknown();
    }
    // SAFETY: statvfs returned success and initialized the output structure.
    let stats = unsafe { stats.assume_init() };
    let block_size = stats.f_frsize;
    let total = stats.f_blocks.saturating_mul(block_size);
    let available = stats.f_bavail.saturating_mul(block_size);
    if total == 0 {
        return DiskSnapshot::unknown();
    }
    let used_percent = (total.saturating_sub(available)) as f64 / total as f64 * 100.0;
    let status = if used_percent >= 90.0 {
        DiskStatus::Critical
    } else if used_percent >= 80.0 {
        DiskStatus::Warning
    } else {
        DiskStatus::Normal
    };
    DiskSnapshot {
        status,
        total_bytes: Some(total),
        available_bytes: Some(available),
        used_percent: Some(used_percent),
    }
}
