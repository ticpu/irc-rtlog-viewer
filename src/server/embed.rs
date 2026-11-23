//! The embed and full-message JSON endpoints and the checks that gate their remote fetches.

use std::sync::Arc;

use std::net::IpAddr;

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use super::cache_control;
use crate::AppState;
use crate::embed;
use crate::logfiles::{read_log_file, resolve_log_path};
use crate::parser::{LineKind, parse_line};
use crate::templates;

/// Whether `url` is, byte for byte, a link the day's page shows.
fn is_linked_on(channel: &crate::Channel, date: &str, url: &str) -> bool {
    let Some((path, format)) = resolve_log_path(channel, date) else {
        return false;
    };
    let Ok(content) = read_log_file(&path) else {
        return false;
    };
    content
        .lines()
        .filter_map(|l| parse_line(l, format))
        .any(|line| {
            templates::linked_text(&line.kind)
                .is_some_and(|text| templates::urls(text).any(|span| &text[span] == url))
        })
}

/// A slot and a rate token for one remote fetch, or the refusal to send.
fn fetch_slot(
    state: &AppState,
    addr: IpAddr,
) -> Result<tokio::sync::OwnedSemaphorePermit, Box<Response>> {
    // The slot is taken before the rate token so a busy refusal costs nothing.
    let slot = Arc::clone(
        &state
            .limits
            .embed_slots,
    )
    .try_acquire_owned()
    .map_err(|_| {
        Box::new(
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "5".to_owned())],
                "link fetches are busy",
            )
                .into_response(),
        )
    })?;
    state
        .limits
        .charge_embed(addr)
        .map_err(|wait| {
            let secs = wait
                .as_secs()
                .max(1)
                .to_string();
            Box::new(
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, secs)],
                    "too many link fetches",
                )
                    .into_response(),
            )
        })?;
    Ok(slot)
}

/// Whether `nick` posted, in the day's log, a long-message notice linking `url`.
/// Several nicks may link one upload, so the line is named by both.
fn has_full_message_notice(channel: &crate::Channel, date: &str, nick: &str, url: &str) -> bool {
    let Some((path, format)) = resolve_log_path(channel, date) else {
        return false;
    };
    let Ok(content) = read_log_file(&path) else {
        return false;
    };
    content
        .lines()
        .filter_map(|l| parse_line(l, format))
        .any(|line| match &line.kind {
            LineKind::Message { nick: n, text } | LineKind::Action { nick: n, text } => {
                n == nick && templates::full_message_url(text) == Some(url)
            }
            _ => false,
        })
}

/// The bridged long message `nick`'s line of the day's log links, as JSON:
/// its text when the attachment is named after that nick, else `{}`.
pub(super) async fn serve_full_message(
    state: Arc<AppState>,
    channel: crate::Channel,
    date: String,
    url: Option<String>,
    nick: Option<String>,
    addr: IpAddr,
) -> Response {
    let Some(embeds) = &state.embeds else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let (Some(url), Some(nick)) = (url, nick) else {
        return (StatusCode::BAD_REQUEST, "missing url or nick").into_response();
    };
    let Some(parsed) = url::Url::parse(&url)
        .ok()
        .filter(|u| embeds.expands(u))
    else {
        return (StatusCode::NOT_FOUND, "not an expandable link").into_response();
    };
    let (check_url, check_nick) = (url.clone(), nick.clone());
    match tokio::task::spawn_blocking(move || {
        has_full_message_notice(&channel, &date, &check_nick, &check_url)
    })
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::NOT_FOUND,
                "no long-message notice by this nick for this link",
            )
                .into_response();
        }
        Err(e) => {
            eprintln!("embeds: the log check failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    }
    let _slot = if embeds
        .is_cached(&parsed, embed::Kind::FullMessage)
        .await
    {
        None
    } else {
        match fetch_slot(&state, addr) {
            Ok(slot) => Some(slot),
            Err(refused) => return *refused,
        }
    };
    #[derive(Serialize)]
    struct Body<'a> {
        text: &'a str,
        truncated: bool,
        host: &'a str,
    }
    let message = embeds
        .full_message(&parsed, &nick)
        .await;
    let body = match &message {
        Some(m) => serde_json::to_string(&Body {
            text: &m.text,
            truncated: m.truncated,
            host: parsed
                .host_str()
                .unwrap_or_default(),
        }),
        None => Ok("{}".to_owned()),
    };
    match body {
        Ok(body) => (
            [
                (header::CONTENT_TYPE, "application/json"),
                cache_control("private, max-age=3600"),
            ],
            body,
        )
            .into_response(),
        Err(e) => {
            eprintln!("embeds: cannot serialize a long message: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// The OpenGraph card of a link in the day's log, as JSON.
pub(super) async fn serve_embed(
    state: Arc<AppState>,
    channel: crate::Channel,
    date: String,
    url: Option<String>,
    addr: IpAddr,
) -> Response {
    let Some(embeds) = &state.embeds else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Some(url) = url else {
        return (StatusCode::BAD_REQUEST, "missing url").into_response();
    };
    let check = url.clone();
    match tokio::task::spawn_blocking(move || is_linked_on(&channel, &date, &check)).await {
        Ok(true) => {}
        Ok(false) => {
            return (StatusCode::NOT_FOUND, "not a link in this day's log").into_response();
        }
        Err(e) => {
            eprintln!("embeds: the log check failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    }
    let card = match url::Url::parse(&url) {
        Err(_) => embed::Card::default(),
        Ok(parsed)
            if embeds
                .is_cached(&parsed, embed::Kind::Card)
                .await =>
        {
            embeds
                .card(&parsed)
                .await
        }
        Ok(parsed) => {
            let _slot = match fetch_slot(&state, addr) {
                Ok(slot) => slot,
                Err(refused) => return *refused,
            };
            embeds
                .card(&parsed)
                .await
        }
    };
    match serde_json::to_string(&card) {
        Ok(body) => (
            [
                (header::CONTENT_TYPE, "application/json"),
                cache_control("private, max-age=3600"),
            ],
            body,
        )
            .into_response(),
        Err(e) => {
            eprintln!("embeds: cannot serialize a card: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}
