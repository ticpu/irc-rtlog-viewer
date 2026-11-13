//! Streams lines appended to channel logs to SSE viewers and to presence.
//!
//! Each batch of new lines is parsed once and fed to presence before the
//! SSE sender lookup, so stamps advance with no viewer subscribed.
//!
//! Offsets start at every file's current end, ZNC `status/` files beside
//! the channel directories included: lines already on disk reach viewers
//! with the page and presence through its scan, and a "Connected to IRC"
//! line from before the start is never read as a new one.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::parser::{LogFormat, LogLine, parse_line};
use crate::presence::{LiveClock, Presence};
use crate::templates::render_line;
use crate::{AppState, ChannelNode};
use notify::{Event, EventKind, RecursiveMode, Watcher};

/// Tails the logs until the returned watcher is dropped, which closes its
/// event channel and ends the tail thread.
pub fn start_watcher(state: Arc<AppState>) -> notify::RecommendedWatcher {
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();

    let mut watcher = notify::recommended_watcher(tx).expect("failed to create file watcher");
    for dir in &state.logs_dirs {
        watcher
            .watch(dir, RecursiveMode::Recursive)
            .expect("failed to watch logs directory");
    }

    // Taken after the watch starts, so a line appended in between is either
    // already counted here or reported by a queued event.
    let mut offsets = Offsets::at_current_ends(&state.channels);
    let live = state
        .presence
        .as_ref()
        .map(|p| (Arc::clone(p), p.live_clock()));
    tokio::task::spawn_blocking(move || tail_loop(rx, &mut offsets, live, &state));
    watcher
}

fn tail_loop(
    rx: std::sync::mpsc::Receiver<notify::Result<Event>>,
    offsets: &mut Offsets,
    mut live: Option<(Arc<Presence>, LiveClock)>,
    state: &AppState,
) {
    for event in rx {
        let Ok(event) = event else { continue };
        if !matches!(event.kind, EventKind::Modify(_)) {
            continue;
        }

        for path in &event.paths {
            let Some(ext) = path.extension() else {
                continue;
            };
            if ext != "log" {
                continue;
            }

            let Some((channel_key, format)) = resolve_channel(path, state) else {
                continue;
            };

            let new_lines = match offsets.new_lines(path) {
                Ok(lines) => lines,
                Err(e) => {
                    eprintln!("tail: cannot read new lines of {}: {e}", path.display());
                    continue;
                }
            };
            let parsed: Vec<LogLine> = new_lines
                .lines()
                .filter_map(|line| parse_line(line, format))
                .collect();
            if parsed.is_empty() {
                continue;
            }

            if let Some((presence, clock)) = live.as_mut() {
                if let Some(stamps) = clock.stamps(path, format, &parsed) {
                    presence.feed(channel_key.clone(), stamps);
                }
            }

            let senders = state
                .sse_senders
                .blocking_read();
            let Some(sender) = senders.get(&channel_key) else {
                continue;
            };
            for line in &parsed {
                // An error only means no viewer is subscribed right now.
                let _ = sender.send(render_line(line).into_string());
            }
        }
    }
}

/// How far into each log file the tail has read.
struct Offsets(HashMap<PathBuf, u64>);

impl Offsets {
    /// Every existing `.log` of the channel directories and of the ZNC
    /// `status/` directories beside them, at its current end.
    fn at_current_ends(channels: &ChannelNode) -> Self {
        let mut offsets = Self(HashMap::new());
        let mut status_dirs = HashSet::new();
        let mut stack = vec![channels];
        while let Some(node) = stack.pop() {
            stack.extend(
                node.children
                    .values(),
            );
            let Some(channel) = &node.channel else {
                continue;
            };
            for dir in &channel.dirs {
                offsets.seed_dir(&dir.path);
                if dir.format != LogFormat::Znc {
                    continue;
                }
                if let Some(network) = dir
                    .path
                    .parent()
                {
                    status_dirs.insert(network.join("status"));
                }
            }
        }
        for dir in status_dirs {
            // A network whose status window was never logged has no such directory.
            if dir.is_dir() {
                offsets.seed_dir(&dir);
            }
        }
        offsets
    }

    fn seed_dir(&mut self, dir: &Path) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("tail: cannot list {}: {e}", dir.display());
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension() != Some("log".as_ref()) {
                continue;
            }
            match entry.metadata() {
                Ok(meta) => {
                    self.0
                        .insert(path, meta.len());
                }
                Err(e) => eprintln!("tail: cannot stat {}: {e}", path.display()),
            }
        }
    }

    /// Complete lines appended since the last call. A trailing partial line
    /// stays unread until its newline arrives; invalid UTF-8 is replaced.
    fn new_lines(&mut self, path: &Path) -> io::Result<String> {
        let mut file = File::open(path)?;
        let size = file
            .metadata()?
            .len();
        let mut pos = self
            .0
            .get(path)
            .copied()
            .unwrap_or(0);
        // Shorter than what was read: truncated or replaced, so start over.
        if size < pos {
            pos = 0;
        }
        file.seek(SeekFrom::Start(pos))?;
        let mut buf = Vec::new();
        file.take(size - pos)
            .read_to_end(&mut buf)?;
        let complete = buf
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        self.0
            .insert(path.to_owned(), pos + complete as u64);
        Ok(String::from_utf8_lossy(&buf[..complete]).into_owned())
    }
}

fn resolve_channel(path: &PathBuf, state: &AppState) -> Option<(String, LogFormat)> {
    let abs = std::fs::canonicalize(path).ok()?;
    let parent = abs.parent()?;
    for logs_dir in &state.logs_dirs {
        if let Ok(rel) = parent.strip_prefix(logs_dir) {
            let segments: Vec<String> = rel
                .components()
                .map(|c| {
                    c.as_os_str()
                        .to_string_lossy()
                        .to_string()
                })
                .collect();
            if let Some(channel) = find_channel_in_tree(&state.channels, &segments, 0) {
                let key = channel
                    .path_segments
                    .join("/");
                let format = channel
                    .dirs
                    .iter()
                    .find(|d| parent.starts_with(&d.path))
                    .map(|d| d.format)
                    .unwrap_or(channel.dirs[0].format);
                return Some((key, format));
            }
        }
    }
    None
}

fn find_channel_in_tree<'a>(
    node: &'a crate::ChannelNode,
    segments: &[String],
    depth: usize,
) -> Option<&'a crate::Channel> {
    if depth >= segments.len() {
        return node
            .channel
            .as_ref();
    }

    let child = node
        .children
        .get(&segments[depth])?;
    find_channel_in_tree(child, segments, depth + 1)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Duration;

    use super::*;
    use crate::ChannelDir;
    use crate::test_support::{add_channel, app_state, tempdir};

    fn append(path: &Path, bytes: &[u8]) {
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    #[test]
    fn new_lines_holds_back_partial_lines_and_survives_truncation() {
        let dir = tempdir();
        let log = dir
            .path()
            .join("2026-01-01.log");
        std::fs::write(&log, "old\n").unwrap();
        let mut offsets = Offsets(HashMap::from([(log.clone(), 4)]));

        append(&log, b"one\ntw");
        assert_eq!(
            offsets
                .new_lines(&log)
                .unwrap(),
            "one\n"
        );
        append(&log, b"o\n");
        assert_eq!(
            offsets
                .new_lines(&log)
                .unwrap(),
            "two\n"
        );
        assert_eq!(
            offsets
                .new_lines(&log)
                .unwrap(),
            ""
        );

        append(&log, b"caf\xe9\n");
        assert_eq!(
            offsets
                .new_lines(&log)
                .unwrap(),
            "caf\u{fffd}\n"
        );

        std::fs::write(&log, "new\n").unwrap();
        assert_eq!(
            offsets
                .new_lines(&log)
                .unwrap(),
            "new\n"
        );
    }

    #[tokio::test]
    async fn watcher_streams_only_lines_appended_after_start() {
        let root = tempdir();
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let dir = root_path.join("#c");
        std::fs::create_dir(&dir).unwrap();
        let log = dir.join("2026-01-01.log");
        std::fs::write(&log, "2026-01-01T00:00:00Z <a> before start\n").unwrap();

        let mut channels = ChannelNode::default();
        let dirs = vec![ChannelDir {
            path: dir.clone(),
            format: LogFormat::Iso8601,
        }];
        add_channel(&mut channels, &["#c"], dirs);
        let mut state = app_state(channels, "logs_dirs: []");
        state.logs_dirs = vec![root_path];
        let state = Arc::new(state);
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        state
            .sse_senders
            .write()
            .await
            .insert("#c".into(), tx);
        let watcher = start_watcher(Arc::clone(&state));

        append(&log, b"2026-01-01T00:00:01Z <a> after start\n");
        let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("no line streamed within 5 s")
            .unwrap();
        assert!(first.contains("after start"), "{first}");
        assert!(
            rx.try_recv()
                .is_err(),
            "more than the appended line was streamed"
        );
        drop(watcher);
    }

    /// `net/#c` under `root`, in ZNC format, with one day file holding `day`.
    fn znc_channel(root: &Path, day: &str) -> (ChannelNode, PathBuf) {
        let dir = root.join("net/#c");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("2026-01-01.log");
        std::fs::write(&log, day).unwrap();
        let mut channels = ChannelNode::default();
        let dirs = vec![ChannelDir {
            path: dir,
            format: LogFormat::Znc,
        }];
        add_channel(&mut channels, &["net", "#c"], dirs);
        (channels, log)
    }

    #[test]
    fn status_lines_on_disk_at_start_are_not_read_as_new() {
        let root = tempdir();
        let (channels, _) = znc_channel(root.path(), "[00:00:00] <a> hi\n");
        let status = root
            .path()
            .join("net/status");
        std::fs::create_dir(&status).unwrap();
        let log = status.join("2026-01-01.log");
        std::fs::write(&log, "[00:00:01] Connected to IRC (irc.example.org)\n").unwrap();

        let mut offsets = Offsets::at_current_ends(&channels);
        assert_eq!(
            offsets
                .new_lines(&log)
                .unwrap(),
            ""
        );
        append(&log, b"[00:05:00] Connected to IRC (irc.example.org)\n");
        assert_eq!(
            offsets
                .new_lines(&log)
                .unwrap(),
            "[00:05:00] Connected to IRC (irc.example.org)\n"
        );
    }

    #[tokio::test]
    async fn appended_quit_reaches_presence_with_no_viewer_subscribed() {
        let root = tempdir();
        let root_path = std::fs::canonicalize(root.path()).unwrap();
        let day = "[10:00:00] *** Joins: alice (~a@h)\n";
        let (scanned, log) = znc_channel(&root_path, day);
        let presence = crate::presence::start(
            root_path.join("presence.yaml"),
            jiff::tz::TimeZone::UTC,
            &scanned,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while presence.scanning() {
            assert!(
                std::time::Instant::now() < deadline,
                "scan not done within 5 s"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let (channels, _) = znc_channel(&root_path, day);
        let mut state = app_state(channels, "logs_dirs: []");
        state.logs_dirs = vec![root_path.clone()];
        state.presence = Some(Arc::clone(&presence));
        let watcher = start_watcher(Arc::new(state));

        append(&log, b"[10:00:05] *** Quits: alice (~a@h) (bye)\n");
        let want: jiff::Timestamp = "2026-01-01T10:00:05Z"
            .parse()
            .unwrap();
        loop {
            let offline = presence
                .channel("net/#c")
                .and_then(|c| {
                    c.users
                        .get("alice")
                        .and_then(|u| u.offline)
                });
            if offline == Some(want) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline + Duration::from_secs(5),
                "the quit did not reach presence within 5 s: {offline:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(watcher);
        presence
            .shutdown()
            .await;
    }
}
