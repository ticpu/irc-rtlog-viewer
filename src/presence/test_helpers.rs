//! Builders shared by the presence modules' tests.

use std::path::{Path, PathBuf};

use jiff::Timestamp;
use jiff::tz::TimeZone;

use super::scan::{ScanChannel, ScanSummary, scan};
use super::{Archive, ChannelPresence, Input, KnownFiles, UserStamps, casemap};
use crate::ChannelDir;
use crate::parser::{LineKind, LogFormat};

pub(super) fn ts(s: &str) -> Timestamp {
    s.parse()
        .unwrap()
}

pub(super) fn user(
    nick: &str,
    online: Option<&str>,
    offline: Option<&str>,
    spoke: Option<&str>,
) -> (String, UserStamps) {
    (
        casemap(nick),
        UserStamps {
            nick: nick.to_owned(),
            online: online.map(ts),
            offline: offline.map(ts),
            spoke: spoke.map(ts),
        },
    )
}

/// A `#c` channel in `root` with one ZNC or ISO8601 directory of `files`.
pub(super) fn channel_dir(root: &Path, format: LogFormat, files: &[(&str, &str)]) -> ChannelDir {
    let dir = root.join(match format {
        LogFormat::Znc => "znc",
        LogFormat::Iso8601 => "iso",
    });
    std::fs::create_dir_all(&dir).unwrap();
    for (name, content) in files {
        std::fs::write(dir.join(name), content).unwrap();
    }
    ChannelDir { path: dir, format }
}

pub(super) fn scan_dirs(
    dirs: Vec<ChannelDir>,
    known: &KnownFiles,
    tz: &TimeZone,
) -> (Vec<Input>, ScanSummary) {
    let channels = [ScanChannel {
        key: "#c".into(),
        dirs,
    }];
    let mut inputs = Vec::new();
    let summary = scan(&channels, known, tz, &mut |input| {
        inputs.push(input);
        true
    });
    (inputs, summary)
}

/// Feeds scanner output to `archive` the way the actor does.
pub(super) fn absorb(archive: &mut Archive, inputs: Vec<Input>) -> Vec<PathBuf> {
    let mut parsed = Vec::new();
    for input in inputs {
        match input {
            Input::File {
                channel,
                path,
                stat,
                stamps,
            } => {
                parsed.push(path.clone());
                archive.absorb_file(channel, path, stat, &stamps);
            }
            Input::ScanFinished { seen, unlisted } => {
                archive.drop_gone(&seen, &unlisted);
            }
            Input::Lines { .. } | Input::Session(_) | Input::Shutdown(_) => {
                unreachable!("the scanner sends only File and ScanFinished")
            }
        }
    }
    parsed
}

pub(super) fn file_stamps(lines: &[(&str, LineKind)]) -> ChannelPresence {
    let mut stamps = ChannelPresence::default();
    for (at, kind) in lines {
        stamps.apply(ts(at), kind);
    }
    stamps
}

pub(super) fn join(nick: &str) -> LineKind {
    LineKind::Join {
        nick: nick.into(),
        userhost: "~u@h".into(),
    }
}

pub(super) fn says(nick: &str) -> LineKind {
    LineKind::Message {
        nick: nick.into(),
        text: "x".into(),
    }
}

pub(super) fn quits(nick: &str) -> LineKind {
    LineKind::Quit {
        nick: nick.into(),
        userhost: "~u@h".into(),
        reason: String::new(),
    }
}

pub(super) fn topic(nick: &str, text: &str) -> LineKind {
    LineKind::Topic {
        nick: nick.into(),
        topic: text.into(),
    }
}
