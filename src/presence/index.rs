//! The index file: its on-disk shape, loading, atomic writing, and the file stats it compares.
//!
//! # The index is a cache
//!
//! A missing, unreadable or corrupt index, or one with another version, is
//! logged and the archive is rescanned in full. The index is written after
//! the scan, every 10 minutes when something changed, and at shutdown, each
//! time to a temporary file in the same directory that is fsynced and renamed
//! over the target, followed by an fsync of the directory. A failed write is
//! logged and the stamps stay in memory.

use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_saphyr::{Budget, SerializerOptions};

use super::{Archive, ModeHistory, Topic, UserStamps};
use crate::yaml_options;

pub(super) const INDEX_VERSION: u32 = 2;

/// What the index compares to decide whether a file needs reparsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    /// Modification time.
    pub mtime: Timestamp,
    /// Size in bytes.
    pub size: u64,
}

pub(super) fn file_stat(path: &Path) -> io::Result<FileStat> {
    let meta = std::fs::metadata(path)?;
    let mtime = Timestamp::try_from(meta.modified()?).map_err(io::Error::other)?;
    Ok(FileStat {
        mtime,
        size: meta.len(),
    })
}

#[derive(Deserialize)]
struct IndexHeader {
    version: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct IndexDoc {
    pub(super) version: u32,
    pub(super) channels: Vec<IndexChannel>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct IndexChannel {
    pub(super) channel: String,
    pub(super) files: Vec<IndexFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) topic: Option<Topic>,
    #[serde(default)]
    pub(super) users: Vec<UserStamps>,
    #[serde(default, skip_serializing_if = "is_empty_history")]
    pub(super) modes: ModeHistory,
}

fn is_empty_history(modes: &ModeHistory) -> bool {
    modes == &ModeHistory::default()
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct IndexFile {
    pub(super) path: String,
    pub(super) mtime: Timestamp,
    pub(super) size: u64,
}

/// Why the index could not be loaded.
#[derive(Debug)]
pub(super) enum IndexError {
    Read(io::Error),
    Parse(serde_saphyr::Error),
    Version(u32),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(e) => write!(f, "cannot read: {e}"),
            Self::Parse(e) => write!(f, "cannot parse: {e}"),
            Self::Version(v) => write!(f, "version {v}, expected {INDEX_VERSION}"),
        }
    }
}

/// Deserializer options for the index: the config's, with the event and
/// node caps raised to the file's length, since every node takes at least a byte.
fn index_options(len: usize) -> serde_saphyr::Options {
    let mut budget = Budget::default();
    budget.max_events = budget
        .max_events
        .max(len);
    budget.max_nodes = budget
        .max_nodes
        .max(len);
    let mut options = yaml_options();
    options.budget = Some(budget);
    options
}

pub(super) fn load_index(path: &Path) -> Result<Archive, IndexError> {
    let content = std::fs::read_to_string(path).map_err(IndexError::Read)?;
    let header: IndexHeader =
        serde_saphyr::from_str_with_options(&content, index_options(content.len()))
            .map_err(IndexError::Parse)?;
    if header.version != INDEX_VERSION {
        return Err(IndexError::Version(header.version));
    }
    let doc: IndexDoc = serde_saphyr::from_str_with_options(&content, index_options(content.len()))
        .map_err(IndexError::Parse)?;
    Ok(Archive::from_index(doc))
}

/// The index at `path`, or an empty archive and the line logging why.
pub(super) fn load_or_rescan(path: &Path) -> (Archive, Option<String>) {
    match load_index(path) {
        Ok(archive) => (archive, None),
        Err(e) => (
            Archive::default(),
            Some(format!(
                "presence: index {}: {e}; rescanning everything",
                path.display()
            )),
        ),
    }
}

/// Why the index could not be written.
#[derive(Debug)]
pub(super) enum WriteError {
    Serialize(serde_saphyr::SerializeError),
    Io(&'static str, io::Error),
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize(e) => write!(f, "cannot serialize: {e}"),
            Self::Io(op, e) => write!(f, "{op}: {e}"),
        }
    }
}

pub(super) fn write_index(path: &Path, doc: &IndexDoc) -> Result<(), WriteError> {
    let dir = match path.parent() {
        Some(dir)
            if !dir
                .as_os_str()
                .is_empty() =>
        {
            dir
        }
        _ => Path::new("."),
    };
    let prefix = path
        .file_name()
        .unwrap_or_else(|| "index".as_ref());
    let mut tmp = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(|e| WriteError::Io("cannot create a temporary file", e))?;
    // Topics must come back byte for byte; folded scalars rewrap at whitespace.
    let mut options = SerializerOptions::default();
    options.prefer_block_scalars = false;
    {
        let mut out = BufWriter::new(tmp.as_file_mut());
        serde_saphyr::to_io_writer_with_options(&mut out, doc, options)
            .map_err(WriteError::Serialize)?;
        out.flush()
            .map_err(|e| WriteError::Io("cannot write", e))?;
    }
    tmp.as_file()
        .sync_all()
        .map_err(|e| WriteError::Io("cannot fsync the temporary file", e))?;
    tmp.persist(path)
        .map_err(|e| WriteError::Io("cannot rename the temporary file", e.error))?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| WriteError::Io("cannot fsync the directory", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::*;

    use crate::parser::LogFormat;
    use crate::presence::scan::ScanSummary;
    use crate::presence::test_helpers::*;
    use crate::presence::*;

    use crate::test_support::tempdir;

    #[test]
    fn index_round_trips_and_only_changed_files_are_reparsed() {
        let root = tempdir();
        let day = |text: &str| format!("[10:00:00] <a> {text}\n");
        let dir = channel_dir(
            root.path(),
            LogFormat::Znc,
            &[
                ("2026-01-01.log", &day("one")),
                ("2026-01-02.log", &day("two")),
            ],
        );
        let unchanged = dir
            .path
            .join("2026-01-01.log");
        let changed = dir
            .path
            .join("2026-01-02.log");
        let tz = TimeZone::UTC;
        let mut archive = Archive::default();
        let (inputs, summary) = scan_dirs(vec![dir.clone()], &archive.known_files(), &tz);
        assert_eq!(
            summary,
            ScanSummary {
                parsed: 2,
                unchanged: 0
            }
        );
        absorb(&mut archive, inputs);
        let state = archive
            .channels
            .get_mut("#c")
            .unwrap();
        // Values the YAML writer has to quote or escape.
        let presence = Arc::make_mut(&mut state.presence);
        presence.apply(
            ts("2026-01-03T00:00:00Z"),
            &topic("[]\\`^{}|", "  #lead: 'q' \"d\"  a\tb  trailing "),
        );
        for nick in ["yes", "123", "-", "~x", "a: b"] {
            presence.apply(ts("2026-01-03T00:00:01Z"), &says(nick));
        }

        let index = root
            .path()
            .join("presence.yaml");
        write_index(&index, &archive.to_index()).unwrap();
        let loaded = load_index(&index).unwrap();
        assert_eq!(loaded, archive);

        std::fs::write(&changed, day("two") + &day("three")).unwrap();
        let (inputs, summary) = scan_dirs(vec![dir.clone()], &loaded.known_files(), &tz);
        assert_eq!(
            summary,
            ScanSummary {
                parsed: 1,
                unchanged: 1
            }
        );
        let mut reloaded = loaded;
        assert_eq!(
            absorb(&mut reloaded, inputs),
            std::slice::from_ref(&changed)
        );

        // A vanished file leaves the file list; its stamps stay.
        let before = reloaded.channels["#c"]
            .presence
            .clone();
        std::fs::remove_file(&unchanged).unwrap();
        let (inputs, _) = scan_dirs(vec![dir], &reloaded.known_files(), &tz);
        absorb(&mut reloaded, inputs);
        let state = &reloaded.channels["#c"];
        assert_eq!(
            state
                .files
                .keys()
                .collect::<Vec<_>>(),
            [&changed]
        );
        assert_eq!(state.presence, before);
    }

    #[test]
    fn a_bad_index_is_reported_and_everything_is_rescanned() {
        let root = tempdir();
        let dir = channel_dir(
            root.path(),
            LogFormat::Znc,
            &[
                ("2026-01-01.log", "[10:00:00] <a> one\n"),
                ("2026-01-02.log", "[10:00:00] <a> two\n"),
            ],
        );
        let index = root
            .path()
            .join("presence.yaml");
        for (content, reason) in [
            ("version: 2\nchannels: [ {\n", "cannot parse"),
            ("version: 1\nchannels: []\n", "version 1, expected 2"),
            ("", "cannot parse"),
        ] {
            std::fs::write(&index, content).unwrap();
            let (archive, problem) = load_or_rescan(&index);
            let problem = problem.expect("a bad index is reported");
            assert!(
                problem.starts_with(&format!("presence: index {}: {reason}", index.display())),
                "{problem}"
            );
            assert!(problem.ends_with("; rescanning everything"), "{problem}");
            assert_eq!(archive, Archive::default());
            let (_, summary) = scan_dirs(vec![dir.clone()], &archive.known_files(), &TimeZone::UTC);
            assert_eq!(summary.parsed, 2);
        }
        std::fs::remove_file(&index).unwrap();
        let (_, problem) = load_or_rescan(&index);
        assert!(
            problem
                .unwrap()
                .contains("cannot read")
        );
    }

    #[test]
    fn index_write_leaves_only_the_index() {
        let root = tempdir();
        let index = root
            .path()
            .join("presence.yaml");
        std::fs::write(&index, "stale").unwrap();
        let mut archive = Archive::default();
        archive.absorb_file(
            "#c".into(),
            "/x/2026-01-01.log".into(),
            FileStat {
                mtime: ts("2026-01-01T00:00:00.123456789Z"),
                size: 3,
            },
            &file_stamps(&[("2026-01-01T00:00:01Z", says("a"))]),
        );
        write_index(&index, &archive.to_index()).unwrap();
        let names: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|e| {
                e.unwrap()
                    .file_name()
            })
            .collect();
        assert_eq!(names, ["presence.yaml"]);
        assert_eq!(load_index(&index).unwrap(), archive);
    }
}
