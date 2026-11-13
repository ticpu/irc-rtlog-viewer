use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::sync::{Arc, LazyLock};

use axum::Router;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Query, State};
use axum::http::HeaderMap;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware::map_response;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use cookie::{Cookie, SameSite};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::AppState;
use crate::limits;
use crate::parser::{LogFormat, parse_line};
use crate::presence::{ChannelPresence, json_body};
use crate::search::search_channel;
use crate::templates;

static CSS: &str = include_str!("../static/style.css");
static JS: &str = include_str!("../static/app.js");

/// Asset URLs carry a hash of their content: browsers and proxies keep them for
/// a day or more, and a page must never pair new markup with an old script.
pub static CSS_URL: LazyLock<String> =
    LazyLock::new(|| format!("/static/style.css?v={}", content_version(CSS)));
pub static JS_URL: LazyLock<String> =
    LazyLock::new(|| format!("/static/app.js?v={}", content_version(JS)));

fn content_version(content: &str) -> String {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// No inline script or handler anywhere; inline style attributes stay allowed for
/// the per-nick colours. frame-src and frame-ancestors cover the ask result iframe.
///
/// CSP is the default for every response. Log pages send LOG_PAGE_CSP instead,
/// which adds https: images and media and the youtube-nocookie player for the
/// embeds app.js builds once the viewer turns them on. Ask output stays on CSP:
/// its markdown embeds verbatim IRC lines, and an attacker-chosen image there
/// would load without the viewer opting in.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
    style-src-attr 'unsafe-inline'; connect-src 'self'; frame-src 'self'; \
    frame-ancestors 'self'; form-action 'self'; base-uri 'none'";
const LOG_PAGE_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
    style-src-attr 'unsafe-inline'; connect-src 'self'; \
    frame-src 'self' https://www.youtube-nocookie.com; \
    frame-ancestors 'self'; form-action 'self'; base-uri 'none'; \
    img-src 'self' https:; media-src https:";

fn cache_control(value: &str) -> (header::HeaderName, &str) {
    (header::CACHE_CONTROL, value)
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(index))
        .route("/static/style.css", get(serve_css))
        .route("/static/app.js", get(serve_js))
        .route("/ask/output/{filename}", get(serve_ask_output))
        .fallback(get(wildcard))
        .layer(map_response(security_headers))
}

async fn index(State(state): State<Arc<AppState>>) -> Response {
    let mut resp = templates::page(
        &state
            .config
            .title,
        &state.channels,
        &state
            .config
            .base_path,
        maud::html! {
            h1 { (&state.config.title) }
            p { "Select a channel from the sidebar." }
        },
    )
    .into_response();
    resp.headers_mut()
        .insert(
            header::CACHE_CONTROL,
            "private, no-cache"
                .parse()
                .unwrap(),
        );
    resp
}

async fn security_headers(mut resp: Response) -> Response {
    let headers = resp.headers_mut();
    headers
        .entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(HeaderValue::from_static(CSP));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

async fn serve_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            cache_control("public, max-age=86400, s-maxage=604800"),
        ],
        JS,
    )
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
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    uri: Uri,
    Query(search): Query<SearchQuery>,
) -> Response {
    let addr = state
        .limits
        .client_addr(peer, &headers);
    // Split before decoding so an encoded %2F stays inside its segment.
    let decoded: Result<Vec<_>, _> = uri
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| percent_decode_str(s).decode_utf8())
        .collect();
    let Ok(decoded) = decoded else {
        return (StatusCode::BAD_REQUEST, "path is not valid UTF-8").into_response();
    };
    let segments: Vec<&str> = decoded
        .iter()
        .map(|s| s.as_ref())
        .collect();

    if segments.is_empty() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let last = *segments
        .last()
        .unwrap();

    // Check for "ask/stream" (two trailing segments)
    if segments.len() >= 3
        && segments[segments.len() - 2] == "ask"
        && segments[segments.len() - 1] == "stream"
    {
        let channel_segments = &segments[..segments.len() - 2];
        if let Some(channel) = find_channel(&state.channels, channel_segments).cloned() {
            let query = search
                .q
                .unwrap_or_default();
            return serve_ask_stream(state, channel, &query, addr, &headers)
                .await
                .into_response();
        }
    }

    // Try to find channel with all segments vs. all-but-last
    if last == "today"
        || last == "latest"
        || last == "search"
        || last == "presence"
        || last == "ask"
        || looks_like_date(last)
        || is_date_raw(last, segments.len())
    {
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
                    let bp = &state
                        .config
                        .base_path;
                    let encoded = channel.url_path();
                    let date = latest_date(&channel);
                    Redirect::temporary(&format!("{bp}/{encoded}/{date}")).into_response()
                }
                "latest" => serve_sse(state, &channel)
                    .await
                    .into_response(),
                "search" => {
                    let query = search
                        .q
                        .unwrap_or_default();
                    search_with_limits(state, channel, query, addr).await
                }
                "presence" => serve_presence(&state, &channel, &headers).await,
                "ask" => serve_ask_page(&state, &channel, &headers),
                "raw" => {
                    let date = segments[segments.len() - 2].to_owned();
                    off_runtime(move || serve_raw(&channel, &date)).await
                }
                date if looks_like_date(date) => {
                    let date = date.to_owned();
                    off_runtime(move || serve_log_page(&state, &channel, &date)).await
                }
                _ => (StatusCode::NOT_FOUND, "not found").into_response(),
            };
        }
    }

    // Maybe bare channel path → redirect to latest date
    if let Some(channel) = find_channel(&state.channels, &segments) {
        let bp = &state
            .config
            .base_path;
        let encoded = channel.url_path();
        let date = latest_date(channel);
        return Redirect::temporary(&format!("{bp}/{encoded}/{date}")).into_response();
    }

    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// Applies the search page's limits, then runs the search on the blocking pool.
async fn search_with_limits(
    state: Arc<AppState>,
    channel: crate::Channel,
    query: String,
    addr: IpAddr,
) -> Response {
    let max_len = state
        .config
        .search
        .max_query_len;
    if query
        .chars()
        .count()
        > max_len
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("query too long, limit is {max_len} characters"),
        )
            .into_response();
    }
    // The slot is taken before the rate token so a busy refusal costs nothing.
    let Ok(slot) = Arc::clone(
        &state
            .limits
            .search_slots,
    )
    .try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "5")],
            "search is busy, try again in a few seconds",
        )
            .into_response();
    };
    if let Err(wait) = state
        .limits
        .charge_search(addr)
    {
        let secs = wait
            .as_secs()
            .max(1);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, secs.to_string())],
            format!("too many searches, try again in {secs} s"),
        )
            .into_response();
    }
    off_runtime(move || {
        let response = serve_search(&state, &channel, &query);
        drop(slot);
        response
    })
    .await
}

/// Runs file-reading work on the blocking pool: the runtime has one thread, which
/// a scan over a channel's logs would otherwise hold against every request.
async fn off_runtime(f: impl FnOnce() -> Response + Send + 'static) -> Response {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| {
            eprintln!("blocking handler failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        })
}

fn is_date_raw(last: &str, len: usize) -> bool {
    last == "raw" && len >= 2
}

fn looks_like_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4]
            .iter()
            .all(u8::is_ascii_digit)
        && b[5..7]
            .iter()
            .all(u8::is_ascii_digit)
        && b[8..10]
            .iter()
            .all(u8::is_ascii_digit)
}

pub fn today_date() -> String {
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

fn find_channel<'a>(node: &'a crate::ChannelNode, segments: &[&str]) -> Option<&'a crate::Channel> {
    let mut current = node;
    for seg in segments {
        current = current
            .children
            .get(*seg)?;
    }
    current
        .channel
        .as_ref()
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read error: {e}"),
            )
                .into_response();
        }
    };

    let lines: Vec<_> = content
        .lines()
        .filter_map(|l| parse_line(l, format))
        .collect();

    let dates = channel_dates(channel);
    let idx = dates
        .iter()
        .position(|d| d == date);
    let prev = idx
        .and_then(|i| if i > 0 { dates.get(i - 1) } else { None })
        .map(|s| s.as_str());
    let next = idx
        .and_then(|i| dates.get(i + 1))
        .map(|s| s.as_str());
    let is_today = date == today_date();

    let mut resp = templates::log_page(&templates::LogPageContext {
        title: &state
            .config
            .title,
        tree: &state.channels,
        channel,
        date,
        lines: &lines,
        prev_date: prev,
        next_date: next,
        is_today,
        ai_enabled: state
            .config
            .ai
            .is_some(),
        base_path: &state
            .config
            .base_path,
        search_max_len: state
            .config
            .search
            .max_query_len,
    })
    .into_response();
    let cc = if is_today {
        "public, max-age=30, s-maxage=120"
    } else {
        "public, max-age=3600, s-maxage=86400"
    };
    let headers = resp.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        cc.parse()
            .unwrap(),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(LOG_PAGE_CSP),
    );
    resp
}

fn serve_search(state: &AppState, channel: &crate::Channel, query: &str) -> Response {
    let results = search_channel(
        channel,
        query,
        state
            .config
            .search_limit,
        state
            .config
            .search
            .max_days,
    );
    let mut resp = templates::search_page(
        &state
            .config
            .title,
        &state.channels,
        channel,
        query,
        &results,
        &state
            .config
            .base_path,
        state
            .config
            .search
            .max_query_len,
    )
    .into_response();
    resp.headers_mut()
        .insert(
            header::CACHE_CONTROL,
            "private, no-cache"
                .parse()
                .unwrap(),
        );
    resp
}

fn serve_raw(channel: &crate::Channel, date: &str) -> Response {
    let Some((path, _)) = resolve_log_path(channel, date) else {
        return (StatusCode::NOT_FOUND, format!("no log for {date}")).into_response();
    };
    let cc = if date == today_date() {
        "public, max-age=60, s-maxage=300"
    } else {
        "public, max-age=86400, s-maxage=604800"
    };
    match read_log_file(&path) {
        Ok(content) => (
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                cache_control(cc),
            ],
            content,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read error: {e}"),
        )
            .into_response(),
    }
}

/// Channels with more users than this build their presence JSON on the blocking pool.
const PRESENCE_INLINE_USERS: usize = 1000;

/// The channel's archive stamps as JSON, tagged with a hash of the body.
async fn serve_presence(
    state: &AppState,
    channel: &crate::Channel,
    headers: &HeaderMap,
) -> Response {
    let Some(presence) = &state.presence else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let scanning = presence.scanning();
    let snapshot = presence.channel(
        &channel
            .path_segments
            .join("/"),
    );
    let if_none_match = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| {
            v.to_str()
                .ok()
        })
        .map(str::to_owned);
    let large = snapshot
        .as_ref()
        .is_some_and(|c| {
            c.users
                .len()
                > PRESENCE_INLINE_USERS
        });
    let respond =
        move || presence_response(scanning, snapshot.as_deref(), if_none_match.as_deref());
    if large {
        off_runtime(respond).await
    } else {
        respond()
    }
}

fn presence_response(
    scanning: bool,
    channel: Option<&ChannelPresence>,
    if_none_match: Option<&str>,
) -> Response {
    let body = match json_body(scanning, channel) {
        Ok(body) => body,
        Err(e) => {
            eprintln!("presence: cannot serialize the JSON body: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    let etag = format!("\"{}\"", content_version(&body));
    let cc = cache_control("private, max-age=30");
    if if_none_match.is_some_and(|inm| etag_matches(inm, &etag)) {
        return (
            StatusCode::NOT_MODIFIED,
            [cc, (header::ETAG, etag.as_str())],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            cc,
            (header::ETAG, etag.as_str()),
        ],
        body,
    )
        .into_response()
}

/// If-None-Match under RFC 9110's weak comparison: `*`, or any listed tag
/// equal to `etag` once `W/` is dropped. Cloudflare weakens tags it compresses.
fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    let ours = etag
        .strip_prefix("W/")
        .unwrap_or(etag);
    if_none_match
        .split(',')
        .map(str::trim)
        .any(|tag| {
            tag == "*"
                || tag
                    .strip_prefix("W/")
                    .unwrap_or(tag)
                    == ours
        })
}

async fn serve_sse(
    state: Arc<AppState>,
    channel: &crate::Channel,
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

/// A day file's text, decompressed when it ends in `.zst`. Invalid UTF-8 is
/// replaced with U+FFFD, as in the tail, so one stray byte does not cost the whole day.
pub fn read_log_file(path: &Path) -> io::Result<String> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();

    if path
        .extension()
        .and_then(|e| e.to_str())
        == Some("zst")
    {
        let mut decoder = zstd::Decoder::new(BufReader::new(file))?;
        decoder.read_to_end(&mut bytes)?;
    } else {
        let mut reader = BufReader::new(file);
        reader.read_to_end(&mut bytes)?;
    }

    Ok(String::from_utf8(bytes)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
}

pub fn resolve_log_path(
    channel: &crate::Channel,
    date: &str,
) -> Option<(std::path::PathBuf, LogFormat)> {
    // Callers include the AI tools, whose date is model output.
    if !looks_like_date(date) {
        return None;
    }
    for dir in &channel.dirs {
        let plain = dir
            .path
            .join(format!("{date}.log"));
        if plain.exists() {
            return Some((plain, dir.format));
        }
        let zst = dir
            .path
            .join(format!("{date}.log.zst"));
        if zst.exists() {
            return Some((zst, dir.format));
        }
    }
    None
}

fn latest_date(channel: &crate::Channel) -> String {
    let dates = channel_dates(channel);
    dates
        .last()
        .cloned()
        .unwrap_or_else(today_date)
}

pub fn channel_dates(channel: &crate::Channel) -> Vec<String> {
    let mut dates = std::collections::BTreeSet::new();
    for dir in &channel.dirs {
        let Ok(entries) = std::fs::read_dir(&dir.path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(name) = entry
                .file_name()
                .into_string()
            else {
                continue;
            };
            let date = name
                .strip_suffix(".log")
                .or_else(|| name.strip_suffix(".log.zst"));
            if let Some(d) = date {
                if d.len() == 10 {
                    dates.insert(d.to_string());
                }
            }
        }
    }
    dates
        .into_iter()
        .collect()
}

fn serve_ask_page(state: &AppState, channel: &crate::Channel, headers: &HeaderMap) -> Response {
    let Some(ai_config) = &state
        .config
        .ai
    else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let mut resp = templates::ask_page(
        &state
            .config
            .title,
        &state.channels,
        channel,
        &state
            .config
            .base_path,
        ai_config.max_query_len,
    )
    .into_response();
    resp.headers_mut()
        .insert(
            header::CACHE_CONTROL,
            "private, no-cache"
                .parse()
                .unwrap(),
        );
    if limits::client_id(headers).is_none() {
        let id = match limits::new_client_id() {
            Ok(id) => id,
            Err(e) => {
                eprintln!("ask: cannot generate a client id: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "cannot generate a client id",
                )
                    .into_response();
            }
        };
        let bp = &state
            .config
            .base_path;
        let cookie = Cookie::build((limits::ASK_COOKIE, id))
            .path(if bp.is_empty() { "/" } else { bp.as_str() })
            .max_age(cookie::time::Duration::days(365))
            .http_only(true)
            .same_site(SameSite::Strict)
            .build();
        match HeaderValue::from_str(&cookie.to_string()) {
            Ok(v) => {
                resp.headers_mut()
                    .insert(header::SET_COOKIE, v);
            }
            Err(e) => {
                eprintln!("ask: cookie for base path {bp:?} is not a valid header: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "invalid base_path for a cookie",
                )
                    .into_response();
            }
        }
    }
    resp
}

async fn serve_ask_stream(
    state: Arc<AppState>,
    channel: crate::Channel,
    query: &str,
    addr: IpAddr,
    headers: &HeaderMap,
) -> Response {
    if state
        .config
        .ai
        .is_none()
    {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let query = query.to_string();
    if query.is_empty() {
        return (StatusCode::BAD_REQUEST, "query is required").into_response();
    }

    // chars, not bytes, so the limit matches what the input's maxlength counts
    let max_query_len = state
        .config
        .ai
        .as_ref()
        .unwrap()
        .max_query_len;
    if query
        .chars()
        .count()
        > max_query_len
    {
        return (
            StatusCode::BAD_REQUEST,
            format!("query too long, limit is {max_query_len} characters"),
        )
            .into_response();
    }

    // EventSource cannot read a status code, so refusals from here on reach the
    // page as an ask_error event.
    let Some(client_id) = limits::client_id(headers) else {
        return sse_error("missing ask_id cookie, reload the page".into());
    };
    let semaphore = state
        .ai_semaphore
        .as_ref()
        .unwrap();
    let permit = match semaphore
        .clone()
        .try_acquire_owned()
    {
        Ok(p) => p,
        Err(_) => {
            return sse_error("AI search is busy, try again later".into());
        }
    };
    if let Err(refusal) = state
        .limits
        .charge_ask(&client_id, addr)
    {
        return sse_error(refusal.to_string());
    }
    let client = crate::ai::AskClient {
        id: client_id,
        addr,
    };

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<crate::ai::SseEvent>();

    tokio::spawn(async move {
        crate::ai::run_ai_session(query, channel, state, client, tx).await;
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
        Ok::<_, std::convert::Infallible>(
            Event::default()
                .event(event_type)
                .data(data),
        )
    });

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// A one-event stream carrying `msg` as an ask_error.
fn sse_error(msg: String) -> Response {
    let event = Event::default()
        .event("ask_error")
        .data(serde_json::json!({"error": msg}).to_string());
    Sse::new(tokio_stream::once(Ok::<_, std::convert::Infallible>(event))).into_response()
}

async fn serve_ask_output(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> Response {
    let ai_config = match &state
        .config
        .ai
    {
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
        let path = ai_config
            .output_dir
            .join(&filename);
        return match std::fs::read_to_string(&path) {
            Ok(content) => (
                [
                    (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                    cache_control("public, max-age=3600, s-maxage=86400"),
                ],
                content,
            )
                .into_response(),
            Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
        };
    }

    if filename.ends_with(".html") {
        let md_name = format!(
            "{}.md",
            filename
                .strip_suffix(".html")
                .unwrap()
        );
        let path = ai_config
            .output_dir
            .join(&md_name);
        return match std::fs::read_to_string(&path) {
            Ok(content) => {
                let mut resp = templates::ask_output_page(
                    &state
                        .config
                        .title,
                    &md_name,
                    &content,
                    &state
                        .config
                        .base_path,
                )
                .into_response();
                resp.headers_mut()
                    .insert(
                        header::CACHE_CONTROL,
                        "public, max-age=3600, s-maxage=86400"
                            .parse()
                            .unwrap(),
                    );
                resp
            }
            Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
        };
    }

    (StatusCode::BAD_REQUEST, "invalid filename").into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::test_support::{add_channel, app, app_state, stalled_channel, tempdir};
    use crate::{ChannelDir, ChannelNode};

    const PEER: &str = "192.0.2.1:4000";

    const CHANNELS: &[&[&str]] = &[
        &["OFTC", "#bcache"],
        &["net", "#café"],
        &["net", "#a b'\"<"],
    ];

    fn state() -> Arc<AppState> {
        let mut root = ChannelNode::default();
        for segments in CHANNELS {
            add_channel(&mut root, segments, Vec::new());
        }
        Arc::new(app_state(root, "logs_dirs: []"))
    }

    async fn get_status(uri: &str) -> (StatusCode, Option<String>) {
        let resp = app(state(), PEER)
            .oneshot(
                Request::get(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let location = resp
            .headers()
            .get(header::LOCATION)
            .map(|v| {
                v.to_str()
                    .unwrap()
                    .to_owned()
            });
        (resp.status(), location)
    }

    #[tokio::test]
    async fn bare_channel_redirects_with_or_without_trailing_slash() {
        for uri in ["/OFTC/%23bcache", "/OFTC/%23bcache/"] {
            let (status, location) = get_status(uri).await;
            assert_eq!(status, StatusCode::TEMPORARY_REDIRECT, "{uri}");
            let location = location.unwrap();
            assert!(
                location.starts_with("/OFTC/%23bcache/"),
                "{uri} -> {location}"
            );
            assert!(
                looks_like_date(&location["/OFTC/%23bcache/".len()..]),
                "{uri} -> {location}"
            );
        }
    }

    #[tokio::test]
    async fn today_resolves_with_trailing_slash() {
        let (status, _) = get_status("/OFTC/%23bcache/today/").await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    }

    #[tokio::test]
    async fn every_channel_round_trips_through_its_url() {
        let state = state();
        let mut stack = vec![&state.channels];
        let mut seen = 0;
        while let Some(node) = stack.pop() {
            stack.extend(
                node.children
                    .values(),
            );
            let Some(channel) = &node.channel else {
                continue;
            };
            seen += 1;
            let url = format!("/{}", channel.url_path());
            assert!(
                url.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~%/".contains(&b)),
                "{url}"
            );
            let (status, location) = get_status(&url).await;
            assert_eq!(status, StatusCode::TEMPORARY_REDIRECT, "{url}");
            let location = location.unwrap();
            assert!(
                location.starts_with(&format!("{url}/")),
                "{url} -> {location}"
            );
        }
        assert_eq!(seen, CHANNELS.len());
    }

    #[tokio::test]
    async fn escape_at_end_of_path_is_decoded() {
        let (status, _) = get_status("/OFTC/%23bcach%65").await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
    }

    #[tokio::test]
    async fn encoded_slash_stays_in_its_segment() {
        let (status, _) = get_status("/OFTC%2F%23bcache").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn invalid_utf8_in_path_is_rejected() {
        let (status, _) = get_status("/OFTC/%FF").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn file_backed_pages_serve_the_log() {
        let root = tempdir();
        std::fs::write(
            root.path()
                .join("2026-01-01.log"),
            "2026-01-01T00:00:00Z <a> needle\n",
        )
        .unwrap();
        let mut channels = ChannelNode::default();
        let dirs = vec![ChannelDir {
            path: root
                .path()
                .to_owned(),
            format: LogFormat::Iso8601,
        }];
        add_channel(&mut channels, &["#c"], dirs);
        let app = app(Arc::new(app_state(channels, "logs_dirs: []")), PEER);
        for uri in [
            "/%23c/search?q=needle",
            "/%23c/2026-01-01",
            "/%23c/2026-01-01/raw",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::get(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(body.contains("needle"), "{uri}: {body}");
        }
    }

    #[tokio::test]
    async fn invalid_utf8_in_a_log_is_replaced_not_fatal() {
        let root = tempdir();
        std::fs::write(
            root.path()
                .join("2026-01-01.log"),
            b"2026-01-01T00:00:00Z <a> caf\xe9 needle\n",
        )
        .unwrap();
        let mut channels = ChannelNode::default();
        let dirs = vec![ChannelDir {
            path: root
                .path()
                .to_owned(),
            format: LogFormat::Iso8601,
        }];
        add_channel(&mut channels, &["#c"], dirs);
        let app = app(Arc::new(app_state(channels, "logs_dirs: []")), PEER);
        for uri in ["/%23c/2026-01-01", "/%23c/2026-01-01/raw"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::get(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(body.contains("caf\u{fffd} needle"), "{uri}: {body}");
        }
    }

    #[tokio::test]
    async fn a_stalled_search_does_not_block_other_requests() {
        let (_root, channels, release, writer) = stalled_channel();
        let app = app(Arc::new(app_state(channels, "logs_dirs: []")), PEER);

        let search = tokio::spawn(
            app.clone()
                .oneshot(
                    Request::get("/%23c/search?q=x")
                        .body(Body::empty())
                        .unwrap(),
                ),
        );
        tokio::task::yield_now().await;
        let other = app
            .oneshot(
                Request::get("/%23c")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(other.status(), StatusCode::TEMPORARY_REDIRECT);
        // Fails only once the writer has timed out, which the join below reports.
        let _ = release.send(());

        assert!(
            !writer
                .join()
                .unwrap(),
            "runtime was blocked by the search"
        );
        let search = search
            .await
            .unwrap()
            .unwrap();
        assert_eq!(search.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn pages_carry_security_headers_and_no_inline_script() {
        let root = tempdir();
        std::fs::write(
            root.path()
                .join("2026-01-01.log"),
            "2026-01-01T00:00:00Z <a> hi\n",
        )
        .unwrap();
        let mut channels = ChannelNode::default();
        let dirs = vec![ChannelDir {
            path: root
                .path()
                .to_owned(),
            format: LogFormat::Iso8601,
        }];
        add_channel(&mut channels, &["#c"], dirs);
        let out = tempdir();
        std::fs::write(
            out.path()
                .join("x.md"),
            "# answer\n",
        )
        .unwrap();
        let state = app_state(
            channels,
            &format!(
                "{{logs_dirs: [], ai: {{api_key: k, output_dir: '{}'}}}}",
                out.path()
                    .display()
            ),
        );
        let app = app(Arc::new(state), PEER);
        let inline = regex::Regex::new(r"<script>|<script [^>]*>[^<]|\son[a-z]+=").unwrap();
        for (uri, csp) in [
            ("/", CSP),
            ("/%23c/2026-01-01", LOG_PAGE_CSP),
            ("/%23c/search?q=hi", CSP),
            ("/%23c/ask", CSP),
            ("/ask/output/x.html", CSP),
            ("/static/app.js", CSP),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::get(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let headers = resp.headers();
            assert_eq!(headers[header::CONTENT_SECURITY_POLICY], csp, "{uri}");
            assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{uri}");
            if uri.ends_with(".js") {
                continue;
            }
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(body.contains(&format!(r#"href="{}""#, *CSS_URL)), "{uri}");
            if !uri.starts_with("/ask/output/") {
                assert!(body.contains(&format!(r#"src="{}""#, *JS_URL)), "{uri}");
            }
            assert!(!inline.is_match(&body), "{uri}: {body}");
            if csp == LOG_PAGE_CSP {
                assert_log_page_toggles(&body);
            }
        }
    }

    fn assert_log_page_toggles(body: &str) {
        let toggle = |id: &str| {
            let start = body
                .find(&format!(r#"<input id="{id}""#))
                .unwrap_or_else(|| panic!("{id} missing: {body}"));
            let end = start
                + body[start..]
                    .find('>')
                    .unwrap();
            (start, body[start..end].contains(" checked"))
        };
        let live = toggle("toggle-live");
        let events = toggle("toggle-events");
        let embeds = toggle("toggle-embeds");
        assert!(live.0 < events.0 && events.0 < embeds.0, "{body}");
        assert_eq!((live.1, events.1, embeds.1), (true, false, false), "{body}");
        assert!(
            body.contains(r#"<div id="log" class="hide-events""#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn unknown_channel_is_not_found() {
        let (status, _) = get_status("/OFTC/%23nope/").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

#[cfg(test)]
mod limit_tests {

    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::post;
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    use super::*;
    use crate::test_support::{add_channel, app, app_state, stalled_channel, tempdir};
    use crate::{ChannelDir, ChannelNode};

    const PEER: &str = "192.0.2.1:4000";

    /// A `#c` channel with one log file per `(date, line)`.
    fn channel_with_logs(days: &[(&str, &str)]) -> (tempfile::TempDir, ChannelNode) {
        let root = tempdir();
        for (date, text) in days {
            std::fs::write(
                root.path()
                    .join(format!("{date}.log")),
                format!("{date}T00:00:00Z <a> {text}\n"),
            )
            .unwrap();
        }
        let mut channels = ChannelNode::default();
        let dirs = vec![ChannelDir {
            path: root
                .path()
                .to_owned(),
            format: LogFormat::Iso8601,
        }];
        add_channel(&mut channels, &["#c"], dirs);
        (root, channels)
    }

    async fn get(
        app: &Router,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, String) {
        let mut req = Request::get(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = app
            .clone()
            .oneshot(
                req.body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp
            .headers()
            .clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    type Requests = Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

    /// Stands in for the Messages API: records each request body, with its
    /// anthropic-beta header added, and answers every one with `reply`.
    async fn fake_api(reply: serde_json::Value) -> (String, Requests) {
        let requests = Requests::default();
        let recorded = Arc::clone(&requests);
        let api = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, body: String| {
                let recorded = Arc::clone(&recorded);
                let reply = reply.to_string();
                async move {
                    let mut body: serde_json::Value = serde_json::from_str(&body).unwrap();
                    // Kept beside the body so tests can check the beta header.
                    body["anthropic-beta"] = headers
                        .get("anthropic-beta")
                        .and_then(|v| {
                            v.to_str()
                                .ok()
                        })
                        .into();
                    recorded
                        .lock()
                        .unwrap()
                        .push(body);
                    ([(header::CONTENT_TYPE, "application/json")], reply)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener
            .local_addr()
            .unwrap();
        tokio::spawn(async move { axum::serve(listener, api).await });
        (format!("http://{addr}/v1/messages"), requests)
    }

    /// Always asks for one more display call, so a session runs until stopped.
    fn display_reply() -> serde_json::Value {
        serde_json::json!({
            "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "t", "name": "display", "input": {"text": "x"}}],
        })
    }

    /// The router with AI enabled against `api_url`; `ai_extra` adds keys to the ai section.
    fn ask_app(api_url: String, out: &Path, ai_extra: &str) -> Router {
        let (_logs, channels) = channel_with_logs(&[]);
        let yaml = format!(
            "{{logs_dirs: [], ai: {{api_key: k, output_dir: '{}'{ai_extra}}}}}",
            out.display()
        );
        let mut state = app_state(channels, &yaml);
        state.api_url = api_url;
        crate::install_crypto_provider();
        // Its own client: the proxy variables of the test environment must not
        // route a loopback request.
        state.reqwest_client = Some(
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap(),
        );
        state.ai_semaphore = Some(Arc::new(Semaphore::new(1)));
        app(Arc::new(state), PEER)
    }

    const ASK: &str = "/%23c/ask/stream?q=x";

    fn cookie(suffix: &str) -> String {
        format!(
            "{}=0000000000000000000000000000{suffix}",
            limits::ASK_COOKIE
        )
    }

    #[tokio::test]
    async fn ask_sessions_stop_at_the_client_then_the_address_budget() {
        let (url, requests) = fake_api(display_reply()).await;
        let out = tempdir();
        let app = ask_app(
            url,
            out.path(),
            ", requests_per_day_per_client: 3, requests_per_day_per_ip: 5",
        );
        let hits = || {
            requests
                .lock()
                .unwrap()
                .len()
        };

        let (_, _, body) = get(&app, ASK, &[("cookie", &cookie("00aa"))]).await;
        assert!(body.contains("used up for this browser"), "{body}");
        assert_eq!(hits(), 3);

        let (_, _, body) = get(&app, ASK, &[("cookie", &cookie("00bb"))]).await;
        assert!(body.contains("used up for your address"), "{body}");
        assert_eq!(hits(), 5);

        let (_, _, body) = get(&app, ASK, &[]).await;
        assert!(body.contains("missing ask_id cookie"), "{body}");
        assert_eq!(hits(), 5);
    }

    #[tokio::test]
    async fn ask_requests_only_append_to_the_conversation() {
        let (url, requests) = fake_api(display_reply()).await;
        let out = tempdir();
        let app = ask_app(url, out.path(), ", max_tool_calls: 4");
        get(&app, ASK, &[("cookie", &cookie("00cc"))]).await;

        let requests = requests
            .lock()
            .unwrap();
        assert_eq!(requests.len(), 4);
        for pair in requests.windows(2) {
            let before = pair[0]["messages"]
                .as_array()
                .unwrap();
            let after = pair[1]["messages"]
                .as_array()
                .unwrap();
            assert_eq!(
                &after[..before.len()],
                &before[..],
                "an earlier message changed"
            );
            assert_eq!(pair[0]["system"], pair[1]["system"]);
            assert_eq!(pair[0]["tools"], pair[1]["tools"]);
        }
        assert_eq!(requests[0]["model"], "claude-haiku-5-5");
        assert_eq!(requests[0]["cache_control"]["type"], "ephemeral");
    }

    #[tokio::test]
    async fn ask_requests_clear_old_tool_results_server_side() {
        let (url, requests) = fake_api(display_reply()).await;
        let out = tempdir();
        let app = ask_app(url, out.path(), ", max_tool_calls: 1");
        get(&app, ASK, &[("cookie", &cookie("00ee"))]).await;
        let requests = requests
            .lock()
            .unwrap();
        assert_eq!(
            requests[0]["anthropic-beta"],
            "context-management-2025-06-27"
        );
        let edit = &requests[0]["context_management"]["edits"][0];
        assert_eq!(edit["type"], "clear_tool_uses_20250919");
        assert_eq!(edit["trigger"]["type"], "input_tokens");
    }

    #[tokio::test]
    async fn a_session_past_the_token_guard_stops_cleanly() {
        let mut reply = display_reply();
        reply["usage"] = serde_json::json!({
            "input_tokens": 1_000,
            "cache_read_input_tokens": 95_000,
            "cache_creation_input_tokens": 0,
            "output_tokens": 10,
        });
        let (url, requests) = fake_api(reply).await;
        let out = tempdir();
        let app = ask_app(url, out.path(), "");
        let (_, _, body) = get(&app, ASK, &[("cookie", &cookie("00ff"))]).await;
        assert!(
            body.contains("the conversation passed 90000 tokens"),
            "{body}"
        );
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn running_out_of_requests_saves_what_was_found() {
        let (url, _) = fake_api(serde_json::json!({
            "stop_reason": "tool_use",
            "content": [{"type": "tool_use", "id": "t", "name": "output", "input": {"text": "found it"}}],
        }))
        .await;
        let out = tempdir();
        let app = ask_app(url, out.path(), ", max_tool_calls: 3");
        let (_, _, body) = get(&app, ASK, &[("cookie", &cookie("0a11"))]).await;
        assert!(
            body.contains("stopped after 3 API requests; saving"),
            "{body}"
        );
        assert!(body.contains("event: done"), "{body}");
        let saved: Vec<_> = std::fs::read_dir(out.path())
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(saved.len(), 1);
        let text = std::fs::read_to_string(saved[0].path()).unwrap();
        assert_eq!(
            text.matches("found it")
                .count(),
            3,
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_refusal_reaches_the_page_with_its_category() {
        let (url, _) = fake_api(serde_json::json!({
            "stop_reason": "refusal",
            "stop_details": {"type": "refusal", "category": "cyber"},
            "content": [],
        }))
        .await;
        let out = tempdir();
        let app = ask_app(url, out.path(), "");
        let (_, _, body) = get(&app, ASK, &[("cookie", &cookie("00dd"))]).await;
        assert!(body.contains("declined this question (cyber)"), "{body}");
    }

    #[tokio::test]
    async fn ask_page_hands_out_the_cookie_once() {
        let (_logs, channels) = channel_with_logs(&[]);
        let state = app_state(
            channels,
            "{logs_dirs: [], base_path: /irc, ai: {api_key: k, output_dir: /x}}",
        );
        let app = app(Arc::new(state), PEER);
        let (status, headers, _) = get(&app, "/%23c/ask", &[]).await;
        assert_eq!(status, StatusCode::OK);
        let cookie = headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        for attr in ["HttpOnly", "SameSite=Strict", "Path=/irc", "Max-Age="] {
            assert!(cookie.contains(attr), "{cookie}");
        }
        let pair = cookie
            .split(';')
            .next()
            .unwrap();
        let (_, headers, _) = get(&app, "/%23c/ask", &[("cookie", pair)]).await;
        assert!(
            headers
                .get(header::SET_COOKIE)
                .is_none()
        );
    }

    #[tokio::test]
    async fn search_rejects_long_queries() {
        let (_logs, channels) = channel_with_logs(&[]);
        let app = app(
            Arc::new(app_state(
                channels,
                "{logs_dirs: [], search: {max_query_len: 5}}",
            )),
            PEER,
        );
        let (status, _, _) = get(&app, "/%23c/search?q=%C3%A9%C3%A9%C3%A9%C3%A9%C3%A9", &[]).await;
        assert_eq!(status, StatusCode::OK, "five two-byte characters fit");
        let (status, _, _) = get(&app, "/%23c/search?q=abcdef", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn search_rate_is_per_client_address() {
        let (_logs, channels) = channel_with_logs(&[]);
        let state = Arc::new(app_state(
            channels,
            "{logs_dirs: [], search: {per_ip_per_minute: 2}}",
        ));
        let direct = app(Arc::clone(&state), PEER);
        let proxied = app(Arc::clone(&state), "[::1]:5000");
        let q = "/%23c/search?q=x";
        for _ in 0..2 {
            assert_eq!(
                get(&direct, q, &[])
                    .await
                    .0,
                StatusCode::OK
            );
        }
        let (status, headers, _) = get(&direct, q, &[]).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(headers.contains_key(header::RETRY_AFTER));

        // Through the trusted proxy, X-Real-IP picks the bucket.
        let real = [("x-real-ip", "192.0.2.1")];
        assert_eq!(
            get(&proxied, q, &real)
                .await
                .0,
            StatusCode::TOO_MANY_REQUESTS
        );
        let other = [("x-real-ip", "198.51.100.7")];
        assert_eq!(
            get(&proxied, q, &other)
                .await
                .0,
            StatusCode::OK
        );
        // From an untrusted peer the header is ignored.
        assert_eq!(
            get(&direct, q, &other)
                .await
                .0,
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[tokio::test]
    async fn search_stops_at_the_day_bound() {
        let (_logs, channels) =
            channel_with_logs(&[("2026-01-01", "needle"), ("2026-01-02", "hay")]);
        let app = app(
            Arc::new(app_state(
                channels,
                "{logs_dirs: [], search: {max_days: 1}}",
            )),
            PEER,
        );
        let (_, _, body) = get(&app, "/%23c/search?q=needle", &[]).await;
        assert!(body.contains("no results"), "{body}");
        assert!(body.contains("Searched the newest 1 days"), "{body}");
        let (_, _, body) = get(&app, "/%23c/search?q=hay", &[]).await;
        assert!(body.contains("Searched the newest 1 days"), "{body}");
    }

    #[tokio::test]
    async fn search_beyond_the_concurrency_limit_is_busy() {
        let (_root, channels, release, writer) = stalled_channel();
        let state = Arc::new(app_state(
            channels,
            "{logs_dirs: [], search: {max_concurrent: 1}}",
        ));
        let app = app(Arc::clone(&state), PEER);

        // The first search holds the only slot while it blocks opening the FIFO.
        let stalled = tokio::spawn({
            let app = app.clone();
            async move {
                get(&app, "/%23c/search?q=x", &[])
                    .await
                    .0
            }
        });
        let slots = &state
            .limits
            .search_slots;
        for _ in 0..10_000 {
            if slots.available_permits() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            slots.available_permits(),
            0,
            "first search never took the slot"
        );
        let (status, headers, _) = get(&app, "/%23c/search?q=y", &[]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(headers.contains_key(header::RETRY_AFTER));

        let _ = release.send(());
        assert!(
            !writer
                .join()
                .unwrap(),
            "the stalled search was released by the watchdog"
        );
        assert_eq!(
            stalled
                .await
                .unwrap(),
            StatusCode::OK
        );
    }
}
