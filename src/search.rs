use crate::Channel;
use crate::parser::{LogLine, parse_line};
use crate::server::{channel_dates, read_log_file, resolve_log_path};

pub struct SearchResults {
    /// Matching lines with their date, newest day first.
    pub lines: Vec<(String, LogLine)>,
    /// Set to the day bound when older days were left unscanned.
    pub stopped_at_days: Option<usize>,
}

/// Case-insensitive substring search over the newest `max_days` days.
pub fn search_channel(
    channel: &Channel,
    query: &str,
    limit: usize,
    max_days: usize,
) -> SearchResults {
    let query_lower = query.to_lowercase();
    let mut lines = Vec::new();
    let mut dates = channel_dates(channel);
    dates.reverse();
    let stopped_at_days = (dates.len() > max_days).then_some(max_days);

    for date in dates
        .into_iter()
        .take(max_days)
    {
        let Some((path, format)) = resolve_log_path(channel, &date) else {
            continue;
        };
        let Ok(content) = read_log_file(&path) else {
            continue;
        };

        for raw_line in content.lines() {
            if raw_line
                .to_lowercase()
                .contains(&query_lower)
            {
                if let Some(parsed) = parse_line(raw_line, format) {
                    lines.push((date.clone(), parsed));
                    if lines.len() >= limit {
                        return SearchResults {
                            lines,
                            stopped_at_days: None,
                        };
                    }
                }
            }
        }
    }

    SearchResults {
        lines,
        stopped_at_days,
    }
}
