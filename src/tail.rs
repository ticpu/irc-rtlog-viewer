use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use crate::AppState;
use crate::parser::{LogFormat, parse_line};
use crate::templates::render_line;

pub fn start_watcher(state: Arc<AppState>) {
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<Event>>();

    let mut watcher = notify::recommended_watcher(tx).expect("failed to create file watcher");
    for dir in &state.logs_dirs {
        watcher
            .watch(dir, RecursiveMode::Recursive)
            .expect("failed to watch logs directory");
    }

    tokio::task::spawn_blocking(move || {
        let _watcher = watcher;
        let mut positions: HashMap<PathBuf, u64> = HashMap::new();
        tail_loop(rx, &mut positions, &state);
    });
}

fn tail_loop(
    rx: std::sync::mpsc::Receiver<notify::Result<Event>>,
    positions: &mut HashMap<PathBuf, u64>,
    state: &AppState,
) {
    for event in rx {
        let Ok(event) = event else { continue };
        if !matches!(event.kind, EventKind::Modify(_)) {
            continue;
        }

        for path in &event.paths {
            let Some(ext) = path.extension() else { continue };
            if ext != "log" {
                continue;
            }

            let Some((channel_key, format)) = resolve_channel(path, state) else {
                continue;
            };

            let new_lines = read_new_bytes(path, positions);
            if new_lines.is_empty() {
                continue;
            }

            let senders = state.sse_senders.blocking_read();
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

fn read_new_bytes(path: &PathBuf, positions: &mut HashMap<PathBuf, u64>) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(meta) = file.metadata() else {
        return String::new();
    };
    let size = meta.len();
    let pos = positions.get(path).copied().unwrap_or(0);

    if size <= pos {
        positions.insert(path.clone(), size);
        return String::new();
    }

    if file.seek(SeekFrom::Start(pos)).is_err() {
        return String::new();
    }

    let mut buf = String::new();
    let _ = file.read_to_string(&mut buf);
    positions.insert(path.clone(), size);
    buf
}

fn resolve_channel(path: &PathBuf, state: &AppState) -> Option<(String, LogFormat)> {
    let abs = std::fs::canonicalize(path).ok()?;
    let parent = abs.parent()?;
    for logs_dir in &state.logs_dirs {
        if let Ok(rel) = parent.strip_prefix(logs_dir) {
            let segments: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
            if let Some(result) = find_channel_in_tree(&state.channels, &segments, 0) {
                return Some(result);
            }
        }
    }
    None
}

fn find_channel_in_tree(
    node: &crate::ChannelNode,
    segments: &[String],
    depth: usize,
) -> Option<(String, LogFormat)> {
    if depth >= segments.len() {
        let ch = node.channel.as_ref()?;
        let key = ch.path_segments.join("/");
        return Some((key, ch.format));
    }

    let child = node.children.get(&segments[depth])?;
    find_channel_in_tree(child, segments, depth + 1)
}
