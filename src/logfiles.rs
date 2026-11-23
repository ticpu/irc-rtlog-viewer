//! Locating and reading a channel's day files, and the calendar dates they are named by.

use std::io::{self, BufReader, Read};
use std::path::Path;

use crate::parser::LogFormat;

/// A day file's text, decompressed when it ends in `.zst`. Invalid UTF-8 is
/// replaced with U+FFFD, as in the tail, so one stray byte does not cost the whole day.
pub fn read_log_file(path: &Path) -> io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();

    if path
        .extension()
        .and_then(|e| e.to_str())
        == Some("zst")
    {
        let mut decoder = zstd::Decoder::new(BufReader::new(file))?;
        decoder.read_to_end(&mut bytes)?;
    } else {
        let mut reader = BufReader::new(file);
        reader.read_to_end(&mut bytes)?;
    }

    Ok(String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

pub fn resolve_log_path(
    channel: &crate::Channel,
    date: &str,
) -> Option<(std::path::PathBuf, LogFormat)> {
    // Callers include the AI tools, whose date is model output.
    if !looks_like_date(date) {
        return None;
    }
    for dir in &channel.dirs {
        let plain = dir
            .path
            .join(format!("{date}.log"));
        if plain.exists() {
            return Some((plain, dir.format));
        }
        let zst = dir
            .path
            .join(format!("{date}.log.zst"));
        if zst.exists() {
            return Some((zst, dir.format));
        }
    }
    None
}

pub fn latest_date(channel: &crate::Channel) -> String {
    let dates = channel_dates(channel);
    dates
        .last()
        .cloned()
        .unwrap_or_else(today_date)
}

pub fn channel_dates(channel: &crate::Channel) -> Vec<String> {
    let mut dates = std::collections::BTreeSet::new();
    for dir in &channel.dirs {
        let Ok(entries) = std::fs::read_dir(&dir.path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry
                .file_name()
                .into_string()
            else {
                continue;
            };
            let date = name
                .strip_suffix(".log")
                .or_else(|| name.strip_suffix(".log.zst"));
            if let Some(d) = date {
                if d.len() == 10 {
                    dates.insert(d.to_string());
                }
            }
        }
    }
    dates
        .into_iter()
        .collect()
}

pub fn looks_like_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4]
            .iter()
            .all(u8::is_ascii_digit)
        && b[5..7]
            .iter()
            .all(u8::is_ascii_digit)
        && b[8..10]
            .iter()
            .all(u8::is_ascii_digit)
}

pub fn today_date() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days = now / 86400;
    epoch_days_to_date(days)
}

fn epoch_days_to_date(days: u64) -> String {
    // Civil date from days since epoch (Euclidean affine algorithm)
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}
