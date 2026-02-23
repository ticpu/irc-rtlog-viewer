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
use crate::parser::{LogFormat, parse_line};
use crate::search::search_channel;
use crate::templates;

static CSS: &str = include_str!("../static/style.css");

fn cache_control(value: &str) -> (header::HeaderName, &str) {
    (header::CACHE_CONTROL, value)
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(index))
        .route("/static/style.css", get(serve_css))
        .route("/ask/output/{filename}", get(serve_ask_output))
        .fallback(get(wildcard))
}

async fn index(State(state): State<Arc<AppState>>) -> Response {
    let mut resp = templates::page(&state.config.title, &state.channels, &state.config.base_path, maud::html! {
        h1 { (&state.config.title) }
        p { "Select a channel from the sidebar." }
    }).into_response();
    resp.headers_mut().insert(header::CACHE_CONTROL, "private, no-cache".parse().unwrap());
    resp
}

async fn serve_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css"),
            cache_control("public, max-age=86400, s-maxage=604800"),
        ],
        CSS,
    )
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

    // Check for "ask/stream" (two trailing segments)
    if segments.len() >= 3
        && segments[segments.len() - 2] == "ask"
        && segments[segments.len() - 1] == "stream"
    {
        let channel_segments = &segments[..segments.len() - 2];
        if let Some(channel) = find_channel(&state.channels, channel_segments).cloned() {
            let query = search.q.unwrap_or_default();
            return serve_ask_stream(state, channel, &query).await.into_response();
        }
    }

    // Try to find channel with all segments vs. all-but-last
    if last == "today" || last == "latest" || last == "search" || last == "ask" || looks_like_date(last) || is_date_raw(last, segments.len()) {
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
                    let bp = &state.config.base_path;
                    let encoded = channel.path_segments.join("/").replace('#', "%23");
                    let date = latest_date(&channel);
                    Redirect::temporary(&format!("{bp}/{encoded}/{date}")).into_response()
                }
                "latest" => serve_sse(state, &channel).await.into_response(),
                "search" => {
                    let query = search.q.unwrap_or_default();
                    serve_search(&state, &channel, &query).into_response()
                }
                "ask" => serve_ask_page(&state, &channel).into_response(),
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

    // Maybe bare channel path → redirect to latest date
    if let Some(channel) = find_channel(&state.channels, &segments) {
        let bp = &state.config.base_path;
        let encoded = channel.path_segments.join("/").replace('#', "%23");
        let date = latest_date(channel);
        return Redirect::temporary(&format!("{bp}/{encoded}/{date}")).into_response();
    }

    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn is_date_raw(last: &str, len: usize) -> bool {
    last == "raw" && len >= 2
}

fn looks_like_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
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

fn serve_log_page(state: &AppState, channel: &crate::Channel, date: &str) -> Response {
    let (path, format) = match resolve_log_path(channel, date) {
        Some(r) => r,
        None => {
            return (StatusCode::NOT_FOUND, format!("no log for {date}")).into_response();
        }
    };
    let content = match read_log_file(&path) {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("read error: {e}")).into_response();
        }
    };

    let lines: Vec<_> = content
        .lines()
        .filter_map(|l| parse_line(l, format))
        .collect();

    let dates = channel_dates(channel);
    let idx = dates.iter().position(|d| d == date);
    let prev = idx.and_then(|i| if i > 0 { dates.get(i - 1) } else { None }).map(|s| s.as_str());
    let next = idx.and_then(|i| dates.get(i + 1)).map(|s| s.as_str());
    let is_today = date == today_date();

    let mut resp = templates::log_page(&templates::LogPageContext {
        title: &state.config.title,
        tree: &state.channels,
        channel,
        date,
        lines: &lines,
        prev_date: prev,
        next_date: next,
        is_today,
        ai_enabled: state.config.ai.is_some(),
        base_path: &state.config.base_path,
    }).into_response();
    let cc = if is_today {
        "public, max-age=30, s-maxage=120"
    } else {
        "public, max-age=3600, s-maxage=86400"
    };
    resp.headers_mut().insert(header::CACHE_CONTROL, cc.parse().unwrap());
    resp
}

fn serve_search(state: &AppState, channel: &crate::Channel, query: &str) -> Response {
    let results = search_channel(channel, query, state.config.search_limit);
    let mut resp = templates::search_page(&state.config.title, &state.channels, channel, query, &results, &state.config.base_path)
        .into_response();
    resp.headers_mut().insert(header::CACHE_CONTROL, "private, no-cache".parse().unwrap());
    resp
}

async fn serve_raw(channel: &crate::Channel, date: &str) -> Response {
    let Some((path, _)) = resolve_log_path(channel, date) else {
        return (StatusCode::NOT_FOUND, format!("no log for {date}")).into_response();
    };
    let cc = if date == today_date() {
        "public, max-age=60, s-maxage=300"
    } else {
        "public, max-age=86400, s-maxage=604800"
    };
    match read_log_file(&path) {
        Ok(content) => {
            (
                [
                    (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                    cache_control(cc),
                ],
                content,
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("read error: {e}")).into_response(),
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

pub fn resolve_log_path(channel: &crate::Channel, date: &str) -> Option<(std::path::PathBuf, LogFormat)> {
    for dir in &channel.dirs {
        let plain = dir.path.join(format!("{date}.log"));
        if plain.exists() {
            return Some((plain, dir.format));
        }
        let zst = dir.path.join(format!("{date}.log.zst"));
        if zst.exists() {
            return Some((zst, dir.format));
        }
    }
    None
}

fn latest_date(channel: &crate::Channel) -> String {
    let dates = channel_dates(channel);
    dates.last().cloned().unwrap_or_else(today_date)
}

pub fn channel_dates(channel: &crate::Channel) -> Vec<String> {
    let mut dates = std::collections::BTreeSet::new();
    for dir in &channel.dirs {
        let Ok(entries) = std::fs::read_dir(&dir.path) else { continue };
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            let date = name.strip_suffix(".log")
                .or_else(|| name.strip_suffix(".log.zst"));
            if let Some(d) = date {
                if d.len() == 10 {
                    dates.insert(d.to_string());
                }
            }
        }
    }
    dates.into_iter().collect()
}

fn serve_ask_page(state: &AppState, channel: &crate::Channel) -> Response {
    if state.config.ai.is_none() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let mut resp = templates::ask_page(&state.config.title, &state.channels, channel, &state.config.base_path).into_response();
    resp.headers_mut().insert(header::CACHE_CONTROL, "private, no-cache".parse().unwrap());
    resp
}

async fn serve_ask_stream(
    state: Arc<AppState>,
    channel: crate::Channel,
    query: &str,
) -> Response {
    if state.config.ai.is_none() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let query = query.to_string();
    if query.is_empty() {
        return (StatusCode::BAD_REQUEST, "query is required").into_response();
    }

    let semaphore = state.ai_semaphore.as_ref().unwrap();
    let permit = match semaphore.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "AI search is busy, try again later",
            )
                .into_response();
        }
    };

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<crate::ai::SseEvent>();

    tokio::spawn(async move {
        crate::ai::run_ai_session(query, channel, state, tx).await;
        drop(permit);
    });

    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(|event| {
        let (event_type, data) = match event {
            crate::ai::SseEvent::ToolCall {
                name,
                input_summary,
            } => (
                "tool_call",
                serde_json::json!({"name": name, "input": input_summary}).to_string(),
            ),
            crate::ai::SseEvent::ToolResult {
                name,
                output_preview,
            } => (
                "tool_result",
                serde_json::json!({"name": name, "output": output_preview}).to_string(),
            ),
            crate::ai::SseEvent::Display(text) => {
                ("display", serde_json::json!({"text": text}).to_string())
            }
            crate::ai::SseEvent::Done { url, output } => (
                "done",
                serde_json::json!({"url": url, "output": output}).to_string(),
            ),
            crate::ai::SseEvent::Error(msg) => {
                ("ask_error", serde_json::json!({"error": msg}).to_string())
            }
        };
        Ok::<_, std::convert::Infallible>(Event::default().event(event_type).data(data))
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn serve_ask_output(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> Response {
    let ai_config = match &state.config.ai {
        Some(c) => c,
        None => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };

    if !filename
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
    {
        return (StatusCode::BAD_REQUEST, "invalid filename").into_response();
    }
    if filename.ends_with(".md") {
        let path = ai_config.output_dir.join(&filename);
        return match std::fs::read_to_string(&path) {
            Ok(content) => {
                (
                    [
                        (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                        cache_control("public, max-age=3600, s-maxage=86400"),
                    ],
                    content,
                )
                    .into_response()
            }
            Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
        };
    }

    if filename.ends_with(".html") {
        let md_name = format!("{}.md", filename.strip_suffix(".html").unwrap());
        let path = ai_config.output_dir.join(&md_name);
        return match std::fs::read_to_string(&path) {
            Ok(content) => {
                let mut resp = templates::ask_output_page(&state.config.title, &md_name, &content, &state.config.base_path).into_response();
                resp.headers_mut().insert(header::CACHE_CONTROL, "public, max-age=3600, s-maxage=86400".parse().unwrap());
                resp
            }
            Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
        };
    }

    (StatusCode::BAD_REQUEST, "invalid filename").into_response()
}
