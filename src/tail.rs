use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::parser::{LogFormat, parse_line};
use crate::templates::render_line;
use crate::{AppState, ChannelNode};
use notify::{Event, EventKind, RecursiveMode, Watcher};

/// Streams lines appended to channel logs until the returned watcher is dropped,
/// which closes its event channel and ends the tail thread.
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
    tokio::task::spawn_blocking(move || tail_loop(rx, &mut offsets, &state));
    watcher
}

fn tail_loop(
    rx: std::sync::mpsc::Receiver<notify::Result<Event>>,
    offsets: &mut Offsets,
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
            if new_lines.is_empty() {
                continue;
            }

            let senders = state
                .sse_senders
                .blocking_read();
            let Some(sender) = senders.get(&channel_key) else {
                continue;
            };

            for raw_line in new_lines.lines() {
                if raw_line.is_empty() {
                    continue;
                }
                if let Some(parsed) = parse_line(raw_line, format) {
                    let html = render_line(&parsed).into_string();
                    let _ = sender.send(html);
                }
            }
        }
    }
}

/// How far into each log file the tail has read.
struct Offsets(HashMap<PathBuf, u64>);

impl Offsets {
    /// Every existing `.log` at its current end: what is already on disk
    /// reaches viewers with the page, not through the stream.
    fn at_current_ends(channels: &ChannelNode) -> Self {
        let mut offsets = HashMap::new();
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
                let entries = match std::fs::read_dir(&dir.path) {
                    Ok(entries) => entries,
                    Err(e) => {
                        eprintln!(
                            "tail: cannot list {}: {e}",
                            dir.path
                                .display()
                        );
                        continue;
                    }
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension() != Some("log".as_ref()) {
                        continue;
                    }
                    if let Ok(meta) = entry.metadata() {
                        offsets.insert(path, meta.len());
                    }
                }
            }
        }
        Self(offsets)
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
}
