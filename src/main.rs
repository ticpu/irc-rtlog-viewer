use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use tokio::sync::RwLock;
use tokio::sync::broadcast;

mod parser;
mod search;
mod server;
mod tail;
mod templates;

use parser::LogFormat;

#[derive(Parser)]
#[command(version, about = "IRC log viewer")]
pub struct Args {
    #[arg(short = 'l', long, default_value = "./logs")]
    logs_dir: PathBuf,
    #[arg(short, long, default_value = "0.0.0.0:8080")]
    bind: String,
    #[arg(short, long, default_value = "IRC Logs")]
    title: String,
}

#[derive(Debug, Clone)]
pub struct Channel {
    pub name: String,
    pub path_segments: Vec<String>,
    pub fs_path: PathBuf,
    pub format: LogFormat,
}

#[derive(Debug, Default)]
pub struct ChannelNode {
    pub channel: Option<Channel>,
    pub children: BTreeMap<String, ChannelNode>,
}

pub struct AppState {
    pub args: Args,
    pub channels: ChannelNode,
    pub sse_senders: RwLock<HashMap<String, broadcast::Sender<String>>>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let logs_dir = std::fs::canonicalize(&args.logs_dir).unwrap_or_else(|e| {
        eprintln!("cannot access logs dir {:?}: {e}", args.logs_dir);
        std::process::exit(1);
    });

    let mut root = ChannelNode::default();
    discover_channels(&logs_dir, &[], &mut root);

    let bind = args.bind.clone();
    let state = Arc::new(AppState {
        args: Args { logs_dir, ..args },
        channels: root,
        sse_senders: RwLock::new(HashMap::new()),
    });

    tail::start_watcher(Arc::clone(&state));

    let app = server::router().with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind(&bind).await.unwrap_or_else(|e| {
        eprintln!("cannot bind {bind}: {e}");
        std::process::exit(1);
    });
    eprintln!("listening on {bind}");
    axum::serve(listener, app).await.unwrap();
}

fn discover_channels(
    dir: &Path,
    segments: &[String],
    root: &mut ChannelNode,
) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };

    let mut subdirs = Vec::new();
    let mut has_logs = false;

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            subdirs.push((name, entry.path()));
        } else if name.ends_with(".log") || name.ends_with(".log.zst") {
            let date_part = name.strip_suffix(".log")
                .or_else(|| name.strip_suffix(".log.zst"))
                .unwrap_or("");
            if date_part.len() == 10 {
                has_logs = true;
            }
        }
    }

    if has_logs && !segments.is_empty() {
        let leaf_name = segments.last().unwrap();
        let format = detect_channel_format(dir);
        let channel = Channel {
            name: leaf_name.clone(),
            path_segments: segments.to_vec(),
            fs_path: dir.to_path_buf(),
            format,
        };
        insert_channel(root, segments, channel);
    }

    // If any sibling subdir starts with #, only recurse into #-prefixed dirs
    // (filters out ZNC private query logs like "qwebirc56163")
    let has_hash_sibling = subdirs.iter().any(|(name, _)| name.starts_with('#'));
    for (name, path) in subdirs {
        if has_hash_sibling && !name.starts_with('#') {
            continue;
        }
        let mut child_segments = segments.to_vec();
        child_segments.push(name);
        discover_channels(&path, &child_segments, root);
    }
}

fn detect_channel_format(dir: &Path) -> LogFormat {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return LogFormat::Iso8601;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".log") || name.ends_with(".log.zst") {
            if let Ok(content) = server::read_log_file(&entry.path()) {
                if let Some(first_line) = content.lines().next() {
                    return parser::detect_format(first_line);
                }
            }
        }
    }

    LogFormat::Iso8601
}

fn insert_channel(root: &mut ChannelNode, segments: &[String], channel: Channel) {
    let mut node = root;
    for seg in segments {
        node = node.children.entry(seg.clone()).or_default();
    }
    node.channel = Some(channel);
}
