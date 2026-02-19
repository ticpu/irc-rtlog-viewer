use std::path::Path;

use crate::parser::{LogFormat, LogLine, parse_line};
use crate::server::read_log_file;

pub fn search_channel(
    dir: &Path,
    format: LogFormat,
    query: &str,
    limit: usize,
) -> Vec<(String, LogLine)> {
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();
    let mut dates = list_dates(dir);
    dates.sort_unstable();
    dates.reverse();

    for date in dates {
        let path = dir.join(format!("{date}.log"));
        let zst_path = dir.join(format!("{date}.log.zst"));
        let file_path = if path.exists() { &path } else { &zst_path };

        let content = match read_log_file(file_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for raw_line in content.lines() {
            if raw_line.to_lowercase().contains(&query_lower) {
                if let Some(parsed) = parse_line(raw_line, format) {
                    results.push((date.clone(), parsed));
                    if results.len() >= limit {
                        return results;
                    }
                }
            }
        }
    }

    results
}

fn list_dates(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let date = name.strip_suffix(".log")
                .or_else(|| name.strip_suffix(".log.zst"))?;
            if date.len() == 10 {
                Some(date.to_string())
            } else {
                None
            }
        })
        .collect()
}
