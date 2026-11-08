//! Builders for an `AppState` over a hand-made channel tree.

use std::collections::HashMap;

use tokio::sync::RwLock;

use crate::{AppState, Channel, ChannelDir, ChannelNode};

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

pub fn app_state(channels: ChannelNode) -> AppState {
    AppState {
        config: serde_yaml::from_str("logs_dirs: []").unwrap(),
        logs_dirs: Vec::new(),
        channels,
        sse_senders: RwLock::new(HashMap::new()),
        ai_semaphore: None,
        reqwest_client: None,
    }
}

/// A temporary directory under `target/`, removed on drop.
pub fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("test-")
        .tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/target"))
        .unwrap()
}
