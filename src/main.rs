use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, Semaphore};
use tokio::sync::broadcast;

mod ai;
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
    #[serde(default)]
    pub base_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai: Option<AiConfig>,
}

fn default_bind() -> String { "0.0.0.0:8080".into() }
fn default_title() -> String { "IRC Logs".into() }
fn default_search_limit() -> usize { 10000 }
fn default_ai_model() -> String { "claude-haiku-4-5-20251001".into() }
fn default_ai_max_concurrent() -> usize { 1 }

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            title: default_title(),
            search_limit: default_search_limit(),
            logs_dirs: vec![PathBuf::from("./logs")],
            base_path: String::new(),
            ai: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AiConfig {
    pub api_key: String,
    #[serde(default = "default_ai_model")]
    pub model: String,
    pub output_dir: PathBuf,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default = "default_ai_max_concurrent")]
    pub max_concurrent: usize,
}

#[derive(Debug, Clone)]
pub struct ChannelDir {
    pub path: PathBuf,
    pub format: LogFormat,
}

#[derive(Debug, Clone)]
pub struct Channel {
    pub name: String,
    pub path_segments: Vec<String>,
    pub dirs: Vec<ChannelDir>,
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
    pub ai_semaphore: Option<Arc<Semaphore>>,
    pub reqwest_client: Option<reqwest::Client>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();

    let mut config: Config = if cli.config.exists() {
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
        let mut yaml = serde_yaml::to_string(&config).unwrap();
        yaml.push_str(concat!(
            "#base_path: /irc\n",
            "#ai:\n",
            "#  api_key: sk-ant-api03-...\n",
            "#  model: claude-haiku-4-5-20251001\n",
            "#  output_dir: /var/lib/irc-logs/ask\n",
            "#  base_url: https://example.com/ask\n",
            "#  max_concurrent: 1\n",
        ));
        std::fs::write(&cli.config, &yaml).unwrap_or_else(|e| {
            eprintln!("cannot write default config {:?}: {e}", cli.config);
            std::process::exit(1);
        });
        eprintln!("created default config {:?}, edit and restart", cli.config);
        std::process::exit(0);
    };

    // Normalize base_path: strip trailing /, ensure leading / if non-empty
    let bp = config.base_path.trim_matches('/');
    config.base_path = if bp.is_empty() {
        String::new()
    } else {
        format!("/{bp}")
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
    let (ai_semaphore, reqwest_client) = match &config.ai {
        Some(ai) => {
            eprintln!("ai: enabled, model={}, max_concurrent={}", ai.model, ai.max_concurrent);
            (
                Some(Arc::new(Semaphore::new(ai.max_concurrent))),
                Some(reqwest::Client::new()),
            )
        }
        None => (None, None),
    };
    let state = Arc::new(AppState {
        config,
        logs_dirs,
        channels: root,
        sse_senders: RwLock::new(HashMap::new()),
        ai_semaphore,
        reqwest_client,
    });

    tail::start_watcher(Arc::clone(&state));

    let app = if state.config.base_path.is_empty() {
        server::router().with_state(Arc::clone(&state))
    } else {
        Router::new()
            .nest(&state.config.base_path, server::router())
            .with_state(Arc::clone(&state))
    };
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
        let format = detect_channel_format(dir);
        let channel_dir = ChannelDir {
            path: dir.to_path_buf(),
            format,
        };
        insert_channel(root, segments, channel_dir);
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

fn insert_channel(root: &mut ChannelNode, segments: &[String], dir: ChannelDir) {
    let mut node = root;
    for seg in segments {
        node = node.children.entry(seg.clone()).or_default();
    }
    if let Some(channel) = &mut node.channel {
        channel.dirs.push(dir);
    } else {
        node.channel = Some(Channel {
            name: segments.last().unwrap().clone(),
            path_segments: segments.to_vec(),
            dirs: vec![dir],
        });
    }
}
