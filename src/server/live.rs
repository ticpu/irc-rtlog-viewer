//! The /latest SSE stream of a channel, and the log line recording each stream.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use std::net::IpAddr;

use axum::http::HeaderMap;
use axum::http::header;
use axum::response::sse::{Event, KeepAlive, Sse};
use jiff::Timestamp;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::AppState;

/// Prefix of every stream id: an epoch second and random bits drawn by this
/// run, so ids stay unique across restarts. The ujail makes every pid 1.
static SSE_RUN: LazyLock<String> = LazyLock::new(|| {
    let epoch = Timestamp::now().as_second();
    match getrandom::u32() {
        Ok(tag) => format!("{epoch}-{tag:08x}"),
        Err(e) => {
            eprintln!("sse: cannot draw a run tag, stream ids carry the epoch only: {e}");
            epoch.to_string()
        }
    }
});
static SSE_NEXT: AtomicU64 = AtomicU64::new(1);

/// One open /latest stream: logs `sse: open` when made and `sse: close` when
/// the response stream is dropped, which is when the proxy closes the connection.
/// Keys keep a fixed order with the unquoted ones first, so a reader can split
/// on spaces; channel and ua are Rust-debug-quoted, which also escapes newlines.
struct SseStreamLog {
    id: String,
    client: IpAddr,
    channel: String,
    started: std::time::Instant,
    msgs: u64,
}

impl SseStreamLog {
    fn open(client: IpAddr, channel: String, headers: &HeaderMap) -> Self {
        let log = Self {
            id: format!("{}-{}", *SSE_RUN, SSE_NEXT.fetch_add(1, Ordering::Relaxed)),
            client,
            channel,
            started: std::time::Instant::now(),
            msgs: 0,
        };
        sse_log(log.open_line(headers));
        log
    }

    fn open_line(&self, headers: &HeaderMap) -> String {
        let ua = headers
            .get(header::USER_AGENT)
            .map(|v| String::from_utf8_lossy(v.as_bytes()))
            .unwrap_or_default();
        format!(
            "sse: open id={} client={} channel={:?} ua={ua:?}",
            self.id, self.client, self.channel
        )
    }

    fn close_line(&self) -> String {
        format!(
            "sse: close id={} client={} secs={} msgs={} channel={:?}",
            self.id,
            self.client,
            self.started
                .elapsed()
                .as_secs(),
            self.msgs,
            self.channel
        )
    }
}

impl Drop for SseStreamLog {
    fn drop(&mut self) {
        sse_log(self.close_line());
    }
}

#[cfg(not(test))]
fn sse_log(line: String) {
    eprintln!("{line}");
}

#[cfg(test)]
thread_local! {
    static SSE_LOG: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn sse_log(line: String) {
    SSE_LOG.with_borrow_mut(|lines| lines.push(line));
}

pub(super) async fn serve_sse(
    state: Arc<AppState>,
    channel: &crate::Channel,
    client: IpAddr,
    headers: &HeaderMap,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let key = channel
        .path_segments
        .join("/");
    let rx = {
        let mut senders = state
            .sse_senders
            .write()
            .await;
        let sender = senders
            .entry(key.clone())
            .or_insert_with(|| broadcast::channel(256).0);
        sender.subscribe()
    };

    // Owned by the stream so its Drop marks the end of the connection. The
    // whole-variable borrow stops the closure capturing only `log.msgs`.
    let mut log = SseStreamLog::open(client, key, headers);
    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        let log = &mut log;
        let line = result.ok()?;
        match serde_json::to_string(&line) {
            Ok(json) => {
                log.msgs += 1;
                Some(Ok::<_, std::convert::Infallible>(
                    Event::default().data(json),
                ))
            }
            Err(e) => {
                eprintln!("sse: cannot encode a line of {}: {e}", line.date);
                None
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::ChannelNode;
    use crate::test_support::{add_channel, app, app_state};

    #[tokio::test]
    async fn latest_logs_its_client_on_open_and_its_messages_on_close() {
        let mut channels = ChannelNode::default();
        add_channel(&mut channels, &["#c"], Vec::new());
        let state = Arc::new(app_state(channels, "logs_dirs: []"));
        let resp = app(Arc::clone(&state), "[::1]:5000")
            .oneshot(
                Request::get("/%23c/latest")
                    .header("x-real-ip", "2001:db8::7")
                    .header(header::USER_AGENT, r#"UA "quoted""#)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        state
            .sse_senders
            .read()
            .await["#c"]
            .send(crate::tail::LiveLine {
                date: "2026-01-01".into(),
                html: "x".into(),
            })
            .unwrap();
        let mut body = resp
            .into_body()
            .into_data_stream();
        body.next()
            .await
            .unwrap()
            .unwrap();
        drop(body);

        let lines = SSE_LOG.take();
        let [open, close] = lines.as_slice() else {
            panic!("{lines:?}");
        };
        let id = open
            .split(' ')
            .nth(2)
            .unwrap();
        assert_eq!(
            *open,
            format!(r##"sse: open {id} client=2001:db8::7 channel="#c" ua="UA \"quoted\"""##)
        );
        assert_eq!(
            *close,
            format!(r##"sse: close {id} client=2001:db8::7 secs=0 msgs=1 channel="#c""##)
        );
    }
}
