use jiff::Timestamp;

use crate::Channel;
use crate::parser::{LogLine, parse_line};
use crate::presence::LogClock;
use crate::server::{channel_dates, read_log_file, resolve_log_path};

/// One matching line.
pub struct Found {
    /// Its day file's date.
    pub date: String,
    pub line: LogLine,
    /// Its UTC instant, `None` where unknown.
    pub at: Option<Timestamp>,
    /// Zone its file time is in.
    pub zone: String,
}

pub struct SearchResults {
    /// Matching lines, newest day first.
    pub lines: Vec<Found>,
    /// Set to the day bound when older days were left unscanned.
    pub stopped_at_days: Option<usize>,
}

/// Case-insensitive substring search over the newest `max_days` days, with
/// times converted by `clock`.
pub fn search_channel(
    channel: &Channel,
    query: &str,
    limit: usize,
    max_days: usize,
    clock: &mut LogClock,
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

        let raw: Vec<&str> = content
            .lines()
            .collect();
        if !raw
            .iter()
            .any(|l| {
                l.to_lowercase()
                    .contains(&query_lower)
            })
        {
            continue;
        }
        // The fold rule needs every line of the file, not only the matches.
        let parsed: Vec<(usize, LogLine)> = raw
            .iter()
            .enumerate()
            .filter_map(|(i, l)| parse_line(l, format).map(|p| (i, p)))
            .collect();
        let stamps = clock
            .stamp(
                &path,
                format,
                parsed
                    .iter()
                    .map(|(_, l)| l),
            )
            .unwrap_or_else(|| vec![None; parsed.len()]);
        for ((i, line), at) in parsed
            .into_iter()
            .zip(stamps)
        {
            if !raw[i]
                .to_lowercase()
                .contains(&query_lower)
            {
                continue;
            }
            lines.push(Found {
                date: date.clone(),
                line,
                at,
                zone: clock
                    .zone_name(format)
                    .to_owned(),
            });
            if lines.len() >= limit {
                return SearchResults {
                    lines,
                    stopped_at_days: None,
                };
            }
        }
    }

    SearchResults {
        lines,
        stopped_at_days,
    }
}
