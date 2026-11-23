//! Per-nick archive stamps (last online, last offline, last spoke) and the
//! latest topic of each channel, computed from the log files in the
//! background, cached in a YAML index, and served by the presence endpoint
//! together with the live snapshot a ZNC session provides.
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
//! "changes topic to" lines; the channel modes from the newest
//! `Server sets mode` line (ZNC's logged 324 reply; a server's own changes
//! carry its name instead) and the setting changes after it, kept as a
//! [`ModeHistory`] and replayed with the network's mode table when the JSON
//! is built. Two changes in the same second replay in sorted, not logged,
//! order. Users are keyed by the nick under rfc1459
//! casemapping, the network's CASEMAPPING: ASCII lowercase, plus `[]\~`
//! folding to `{}|^`.
//!
//! # Ownership
//!
//! One actor task owns the mutable state and takes [`Input`] messages: a
//! parsed file from the scanner, lines from the tail, ZNC session events,
//! the end of the scan, and a shutdown request. Live lines merge into the
//! stamps but leave the file's index entry alone: its stat no longer
//! matches, so the next start reparses the file, which the max-merge makes
//! harmless. The tail stamps its lines with a [`LogClock`], which carries
//! the fold rule's previous line across batches of the same day file. Log
//! pages and search use the same clock for the UTC instant each line's time
//! link carries, so a page and the stamps never disagree.
//!
//! # Live snapshots and the session race
//!
//! The actor also owns each channel's [`LiveChannel`], never persisted. A
//! ZNC session is numbered by an epoch and reported as [`SessionEvent`]s.
//! From its start, lines for a channel whose burst has not arrived yet are
//! queued as well as applied to the old snapshot; the burst replaces the
//! snapshot and the queue is replayed on it, so a quit logged while ZNC was
//! sending the 353 cannot bring the quitter back. Replay is safe because
//! every delta is idempotent (see live.rs). A burst from an epoch that is
//! not the current session's is dropped. When the session ends, snapshots
//! it did not replace go stale, and every one when it failed.
//!
//! Readers see an `ArcSwap` snapshot of `Arc<ChannelPresence>` per channel,
//! republished at most once per second for the channels that changed, and an
//! atomic scanning flag stored after the final republish. No lock guards the
//! state.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::ChannelNode;
use crate::live::{LiveChannel, ModeLine};
use crate::parser::LineKind;

mod clock;
mod index;
mod json;
mod scan;
#[cfg(test)]
mod test_helpers;

pub use clock::LogClock;
pub use json::json_body;
pub use scan::channel_keys;

use index::{
    FileStat, INDEX_VERSION, IndexChannel, IndexDoc, IndexFile, load_or_rescan, write_index,
};
use scan::{ScanChannel, scan, scan_channels};

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
    /// Channel modes: the latest logged 324 reply and the changes since.
    pub modes: ModeHistory,
}

/// The newest full mode set ZNC logged after a join, as
/// `*** Server sets mode: +nt`, and the channel-mode changes logged at or
/// after it, sorted. The set is the newest by (time, modes, args), like the
/// topic, so merging stays order-independent; changes from before it are
/// dropped. Lines that only change member prefixes or lists are not kept:
/// they are frequent and never change the channel's settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeHistory {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<ModeLine>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<ModeLine>,
}

/// Mode letters that are member prefixes or list entries on common
/// networks; a line made only of these leaves the channel's settings alone.
const NOT_SETTINGS: &str = "+-ovhaqbeI";

impl ModeHistory {
    fn apply(&mut self, nick: &str, line: ModeLine) {
        if nick == "Server" {
            self.set_baseline(line);
        } else if line
            .modes
            .chars()
            .any(|c| !NOT_SETTINGS.contains(c))
        {
            self.changes
                .push(line);
            self.normalize();
        }
    }

    fn set_baseline(&mut self, line: ModeLine) {
        if self
            .baseline
            .as_ref()
            .is_none_or(|current| &line > current)
        {
            self.baseline = Some(line);
            self.normalize();
        }
    }

    fn merge(&mut self, other: &ModeHistory) {
        if let Some(line) = &other.baseline {
            self.set_baseline(line.clone());
        }
        self.changes
            .extend(
                other
                    .changes
                    .iter()
                    .cloned(),
            );
        self.normalize();
    }

    fn normalize(&mut self) {
        self.changes
            .sort();
        self.changes
            .dedup();
        if let Some(base) = &self.baseline {
            self.changes
                .retain(|line| line.at >= base.at);
        }
    }
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
            LineKind::Mode { nick, modes, args } => self
                .modes
                .apply(
                    nick,
                    ModeLine {
                        at,
                        modes: modes.clone(),
                        args: args.clone(),
                    },
                ),
            LineKind::Notice { .. } => {}
        }
    }

    /// Takes the maximum of every stamp in `self` and `other`.
    pub fn merge(&mut self, other: &ChannelPresence) {
        if let Some(topic) = &other.topic {
            self.set_topic(topic.clone());
        }
        self.modes
            .merge(&other.modes);
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
pub fn casemap(nick: &str) -> String {
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

/// What the ZNC session reports to presence, in order.
#[derive(Debug)]
pub enum SessionEvent {
    /// Session `epoch` is about to connect: lines read from now on are
    /// queued per channel until that channel's burst.
    Started { epoch: u64 },
    /// One channel's burst from session `epoch`.
    Burst {
        epoch: u64,
        channel: String,
        live: Box<LiveChannel>,
    },
    /// Session `epoch` is over. A snapshot it did not replace goes stale,
    /// every snapshot when it failed.
    Ended { epoch: u64, ok: bool },
    /// The bouncer reconnected to IRC: every snapshot is stale until the next session.
    Stale,
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
        lines: Vec<(Timestamp, LineKind)>,
    },
    /// Progress of the ZNC session.
    Session(SessionEvent),
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
                let mut modes = ModeHistory::default();
                modes.merge(&channel.modes);
                let mut presence = ChannelPresence {
                    topic: channel.topic,
                    users: HashMap::new(),
                    modes,
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
                    modes: state
                        .presence
                        .modes
                        .clone(),
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

/// What request handlers read: the published stamps and the scanning flag.
pub struct Presence {
    channels: ArcSwap<HashMap<String, Arc<ChannelPresence>>>,
    live: ArcSwap<HashMap<String, Arc<LiveChannel>>>,
    scanning: AtomicBool,
    inbox: mpsc::Sender<Input>,
}

impl Presence {
    /// Applies `lines`, read live from the channel keyed `channel`, to its
    /// stamps and live snapshot. Blocks while the actor's inbox is full;
    /// call it from a blocking thread.
    pub fn feed(&self, channel: String, lines: Vec<(Timestamp, LineKind)>) {
        // Fails only once the actor has stopped at shutdown. The file's stat
        // has changed since its index entry, so the next start reparses it.
        let _ = self
            .inbox
            .blocking_send(Input::Lines { channel, lines });
    }

    /// Hands the actor one step of a ZNC session.
    pub async fn session(&self, event: SessionEvent) {
        if self
            .inbox
            .send(Input::Session(event))
            .await
            .is_err()
        {
            eprintln!("presence: the actor stopped, dropping a ZNC session event");
        }
    }

    /// The last published live snapshot of the channel keyed `key`.
    pub fn live(&self, key: &str) -> Option<Arc<LiveChannel>> {
        self.live
            .load()
            .get(key)
            .cloned()
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
        live: ArcSwap::from_pointee(HashMap::new()),
        scanning: AtomicBool::new(true),
        inbox,
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
        live: LiveState::default(),
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

/// Live snapshots per channel and the session in progress, if any.
#[derive(Debug, Default)]
struct LiveState {
    channels: HashMap<String, Arc<LiveChannel>>,
    session: Option<PendingSession>,
    /// Channels changed since the last publish.
    dirty: HashSet<String>,
}

#[derive(Debug)]
struct PendingSession {
    epoch: u64,
    /// Channels whose burst already replaced their snapshot.
    bursted: HashSet<String>,
    /// Lines read since the session started, for channels not yet bursted.
    queued: HashMap<String, Vec<(Timestamp, LineKind)>>,
}

impl LiveState {
    fn lines(&mut self, channel: &str, lines: Vec<(Timestamp, LineKind)>) {
        if let Some(live) = self
            .channels
            .get_mut(channel)
        {
            let live = Arc::make_mut(live);
            for (at, kind) in &lines {
                live.apply(*at, kind);
            }
            self.dirty
                .insert(channel.to_owned());
        }
        // The burst may predate or postdate these lines; replaying them on it is idempotent.
        if let Some(session) = &mut self.session {
            if !session
                .bursted
                .contains(channel)
            {
                session
                    .queued
                    .entry(channel.to_owned())
                    .or_default()
                    .extend(lines);
            }
        }
    }

    fn session(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Started { epoch } => {
                self.session = Some(PendingSession {
                    epoch,
                    bursted: HashSet::new(),
                    queued: HashMap::new(),
                });
            }
            SessionEvent::Burst {
                epoch,
                channel,
                mut live,
            } => {
                let Some(session) = self
                    .session
                    .as_mut()
                    .filter(|s| s.epoch == epoch)
                else {
                    eprintln!(
                        "presence: dropping a burst of {channel} from ended ZNC session {epoch}"
                    );
                    return;
                };
                for (at, kind) in session
                    .queued
                    .remove(&channel)
                    .unwrap_or_default()
                {
                    live.apply(at, &kind);
                }
                session
                    .bursted
                    .insert(channel.clone());
                self.channels
                    .insert(channel.clone(), Arc::from(live));
                self.dirty
                    .insert(channel);
            }
            SessionEvent::Ended { epoch, ok } => {
                let Some(session) = self
                    .session
                    .take_if(|s| s.epoch == epoch)
                else {
                    eprintln!("presence: end of ZNC session {epoch}, which is not the current one");
                    return;
                };
                let keep = if ok { session.bursted } else { HashSet::new() };
                self.mark_stale(|key| !keep.contains(key));
            }
            SessionEvent::Stale => self.mark_stale(|_| true),
        }
    }

    fn mark_stale(&mut self, which: impl Fn(&str) -> bool) {
        for (key, live) in &mut self.channels {
            if live.stale || !which(key) {
                continue;
            }
            Arc::make_mut(live).stale = true;
            self.dirty
                .insert(key.clone());
        }
    }
}

struct Actor {
    archive: Archive,
    live: LiveState,
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
            Input::Lines { channel, lines } => {
                let state = self
                    .archive
                    .channels
                    .entry(channel.clone())
                    .or_default();
                let stamps = Arc::make_mut(&mut state.presence);
                for (at, kind) in &lines {
                    stamps.apply(*at, kind);
                }
                self.live
                    .lines(&channel, lines);
                self.dirty
                    .insert(channel);
                self.unsaved = true;
            }
            Input::Session(event) => self
                .live
                .session(event),
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
        self.publish_live();
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

    fn publish_live(&mut self) {
        if self
            .live
            .dirty
            .is_empty()
        {
            return;
        }
        let mut channels = HashMap::clone(
            &self
                .shared
                .live
                .load(),
        );
        for key in self
            .live
            .dirty
            .drain()
        {
            if let Some(live) = self
                .live
                .channels
                .get(&key)
            {
                channels.insert(key, Arc::clone(live));
            }
        }
        self.shared
            .live
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

#[cfg(test)]
mod tests {

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use jiff::tz::TimeZone;
    use tower::ServiceExt;

    use super::index::load_index;
    use super::test_helpers::*;
    use super::*;
    use crate::live::ModeSpec;
    use crate::parser::LogFormat;

    use crate::test_support::{add_channel, app, app_state, tempdir};

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
                "snapshot_at": null,
                "stale": false,
                "modes": null,
                "topic": {"source": "archive", "nick": "op", "text": "hello", "at": "2026-01-01T10:01:00Z"},
                "users": {
                    "Foo": {"live": null, "prefixes": "", "online": "2026-01-01T10:00:05Z", "offline": null, "spoke": "2026-01-01T10:00:05Z"},
                    "bar": {"live": null, "prefixes": "", "online": null, "offline": "2026-01-01T10:02:00Z", "spoke": null},
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
    async fn log_page_points_at_presence_only_when_it_is_on() {
        let root = tempdir();
        let tree = || {
            let dir = channel_dir(
                root.path(),
                LogFormat::Znc,
                &[("2026-01-01.log", "[10:00:00] <Foo[x]> hi\n")],
            );
            let mut channels = ChannelNode::default();
            add_channel(&mut channels, &["#c"], vec![dir]);
            channels
        };
        let presence = start(
            root.path()
                .join("presence.yaml"),
            TimeZone::UTC,
            &tree(),
        );
        for on in [true, false] {
            let mut state = app_state(tree(), "logs_dirs: []");
            state.presence = on.then(|| Arc::clone(&presence));
            let app = app(Arc::new(state), "192.0.2.1:4000");
            let (status, _, body) = get(&app, "/%23c/2026-01-01", None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(body.contains(r#"data-nick="Foo[x]""#), "{body}");
            // Log times use the zone selector with or without presence.
            assert!(body.contains(r#"<select id="tz-select""#), "{body}");
            for wired in [
                r#"data-presence="/%23c/presence""#,
                r#"<p id="topic" hidden"#,
            ] {
                assert_eq!(
                    body.contains(wired),
                    on,
                    "{wired} with presence {on}: {body}"
                );
            }
        }
        presence
            .shutdown()
            .await;
    }

    fn burst(epoch: u64, names: &[&str]) -> SessionEvent {
        SessionEvent::Burst {
            epoch,
            channel: "#c".into(),
            live: Box::new(LiveChannel::from_burst(
                names
                    .iter()
                    .copied(),
                None,
                ts("2026-01-01T00:00:00Z"),
                ModeSpec::default(),
            )),
        }
    }

    fn members(live: &LiveState, key: &str) -> Vec<String> {
        let mut nicks: Vec<String> = live.channels[key]
            .members
            .values()
            .map(|m| {
                m.nick
                    .clone()
            })
            .collect();
        nicks.sort();
        nicks
    }

    #[test]
    fn a_line_read_before_its_channels_burst_is_replayed_on_it() {
        let at = ts("2026-01-01T00:00:01Z");
        let mut live = LiveState::default();
        live.session(SessionEvent::Started { epoch: 1 });
        // The quit is logged while ZNC is still sending the burst, which lists alice.
        live.lines("#c", vec![(at, quits("alice"))]);
        live.session(burst(1, &["alice", "bob"]));
        assert_eq!(members(&live, "#c"), ["bob"]);

        // After its burst, a channel's lines apply directly and are not queued.
        live.lines("#c", vec![(at, join("carol"))]);
        assert_eq!(members(&live, "#c"), ["bob", "carol"]);
        assert!(
            live.session
                .as_ref()
                .unwrap()
                .queued
                .is_empty()
        );
        live.session(SessionEvent::Ended { epoch: 1, ok: true });
        assert!(!live.channels["#c"].stale);
    }

    #[test]
    fn snapshots_a_session_did_not_replace_go_stale() {
        let mut live = LiveState::default();
        live.session(SessionEvent::Started { epoch: 1 });
        live.session(burst(1, &["a"]));
        live.session(SessionEvent::Ended { epoch: 1, ok: true });
        assert!(!live.channels["#c"].stale);

        // A session with no JOIN for #c leaves its old snapshot, stale.
        live.session(SessionEvent::Started { epoch: 2 });
        live.session(SessionEvent::Ended { epoch: 2, ok: true });
        assert!(live.channels["#c"].stale);

        // A failed session stales even the channels it did burst.
        live.session(SessionEvent::Started { epoch: 3 });
        live.session(burst(3, &["b"]));
        assert!(!live.channels["#c"].stale);
        live.session(SessionEvent::Ended {
            epoch: 3,
            ok: false,
        });
        assert!(live.channels["#c"].stale);
        assert_eq!(members(&live, "#c"), ["b"]);

        // A burst from a session that is over changes nothing.
        live.session(burst(3, &["late"]));
        assert_eq!(members(&live, "#c"), ["b"]);

        live.session(SessionEvent::Started { epoch: 4 });
        live.session(burst(4, &["c"]));
        live.session(SessionEvent::Ended { epoch: 4, ok: true });
        live.session(SessionEvent::Stale);
        assert!(live.channels["#c"].stale);
    }

    #[test]
    fn channel_modes_come_from_the_newest_baseline_and_later_changes() {
        let root = tempdir();
        let dir = channel_dir(
            root.path(),
            LogFormat::Znc,
            &[
                (
                    "2026-01-01.log",
                    "[10:00:00] *** Server sets mode: +nt \n[10:05:00] *** op sets mode: +m\n",
                ),
                (
                    "2026-01-02.log",
                    "[09:00:00] *** Server sets mode: +kst sekrit\n[09:10:00] *** op sets mode: +l 25\n[09:11:00] *** op sets mode: +o bob\n[09:12:00] *** irc.example.org sets mode: +v bob\n",
                ),
            ],
        );
        let (inputs, _) = scan_dirs(vec![dir], &KnownFiles::new(), &TimeZone::UTC);
        let files: Vec<ChannelPresence> = inputs
            .into_iter()
            .filter_map(|input| match input {
                Input::File { stamps, .. } => Some(stamps),
                _ => None,
            })
            .collect();
        let mut forward = ChannelPresence::default();
        let mut backward = ChannelPresence::default();
        for file in &files {
            forward.merge(file);
        }
        for file in files
            .iter()
            .rev()
        {
            backward.merge(file);
        }
        assert_eq!(forward, backward);
        // The day-one +m predates the newest baseline; the +o and +v lines are not settings.
        assert_eq!(
            forward.modes,
            ModeHistory {
                baseline: Some(ModeLine {
                    at: ts("2026-01-02T09:00:00Z"),
                    modes: "+kst".into(),
                    args: vec!["sekrit".into()],
                }),
                changes: vec![ModeLine {
                    at: ts("2026-01-02T09:10:00Z"),
                    modes: "+l".into(),
                    args: vec!["25".into()],
                }],
            }
        );
        let body: serde_json::Value =
            serde_json::from_str(&json_body(false, Some(&forward), None).unwrap()).unwrap();
        assert_eq!(body["modes"], "+klst 25");

        let mut archive = Archive::default();
        archive.absorb_file(
            "#c".into(),
            "/x/2026-01-02.log".into(),
            FileStat {
                mtime: ts("2026-01-02T00:00:00Z"),
                size: 1,
            },
            &forward,
        );
        let index = root
            .path()
            .join("presence.yaml");
        write_index(&index, &archive.to_index()).unwrap();
        assert_eq!(load_index(&index).unwrap(), archive);
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
