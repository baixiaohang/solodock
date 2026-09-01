use std::{fs, path::Path};

use serde::Serialize;

const MEMINFO_PATH: &str = "/proc/meminfo";

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct MemorySnapshot {
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    pub used_percent: Option<f64>,
}

impl MemorySnapshot {
    pub const fn unknown() -> Self {
        Self {
            total_bytes: None,
            available_bytes: None,
            used_percent: None,
        }
    }
}

pub fn probe() -> MemorySnapshot {
    probe_path(Path::new(MEMINFO_PATH))
}

fn probe_path(path: &Path) -> MemorySnapshot {
    let Ok(contents) = fs::read_to_string(path) else {
        return MemorySnapshot::unknown();
    };
    parse(&contents).unwrap_or_else(MemorySnapshot::unknown)
}

fn parse(contents: &str) -> Option<MemorySnapshot> {
    let mut total_kib = None;
    let mut available_kib = None;
    for line in contents.lines() {
        let (name, value) = line.split_once(':')?;
        if name != "MemTotal" && name != "MemAvailable" {
            continue;
        }
        let mut fields = value.split_whitespace();
        let kib = fields.next()?.parse::<u64>().ok()?;
        if fields.next() != Some("kB") || fields.next().is_some() {
            return None;
        }
        match name {
            "MemTotal" => total_kib = Some(kib),
            "MemAvailable" => available_kib = Some(kib),
            _ => unreachable!(),
        }
    }
    let total_bytes = total_kib?.checked_mul(1024)?;
    let available_bytes = available_kib?.checked_mul(1024)?;
    if total_bytes == 0 || available_bytes > total_bytes {
        return None;
    }
    Some(MemorySnapshot {
        total_bytes: Some(total_bytes),
        available_bytes: Some(available_bytes),
        used_percent: Some(
            total_bytes.saturating_sub(available_bytes) as f64 / total_bytes as f64 * 100.0,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_linux_memavailable() {
        let snapshot =
            parse("MemTotal:       1024000 kB\nMemFree: 1 kB\nMemAvailable:    256000 kB\n")
                .unwrap();
        assert_eq!(snapshot.total_bytes, Some(1_048_576_000));
        assert_eq!(snapshot.available_bytes, Some(262_144_000));
        assert_eq!(snapshot.used_percent, Some(75.0));
    }

    #[test]
    fn rejects_missing_invalid_and_overflowing_values() {
        assert!(parse("MemTotal: 1024 kB\n").is_none());
        assert!(parse("MemTotal: nope kB\nMemAvailable: 1 kB\n").is_none());
        assert!(parse("MemTotal: 1 MB\nMemAvailable: 1 kB\n").is_none());
        assert!(parse(&format!("MemTotal: {} kB\nMemAvailable: 1 kB\n", u64::MAX)).is_none());
        assert!(parse("MemTotal: 1 kB\nMemAvailable: 2 kB\n").is_none());
    }
}
