use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use axum::Router;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Query, State};
use axum::http::HeaderMap;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware::map_response;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use jiff::{SignedDuration, Timestamp};
use percent_encoding::percent_decode_str;
use serde::Deserialize;

use crate::AppState;
mod ask;
mod embed;
mod live;

use crate::live::LiveChannel;
use crate::logfiles::{
    channel_dates, latest_date, looks_like_date, read_log_file, resolve_log_path, today_date,
};
use crate::parser::{LogFormat, LogLine, parse_line};
use crate::presence::{ChannelPresence, LogClock, json_body};
use crate::search::search_channel;
use crate::templates;
use ask::{serve_ask_output, serve_ask_page, serve_ask_stream};
use embed::{serve_embed, serve_full_message};
use live::serve_sse;

/// How far back /today reaches into the previous day file.
const TODAY_CARRY_HOURS: i64 = 12;

static CSS: &str = include_str!("../static/style.css");
static JS: &str = include_str!("../static/app.js");
static ICON: &[u8] = include_bytes!("../static/icon-32.png");
static TOUCH_ICON: &[u8] = include_bytes!("../static/apple-touch-icon.png");

/// Asset URLs carry a hash of their content: browsers and proxies keep them for
/// a day or more, and a page must never pair new markup with an old script.
pub static CSS_URL: LazyLock<String> =
    LazyLock::new(|| format!("/static/style.css?v={}", content_version(CSS)));
pub static JS_URL: LazyLock<String> =
    LazyLock::new(|| format!("/static/app.js?v={}", content_version(JS)));
pub static ICON_URL: LazyLock<String> =
    LazyLock::new(|| format!("/static/icon-32.png?v={}", content_version(ICON)));
pub static TOUCH_ICON_URL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "/static/apple-touch-icon.png?v={}",
        content_version(TOUCH_ICON)
    )
});

fn content_version<T: Hash + ?Sized>(content: &T) -> String {
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
    style-src-attr 'unsafe-inline'; connect-src 'self'; img-src 'self'; frame-src 'self'; \
    frame-ancestors 'self'; form-action 'self'; base-uri 'none'";
const LOG_PAGE_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
    style-src-attr 'unsafe-inline'; connect-src 'self'; \
    frame-src 'self' https://www.youtube-nocookie.com; \
    frame-ancestors 'self'; form-action 'self'; base-uri 'none'; \
    img-src 'self' https:; media-src https:";

pub(super) fn cache_control(value: &str) -> (header::HeaderName, &str) {
    (header::CACHE_CONTROL, value)
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(index))
        .route("/static/style.css", get(serve_css))
        .route("/static/app.js", get(serve_js))
        .route("/static/icon-32.png", get(|| async { png(ICON) }))
        .route(
            "/static/apple-touch-icon.png",
            get(|| async { png(TOUCH_ICON) }),
        )
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

fn png(body: &'static [u8]) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            cache_control("public, max-age=86400, s-maxage=604800"),
        ],
        body,
    )
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    /// The link an embed request is for.
    url: Option<String>,
    /// The nick of the line a long-message request is for.
    nick: Option<String>,
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
        || is_date_action(last, segments.len())
    {
        let channel_segments = &segments[..segments.len() - 1];
        // Handle YYYY-MM-DD/raw and YYYY-MM-DD/embed
        let (action, channel_segments) = if is_date_action(last, segments.len()) {
            let date_seg = segments[segments.len() - 2];
            if looks_like_date(date_seg) {
                (last, &segments[..segments.len() - 2])
            } else {
                (last, channel_segments)
            }
        } else {
            (last, channel_segments)
        };

        if let Some(channel) = find_channel(&state.channels, channel_segments).cloned() {
            return match action {
                "today" => {
                    off_runtime(move || {
                        let date = latest_date(&channel);
                        let since =
                            Timestamp::now() - SignedDuration::from_hours(TODAY_CARRY_HOURS);
                        serve_log_page(&state, &channel, &date, Some(since))
                    })
                    .await
                }
                "latest" => serve_sse(state, &channel, addr, &headers)
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
                "embed" => {
                    let date = segments[segments.len() - 2].to_owned();
                    serve_embed(state, channel, date, search.url, addr).await
                }
                "fullmsg" => {
                    let date = segments[segments.len() - 2].to_owned();
                    serve_full_message(state, channel, date, search.url, search.nick, addr).await
                }
                date if looks_like_date(date) => {
                    let date = date.to_owned();
                    off_runtime(move || serve_log_page(&state, &channel, &date, None)).await
                }
                _ => (StatusCode::NOT_FOUND, "not found").into_response(),
            };
        }
    }

    if let Some(channel) = find_channel(&state.channels, &segments) {
        let bp = &state
            .config
            .base_path;
        let encoded = channel.url_path();
        return Redirect::temporary(&format!("{bp}/{encoded}/today")).into_response();
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

/// `YYYY-MM-DD/raw`, `/embed` or `/fullmsg` after a channel.
fn is_date_action(last: &str, len: usize) -> bool {
    matches!(last, "raw" | "embed" | "fullmsg") && len >= 2
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

/// The lines of the day file `path` with their UTC stamps.
fn stamped_lines(
    clock: &mut LogClock,
    path: &Path,
    format: LogFormat,
) -> io::Result<Vec<(LogLine, Option<Timestamp>)>> {
    let content = read_log_file(path)?;
    let parsed: Vec<_> = content
        .lines()
        .filter_map(|l| parse_line(l, format))
        .collect();
    let stamps = clock
        .stamp(path, format, &parsed)
        .unwrap_or_default();
    let mut stamps = stamps.into_iter();
    Ok(parsed
        .into_iter()
        .map(|line| {
            (
                line,
                stamps
                    .next()
                    .flatten(),
            )
        })
        .collect())
}

/// The lines of the day file `date` stamped at or after `since`.
fn carried_day(
    clock: &mut LogClock,
    channel: &crate::Channel,
    date: &str,
    since: Timestamp,
) -> Option<templates::CarriedDay> {
    let (path, format) = resolve_log_path(channel, date)?;
    let lines = match stamped_lines(clock, &path, format) {
        Ok(lines) => lines,
        Err(e) => {
            eprintln!("today: cannot read {} to carry over: {e}", path.display());
            return None;
        }
    };
    let lines: Vec<_> = lines
        .into_iter()
        .filter(|(_, at)| at.is_some_and(|at| at >= since))
        .collect();
    (!lines.is_empty()).then(|| templates::CarriedDay {
        date: date.to_owned(),
        lines,
        zone: clock
            .zone_name(format)
            .to_owned(),
    })
}

/// The page of one day file. `today_since` is set when served as /today,
/// which no cache may keep past the day: lines of the previous day file
/// stamped since then come first.
fn serve_log_page(
    state: &AppState,
    channel: &crate::Channel,
    date: &str,
    today_since: Option<Timestamp>,
) -> Response {
    let (path, format) = match resolve_log_path(channel, date) {
        Some(r) => r,
        None => {
            return (StatusCode::NOT_FOUND, format!("no log for {date}")).into_response();
        }
    };
    let mut clock = state.log_clock();
    let lines = match stamped_lines(&mut clock, &path, format) {
        Ok(lines) => lines,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read error: {e}"),
            )
                .into_response();
        }
    };

    let dates = channel_dates(channel);
    let idx = dates
        .iter()
        .position(|d| d == date);
    let prev = idx
        .and_then(|i| if i > 0 { dates.get(i - 1) } else { None })
        .map(|s| s.as_str());
    let follow = today_since.is_some();
    let carried = today_since
        .zip(prev)
        .and_then(|(since, prev)| carried_day(&mut clock, channel, prev, since));
    let next = idx
        .and_then(|i| dates.get(i + 1))
        .map(|s| s.as_str());
    // The newest file, not the UTC date: a ZNC day file follows log_timezone.
    let is_today = dates
        .last()
        .is_some_and(|d| d == date);

    let mut resp = templates::log_page(&templates::LogPageContext {
        title: &state
            .config
            .title,
        tree: &state.channels,
        channel,
        date,
        lines: &lines,
        zone: clock.zone_name(format),
        prev_date: prev,
        next_date: next,
        is_today,
        follow,
        carried: carried.as_ref(),
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
        presence: state
            .presence
            .is_some(),
        embeds: state
            .embeds
            .is_some(),
        full_message_lines: state
            .config
            .embeds
            .as_ref()
            .filter(|e| {
                state
                    .embeds
                    .is_some()
                    && !e
                        .full_message_hosts
                        .is_empty()
            })
            .map(|e| e.full_message_lines),
    })
    .into_response();
    // A cached /today from the day before would reload at every live line.
    let cc = if follow {
        "no-cache"
    } else if is_today {
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
        &mut state.log_clock(),
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
    let key = channel
        .path_segments
        .join("/");
    let snapshot = presence.channel(&key);
    let live = presence.live(&key);
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
    let respond = move || {
        presence_response(
            scanning,
            snapshot.as_deref(),
            live.as_deref(),
            if_none_match.as_deref(),
        )
    };
    if large {
        off_runtime(respond).await
    } else {
        respond()
    }
}

fn presence_response(
    scanning: bool,
    channel: Option<&ChannelPresence>,
    live: Option<&LiveChannel>,
    if_none_match: Option<&str>,
) -> Response {
    let body = match json_body(scanning, channel, live) {
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

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;
    use crate::test_support::{PEER, add_channel, app, app_state, stalled_channel, tempdir};
    use crate::{ChannelDir, ChannelNode};

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
    async fn bare_channel_redirects_to_today_with_or_without_trailing_slash() {
        for uri in ["/OFTC/%23bcache", "/OFTC/%23bcache/"] {
            let (status, location) = get_status(uri).await;
            assert_eq!(status, StatusCode::TEMPORARY_REDIRECT, "{uri}");
            assert_eq!(location.as_deref(), Some("/OFTC/%23bcache/today"), "{uri}");
        }
    }

    /// `#c` with two day files in ZNC format, the newest 2026-01-02.
    fn two_day_channel(root: &Path) -> axum::Router {
        std::fs::write(root.join("2026-01-01.log"), "[10:00:00] <a> day one\n").unwrap();
        std::fs::write(root.join("2026-01-02.log"), "[10:00:00] <a> day two\n").unwrap();
        let mut channels = ChannelNode::default();
        let dirs = vec![ChannelDir {
            path: root.to_owned(),
            format: LogFormat::Znc,
        }];
        add_channel(&mut channels, &["#c"], dirs);
        app(Arc::new(app_state(channels, "logs_dirs: []")), PEER)
    }

    #[tokio::test]
    async fn today_serves_the_newest_day_in_place_and_follows_it() {
        let root = tempdir();
        let app = two_day_channel(root.path());
        for uri in ["/%23c/today", "/%23c/today/"] {
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
            assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-cache", "{uri}");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(body.contains("day two"), "{uri}: {body}");
            assert!(!body.contains("day one"), "{uri}: {body}");
            assert!(body.contains(r#"data-date="2026-01-02""#), "{uri}");
            assert!(body.contains(r#"data-latest="/%23c/latest""#), "{uri}");
            assert!(body.contains("data-follow"), "{uri}");
            assert!(body.contains(r#"href="/%23c/2026-01-02#T100000""#), "{uri}");
        }
    }

    #[tokio::test]
    async fn today_carries_the_previous_day_lines_since_the_cutoff() {
        let root = tempdir();
        std::fs::write(
            root.path()
                .join("2026-01-01.log"),
            "[14:59:59] <a> too old\n[15:00:00] <a> carried over\n",
        )
        .unwrap();
        std::fs::write(
            root.path()
                .join("2026-01-02.log"),
            "[10:00:00] <a> day two\n",
        )
        .unwrap();
        let mut channels = ChannelNode::default();
        let dirs = vec![ChannelDir {
            path: root
                .path()
                .to_owned(),
            format: LogFormat::Znc,
        }];
        add_channel(&mut channels, &["#c"], dirs);
        let state = app_state(channels, "logs_dirs: []");
        let channel = find_channel(&state.channels, &["#c"]).unwrap();
        let page = |since: &str| {
            let since = since
                .parse()
                .unwrap();
            serve_log_page(&state, channel, "2026-01-02", Some(since))
        };
        async fn text(resp: Response) -> String {
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(body.to_vec()).unwrap()
        }

        let body = text(page("2026-01-01T15:00:00Z")).await;
        assert!(!body.contains("too old"), "{body}");
        let carried = body
            .find(r#"<div class="carried" data-date="2026-01-01">"#)
            .expect("carried section");
        let line = body
            .find(r#"href="/%23c/2026-01-01#T150000""#)
            .expect("carried line links to its own day");
        let separator = body
            .find(r#"<div class="day-separator"><a href="/%23c/2026-01-02">2026-01-02</a></div>"#)
            .expect("separator");
        let today = body
            .find("day two")
            .expect("today's line");
        assert!(
            carried < line && line < separator && separator < today,
            "{body}"
        );

        let body = text(page("2026-01-01T15:00:01Z")).await;
        assert!(!body.contains("carried"), "{body}");
        assert!(!body.contains("day-separator"), "{body}");
    }

    #[tokio::test]
    async fn only_the_newest_dated_page_is_live_and_neither_follows() {
        let root = tempdir();
        let app = two_day_channel(root.path());
        let newest = body_of(&app, "/%23c/2026-01-02").await;
        assert!(newest.contains("data-latest"), "{newest}");
        assert!(!newest.contains("data-follow"), "{newest}");
        let older = body_of(&app, "/%23c/2026-01-01").await;
        assert!(!older.contains("data-latest"), "{older}");
        assert!(!older.contains("data-follow"), "{older}");
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
            ("/static/icon-32.png", CSP),
            ("/static/apple-touch-icon.png", CSP),
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
            if uri.ends_with(".png") {
                assert_eq!(headers[header::CONTENT_TYPE], "image/png", "{uri}");
                let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
                assert!(body.starts_with(b"\x89PNG\r\n\x1a\n"), "{uri}");
                continue;
            }
            if uri.ends_with(".js") {
                continue;
            }
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(body.contains(&format!(r#"href="{}""#, *CSS_URL)), "{uri}");
            assert!(body.contains(&format!(r#"href="{}""#, *ICON_URL)), "{uri}");
            assert!(
                body.contains(&format!(r#"href="{}""#, *TOUCH_ICON_URL)),
                "{uri}"
            );
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

    async fn body_of(app: &axum::Router, uri: &str) -> String {
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
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn line_times_carry_their_utc_instant_and_keep_file_time_anchors() {
        let root = tempdir();
        let znc = root
            .path()
            .join("znc");
        let iso = root
            .path()
            .join("iso");
        std::fs::create_dir_all(&znc).unwrap();
        std::fs::create_dir_all(&iso).unwrap();
        // 01:xx repeats on 2026-11-01 in EST5EDT; 01:10 after 01:50 is the second pass.
        std::fs::write(
            znc.join("2026-11-01.log"),
            "[01:50:00] <a> first hello\n[01:10:00] <a> second hello\n",
        )
        .unwrap();
        std::fs::write(
            iso.join("2026-11-01.log"),
            "2026-11-01T23:59:59Z <b> iso hello\n",
        )
        .unwrap();
        let mut channels = ChannelNode::default();
        let dirs = |path, format| vec![ChannelDir { path, format }];
        add_channel(
            &mut channels,
            &["#z"],
            dirs(znc, crate::parser::LogFormat::Znc),
        );
        add_channel(
            &mut channels,
            &["#i"],
            dirs(iso, crate::parser::LogFormat::Iso8601),
        );
        let posix = "EST5EDT,M3.2.0,M11.1.0";
        let mut state = app_state(
            channels,
            &format!("{{logs_dirs: [], log_timezone: '{posix}'}}"),
        );
        state.log_tz = crate::resolve_log_timezone(posix).unwrap();
        let app = app(Arc::new(state), PEER);

        let page = body_of(&app, "/%23z/2026-11-01").await;
        for (hms, anchor, at) in [
            ("01:50:00", "T015000", "2026-11-01T05:50:00Z"),
            ("01:10:00", "T011000", "2026-11-01T06:10:00Z"),
        ] {
            let link = format!(
                r##"<a class="ts" href="/%23z/2026-11-01#{anchor}" data-at="{at}" title="file time {hms} {posix}">{hms}</a>"##
            );
            assert!(page.contains(&link), "{link} missing: {page}");
            assert!(page.contains(&format!(r#"id="{anchor}""#)), "{page}");
        }
        assert!(page.contains(r#"data-date="2026-11-01""#), "{page}");

        let page = body_of(&app, "/%23i/2026-11-01").await;
        let link = r##"<a class="ts" href="/%23i/2026-11-01#T235959" data-at="2026-11-01T23:59:59Z" title="file time 23:59:59 UTC">23:59:59</a>"##;
        assert!(page.contains(link), "{page}");

        let found = body_of(&app, "/%23z/search?q=second").await;
        let link = format!(
            r##"<a class="ts" href="/%23z/2026-11-01#T011000" data-at="2026-11-01T06:10:00Z" title="file time 01:10:00 {posix}">01:10:00</a>"##
        );
        assert!(found.contains(&link), "{link} missing: {found}");
        assert!(
            found.contains(r#"<div class="line" data-date="2026-11-01">"#),
            "{found}"
        );
    }

    #[tokio::test]
    async fn search_renders_events_as_the_day_page_does() {
        let root = tempdir();
        std::fs::write(
            root.path()
                .join("2026-01-01.log"),
            "[10:00:00] *** Joins: needle (~n@host)\n[10:01:00] *** needle is now known as pin\n",
        )
        .unwrap();
        let mut channels = ChannelNode::default();
        let dirs = vec![ChannelDir {
            path: root
                .path()
                .to_owned(),
            format: LogFormat::Znc,
        }];
        add_channel(&mut channels, &["#c"], dirs);
        let app = app(Arc::new(app_state(channels, "logs_dirs: []")), PEER);

        let day = body_of(&app, "/%23c/2026-01-01").await;
        let found = body_of(&app, "/%23c/search?q=needle").await;
        assert!(!found.contains(">event<"), "{found}");
        for anchor in ["T100000", "T100100"] {
            let start = format!(r#"<div class="line event" id="{anchor}">"#);
            let body = day
                .split_once(&start)
                .and_then(|(_, rest)| rest.split_once("</div>"))
                .unwrap_or_else(|| panic!("{start} missing: {day}"))
                .0;
            let ts = body
                .find(r#"<a class="ts""#)
                .unwrap();
            assert!(found.contains(&body[ts..]), "{body} missing: {found}");
            assert!(
                found.contains(r#"<div class="line event" data-date="2026-01-01">"#),
                "{found}"
            );
        }
    }

    #[tokio::test]
    async fn search_hides_events_behind_the_show_events_toggle() {
        let root = tempdir();
        std::fs::write(
            root.path()
                .join("2026-01-01.log"),
            "[10:00:00] *** Joins: needle (~n@host)\n[10:01:00] <a> hay\n",
        )
        .unwrap();
        let mut channels = ChannelNode::default();
        let dirs = vec![ChannelDir {
            path: root
                .path()
                .to_owned(),
            format: LogFormat::Znc,
        }];
        add_channel(&mut channels, &["#c"], dirs);
        let app = app(Arc::new(app_state(channels, "logs_dirs: []")), PEER);

        let note = "Only events matched";
        let found = body_of(&app, "/%23c/search?q=needle").await;
        assert!(
            found.contains(r#"<input id="toggle-events" type="checkbox">"#),
            "{found}"
        );
        assert!(
            found.contains(r#"<div id="log" class="hide-events">"#),
            "{found}"
        );
        assert!(found.contains(note), "{found}");
        let found = body_of(&app, "/%23c/search?q=hay").await;
        assert!(!found.contains(note), "{found}");
    }
}

#[cfg(test)]
mod limit_tests {

    use super::*;
    use crate::test_support::{PEER, app, app_state, channel_with_logs, get, stalled_channel};

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
