//! Converting the `hh:mm:ss` times of day files to UTC stamps.
//!
//! # Log time to UTC
//!
//! A line's time is its day file's date (the `YYYY-MM-DD` file name) with
//! its `hh:mm:ss`. ISO8601 directories are UTC. ZNC directories are in
//! `log_timezone`, an IANA name or a POSIX TZ string resolved at startup.
//! In the fall-back fold, a time is given the earlier offset unless that
//! would put it before the previous line of the same file, so the hour that
//! repeats maps to increasing stamps. A time in the spring-forward gap is
//! shifted forward by the gap's length. A line whose date or time does not
//! convert is skipped and counted, one log line per file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use jiff::civil::{self, Date};
use jiff::tz::{AmbiguousOffset, TimeZone};

use crate::parser::{LogFormat, LogLine, Time};

/// Turns one day file's `hh:mm:ss` times into UTC stamps, in file order.
pub(super) struct DayClock {
    tz: TimeZone,
    date: Date,
    prev: Option<Timestamp>,
}

impl DayClock {
    pub(super) fn new(tz: TimeZone, date: Date) -> Self {
        Self {
            tz,
            date,
            prev: None,
        }
    }

    /// The UTC stamp of `time`, or `None` when it is not a valid time of day.
    pub(super) fn stamp(&mut self, time: Time) -> Option<Timestamp> {
        let time = civil::Time::new(
            i8::try_from(time.hour).ok()?,
            i8::try_from(time.minute).ok()?,
            i8::try_from(time.second).ok()?,
            0,
        )
        .ok()?;
        let local = self
            .tz
            .to_ambiguous_timestamp(
                self.date
                    .to_datetime(time),
            );
        let stamp = match local.offset() {
            AmbiguousOffset::Fold { .. } => {
                let earlier = local
                    .earlier()
                    .ok()?;
                if self
                    .prev
                    .is_some_and(|prev| earlier < prev)
                {
                    local
                        .later()
                        .ok()?
                } else {
                    earlier
                }
            }
            AmbiguousOffset::Unambiguous { .. } | AmbiguousOffset::Gap { .. } => local
                .compatible()
                .ok()?,
        };
        self.prev = Some(stamp);
        Some(stamp)
    }
}

/// Converts the times of log lines to UTC for every reader of a day file:
/// the tail, log pages and search. It keeps one [`DayClock`] per directory
/// for the day file last read, so the fold rule sees the previous line
/// across batches of the same file.
pub struct LogClock {
    log_tz: TimeZone,
    log_tz_name: String,
    /// Per directory: the day file last read and its clock, `None` when its
    /// name is not a date.
    by_dir: HashMap<PathBuf, (PathBuf, Option<DayClock>)>,
}

impl LogClock {
    /// A clock reading ZNC directories in `log_tz`, configured as `log_tz_name`.
    pub fn new(log_tz: TimeZone, log_tz_name: &str) -> Self {
        Self {
            log_tz,
            log_tz_name: log_tz_name.to_owned(),
            by_dir: HashMap::new(),
        }
    }

    /// The zone lines of `format` are written in, as configured.
    pub fn zone_name(&self, format: LogFormat) -> &str {
        match format {
            LogFormat::Znc => &self.log_tz_name,
            LogFormat::Iso8601 => "UTC",
        }
    }

    /// The UTC stamp of each of `lines`, read in order from the day file
    /// `path`; `None` for the whole file when its name is not a date, and for
    /// a line whose time does not exist.
    pub fn stamp<'l>(
        &mut self,
        path: &Path,
        format: LogFormat,
        lines: impl IntoIterator<Item = &'l LogLine>,
    ) -> Option<Vec<Option<Timestamp>>> {
        let dir = path.parent()?;
        let tz = match format {
            LogFormat::Znc => self
                .log_tz
                .clone(),
            LogFormat::Iso8601 => TimeZone::UTC,
        };
        let slot = self
            .by_dir
            .entry(dir.to_owned())
            .or_insert_with(|| (PathBuf::new(), None));
        if slot.0 != path {
            *slot = (path.to_owned(), day_clock(path, tz));
        }
        let clock = slot
            .1
            .as_mut()?;
        let stamps: Vec<Option<Timestamp>> = lines
            .into_iter()
            .map(|line| clock.stamp(line.time))
            .collect();
        let skipped = stamps
            .iter()
            .filter(|s| s.is_none())
            .count();
        if skipped > 0 {
            eprintln!(
                "presence: {skipped} lines of {} have a time that does not exist",
                path.display()
            );
        }
        Some(stamps)
    }
}

/// A clock for the day file `path` (`YYYY-MM-DD.log` or `.log.zst`), or
/// `None`, logged, when its name is not a date.
pub(super) fn day_clock(path: &Path, tz: TimeZone) -> Option<DayClock> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let stem = name
        .strip_suffix(".log")
        .or_else(|| name.strip_suffix(".log.zst"))
        .unwrap_or(name);
    match stem.parse::<Date>() {
        Ok(date) if stem.len() == 10 => Some(DayClock::new(tz, date)),
        Ok(_) => {
            eprintln!(
                "presence: not converting times of {}: not a YYYY-MM-DD name",
                path.display()
            );
            None
        }
        Err(e) => {
            eprintln!(
                "presence: not converting times of {}: not a date: {e}",
                path.display()
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use jiff::tz::TimeZone;

    use super::*;

    use crate::presence::test_helpers::*;
    use crate::presence::*;
    use crate::resolve_log_timezone;

    fn hms(h: u8, m: u8, s: u8) -> Time {
        Time {
            hour: h,
            minute: m,
            second: s,
        }
    }

    fn check_dst(tz: &TimeZone) {
        let mut clock = DayClock::new(
            tz.clone(),
            "2026-11-01"
                .parse()
                .unwrap(),
        );
        let got: Vec<_> = [
            hms(1, 30, 0),
            hms(1, 59, 59),
            hms(1, 0, 0),
            hms(1, 30, 0),
            hms(2, 0, 0),
        ]
        .into_iter()
        .map(|t| {
            clock
                .stamp(t)
                .unwrap()
        })
        .collect();
        let expected = [
            ts("2026-11-01T05:30:00Z"),
            ts("2026-11-01T05:59:59Z"),
            ts("2026-11-01T06:00:00Z"),
            ts("2026-11-01T06:30:00Z"),
            ts("2026-11-01T07:00:00Z"),
        ];
        assert_eq!(got, expected);

        let mut clock = DayClock::new(
            tz.clone(),
            "2026-03-08"
                .parse()
                .unwrap(),
        );
        assert_eq!(clock.stamp(hms(2, 30, 0)), Some(ts("2026-03-08T07:30:00Z")));
        assert_eq!(clock.stamp(hms(24, 0, 0)), None);
        assert_eq!(clock.stamp(hms(23, 59, 60)), None);
    }

    #[test]
    fn posix_log_timezone_folds_forward_and_shifts_gaps_forward() {
        check_dst(&resolve_log_timezone("EST5EDT,M3.2.0,M11.1.0").unwrap());
    }

    #[test]
    fn log_clock_keeps_the_fold_across_batches_and_follows_the_day_file() {
        let posix = "EST5EDT,M3.2.0,M11.1.0";
        let mut clock = LogClock::new(resolve_log_timezone(posix).unwrap(), posix);
        assert_eq!(clock.zone_name(LogFormat::Znc), posix);
        assert_eq!(clock.zone_name(LogFormat::Iso8601), "UTC");
        let line = |h, m| LogLine {
            time: hms(h, m, 0),
            kind: says("a"),
        };
        let at = |stamped: Option<Vec<Option<Timestamp>>>| stamped.unwrap()[0].unwrap();
        let fold_day = Path::new("/l/#c/2026-11-01.log");
        let first = clock.stamp(fold_day, LogFormat::Znc, &[line(1, 50)]);
        assert_eq!(at(first), ts("2026-11-01T05:50:00Z"));
        // The previous batch's 01:50 EDT puts this 01:10 in EST.
        let second = clock.stamp(fold_day, LogFormat::Znc, &[line(1, 10)]);
        assert_eq!(at(second), ts("2026-11-01T06:10:00Z"));

        let next_day = Path::new("/l/#c/2026-11-02.log");
        let third = clock.stamp(next_day, LogFormat::Znc, &[line(0, 5)]);
        assert_eq!(at(third), ts("2026-11-02T05:05:00Z"));
        let iso = Path::new("/l/#i/2026-11-02.log");
        let fourth = clock.stamp(iso, LogFormat::Iso8601, &[line(0, 5)]);
        assert_eq!(at(fourth), ts("2026-11-02T00:05:00Z"));
        let compressed = Path::new("/l/#z/2026-11-02.log.zst");
        let fifth = clock.stamp(compressed, LogFormat::Znc, &[line(0, 5), line(24, 0)]);
        assert_eq!(fifth, Some(vec![Some(ts("2026-11-02T05:05:00Z")), None]));

        assert_eq!(
            clock.stamp(Path::new("/l/#c/today.log"), LogFormat::Znc, &[line(0, 0)]),
            None
        );
    }

    #[test]
    fn iana_log_timezone_folds_forward_and_shifts_gaps_forward() {
        if let Err(e) = TimeZone::get("America/Toronto") {
            eprintln!("skipped: the system tzdb has no America/Toronto: {e}");
            return;
        }
        check_dst(&resolve_log_timezone("America/Toronto").unwrap());
    }
}
