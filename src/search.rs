use crate::Channel;
use crate::parser::{LogLine, parse_line};
use crate::server::{channel_dates, resolve_log_path, read_log_file};

pub fn search_channel(
    channel: &Channel,
    query: &str,
    limit: usize,
) -> Vec<(String, LogLine)> {
    let query_lower = query.to_lowercase();
    let mut results = Vec::new();
    let mut dates = channel_dates(channel);
    dates.reverse();

    for date in dates {
        let Some((path, format)) = resolve_log_path(channel, &date) else { continue };
        let Ok(content) = read_log_file(&path) else { continue };

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
