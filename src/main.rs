use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use serde::{Deserialize, Serialize};
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
struct Cli {
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_title")]
    pub title: String,
    #[serde(default = "default_search_limit")]
    pub search_limit: usize,
    pub logs_dirs: Vec<PathBuf>,
}

fn default_bind() -> String { "0.0.0.0:8080".into() }
fn default_title() -> String { "IRC Logs".into() }
fn default_search_limit() -> usize { 10000 }

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            title: default_title(),
            search_limit: default_search_limit(),
            logs_dirs: vec![PathBuf::from("./logs")],
        }
    }
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
    pub config: Config,
    pub logs_dirs: Vec<PathBuf>,
    pub channels: ChannelNode,
    pub sse_senders: RwLock<HashMap<String, broadcast::Sender<String>>>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let config: Config = if cli.config.exists() {
        let content = std::fs::read_to_string(&cli.config).unwrap_or_else(|e| {
            eprintln!("cannot read config {:?}: {e}", cli.config);
            std::process::exit(1);
        });
        serde_yaml::from_str(&content).unwrap_or_else(|e| {
            eprintln!("invalid config {:?}: {e}", cli.config);
            std::process::exit(1);
        })
    } else {
        let config = Config::default();
        let yaml = serde_yaml::to_string(&config).unwrap();
        std::fs::write(&cli.config, &yaml).unwrap_or_else(|e| {
            eprintln!("cannot write default config {:?}: {e}", cli.config);
            std::process::exit(1);
        });
        eprintln!("created default config {:?}, edit and restart", cli.config);
        std::process::exit(0);
    };

    let logs_dirs: Vec<PathBuf> = config.logs_dirs.iter().map(|d| {
        std::fs::canonicalize(d).unwrap_or_else(|e| {
            eprintln!("cannot access logs dir {d:?}: {e}");
            std::process::exit(1);
        })
    }).collect();

    let mut root = ChannelNode::default();
    for dir in &logs_dirs {
        discover_channels(dir, &[], &mut root);
    }

    let bind = config.bind.clone();
    let state = Arc::new(AppState {
        config,
        logs_dirs,
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
