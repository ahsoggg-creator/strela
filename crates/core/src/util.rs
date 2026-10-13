//! Small formatting helpers shared by the CLI and the GUI.

use std::path::PathBuf;
use std::time::Duration;

/// `%LOCALAPPDATA%\Strela` — settings, saved index, job queue. Created on demand by the callers.
/// The `STRELA_DATA_DIR` environment variable overrides it (tests, screenshots, portable use).
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("STRELA_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Strela")
}

/// `1.5 ГБ`, `312 КБ`, `17 Б`.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} Б")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

pub fn speed(bps: f64) -> String {
    format!("{}/с", bytes(bps.max(0.0) as u64))
}

/// `1:05`, `1:02:03`, `0:07`.
pub fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// Unix seconds -> `2024-05-17 13:45` in the local time zone.
pub fn date(unix: u32) -> String {
    if unix == 0 {
        return String::new();
    }
    match crate::winapi::unix_to_local(unix) {
        Some((y, m, d, h, min)) => format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}"),
        None => date_utc(unix),
    }
}

/// Unix seconds -> `2024-05-17 13:45` (UTC).
pub fn date_utc(unix: u32) -> String {
    if unix == 0 {
        return String::new();
    }
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // civil from days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, secs / 60 % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(bytes(17), "17 Б");
        assert_eq!(bytes(1536), "1.50 КБ");
        assert_eq!(bytes(5 << 30), "5.00 ГБ");
        assert_eq!(bytes(250 << 20), "250 МБ");
        assert_eq!(clock(Duration::from_secs(65)), "1:05");
        assert_eq!(clock(Duration::from_secs(3723)), "1:02:03");
        assert_eq!(date_utc(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(date(1_700_000_000).len(), 16);
        assert_eq!(date(0), "");
    }
}
