use std::io::{self, BufReader, Read};
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::AppState;
use crate::parser::parse_line;
use crate::search::search_channel;
use crate::templates;

static CSS: &str = include_str!("../static/style.css");

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(index))
        .route("/static/style.css", get(serve_css))
        .fallback(get(wildcard))
}

async fn index(State(state): State<Arc<AppState>>) -> Response {
    if let Some(first) = first_channel(&state.channels) {
        let encoded = first.path_segments.join("/").replace('#', "%23");
        Redirect::temporary(&format!("/{encoded}/today")).into_response()
    } else {
        (StatusCode::NOT_FOUND, "no channels found").into_response()
    }
}

async fn serve_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css")], CSS)
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

async fn wildcard(
    State(state): State<Arc<AppState>>,
    uri: Uri,
    Query(search): Query<SearchQuery>,
) -> Response {
    let path = percent_decode(uri.path().trim_start_matches('/'));
    let segments: Vec<&str> = path.split('/').collect();

    if segments.is_empty() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let last = *segments.last().unwrap();

    // Try to find channel with all segments vs. all-but-last
    if last == "today" || last == "latest" || last == "search" || looks_like_date(last) || is_date_raw(last, segments.len()) {
        let channel_segments = &segments[..segments.len() - 1];
        // Handle YYYY-MM-DD/raw
        let (action, channel_segments) = if last == "raw" && segments.len() >= 2 {
            let date_seg = segments[segments.len() - 2];
            if looks_like_date(date_seg) {
                ("raw", &segments[..segments.len() - 2])
            } else {
                (last, channel_segments)
            }
        } else {
            (last, channel_segments)
        };

        if let Some(channel) = find_channel(&state.channels, channel_segments).cloned() {
            return match action {
                "today" => {
                    let today = today_date();
                    let encoded = channel.path_segments.join("/").replace('#', "%23");
                    Redirect::temporary(&format!("/{encoded}/{today}")).into_response()
                }
                "latest" => serve_sse(state, &channel).await.into_response(),
                "search" => {
                    let query = search.q.unwrap_or_default();
                    serve_search(&state, &channel, &query).into_response()
                }
                "raw" => {
                    let date = segments[segments.len() - 2];
                    serve_raw(&channel, date).await.into_response()
                }
                date if looks_like_date(date) => {
                    serve_log_page(&state, &channel, date).into_response()
                }
                _ => (StatusCode::NOT_FOUND, "not found").into_response(),
            };
        }
    }

    // Maybe bare channel path → redirect to today
    if let Some(channel) = find_channel(&state.channels, &segments) {
        let today = today_date();
        let encoded = channel.path_segments.join("/").replace('#', "%23");
        return Redirect::temporary(&format!("/{encoded}/{today}")).into_response();
    }

    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn is_date_raw(last: &str, len: usize) -> bool {
    last == "raw" && len >= 2
}

fn looks_like_date(s: &str) -> bool {
    s.len() == 10 && s.as_bytes().get(4) == Some(&b'-') && s.as_bytes().get(7) == Some(&b'-')
}

fn today_date() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days = now / 86400;
    epoch_days_to_date(days)
}

fn epoch_days_to_date(days: u64) -> String {
    // Civil date from days since epoch (Euclidean affine algorithm)
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn percent_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                result.push(byte as char);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

fn find_channel<'a>(
    node: &'a crate::ChannelNode,
    segments: &[&str],
) -> Option<&'a crate::Channel> {
    let mut current = node;
    for seg in segments {
        current = current.children.get(*seg)?;
    }
    current.channel.as_ref()
}

fn first_channel(node: &crate::ChannelNode) -> Option<&crate::Channel> {
    if let Some(ch) = &node.channel {
        return Some(ch);
    }
    for child in node.children.values() {
        if let Some(ch) = first_channel(child) {
            return Some(ch);
        }
    }
    None
}

fn serve_log_page(state: &AppState, channel: &crate::Channel, date: &str) -> Response {
    let path = resolve_log_path(&channel.fs_path, date);
    let content = match path.and_then(|p| read_log_file(&p).ok()) {
        Some(c) => c,
        None => {
            return (StatusCode::NOT_FOUND, format!("no log for {date}")).into_response();
        }
    };

    let lines: Vec<_> = content
        .lines()
        .filter_map(|l| parse_line(l, channel.format))
        .collect();

    let dates = list_dates(&channel.fs_path);
    let idx = dates.iter().position(|d| d == date);
    let prev = idx.and_then(|i| if i > 0 { dates.get(i - 1) } else { None }).map(|s| s.as_str());
    let next = idx.and_then(|i| dates.get(i + 1)).map(|s| s.as_str());
    let is_today = date == today_date();

    templates::log_page(&templates::LogPageContext {
        title: &state.args.title,
        tree: &state.channels,
        channel,
        date,
        lines: &lines,
        prev_date: prev,
        next_date: next,
        is_today,
    }).into_response()
}

fn serve_search(state: &AppState, channel: &crate::Channel, query: &str) -> Response {
    let results = search_channel(&channel.fs_path, channel.format, query, 200);
    templates::search_page(&state.args.title, &state.channels, channel, query, &results)
        .into_response()
}

async fn serve_raw(channel: &crate::Channel, date: &str) -> Response {
    let path = resolve_log_path(&channel.fs_path, date);
    match path.and_then(|p| read_log_file(&p).ok()) {
        Some(content) => {
            ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], content).into_response()
        }
        None => (StatusCode::NOT_FOUND, format!("no log for {date}")).into_response(),
    }
}

async fn serve_sse(
    state: Arc<AppState>,
    channel: &crate::Channel,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let key = channel.path_segments.join("/");
    let rx = {
        let mut senders = state.sse_senders.write().await;
        let sender = senders
            .entry(key)
            .or_insert_with(|| broadcast::channel(256).0);
        sender.subscribe()
    };

    let stream = BroadcastStream::new(rx).filter_map(|result| {
        result
            .ok()
            .map(|html| Ok::<_, std::convert::Infallible>(Event::default().data(html)))
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub fn read_log_file(path: &Path) -> io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut content = String::new();

    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        let mut decoder = zstd::Decoder::new(BufReader::new(file))?;
        decoder.read_to_string(&mut content)?;
    } else {
        let mut reader = BufReader::new(file);
        reader.read_to_string(&mut content)?;
    }

    Ok(content)
}

pub fn resolve_log_path(dir: &Path, date: &str) -> Option<std::path::PathBuf> {
    let plain = dir.join(format!("{date}.log"));
    if plain.exists() {
        return Some(plain);
    }
    let zst = dir.join(format!("{date}.log.zst"));
    if zst.exists() {
        return Some(zst);
    }
    None
}

fn list_dates(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dates: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let date = name.strip_suffix(".log")
                .or_else(|| name.strip_suffix(".log.zst"))?;
            if date.len() == 10 {
                Some(date.to_string())
            } else {
                None
            }
        })
        .collect();
    dates.sort_unstable();
    dates
}
