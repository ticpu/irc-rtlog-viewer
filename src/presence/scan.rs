//! Parsing the archive's day files into stamps, one file at a time.
//!
//! # Scan
//!
//! The scan runs on the blocking pool, one file at a time, and stats each
//! file before reading it: a file growing during the read is then recorded
//! smaller than what was merged, and reparsed at the next start. A file
//! whose (mtime, size) matches its index entry is not reparsed.

use std::collections::{HashMap, HashSet};
use std::io::{self};
use std::path::{Path, PathBuf};

use jiff::civil::Date;
use jiff::tz::TimeZone;

use super::clock::DayClock;
use super::index::file_stat;
use super::{ChannelPresence, Input, KnownFiles};
use crate::logfiles::read_log_file;
use crate::parser::{LogFormat, parse_line};
use crate::{ChannelDir, ChannelNode};

/// A channel the scan visits.
pub(super) struct ScanChannel {
    pub(super) key: String,
    pub(super) dirs: Vec<ChannelDir>,
}

/// The key of every channel in the tree, as presence and the endpoint use them.
pub fn channel_keys(root: &ChannelNode) -> Vec<String> {
    scan_channels(root)
        .into_iter()
        .map(|c| c.key)
        .collect()
}

pub(super) fn scan_channels(root: &ChannelNode) -> Vec<ScanChannel> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        stack.extend(
            node.children
                .values(),
        );
        if let Some(channel) = &node.channel {
            found.push(ScanChannel {
                key: channel
                    .path_segments
                    .join("/"),
                dirs: channel
                    .dirs
                    .clone(),
            });
        }
    }
    found.sort_by(|a, b| {
        a.key
            .cmp(&b.key)
    });
    found
}

/// Files the scan parsed and the ones it skipped as unchanged.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ScanSummary {
    pub(super) parsed: usize,
    pub(super) unchanged: usize,
}

/// The `YYYY-MM-DD.log` and `.log.zst` files of `dir`, with their dates.
fn day_files(dir: &Path) -> io::Result<Vec<(PathBuf, Date)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("presence: cannot list {}: {e}", dir.display());
                continue;
            }
        };
        let name = entry.file_name();
        let Some(stem) = name
            .to_str()
            .and_then(|n| {
                n.strip_suffix(".log")
                    .or_else(|| n.strip_suffix(".log.zst"))
            })
        else {
            continue;
        };
        if stem.len() != 10 {
            continue;
        }
        match stem.parse::<Date>() {
            Ok(date) => files.push((entry.path(), date)),
            Err(e) => eprintln!(
                "presence: skipping {}: not a date: {e}",
                entry
                    .path()
                    .display()
            ),
        }
    }
    files.sort();
    Ok(files)
}

fn parse_day_file(
    path: &Path,
    format: LogFormat,
    tz: &TimeZone,
    date: Date,
) -> io::Result<ChannelPresence> {
    let content = read_log_file(path)?;
    let mut clock = DayClock::new(tz.clone(), date);
    let mut stamps = ChannelPresence::default();
    let mut skipped = 0usize;
    for line in content.lines() {
        let Some(parsed) = parse_line(line, format) else {
            continue;
        };
        match clock.stamp(parsed.time) {
            Some(at) => stamps.apply(at, &parsed.kind),
            None => skipped += 1,
        }
    }
    if skipped > 0 {
        eprintln!(
            "presence: skipped {skipped} unparsable lines in {}",
            path.display()
        );
    }
    Ok(stamps)
}

/// Parses every day file whose stat differs from `known`, handing each to
/// `send`, then the end of the scan. Stops early when `send` returns false.
pub(super) fn scan(
    channels: &[ScanChannel],
    known: &KnownFiles,
    log_tz: &TimeZone,
    send: &mut dyn FnMut(Input) -> bool,
) -> ScanSummary {
    let utc = TimeZone::UTC;
    let none = HashMap::new();
    let mut summary = ScanSummary::default();
    let mut seen: HashMap<String, HashSet<PathBuf>> = HashMap::new();
    let mut unlisted = HashSet::new();
    for channel in channels {
        let known = known
            .get(&channel.key)
            .unwrap_or(&none);
        let seen = seen
            .entry(
                channel
                    .key
                    .clone(),
            )
            .or_default();
        for dir in &channel.dirs {
            let tz = match dir.format {
                LogFormat::Znc => log_tz,
                LogFormat::Iso8601 => &utc,
            };
            let files = match day_files(&dir.path) {
                Ok(files) => files,
                Err(e) => {
                    eprintln!(
                        "presence: cannot list {}: {e}",
                        dir.path
                            .display()
                    );
                    unlisted.insert(
                        dir.path
                            .clone(),
                    );
                    continue;
                }
            };
            for (path, date) in files {
                let stat = match file_stat(&path) {
                    Ok(stat) => stat,
                    Err(e) => {
                        eprintln!("presence: cannot stat {}: {e}", path.display());
                        continue;
                    }
                };
                seen.insert(path.clone());
                if known.get(&path) == Some(&stat) {
                    summary.unchanged += 1;
                    continue;
                }
                let stamps = match parse_day_file(&path, dir.format, tz, date) {
                    Ok(stamps) => stamps,
                    Err(e) => {
                        eprintln!("presence: cannot read {}: {e}", path.display());
                        continue;
                    }
                };
                summary.parsed += 1;
                let input = Input::File {
                    channel: channel
                        .key
                        .clone(),
                    path,
                    stat,
                    stamps,
                };
                if !send(input) {
                    return summary;
                }
            }
        }
    }
    send(Input::ScanFinished { seen, unlisted });
    summary
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::*;

    use crate::presence::test_helpers::*;
    use crate::presence::*;

    use crate::test_support::tempdir;

    const ZNC_DAY: &str = "\
[00:00:01] *** Joins: alice (~a@192.0.2.1)
[00:00:02] <bob> hello
[00:00:03] * carol waves
[00:00:04] *** Quits: dave (~d@192.0.2.4) (Remote host closed the connection)
[00:00:05] *** Parts: erin (~e@192.0.2.5) (later)
[00:00:06] *** frank was kicked by op (Banned: spam)
[00:00:07] *** gina is now known as hank
[00:00:08] *** op changes topic to 'https://example.org | new topic'
[00:00:09] *** Server sets mode: +nt 
[00:00:10] *** Some other notice
";

    #[test]
    fn every_line_kind_feeds_its_stamp() {
        let root = tempdir();
        let znc = channel_dir(root.path(), LogFormat::Znc, &[("2026-01-02.log", ZNC_DAY)]);
        let iso = channel_dir(
            root.path(),
            LogFormat::Iso8601,
            &[("2026-01-02.log", "2026-01-02T03:04:05Z <ivan> hi\n")],
        );
        // ZNC lines are read in log_timezone, ISO8601 lines stay UTC.
        let tz = TimeZone::fixed(jiff::tz::offset(-5));
        let mut archive = Archive::default();
        let (inputs, _) = scan_dirs(vec![znc, iso], &KnownFiles::new(), &tz);
        absorb(&mut archive, inputs);
        let got = &archive.channels["#c"].presence;

        let expected = HashMap::from([
            user("alice", Some("2026-01-02T05:00:01Z"), None, None),
            user(
                "bob",
                Some("2026-01-02T05:00:02Z"),
                None,
                Some("2026-01-02T05:00:02Z"),
            ),
            user(
                "carol",
                Some("2026-01-02T05:00:03Z"),
                None,
                Some("2026-01-02T05:00:03Z"),
            ),
            user("dave", None, Some("2026-01-02T05:00:04Z"), None),
            user("erin", None, Some("2026-01-02T05:00:05Z"), None),
            user("frank", None, Some("2026-01-02T05:00:06Z"), None),
            user("gina", None, Some("2026-01-02T05:00:07Z"), None),
            user("hank", Some("2026-01-02T05:00:07Z"), None, None),
            user(
                "ivan",
                Some("2026-01-02T03:04:05Z"),
                None,
                Some("2026-01-02T03:04:05Z"),
            ),
        ]);
        assert_eq!(got.users, expected);
        assert_eq!(
            got.topic,
            Some(Topic {
                nick: "op".into(),
                text: "https://example.org | new topic".into(),
                at: ts("2026-01-02T05:00:08Z"),
            })
        );
    }

    #[test]
    fn a_stray_byte_does_not_cost_the_day() {
        let root = tempdir();
        let path = root
            .path()
            .join("2026-01-01.log");
        std::fs::write(
            &path,
            b"[10:00:00] *** Joins: a (~a@h)\n[10:00:01] <b> caf\xe9\n[10:00:02] *** Quits: a (~a@h) (x)\n",
        )
        .unwrap();
        let stamps = parse_day_file(
            &path,
            LogFormat::Znc,
            &TimeZone::UTC,
            "2026-01-01"
                .parse()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            stamps.users,
            HashMap::from([
                user(
                    "a",
                    Some("2026-01-01T10:00:00Z"),
                    Some("2026-01-01T10:00:02Z"),
                    None
                ),
                user(
                    "b",
                    Some("2026-01-01T10:00:01Z"),
                    None,
                    Some("2026-01-01T10:00:01Z")
                ),
            ])
        );
    }
}
