//! Builders for an `AppState` over a hand-made channel tree.

use std::collections::HashMap;

use tokio::sync::RwLock;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;

use axum::Router;
use axum::extract::connect_info::MockConnectInfo;

use crate::limits::Limits;
use crate::parser::LogFormat;
use crate::{AppState, Channel, ChannelDir, ChannelNode, parse_config};

pub fn add_channel(root: &mut ChannelNode, segments: &[&str], dirs: Vec<ChannelDir>) {
    let mut node = root;
    for seg in segments {
        node = node
            .children
            .entry(seg.to_string())
            .or_default();
    }
    node.channel = Some(Channel {
        name: segments
            .last()
            .expect("a channel has at least one segment")
            .to_string(),
        path_segments: segments
            .iter()
            .map(|s| s.to_string())
            .collect(),
        dirs,
    });
}

/// State over `channels` with the config parsed from `yaml`.
pub fn app_state(channels: ChannelNode, yaml: &str) -> AppState {
    let config = parse_config(yaml).unwrap();
    AppState {
        limits: Limits::new(&config),
        api_url: crate::ai::ANTHROPIC_MESSAGES_URL.to_owned(),
        config,
        logs_dirs: Vec::new(),
        channels,
        sse_senders: RwLock::new(HashMap::new()),
        ai_semaphore: None,
        reqwest_client: None,
        presence: None,
        znc: None,
    }
}

/// The router as `peer` would reach it.
pub fn app(state: Arc<AppState>, peer: &str) -> Router {
    crate::server::router()
        .with_state(state)
        .layer(MockConnectInfo(
            peer.parse::<SocketAddr>()
                .unwrap(),
        ))
}

/// A temporary directory under `target/`, removed on drop.
pub fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("test-")
        .tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target"))
        .unwrap()
}

/// A `#c` channel whose only log, 2026-01-01, is a FIFO: reading it blocks
/// until the returned sender fires or a 5 s watchdog runs out, and then gets
/// one line. The watchdog keeps a test that fails mid-stall from hanging on
/// the blocked reader; the handle reports whether it fired.
pub fn stalled_channel() -> (tempfile::TempDir, ChannelNode, Sender<()>, JoinHandle<bool>) {
    let root = tempdir();
    let fifo = root
        .path()
        .join("2026-01-01.log");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo {}", fifo.display());
    let mut channels = ChannelNode::default();
    let dirs = vec![ChannelDir {
        path: root
            .path()
            .to_owned(),
        format: LogFormat::Iso8601,
    }];
    add_channel(&mut channels, &["#c"], dirs);
    let (release, released) = std::sync::mpsc::channel::<()>();
    let writer = std::thread::spawn(move || {
        let timed_out = released
            .recv_timeout(Duration::from_secs(5))
            .is_err();
        std::fs::write(&fifo, "2026-01-01T00:00:00Z <a> x\n").unwrap();
        timed_out
    });
    (root, channels, release, writer)
}
