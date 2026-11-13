//! Per-nick archive stamps (last online, last offline, last spoke) and the
//! latest topic of each channel, computed from the log files in the
//! background, cached in a YAML index, and served by the presence endpoint.
//!
//! # Stamps are maxima of UTC timestamps
//!
//! Every stored value is the maximum of the UTC timestamps that fed it: a
//! user's online, offline and spoke, the topic (ordered by its stamp, then
//! setter, then text), and a user's display nick (the form used by the latest
//! event naming it, ties broken by the greater string). Merging files in any
//! order, merging one twice, or interleaving later live lines therefore gives
//! the same state. A change that replaces a value instead of taking a maximum
//! breaks the scan's file order independence and the index reload.
//!
//! Stamps only grow. A file that disappears (rotated to `.log.zst`, deleted)
//! loses its entry in the index's file list, but what it contributed stays,
//! and so does what a rewritten file no longer contains, until the index is
//! deleted.
//!
//! # Sources
//!
//! online comes from a join, a message, an action and the new side of a nick
//! change; offline from a quit, a part, a kick (the kicked nick) and the old
//! side of a nick change; spoke from messages and actions; the topic from
//! "changes topic to" lines. Users are keyed by the nick under rfc1459
//! casemapping, the network's CASEMAPPING: ASCII lowercase, plus `[]\~`
//! folding to `{}|^`.
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
//!
//! # Ownership
//!
//! One actor task owns the mutable state and takes [`Input`] messages: a
//! parsed file from the scanner, lines from the tail, the end of the scan,
//! and a shutdown request. Live lines merge into the stamps but leave the
//! file's index entry alone: its stat no longer matches, so the next start
//! reparses the file, which the max-merge makes harmless. The tail stamps
//! its lines with a [`LiveClock`], which carries the fold rule's previous
//! line across batches of the same day file.
//! Readers see an `ArcSwap` snapshot of `Arc<ChannelPresence>` per channel,
//! republished at most once per second for the channels that changed, and an
//! atomic scanning flag stored after the final republish. No lock guards the
//! state.
//!
//! # Scan
//!
//! The scan runs on the blocking pool, one file at a time, and stats each
//! file before reading it: a file growing during the read is then recorded
//! smaller than what was merged, and reparsed at the next start. A file
//! whose (mtime, size) matches its index entry is not reparsed.
//!
//! # The index is a cache
//!
//! A missing, unreadable or corrupt index, or one with another version, is
//! logged and the archive is rescanned in full. The index is written after
//! the scan, every 10 minutes when something changed, and at shutdown, each
//! time to a temporary file in the same directory that is fsynced and renamed
//! over the target, followed by an fsync of the directory. A failed write is
//! logged and the stamps stay in memory.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use jiff::Timestamp;
use jiff::civil::{self, Date};
use jiff::tz::{AmbiguousOffset, TimeZone};
use serde::{Deserialize, Serialize};
use serde_saphyr::{Budget, SerializerOptions};
use tokio::sync::{mpsc, oneshot};

use crate::parser::{LineKind, LogFormat, LogLine, Time, parse_line};
use crate::server::read_log_file;
use crate::{ChannelDir, ChannelNode, yaml_options};

const INDEX_VERSION: u32 = 1;
const PUBLISH_EVERY: Duration = Duration::from_secs(1);
const SAVE_EVERY: Duration = Duration::from_secs(600);

/// A channel's topic as last set in the archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topic {
    /// Nick that set it.
    pub nick: String,
    /// Topic text.
    pub text: String,
    /// When it was set.
    pub at: Timestamp,
}

impl Topic {
    fn order(&self) -> (Timestamp, &str, &str) {
        (self.at, &self.nick, &self.text)
    }
}

/// One user's archive stamps, under the display form of its latest event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserStamps {
    /// Display nick.
    pub nick: String,
    /// Latest join, message, action or nick change to this nick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online: Option<Timestamp>,
    /// Latest quit, part, kick or nick change from this nick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offline: Option<Timestamp>,
    /// Latest message or action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoke: Option<Timestamp>,
}

#[derive(Clone, Copy)]
enum Field {
    Online,
    Offline,
    Spoke,
}

impl UserStamps {
    fn new(nick: &str) -> Self {
        Self {
            nick: nick.to_owned(),
            online: None,
            offline: None,
            spoke: None,
        }
    }

    fn latest(&self) -> Option<Timestamp> {
        self.online
            .max(self.offline)
            .max(self.spoke)
    }

    fn note(&mut self, nick: &str, field: Field, at: Timestamp) {
        let newer = match self.latest() {
            None => true,
            Some(latest) => {
                at > latest
                    || (at == latest
                        && nick
                            > self
                                .nick
                                .as_str())
            }
        };
        if newer {
            nick.clone_into(&mut self.nick);
        }
        let slot = match field {
            Field::Online => &mut self.online,
            Field::Offline => &mut self.offline,
            Field::Spoke => &mut self.spoke,
        };
        *slot = (*slot).max(Some(at));
    }

    fn merge(&mut self, other: &UserStamps) {
        let fields = [
            (Field::Online, other.online),
            (Field::Offline, other.offline),
            (Field::Spoke, other.spoke),
        ];
        for (field, at) in fields {
            if let Some(at) = at {
                self.note(&other.nick, field, at);
            }
        }
    }
}

/// Archive stamps of one channel, or of one file before it is merged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelPresence {
    /// Latest topic change seen.
    pub topic: Option<Topic>,
    /// Users keyed by rfc1459-casemapped nick.
    pub users: HashMap<String, UserStamps>,
}

impl ChannelPresence {
    /// Folds one log line stamped `at` into the stamps.
    pub fn apply(&mut self, at: Timestamp, kind: &LineKind) {
        match kind {
            LineKind::Message { nick, .. } | LineKind::Action { nick, .. } => {
                self.note(nick, Field::Online, at);
                self.note(nick, Field::Spoke, at);
            }
            LineKind::Join { nick, .. } => self.note(nick, Field::Online, at),
            LineKind::Quit { nick, .. }
            | LineKind::Part { nick, .. }
            | LineKind::Kick { nick, .. } => self.note(nick, Field::Offline, at),
            LineKind::NickChange { old_nick, new_nick } => {
                self.note(old_nick, Field::Offline, at);
                self.note(new_nick, Field::Online, at);
            }
            LineKind::Topic { nick, topic } => self.set_topic(Topic {
                nick: nick.clone(),
                text: topic.clone(),
                at,
            }),
            LineKind::Mode { .. } | LineKind::Notice { .. } => {}
        }
    }

    /// Takes the maximum of every stamp in `self` and `other`.
    pub fn merge(&mut self, other: &ChannelPresence) {
        if let Some(topic) = &other.topic {
            self.set_topic(topic.clone());
        }
        for (key, user) in &other.users {
            self.users
                .entry(key.clone())
                .or_insert_with(|| UserStamps::new(&user.nick))
                .merge(user);
        }
    }

    fn note(&mut self, nick: &str, field: Field, at: Timestamp) {
        if nick.is_empty() {
            return;
        }
        self.users
            .entry(casemap(nick))
            .or_insert_with(|| UserStamps::new(nick))
            .note(nick, field, at);
    }

    fn set_topic(&mut self, topic: Topic) {
        let newer = self
            .topic
            .as_ref()
            .is_none_or(|current| topic.order() > current.order());
        if newer {
            self.topic = Some(topic);
        }
    }
}

/// rfc1459 casemapping: ASCII lowercase, with `[]\~` as the upper case of `{}|^`.
fn casemap(nick: &str) -> String {
    nick.chars()
        .map(|c| match c {
            '[' => '{',
            ']' => '}',
            '\\' => '|',
            '~' => '^',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

/// Turns one day file's `hh:mm:ss` times into UTC stamps, in file order.
struct DayClock {
    tz: TimeZone,
    date: Date,
    prev: Option<Timestamp>,
}

impl DayClock {
    fn new(tz: TimeZone, date: Date) -> Self {
        Self {
            tz,
            date,
            prev: None,
        }
    }

    /// The UTC stamp of `time`, or `None` when it is not a valid time of day.
    fn stamp(&mut self, time: Time) -> Option<Timestamp> {
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

/// Stamps lines the tail reads, keeping one [`DayClock`] per directory for
/// its current day file, so the fold rule sees the previous live line.
pub struct LiveClock {
    log_tz: TimeZone,
    /// Per directory: the day file last read and its clock, `None` when its
    /// name is not a date.
    by_dir: HashMap<PathBuf, (PathBuf, Option<DayClock>)>,
}

impl LiveClock {
    /// What `lines`, read in order from the day file `path`, contribute.
    /// `None` when the file's name is not a `YYYY-MM-DD` date.
    pub fn stamps<'l>(
        &mut self,
        path: &Path,
        format: LogFormat,
        lines: impl IntoIterator<Item = &'l LogLine>,
    ) -> Option<ChannelPresence> {
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
        let mut stamps = ChannelPresence::default();
        let mut skipped = 0usize;
        for line in lines {
            match clock.stamp(line.time) {
                Some(at) => stamps.apply(at, &line.kind),
                None => skipped += 1,
            }
        }
        if skipped > 0 {
            eprintln!(
                "presence: skipped {skipped} unparsable live lines in {}",
                path.display()
            );
        }
        Some(stamps)
    }
}

/// A clock for the day file `path`, or `None`, logged, when its name is not a date.
fn day_clock(path: &Path, tz: TimeZone) -> Option<DayClock> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    match stem.parse::<Date>() {
        Ok(date) if stem.len() == 10 => Some(DayClock::new(tz, date)),
        Ok(_) => {
            eprintln!(
                "presence: not stamping live lines of {}: not a YYYY-MM-DD name",
                path.display()
            );
            None
        }
        Err(e) => {
            eprintln!(
                "presence: not stamping live lines of {}: not a date: {e}",
                path.display()
            );
            None
        }
    }
}

/// What the index compares to decide whether a file needs reparsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    /// Modification time.
    pub mtime: Timestamp,
    /// Size in bytes.
    pub size: u64,
}

fn file_stat(path: &Path) -> io::Result<FileStat> {
    let meta = std::fs::metadata(path)?;
    let mtime = Timestamp::try_from(meta.modified()?).map_err(io::Error::other)?;
    Ok(FileStat {
        mtime,
        size: meta.len(),
    })
}

/// Messages the actor acts on, in arrival order.
enum Input {
    /// A day file the scanner parsed, with what it contributes.
    File {
        channel: String,
        path: PathBuf,
        stat: FileStat,
        stamps: ChannelPresence,
    },
    /// Lines the tail read from the channel's current day file. The file's
    /// index entry is left alone, so the next start reparses it.
    Lines {
        channel: String,
        stamps: ChannelPresence,
    },
    /// The scan is over: an index entry neither in `seen` nor under an
    /// `unlisted` directory is for a file that no longer exists.
    ScanFinished {
        seen: HashMap<String, HashSet<PathBuf>>,
        unlisted: HashSet<PathBuf>,
    },
    /// Write the index if it changed, acknowledge, and stop.
    Shutdown(oneshot::Sender<()>),
}

/// The archive state the actor owns: per channel, its indexed files and stamps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Archive {
    channels: HashMap<String, ChannelState>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ChannelState {
    files: HashMap<PathBuf, FileStat>,
    presence: Arc<ChannelPresence>,
}

type KnownFiles = HashMap<String, HashMap<PathBuf, FileStat>>;

impl Archive {
    fn absorb_file(
        &mut self,
        channel: String,
        path: PathBuf,
        stat: FileStat,
        stamps: &ChannelPresence,
    ) {
        let state = self
            .channels
            .entry(channel)
            .or_default();
        state
            .files
            .insert(path, stat);
        Arc::make_mut(&mut state.presence).merge(stamps);
    }

    /// Drops the entries of files the scan did not see; true when any went.
    fn drop_gone(
        &mut self,
        seen: &HashMap<String, HashSet<PathBuf>>,
        unlisted: &HashSet<PathBuf>,
    ) -> bool {
        let mut dropped = false;
        for (key, state) in &mut self.channels {
            let seen = seen.get(key);
            let before = state
                .files
                .len();
            state
                .files
                .retain(|path, _| {
                    seen.is_some_and(|s| s.contains(path))
                        || path
                            .parent()
                            .is_some_and(|dir| unlisted.contains(dir))
                });
            dropped |= state
                .files
                .len()
                != before;
        }
        dropped
    }

    fn known_files(&self) -> KnownFiles {
        self.channels
            .iter()
            .map(|(key, state)| {
                (
                    key.clone(),
                    state
                        .files
                        .clone(),
                )
            })
            .collect()
    }

    fn from_index(doc: IndexDoc) -> Self {
        let channels = doc
            .channels
            .into_iter()
            .map(|channel| {
                let files = channel
                    .files
                    .into_iter()
                    .map(|file| {
                        (
                            PathBuf::from(file.path),
                            FileStat {
                                mtime: file.mtime,
                                size: file.size,
                            },
                        )
                    })
                    .collect();
                let mut presence = ChannelPresence {
                    topic: channel.topic,
                    users: HashMap::new(),
                };
                for user in channel.users {
                    let key = casemap(&user.nick);
                    presence
                        .users
                        .entry(key)
                        .or_insert_with(|| UserStamps::new(&user.nick))
                        .merge(&user);
                }
                (
                    channel.channel,
                    ChannelState {
                        files,
                        presence: Arc::new(presence),
                    },
                )
            })
            .collect();
        Self { channels }
    }

    fn to_index(&self) -> IndexDoc {
        let mut channels: Vec<IndexChannel> = self
            .channels
            .iter()
            .map(|(key, state)| {
                let mut files: Vec<IndexFile> = state
                    .files
                    .iter()
                    .map(|(path, stat)| IndexFile {
                        path: path
                            .to_string_lossy()
                            .into_owned(),
                        mtime: stat.mtime,
                        size: stat.size,
                    })
                    .collect();
                files.sort_by(|a, b| {
                    a.path
                        .cmp(&b.path)
                });
                let users: BTreeMap<&String, &UserStamps> = state
                    .presence
                    .users
                    .iter()
                    .collect();
                IndexChannel {
                    channel: key.clone(),
                    files,
                    topic: state
                        .presence
                        .topic
                        .clone(),
                    users: users
                        .into_values()
                        .cloned()
                        .collect(),
                }
            })
            .collect();
        channels.sort_by(|a, b| {
            a.channel
                .cmp(&b.channel)
        });
        IndexDoc {
            version: INDEX_VERSION,
            channels,
        }
    }
}

#[derive(Deserialize)]
struct IndexHeader {
    version: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexDoc {
    version: u32,
    channels: Vec<IndexChannel>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexChannel {
    channel: String,
    files: Vec<IndexFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    topic: Option<Topic>,
    #[serde(default)]
    users: Vec<UserStamps>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    path: String,
    mtime: Timestamp,
    size: u64,
}

/// Why the index could not be loaded.
#[derive(Debug)]
enum IndexError {
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

fn load_index(path: &Path) -> Result<Archive, IndexError> {
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
fn load_or_rescan(path: &Path) -> (Archive, Option<String>) {
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
enum WriteError {
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

fn write_index(path: &Path, doc: &IndexDoc) -> Result<(), WriteError> {
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

/// A channel the scan visits.
struct ScanChannel {
    key: String,
    dirs: Vec<ChannelDir>,
}

fn scan_channels(root: &ChannelNode) -> Vec<ScanChannel> {
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
struct ScanSummary {
    parsed: usize,
    unchanged: usize,
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
fn scan(
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

/// What request handlers read: the published stamps and the scanning flag.
pub struct Presence {
    channels: ArcSwap<HashMap<String, Arc<ChannelPresence>>>,
    scanning: AtomicBool,
    inbox: mpsc::Sender<Input>,
    log_tz: TimeZone,
}

impl Presence {
    /// A stamper for lines the tail reads, ZNC directories read in `log_timezone`.
    pub fn live_clock(&self) -> LiveClock {
        LiveClock {
            log_tz: self
                .log_tz
                .clone(),
            by_dir: HashMap::new(),
        }
    }

    /// Merges `stamps`, read live from the channel keyed `channel`. Blocks
    /// while the actor's inbox is full; call it from a blocking thread.
    pub fn feed(&self, channel: String, stamps: ChannelPresence) {
        // Fails only once the actor has stopped at shutdown. The file's stat
        // has changed since its index entry, so the next start reparses it.
        let _ = self
            .inbox
            .blocking_send(Input::Lines { channel, stamps });
    }

    /// Whether the startup scan is still running. Read it before
    /// [`Presence::channel`]: once false, the snapshot holds the scan's result.
    pub fn scanning(&self) -> bool {
        self.scanning
            .load(Ordering::Acquire)
    }

    /// The last published stamps of the channel keyed `key`.
    pub fn channel(&self, key: &str) -> Option<Arc<ChannelPresence>> {
        self.channels
            .load()
            .get(key)
            .cloned()
    }

    /// Asks the actor to write the index if it changed and stop, and waits
    /// for it to acknowledge.
    pub async fn shutdown(&self) {
        let (ack, acked) = oneshot::channel();
        if self
            .inbox
            .send(Input::Shutdown(ack))
            .await
            .is_err()
        {
            eprintln!("presence: the actor already stopped, index not written at shutdown");
            return;
        }
        if acked
            .await
            .is_err()
        {
            eprintln!("presence: the actor stopped without acknowledging the shutdown write");
        }
    }
}

/// Starts the actor, which loads the index at `index_path` and scans every
/// channel of `root`, ZNC directories read in `log_tz`.
pub fn start(index_path: PathBuf, log_tz: TimeZone, root: &ChannelNode) -> Arc<Presence> {
    let (inbox, rx) = mpsc::channel(64);
    let shared = Arc::new(Presence {
        channels: ArcSwap::from_pointee(HashMap::new()),
        scanning: AtomicBool::new(true),
        inbox,
        log_tz: log_tz.clone(),
    });
    let channels = scan_channels(root);
    tokio::spawn(run_actor(
        Arc::clone(&shared),
        index_path,
        log_tz,
        channels,
        rx,
    ));
    shared
}

async fn run_actor(
    shared: Arc<Presence>,
    index_path: PathBuf,
    log_tz: TimeZone,
    channels: Vec<ScanChannel>,
    rx: mpsc::Receiver<Input>,
) {
    let path = index_path.clone();
    let archive = match tokio::task::spawn_blocking(move || load_or_rescan(&path)).await {
        Ok((archive, problem)) => {
            if let Some(problem) = problem {
                eprintln!("{problem}");
            }
            archive
        }
        Err(e) => {
            eprintln!(
                "presence: loading index {} failed: {e}; rescanning everything",
                index_path.display()
            );
            Archive::default()
        }
    };
    let known = archive.known_files();
    let mut actor = Actor {
        dirty: archive
            .channels
            .keys()
            .cloned()
            .collect(),
        archive,
        shared,
        index_path,
        unsaved: false,
    };
    actor.publish();

    let tx = actor
        .shared
        .inbox
        .clone();
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let summary = scan(&channels, &known, &log_tz, &mut |input| {
            tx.blocking_send(input)
                .is_ok()
        });
        eprintln!(
            "presence: scan done in {:.1?}, {} files parsed, {} unchanged",
            started.elapsed(),
            summary.parsed,
            summary.unchanged
        );
    });
    actor
        .run(rx)
        .await;
}

struct Actor {
    archive: Archive,
    shared: Arc<Presence>,
    index_path: PathBuf,
    /// Channels changed since the last publish.
    dirty: HashSet<String>,
    /// Whether the archive changed since the last successful write.
    unsaved: bool,
}

impl Actor {
    async fn run(mut self, mut inbox: mpsc::Receiver<Input>) {
        let mut publish = tokio::time::interval(PUBLISH_EVERY);
        publish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut save =
            tokio::time::interval_at(tokio::time::Instant::now() + SAVE_EVERY, SAVE_EVERY);
        save.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                input = inbox.recv() => {
                    // Presence holds a sender, so the inbox cannot close under the actor.
                    let Some(input) = input else { return };
                    if self.handle(input).await.is_break() {
                        return;
                    }
                }
                _ = publish.tick() => self.publish(),
                _ = save.tick() => if self.unsaved {
                    self.save().await;
                },
            }
        }
    }

    async fn handle(&mut self, input: Input) -> std::ops::ControlFlow<()> {
        match input {
            Input::File {
                channel,
                path,
                stat,
                stamps,
            } => {
                self.dirty
                    .insert(channel.clone());
                self.archive
                    .absorb_file(channel, path, stat, &stamps);
                self.unsaved = true;
            }
            Input::Lines { channel, stamps } => {
                let state = self
                    .archive
                    .channels
                    .entry(channel.clone())
                    .or_default();
                Arc::make_mut(&mut state.presence).merge(&stamps);
                self.dirty
                    .insert(channel);
                self.unsaved = true;
            }
            Input::ScanFinished { seen, unlisted } => {
                if self
                    .archive
                    .drop_gone(&seen, &unlisted)
                {
                    self.unsaved = true;
                }
                self.publish();
                self.shared
                    .scanning
                    .store(false, Ordering::Release);
                self.save()
                    .await;
            }
            Input::Shutdown(ack) => {
                if self.unsaved {
                    self.save()
                        .await;
                }
                // The caller may have given up waiting; nothing is left to tell it.
                let _ = ack.send(());
                return std::ops::ControlFlow::Break(());
            }
        }
        std::ops::ControlFlow::Continue(())
    }

    fn publish(&mut self) {
        if self
            .dirty
            .is_empty()
        {
            return;
        }
        let mut channels = HashMap::clone(
            &self
                .shared
                .channels
                .load(),
        );
        for key in self
            .dirty
            .drain()
        {
            if let Some(state) = self
                .archive
                .channels
                .get(&key)
            {
                channels.insert(key, Arc::clone(&state.presence));
            }
        }
        self.shared
            .channels
            .store(Arc::new(channels));
    }

    async fn save(&mut self) {
        let snapshot = self
            .archive
            .clone();
        let path = self
            .index_path
            .clone();
        let written =
            tokio::task::spawn_blocking(move || write_index(&path, &snapshot.to_index())).await;
        match written {
            Ok(Ok(())) => self.unsaved = false,
            Ok(Err(e)) => eprintln!(
                "presence: cannot write index {}: {e}",
                self.index_path
                    .display()
            ),
            Err(e) => eprintln!(
                "presence: cannot write index {}: the writer failed: {e}",
                self.index_path
                    .display()
            ),
        }
    }
}

#[derive(Serialize)]
struct PresenceJson<'a> {
    scanning: bool,
    topic: Option<&'a Topic>,
    users: BTreeMap<&'a str, UserJson>,
}

#[derive(Serialize)]
struct UserJson {
    online: Option<Timestamp>,
    offline: Option<Timestamp>,
    spoke: Option<Timestamp>,
}

/// The presence endpoint's JSON body for one channel.
pub fn json_body(scanning: bool, channel: Option<&ChannelPresence>) -> serde_json::Result<String> {
    let users = channel
        .into_iter()
        .flat_map(|c| {
            c.users
                .values()
        })
        .map(|user| {
            (
                user.nick
                    .as_str(),
                UserJson {
                    online: user.online,
                    offline: user.offline,
                    spoke: user.spoke,
                },
            )
        })
        .collect();
    serde_json::to_string(&PresenceJson {
        scanning,
        topic: channel.and_then(|c| {
            c.topic
                .as_ref()
        }),
        users,
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use jiff::tz::TimeZone;
    use tower::ServiceExt;

    use super::*;
    use crate::resolve_log_timezone;
    use crate::test_support::{add_channel, app, app_state, tempdir};

    fn ts(s: &str) -> Timestamp {
        s.parse()
            .unwrap()
    }

    fn user(
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
    fn channel_dir(root: &Path, format: LogFormat, files: &[(&str, &str)]) -> ChannelDir {
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

    fn scan_dirs(
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
    fn absorb(archive: &mut Archive, inputs: Vec<Input>) -> Vec<PathBuf> {
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
                Input::Lines { .. } | Input::Shutdown(_) => {
                    unreachable!("the scanner sends only File and ScanFinished")
                }
            }
        }
        parsed
    }

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

    fn file_stamps(lines: &[(&str, LineKind)]) -> ChannelPresence {
        let mut stamps = ChannelPresence::default();
        for (at, kind) in lines {
            stamps.apply(ts(at), kind);
        }
        stamps
    }

    fn join(nick: &str) -> LineKind {
        LineKind::Join {
            nick: nick.into(),
            userhost: "~u@h".into(),
        }
    }

    fn says(nick: &str) -> LineKind {
        LineKind::Message {
            nick: nick.into(),
            text: "x".into(),
        }
    }

    fn quits(nick: &str) -> LineKind {
        LineKind::Quit {
            nick: nick.into(),
            userhost: "~u@h".into(),
            reason: String::new(),
        }
    }

    fn topic(nick: &str, text: &str) -> LineKind {
        LineKind::Topic {
            nick: nick.into(),
            topic: text.into(),
        }
    }

    #[test]
    fn merging_is_independent_of_order_and_repetition() {
        let files = [
            file_stamps(&[
                ("2026-01-01T00:00:10Z", join("Foo")),
                ("2026-01-01T00:00:20Z", topic("a", "x")),
            ]),
            file_stamps(&[
                ("2026-01-01T00:00:30Z", says("foo")),
                ("2026-01-01T00:00:20Z", topic("b", "y")),
            ]),
            file_stamps(&[
                ("2026-01-01T00:00:30Z", quits("FOO")),
                ("2026-01-01T00:00:05Z", join("Bar")),
                ("2026-01-01T00:00:40Z", quits("bar")),
            ]),
        ];
        let orders = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let merged: Vec<ChannelPresence> = orders
            .iter()
            .map(|order| {
                let mut state = ChannelPresence::default();
                for &i in order
                    .iter()
                    .chain(order)
                {
                    state.merge(&files[i]);
                }
                state
            })
            .collect();
        for state in &merged {
            assert_eq!(state, &merged[0]);
        }
        let state = &merged[0];
        assert_eq!(
            state.users["foo"].nick, "foo",
            "tie at :30 goes to the greater form"
        );
        assert_eq!(state.users["bar"].nick, "bar");
        assert_eq!(
            state
                .topic
                .as_ref()
                .unwrap()
                .nick,
            "b"
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

    #[test]
    fn nicks_are_keyed_under_rfc1459_casemapping() {
        let state = file_stamps(&[
            ("2026-01-01T00:00:01Z", join("Foo[a]\\~")),
            ("2026-01-01T00:00:02Z", says("foo{A}|^")),
        ]);
        assert_eq!(
            state
                .users
                .keys()
                .collect::<Vec<_>>(),
            ["foo{a}|^"]
        );
        let user = &state.users["foo{a}|^"];
        assert_eq!(user.nick, "foo{A}|^");
        assert_eq!(user.online, Some(ts("2026-01-01T00:00:02Z")));
    }

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
    fn live_clock_keeps_the_fold_across_batches_and_follows_the_day_file() {
        let tz = resolve_log_timezone("EST5EDT,M3.2.0,M11.1.0").unwrap();
        let (inbox, _rx) = mpsc::channel(1);
        let presence = Presence {
            channels: ArcSwap::from_pointee(HashMap::new()),
            scanning: AtomicBool::new(false),
            inbox,
            log_tz: tz,
        };
        let mut clock = presence.live_clock();
        let line = |h, m, nick: &str| LogLine {
            time: hms(h, m, 0),
            kind: says(nick),
        };
        let spoke = |stamps: Option<ChannelPresence>, nick: &str| {
            stamps
                .unwrap()
                .users[nick]
                .spoke
                .unwrap()
        };
        let fold_day = Path::new("/l/#c/2026-11-01.log");
        let first = clock.stamps(fold_day, LogFormat::Znc, &[line(1, 50, "a")]);
        assert_eq!(spoke(first, "a"), ts("2026-11-01T05:50:00Z"));
        // The previous batch's 01:50 EDT puts this 01:10 in EST.
        let second = clock.stamps(fold_day, LogFormat::Znc, &[line(1, 10, "b")]);
        assert_eq!(spoke(second, "b"), ts("2026-11-01T06:10:00Z"));

        let next_day = Path::new("/l/#c/2026-11-02.log");
        let third = clock.stamps(next_day, LogFormat::Znc, &[line(0, 5, "c")]);
        assert_eq!(spoke(third, "c"), ts("2026-11-02T05:05:00Z"));
        let iso = Path::new("/l/#i/2026-11-02.log");
        let fourth = clock.stamps(iso, LogFormat::Iso8601, &[line(0, 5, "d")]);
        assert_eq!(spoke(fourth, "d"), ts("2026-11-02T00:05:00Z"));

        assert_eq!(
            clock.stamps(
                Path::new("/l/#c/today.log"),
                LogFormat::Znc,
                &[line(0, 0, "e")]
            ),
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
            ("version: 1\nchannels: [ {\n", "cannot parse"),
            ("version: 2\nchannels: []\n", "version 2, expected 1"),
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

    async fn get(
        app: &axum::Router,
        uri: &str,
        if_none_match: Option<&str>,
    ) -> (StatusCode, Option<String>, String) {
        let mut req = Request::get(uri);
        if let Some(tag) = if_none_match {
            req = req.header(header::IF_NONE_MATCH, tag);
        }
        let resp = app
            .clone()
            .oneshot(
                req.body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let etag = resp
            .headers()
            .get(header::ETAG)
            .map(|v| {
                v.to_str()
                    .unwrap()
                    .to_owned()
            });
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, etag, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn presence_route_serves_the_scan_with_an_etag() {
        let root = tempdir();
        let dir = channel_dir(
            root.path(),
            LogFormat::Znc,
            &[(
                "2026-01-01.log",
                "[10:00:00] *** Joins: Foo (~f@h)\n[10:00:05] <Foo> hi\n[10:01:00] *** op changes topic to 'hello'\n[10:02:00] *** Quits: bar (~b@h) (x)\n",
            )],
        );
        let mut channels = ChannelNode::default();
        add_channel(&mut channels, &["#c"], vec![dir]);
        let index = root
            .path()
            .join("presence.yaml");
        let presence = start(index.clone(), TimeZone::UTC, &channels);
        let mut state = app_state(channels, "logs_dirs: []");
        state.presence = Some(Arc::clone(&presence));
        let app = app(Arc::new(state), "192.0.2.1:4000");

        let deadline = Instant::now() + Duration::from_secs(5);
        while presence.scanning() {
            assert!(Instant::now() < deadline, "scan not done within 5 s");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (status, etag, body) = get(&app, "/%23c/presence", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "scanning": false,
                "topic": {"nick": "op", "text": "hello", "at": "2026-01-01T10:01:00Z"},
                "users": {
                    "Foo": {"online": "2026-01-01T10:00:05Z", "offline": null, "spoke": "2026-01-01T10:00:05Z"},
                    "bar": {"online": null, "offline": "2026-01-01T10:02:00Z", "spoke": null},
                },
            })
        );
        let etag = etag.expect("an ETag");

        for tag in [
            etag.clone(),
            format!("W/{etag}"),
            format!("\"other\", W/{etag}"),
            "*".to_owned(),
        ] {
            let (status, got, body) = get(&app, "/%23c/presence", Some(&tag)).await;
            assert_eq!(status, StatusCode::NOT_MODIFIED, "{tag}");
            assert_eq!(got.as_ref(), Some(&etag), "{tag}");
            assert_eq!(body, "", "{tag}");
        }
        let (status, _, _) = get(&app, "/%23c/presence", Some("\"other\"")).await;
        assert_eq!(status, StatusCode::OK);

        presence
            .shutdown()
            .await;
        // The ack follows the post-scan write, which the actor runs before reading Shutdown.
        let written = load_index(&index).expect("the index is written by shutdown at the latest");
        assert_eq!(
            written.channels["#c"]
                .presence
                .users
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn presence_route_is_absent_without_the_section() {
        let mut channels = ChannelNode::default();
        add_channel(&mut channels, &["#c"], Vec::new());
        let app = app(
            Arc::new(app_state(channels, "logs_dirs: []")),
            "192.0.2.1:4000",
        );
        let (status, _, _) = get(&app, "/%23c/presence", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
