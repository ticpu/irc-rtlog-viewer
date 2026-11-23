//! The ask page, its SSE stream and the saved output pages.

use std::sync::Arc;

use std::net::IpAddr;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use cookie::{Cookie, SameSite};
use tokio_stream::StreamExt;

use super::cache_control;
use crate::AppState;
use crate::limits;
use crate::templates;

pub(super) fn serve_ask_page(
    state: &AppState,
    channel: &crate::Channel,
    headers: &HeaderMap,
) -> Response {
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

pub(super) async fn serve_ask_stream(
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

pub(super) async fn serve_ask_output(
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
    use std::path::Path;
    use std::sync::Arc;

    use axum::Router;
    use axum::http::header;
    use axum::routing::post;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::test_support::{PEER, app, app_state, channel_with_logs, get, tempdir};

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
}
