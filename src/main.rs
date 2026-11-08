//! Web viewer for ZNC and ISO8601 IRC logs, with live tailing and optional
//! AI-assisted search.

use std::collections::{BTreeMap, HashMap};
use std::future::IntoFuture;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use clap::Parser;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;
use tokio::sync::{RwLock, Semaphore};

mod ai;
mod limits;
mod parser;
mod search;
mod server;
mod tail;
mod templates;
#[cfg(test)]
mod test_support;

use parser::LogFormat;

#[derive(Parser)]
#[command(version, about = "IRC log viewer")]
struct Cli {
    #[arg(short, long, default_value = "config.yaml")]
    config: PathBuf,
}

/// Deserialized `config.yaml`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    /// Listen address, `host:port`.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Site title, shown in the sidebar and `<title>`.
    #[serde(default = "default_title")]
    pub title: String,
    /// Maximum number of lines a plain-text search will return.
    #[serde(default = "default_search_limit")]
    pub search_limit: usize,
    /// Roots scanned for channel directories; canonicalized at startup.
    pub logs_dirs: Vec<PathBuf>,
    /// URL prefix when served behind a reverse proxy, normalized to `/foo` or empty.
    #[serde(default)]
    pub base_path: String,
    /// Peers whose X-Real-IP header names the client, for per-address limits.
    #[serde(default = "default_trusted_proxies")]
    pub trusted_proxies: Vec<IpAddr>,
    /// Limits on the plain-text search page.
    #[serde(default)]
    pub search: SearchConfig,
    /// AI search settings; the ask endpoints 404 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai: Option<AiConfig>,
}

fn default_trusted_proxies() -> Vec<IpAddr> {
    vec![Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()]
}
fn default_bind() -> String {
    "[::]:8080".into()
}
fn default_title() -> String {
    "IRC Logs".into()
}
fn default_search_limit() -> usize {
    10000
}
fn default_ai_model() -> String {
    "claude-haiku-4-5-20251001".into()
}
fn default_ai_max_concurrent() -> usize {
    1
}
fn default_ai_max_tool_calls() -> usize {
    100
}
fn default_ai_max_query_len() -> usize {
    500
}
fn default_ai_requests_per_day_per_client() -> NonZeroU32 {
    NonZeroU32::new(100).unwrap()
}
fn default_ai_requests_per_day_per_ip() -> NonZeroU32 {
    NonZeroU32::new(250).unwrap()
}
fn default_search_max_concurrent() -> NonZeroUsize {
    NonZeroUsize::new(2).unwrap()
}
fn default_search_per_ip_per_minute() -> NonZeroU32 {
    NonZeroU32::new(30).unwrap()
}
fn default_search_max_query_len() -> usize {
    200
}
fn default_search_max_days() -> usize {
    365
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            title: default_title(),
            search_limit: default_search_limit(),
            logs_dirs: vec![PathBuf::from("./logs")],
            base_path: String::new(),
            trusted_proxies: default_trusted_proxies(),
            search: SearchConfig::default(),
            ai: None,
        }
    }
}

/// Settings for the AI-backed ask feature.
#[derive(Debug, Serialize, Deserialize)]
pub struct AiConfig {
    /// Anthropic API key.
    pub api_key: String,
    /// Model id used for ask sessions.
    #[serde(default = "default_ai_model")]
    pub model: String,
    /// Directory the generated answer documents are written to and served from.
    pub output_dir: PathBuf,
    /// Ask sessions allowed to run at once; further requests get 503.
    #[serde(default = "default_ai_max_concurrent")]
    pub max_concurrent: usize,
    /// Upper bound on tool-call iterations in one session.
    #[serde(default = "default_ai_max_tool_calls")]
    pub max_tool_calls: usize,
    /// Longest accepted question, in characters.
    #[serde(default = "default_ai_max_query_len")]
    pub max_query_len: usize,
    /// Anthropic API requests one browser (ask_id cookie) may make per day.
    #[serde(default = "default_ai_requests_per_day_per_client")]
    pub requests_per_day_per_client: NonZeroU32,
    /// Anthropic API requests one client address may make per day.
    #[serde(default = "default_ai_requests_per_day_per_ip")]
    pub requests_per_day_per_ip: NonZeroU32,
    /// Replaces the built-in system prompt; the channel list is appended either way.
    #[serde(default)]
    pub system_prompt: Option<String>,
}

/// Limits on the plain-text search page.
#[derive(Debug, Serialize, Deserialize)]
pub struct SearchConfig {
    /// Searches allowed to run at once; further requests get 503.
    #[serde(default = "default_search_max_concurrent")]
    pub max_concurrent: NonZeroUsize,
    /// Searches one client address may start per minute; further requests get 429.
    #[serde(default = "default_search_per_ip_per_minute")]
    pub per_ip_per_minute: NonZeroU32,
    /// Longest accepted query, in characters.
    #[serde(default = "default_search_max_query_len")]
    pub max_query_len: usize,
    /// Days scanned, newest first, before a search stops.
    #[serde(default = "default_search_max_days")]
    pub max_days: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            max_concurrent: default_search_max_concurrent(),
            per_ip_per_minute: default_search_per_ip_per_minute(),
            max_query_len: default_search_max_query_len(),
            max_days: default_search_max_days(),
        }
    }
}

/// One directory holding a channel's logs, with the format its files use.
#[derive(Debug, Clone)]
pub struct ChannelDir {
    /// Absolute path to the directory.
    pub path: PathBuf,
    /// Line format detected from the first log file found.
    pub format: LogFormat,
}

/// A channel, possibly spanning several log directories.
#[derive(Debug, Clone)]
pub struct Channel {
    /// Display name, e.g. `#chan`.
    pub name: String,
    /// URL path components, e.g. `["testnet", "#chan"]`.
    pub path_segments: Vec<String>,
    /// Source directories, searched in order.
    pub dirs: Vec<ChannelDir>,
}

impl Channel {
    /// URL path of the channel, without base path or leading `/`.
    pub fn url_path(&self) -> String {
        self.path_segments
            .iter()
            .map(|s| utf8_percent_encode(s, PATH_SEGMENT).to_string())
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// Everything but RFC 3986 unreserved characters, so an encoded segment is
/// also inert inside an HTML attribute or a JS string.
const PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Node in the channel tree; interior nodes are networks or groupings.
#[derive(Debug, Default)]
pub struct ChannelNode {
    /// Set when this node is itself a channel.
    pub channel: Option<Channel>,
    /// Child nodes keyed by path segment.
    pub children: BTreeMap<String, ChannelNode>,
}

/// Shared state handed to every request handler.
pub struct AppState {
    /// Parsed configuration.
    pub config: Config,
    /// Canonicalized log roots.
    pub logs_dirs: Vec<PathBuf>,
    /// Channel tree discovered at startup.
    pub channels: ChannelNode,
    /// Live-tail broadcasters, keyed by channel path.
    pub sse_senders: RwLock<HashMap<String, broadcast::Sender<String>>>,
    /// Bounds concurrent ask sessions; `None` when AI is disabled.
    pub ai_semaphore: Option<Arc<Semaphore>>,
    /// HTTP client for the Anthropic API; `None` when AI is disabled.
    pub reqwest_client: Option<reqwest::Client>,
    /// Per-client budgets for ask and search.
    pub limits: limits::Limits,
    /// Messages endpoint, pointed at a local stand-in by tests.
    #[cfg(test)]
    pub api_url: String,
}

fn main() {
    // The OpenWrt init caps address space at 250 MB; tokio's default of 512
    // blocking threads with 2 MB stacks could reserve four times that.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(16)
        .build()
        .unwrap_or_else(|e| {
            eprintln!("cannot start the async runtime: {e}");
            std::process::exit(1);
        });
    runtime.block_on(run());
}

async fn run() {
    let cli = Cli::parse();

    let mut config: Config = if cli
        .config
        .exists()
    {
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
            "#  max_concurrent: 1\n",
            "#  max_tool_calls: 100\n",
            "#  max_query_len: 500\n",
            "#  requests_per_day_per_client: 100\n",
            "#  requests_per_day_per_ip: 250\n",
            "#  system_prompt: |\n",
            "#    Custom system prompt text here.\n",
            "#    The channel list is always appended automatically.\n",
        ));
        std::fs::write(&cli.config, &yaml).unwrap_or_else(|e| {
            eprintln!("cannot write default config {:?}: {e}", cli.config);
            std::process::exit(1);
        });
        eprintln!("created default config {:?}, edit and restart", cli.config);
        std::process::exit(0);
    };

    // Normalize base_path: strip trailing /, ensure leading / if non-empty
    let bp = config
        .base_path
        .trim_matches('/');
    config.base_path = if bp.is_empty() {
        String::new()
    } else {
        format!("/{bp}")
    };

    let logs_dirs: Vec<PathBuf> = config
        .logs_dirs
        .iter()
        .map(|d| {
            std::fs::canonicalize(d).unwrap_or_else(|e| {
                eprintln!("cannot access logs dir {d:?}: {e}");
                std::process::exit(1);
            })
        })
        .collect();

    let mut root = ChannelNode::default();
    for dir in &logs_dirs {
        discover_channels(dir, &[], &mut root);
    }

    let bind = config
        .bind
        .clone();
    let (ai_semaphore, reqwest_client) = match &config.ai {
        Some(ai) => {
            eprintln!(
                "ai: enabled, model={}, max_concurrent={}",
                ai.model, ai.max_concurrent
            );
            (
                Some(Arc::new(Semaphore::new(ai.max_concurrent))),
                Some(reqwest::Client::new()),
            )
        }
        None => (None, None),
    };
    let limits = limits::Limits::new(&config);
    let state = Arc::new(AppState {
        config,
        logs_dirs,
        channels: root,
        sse_senders: RwLock::new(HashMap::new()),
        ai_semaphore,
        reqwest_client,
        limits,
        #[cfg(test)]
        api_url: crate::ai::ANTHROPIC_MESSAGES_URL.to_owned(),
    });

    let tail = tail::start_watcher(Arc::clone(&state));
    tokio::spawn(prune_limits(Arc::clone(&state)));

    let app = if state
        .config
        .base_path
        .is_empty()
    {
        server::router().with_state(Arc::clone(&state))
    } else {
        Router::new()
            .nest(
                &state
                    .config
                    .base_path,
                server::router(),
            )
            .with_state(Arc::clone(&state))
    };
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|e| {
            eprintln!("cannot bind {bind}: {e}");
            std::process::exit(1);
        });
    eprintln!("listening on {bind}");
    let (stop, stopping) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = stop.send(true);
    });
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(signalled(stopping.clone()));
    // Graceful shutdown waits for every connection, and live tails never end on
    // their own; procd SIGKILLs after 5 s, so give up on them before that.
    let grace = async {
        signalled(stopping).await;
        tokio::time::sleep(SHUTDOWN_GRACE).await;
    };
    tokio::select! {
        result = server.into_future() => if let Err(e) = result {
            eprintln!("server failed: {e}");
            std::process::exit(1);
        },
        () = grace => eprintln!(
            "connections still open {} s after the signal, closing them",
            SHUTDOWN_GRACE.as_secs()
        ),
    }
    // The tail thread holds no state; stopping it lets the runtime shut down.
    drop(tail);
}

const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Resolves once shutdown has been signalled, or its sender is gone.
async fn signalled(mut stopping: tokio::sync::watch::Receiver<bool>) {
    let _ = stopping
        .wait_for(|s| *s)
        .await;
}

async fn prune_limits(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
    loop {
        tick.tick()
            .await;
        state
            .limits
            .prune();
    }
}

// Under ujail the process is PID 1 in its own namespace, where the kernel drops
// signals from outside unless a handler is installed. Without this, SIGTERM is
// discarded and procd can only stop us by waiting out ujail's SIGKILL timer.
async fn shutdown_signal() {
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => eprintln!("SIGTERM, shutting down"),
        _ = int.recv() => eprintln!("SIGINT, shutting down"),
    }
}

fn discover_channels(dir: &Path, segments: &[String], root: &mut ChannelNode) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    let mut subdirs = Vec::new();
    let mut has_logs = false;

    for entry in entries.flatten() {
        let name = entry
            .file_name()
            .to_string_lossy()
            .to_string();
        if entry
            .file_type()
            .map(|ft| ft.is_dir())
            .unwrap_or(false)
        {
            subdirs.push((name, entry.path()));
        } else if name.ends_with(".log") || name.ends_with(".log.zst") {
            let date_part = name
                .strip_suffix(".log")
                .or_else(|| name.strip_suffix(".log.zst"))
                .unwrap_or("");
            if date_part.len() == 10 {
                has_logs = true;
            }
        }
    }

    if has_logs && is_channel_dir(segments) {
        let format = detect_channel_format(dir);
        let channel_dir = ChannelDir {
            path: dir.to_path_buf(),
            format,
        };
        insert_channel(root, segments, channel_dir);
    }

    for (name, path) in subdirs {
        let mut child_segments = segments.to_vec();
        child_segments.push(name);
        discover_channels(&path, &child_segments, root);
    }
}

/// Only `#` directories are published: anything else, such as a ZNC private
/// query log, stays off the web and out of the AI tools.
fn is_channel_dir(segments: &[String]) -> bool {
    segments
        .last()
        .is_some_and(|name| name.starts_with('#'))
}

fn detect_channel_format(dir: &Path) -> LogFormat {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return LogFormat::Iso8601;
    };

    for entry in entries.flatten() {
        let name = entry
            .file_name()
            .to_string_lossy()
            .to_string();
        if name.ends_with(".log") || name.ends_with(".log.zst") {
            if let Ok(content) = server::read_log_file(&entry.path()) {
                if let Some(first_line) = content
                    .lines()
                    .next()
                {
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
        node = node
            .children
            .entry(seg.clone())
            .or_default();
    }
    if let Some(channel) = &mut node.channel {
        channel
            .dirs
            .push(dir);
    } else {
        node.channel = Some(Channel {
            name: segments
                .last()
                .unwrap()
                .clone(),
            path_segments: segments.to_vec(),
            dirs: vec![dir],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tempdir;

    #[test]
    fn only_hash_directories_become_channels() {
        let root = tempdir();
        for dir in ["OFTC/#chan", "OFTC/somenick", "net2/qwebirc56163", "#top"] {
            let path = root
                .path()
                .join(dir);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("2026-01-01.log"), "").unwrap();
        }
        let mut tree = ChannelNode::default();
        discover_channels(root.path(), &[], &mut tree);

        let mut found = Vec::new();
        let mut stack = vec![&tree];
        while let Some(node) = stack.pop() {
            stack.extend(
                node.children
                    .values(),
            );
            if let Some(channel) = &node.channel {
                found.push(
                    channel
                        .path_segments
                        .join("/"),
                );
            }
        }
        found.sort();
        assert_eq!(found, ["#top", "OFTC/#chan"]);
    }
}
