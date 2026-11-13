//! OpenGraph cards and Matrix bridge long messages for links in the logs:
//! fetched server-side on a reader's request, cached on disk, and shown by
//! app.js.
//!
//! # Only URLs a reader can click
//!
//! The card endpoint serves only a URL that [`crate::templates::urls`]
//! finds in a message, action or topic of that day's log, the same scan
//! that makes the page's links, compared byte for byte with the href. The
//! long-message endpoint serves only the URL of a line ending in
//! `(full message at <URL>)` ([`crate::templates::full_message_url`]) on a
//! listed host. The viewer therefore fetches nothing a log line did not
//! already link.
//!
//! # The fetch guard
//!
//! Every fetch goes through `get`:
//!
//! - Only http and https, on ports 80 and 443. The host is read as a
//!   `url::Host`, never as a string.
//! - A hostname is resolved by [`Guard`] as the client's resolver, which
//!   drops every address that is not global unicast: loopback, private,
//!   shared, link-local, unique local, documentation, multicast, reserved,
//!   the IPv4 addresses embedded in v4-mapped, NAT64 and 6to4 addresses,
//!   the host's own interface addresses and the viewer's bind address.
//!   hyper connects to the addresses the resolver returned, so a second
//!   lookup cannot rebind the name.
//! - hyper never resolves an IP literal, so literals are checked by the same
//!   rules before the request and in the redirect policy, which also caps
//!   redirects at 3.
//! - With an HTTP(S) proxy in the environment, which reqwest honours,
//!   hostnames resolve at the proxy and this resolver never sees them;
//!   literals and redirect targets are still checked. The deployment is
//!   expected to run without one or to trust its proxy's own policy.
//! - One media type per fetch (text/html for cards, text/plain for long
//!   messages), a byte cap, and 5 s for the whole fetch.
//!
//! # Cards
//!
//! The title, description and site name come from og: properties, with
//! `<title>` and `meta name=description` as fallbacks, entity-decoded,
//! whitespace-collapsed and truncated; an empty property does not hide its
//! fallback. og:image is kept only when, resolved against the final URL,
//! it is https on a named host. app.js turns every field into text nodes
//! and the image into an img with no referrer.
//!
//! # Long messages
//!
//! Anyone can link any upload, so a long message replaces its notice only
//! when the attachment's Content-Disposition filename starts with the
//! line's nick and `_`, as the bridge names them. The text is cached by URL
//! alone, so the nick is checked on every serve, not only at fetch time.
//!
//! # Cache
//!
//! Each result, a failure included, is one JSON file
//! `<cache_dir>/<host>/<sha256 of the URL>.json`, written by rename, so one
//! site's entries can be removed as a directory. Cards are reused for a
//! day, long messages for good (they never change), failures for a day.
//! Nothing deletes entries. A cache write that fails is logged and the
//! result still served.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::redirect;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use url::{Host, Url};

const MAX_BODY: usize = 512 * 1024;
#[cfg(not(test))]
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Short enough that the stalled-site test does not wait out the real limit.
#[cfg(test)]
const FETCH_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_REDIRECTS: usize = 3;
const CACHE_FOR: SignedDuration = SignedDuration::from_hours(24);

/// What a card shows; every field is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

/// Why no card could be made for a URL.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("only http and https on ports 80 and 443 are fetched")]
    Scheme,
    #[error("{0} is not a public address")]
    Address(IpAddr),
    #[error("the request failed")]
    Request(#[source] reqwest::Error),
    #[error("the site answered {0}")]
    Status(reqwest::StatusCode),
    #[error("expected {want}, got {got:?}")]
    WrongType { want: String, got: String },
    #[error("no answer within {} s", FETCH_TIMEOUT.as_secs())]
    Timeout,
}

/// Whether `ip` is a global unicast address another host could serve from.
pub fn is_public(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..128).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn embedded_v4(ip: Ipv6Addr, at: usize) -> Ipv4Addr {
    let o = ip.octets();
    Ipv4Addr::new(o[at], o[at + 1], o[at + 2], o[at + 3])
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // Only 2000::/3 is global unicast; this also rules out ::, ::1, ULA, link-local and multicast.
    if s[0] & 0xe000 != 0x2000 {
        return s[0] == 0x0064
            && s[1] == 0xff9b
            && s[2..6] == [0; 4]
            && is_public_v4(embedded_v4(ip, 12));
    }
    match (s[0], s[1]) {
        (0x2002, _) => is_public_v4(embedded_v4(ip, 2)),
        // Teredo, ORCHID and the benchmarking and documentation ranges.
        (0x2001, 0x0000) | (0x2001, 0x0db8) => false,
        (0x2001, b) if (0x0010..0x0030).contains(&b) || b == 0x0002 => false,
        _ => true,
    }
}

/// The address rules, shared by the resolver and the literal checks.
#[derive(Debug, Clone)]
pub struct Guard {
    /// The host's own addresses and the viewer's bind address.
    own: Arc<HashSet<IpAddr>>,
    /// Lets tests fetch from a loopback server on any port.
    #[cfg(test)]
    open: bool,
}

impl Guard {
    /// The rules for this host, `bind` being the viewer's listen address.
    pub fn new(bind: Option<IpAddr>) -> Self {
        let mut own: HashSet<IpAddr> = match if_addrs::get_if_addrs() {
            Ok(addrs) => addrs
                .into_iter()
                .map(|i| {
                    i.ip()
                        .to_canonical()
                })
                .collect(),
            Err(e) => {
                eprintln!(
                    "embeds: cannot list this host's addresses, only the address rules apply: {e}"
                );
                HashSet::new()
            }
        };
        own.extend(bind.map(|b| b.to_canonical()));
        Self {
            own: Arc::new(own),
            #[cfg(test)]
            open: false,
        }
    }

    fn allows(&self, ip: IpAddr) -> bool {
        #[cfg(test)]
        if self.open {
            return ip.is_loopback();
        }
        is_public(ip)
            && !self
                .own
                .contains(&ip.to_canonical())
    }

    /// Checks the scheme, the port and an IP-literal host of `url`.
    pub fn check_url(&self, url: &Url) -> Result<(), FetchError> {
        let scheme_ok = matches!(url.scheme(), "http" | "https");
        let port_ok = matches!(url.port_or_known_default(), Some(80 | 443));
        #[cfg(test)]
        let port_ok = port_ok || self.open;
        if !scheme_ok || !port_ok {
            return Err(FetchError::Scheme);
        }
        let ip = match url.host() {
            Some(Host::Ipv4(v4)) => IpAddr::V4(v4),
            Some(Host::Ipv6(v6)) => IpAddr::V6(v6),
            Some(Host::Domain(_)) => return Ok(()),
            None => return Err(FetchError::Scheme),
        };
        if self.allows(ip) {
            Ok(())
        } else {
            Err(FetchError::Address(ip))
        }
    }
}

impl Resolve for Guard {
    fn resolve(&self, name: Name) -> Resolving {
        let guard = self.clone();
        Box::pin(async move {
            let host = name
                .as_str()
                .to_owned();
            let found: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .collect();
            let allowed: Vec<SocketAddr> = found
                .iter()
                .copied()
                .filter(|a| guard.allows(a.ip()))
                .collect();
            if allowed.is_empty() {
                return Err(format!(
                    "{host} resolves to no public address ({} found)",
                    found.len()
                )
                .into());
            }
            let addrs: Addrs = Box::new(allowed.into_iter());
            Ok(addrs)
        })
    }
}

/// What a cache entry holds, which decides how long a success is kept.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    /// An OpenGraph card, kept a day.
    Card,
    /// A bridged long message, which never changes, kept for good.
    FullMessage,
}

impl Kind {
    fn success_for(self) -> Option<SignedDuration> {
        match self {
            Kind::Card => Some(CACHE_FOR),
            Kind::FullMessage => None,
        }
    }
}

/// The fetcher the endpoints use: cache directory, guard, client and the
/// hosts whose long-message links are expanded.
pub struct Embeds {
    dir: PathBuf,
    guard: Guard,
    client: reqwest::Client,
    full_message_hosts: Vec<String>,
}

impl Embeds {
    /// A fetcher caching in `dir`, guarding against `bind` and this host's
    /// addresses, expanding long messages served by `full_message_hosts`.
    pub fn new(
        dir: PathBuf,
        bind: Option<IpAddr>,
        full_message_hosts: Vec<String>,
    ) -> reqwest::Result<Self> {
        let guard = Guard::new(bind);
        let client = client(&guard)?;
        Ok(Self {
            dir,
            guard,
            client,
            full_message_hosts,
        })
    }

    /// The card for `url`, cached or fetched; empty when there is none.
    pub async fn card(&self, url: &Url) -> Card {
        cached_or_fetch(
            &self.dir,
            url,
            Kind::Card,
            fetch(&self.client, &self.guard, url),
        )
        .await
        .0
        .unwrap_or_default()
    }

    /// Whether `url` is a bridged long message this fetcher may expand:
    /// https on a listed host, under a `media` path segment.
    pub fn expands(&self, url: &Url) -> bool {
        let secure = url.scheme() == "https";
        #[cfg(test)]
        let secure = secure
            || self
                .guard
                .open;
        let listed = match url.host() {
            Some(Host::Domain(host)) => self
                .full_message_hosts
                .iter()
                .any(|h| h.eq_ignore_ascii_case(host)),
            #[cfg(test)]
            Some(_)
                if self
                    .guard
                    .open =>
            {
                true
            }
            _ => false,
        };
        secure
            && listed
            && url
                .path_segments()
                .is_some_and(|mut s| s.any(|seg| seg == "media"))
    }

    /// The long message at `url` when its attachment is named after `nick`.
    /// The text is cached by URL alone, so the nick is checked on every
    /// serve; a mismatch is logged only when the message was just fetched.
    pub async fn full_message(&self, url: &Url, nick: &str) -> Option<FullMessage> {
        let (message, source) = cached_or_fetch(
            &self.dir,
            url,
            Kind::FullMessage,
            fetch_full_message(&self.client, &self.guard, url),
        )
        .await;
        let message = message?;
        if message.sent_by(nick) {
            return Some(message);
        }
        if source == Source::Fetch {
            eprintln!(
                "embeds: not expanding {url} for {nick}: the attachment is named {:?}",
                message.filename
            );
        }
        None
    }

    /// Whether a fresh entry for `url` exists, so serving it costs no fetch.
    pub async fn is_cached(&self, url: &Url, kind: Kind) -> bool {
        cached::<serde::de::IgnoredAny>(&cache_path(&self.dir, url, kind), kind.success_for())
            .await
            .is_some()
    }
}

/// The client for card fetches: the guard as resolver and redirect policy.
pub fn client(guard: &Guard) -> reqwest::Result<reqwest::Client> {
    let policy_guard = guard.clone();
    reqwest::Client::builder()
        .dns_resolver(Arc::new(guard.clone()))
        .redirect(redirect::Policy::custom(move |attempt| {
            if attempt
                .previous()
                .len()
                >= MAX_REDIRECTS
            {
                return attempt.error(format!("more than {MAX_REDIRECTS} redirects"));
            }
            match policy_guard.check_url(attempt.url()) {
                Ok(()) => attempt.follow(),
                Err(e) => attempt.error(e),
            }
        }))
        .user_agent("irc-log-viewer link preview")
        .build()
}

/// Fetches `url` and reads its card.
pub async fn fetch(client: &reqwest::Client, guard: &Guard, url: &Url) -> Result<Card, FetchError> {
    let page = get(client, guard, url, "text/html", MAX_BODY).await?;
    Ok(read_card(&String::from_utf8_lossy(&page.body), &page.url))
}

/// What one guarded GET returned.
struct Fetched {
    /// The URL after redirects.
    url: Url,
    headers: reqwest::header::HeaderMap,
    /// At most the cap's worth of the body.
    body: Vec<u8>,
    /// Whether the body went past the cap.
    truncated: bool,
}

/// GETs `url` through the guard, accepting only `media_type` and reading at
/// most `cap` bytes, all within [`FETCH_TIMEOUT`]. Every fetch goes through here.
async fn get(
    client: &reqwest::Client,
    guard: &Guard,
    url: &Url,
    media_type: &str,
    cap: usize,
) -> Result<Fetched, FetchError> {
    guard.check_url(url)?;
    tokio::time::timeout(FETCH_TIMEOUT, get_inner(client, url, media_type, cap))
        .await
        .unwrap_or(Err(FetchError::Timeout))
}

async fn get_inner(
    client: &reqwest::Client,
    url: &Url,
    media_type: &str,
    cap: usize,
) -> Result<Fetched, FetchError> {
    let mut resp = client
        .get(url.clone())
        .header(ACCEPT, format!("{media_type}; charset=utf-8"))
        .send()
        .await
        .map_err(FetchError::Request)?;
    if !resp
        .status()
        .is_success()
    {
        return Err(FetchError::Status(resp.status()));
    }
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| {
            v.to_str()
                .ok()
        })
        .unwrap_or("")
        .to_owned();
    if !content_type
        .to_ascii_lowercase()
        .starts_with(media_type)
    {
        return Err(FetchError::WrongType {
            want: media_type.to_owned(),
            got: content_type,
        });
    }
    let final_url = resp
        .url()
        .clone();
    let headers = resp
        .headers()
        .clone();
    let mut body = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(FetchError::Request)?
    {
        let room = cap - body.len();
        body.extend_from_slice(
            &chunk[..chunk
                .len()
                .min(room)],
        );
        if chunk.len() > room {
            truncated = true;
            break;
        }
    }
    Ok(Fetched {
        url: final_url,
        headers,
        body,
        truncated,
    })
}

fn clean(text: &str, max_chars: usize) -> Option<String> {
    let decoded = html_escape::decode_html_entities(text);
    let collapsed = decoded
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        return None;
    }
    Some(
        match collapsed
            .char_indices()
            .nth(max_chars)
        {
            Some((cut, _)) => format!("{}…", &collapsed[..cut]),
            None => collapsed,
        },
    )
}

/// The card in `html`, served from `base`.
pub fn read_card(html: &str, base: &Url) -> Card {
    let Ok(dom) = tl::parse(html, tl::ParserOptions::default()) else {
        return Card::default();
    };
    let parser = dom.parser();
    let mut og = std::collections::HashMap::new();
    let mut title_tag = None;
    for node in dom.nodes() {
        let Some(tag) = node.as_tag() else { continue };
        let name = tag
            .name()
            .as_utf8_str()
            .to_ascii_lowercase();
        if name == "title" && title_tag.is_none() {
            title_tag = Some(
                tag.inner_text(parser)
                    .into_owned(),
            );
        } else if name == "meta" {
            let attrs = tag.attributes();
            let key = attrs
                .get("property")
                .flatten()
                .or_else(|| {
                    attrs
                        .get("name")
                        .flatten()
                })
                .map(|b| {
                    b.as_utf8_str()
                        .to_ascii_lowercase()
                });
            let content = attrs
                .get("content")
                .flatten()
                .map(|b| {
                    b.as_utf8_str()
                        .into_owned()
                });
            if let (Some(key), Some(content)) = (key, content) {
                og.entry(key)
                    .or_insert(content);
            }
        }
    }
    // The first of `keys` with text; an empty og:title must not hide <title>.
    let field = |keys: &[&str], max_chars| {
        keys.iter()
            .find_map(|k| {
                og.get(*k)
                    .and_then(|v| clean(v, max_chars))
            })
    };
    let image = field(&["og:image", "og:image:url", "og:image:secure_url"], 8192)
        .and_then(|src| {
            base.join(&src)
                .ok()
        })
        .filter(|u| u.scheme() == "https" && matches!(u.host(), Some(Host::Domain(_))))
        .map(String::from);
    Card {
        title: field(&["og:title", "twitter:title"], 300)
            .or_else(|| title_tag.and_then(|t| clean(&t, 300))),
        description: field(
            &["og:description", "twitter:description", "description"],
            500,
        ),
        site_name: field(&["og:site_name"], 100),
        image,
    }
}

/// One cached result: the value, or why it could not be had.
#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry<T> {
    fetched_at: Timestamp,
    #[serde(default = "Option::default", skip_serializing_if = "Option::is_none")]
    value: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl<T> CacheEntry<T> {
    /// A success is kept for `success_for`, forever when `None`; a failure for a day.
    fn is_fresh(&self, success_for: Option<SignedDuration>) -> bool {
        let keep = match self.value {
            Some(_) => success_for,
            None => Some(CACHE_FOR),
        };
        keep.is_none_or(|keep| {
            self.fetched_at
                .checked_add(keep)
                .is_ok_and(|until| until > Timestamp::now())
        })
    }
}

/// The entry at `path` when it parses and is still fresh.
async fn cached<T: DeserializeOwned>(
    path: &Path,
    success_for: Option<SignedDuration>,
) -> Option<CacheEntry<T>> {
    let bytes = tokio::fs::read(path)
        .await
        .ok()?;
    serde_json::from_slice::<CacheEntry<T>>(&bytes)
        .ok()
        .filter(|e| e.is_fresh(success_for))
}

/// `<dir>/<host>/<sha256 of the URL>.<kind>.json`, so one site's entries are
/// one directory and a card never answers for a long message of the same URL.
fn cache_path(dir: &Path, url: &Url, kind: Kind) -> PathBuf {
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        url.as_str()
            .as_bytes(),
    );
    let mut name = String::new();
    for b in digest.as_ref() {
        // Writing to a String cannot fail.
        let _ = write!(name, "{b:02x}");
    }
    let host = url
        .host_str()
        .unwrap_or("_")
        .to_ascii_lowercase();
    let kind = match kind {
        Kind::Card => "card",
        Kind::FullMessage => "message",
    };
    dir.join(host)
        .join(format!("{name}.{kind}.json"))
}

/// Where a value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Cache,
    Fetch,
}

/// The value for `url` from the cache when fresh, else from `fetch`, cached
/// either way; `None` when the fetch failed, now or within the last day.
async fn cached_or_fetch<T, F>(dir: &Path, url: &Url, kind: Kind, fetch: F) -> (Option<T>, Source)
where
    T: Serialize + DeserializeOwned,
    F: Future<Output = Result<T, FetchError>>,
{
    let path = cache_path(dir, url, kind);
    if let Some(entry) = cached::<T>(&path, kind.success_for()).await {
        return (entry.value, Source::Cache);
    }
    let entry = match fetch.await {
        Ok(value) => CacheEntry {
            fetched_at: Timestamp::now(),
            value: Some(value),
            error: None,
        },
        Err(e) => CacheEntry {
            fetched_at: Timestamp::now(),
            value: None,
            error: Some(crate::znc::error_chain(&e)),
        },
    };
    match serde_json::to_vec(&entry) {
        Ok(bytes) => {
            let shown = path
                .display()
                .to_string();
            let written = tokio::task::spawn_blocking(move || write_entry(&path, &bytes)).await;
            match written {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("embeds: cannot write cache entry {shown}: {e}"),
                Err(e) => eprintln!("embeds: the writer of cache entry {shown} failed: {e}"),
            }
        }
        Err(e) => eprintln!("embeds: cannot serialize the cache entry for {url}: {e}"),
    }
    (entry.value, Source::Fetch)
}

/// Writes `bytes` to `path` through a temporary file in its directory,
/// creating the per-host directory on its first entry.
fn write_entry(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, bytes)?;
    tmp.persist(path)
        .map_err(|e| e.error)?;
    Ok(())
}

/// The longest full message kept; the rest is cut and marked truncated.
const MAX_FULL_MESSAGE: usize = 64 * 1024;

/// A Matrix bridge's long message, as its media server serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullMessage {
    pub text: String,
    /// Whether the text was cut at [`MAX_FULL_MESSAGE`].
    pub truncated: bool,
    /// The attachment's filename, which the bridge sets to `<nick>_<time>.txt`.
    pub filename: String,
}

impl FullMessage {
    /// Whether the attachment is named after `nick`, as the bridge names the
    /// ones it uploads. Anyone can link any upload, so this is what ties
    /// the text to the line it replaces.
    pub fn sent_by(&self, nick: &str) -> bool {
        crate::presence::casemap(&self.filename)
            .strip_prefix(&crate::presence::casemap(nick))
            .is_some_and(|rest| rest.starts_with('_'))
    }
}

/// The filename of a Content-Disposition header: `filename*=utf-8''…`
/// percent-decoded, else a quoted or bare `filename=`.
fn attachment_filename(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let value = headers
        .get(reqwest::header::CONTENT_DISPOSITION)?
        .to_str()
        .ok()?;
    let params: Vec<(&str, &str)> = value
        .split(';')
        .skip(1)
        .filter_map(|p| {
            p.trim()
                .split_once('=')
        })
        .collect();
    let extended = params
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("filename*"))
        .and_then(|(_, v)| {
            let (charset, rest) = v.split_once('\'')?;
            let (_, encoded) = rest.split_once('\'')?;
            charset
                .eq_ignore_ascii_case("utf-8")
                .then(|| {
                    percent_encoding::percent_decode_str(encoded)
                        .decode_utf8_lossy()
                        .into_owned()
                })
        });
    extended.or_else(|| {
        params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("filename"))
            .map(|(_, v)| {
                v.trim_matches('"')
                    .to_owned()
            })
    })
}

async fn fetch_full_message(
    client: &reqwest::Client,
    guard: &Guard,
    url: &Url,
) -> Result<FullMessage, FetchError> {
    let got = get(client, guard, url, "text/plain", MAX_FULL_MESSAGE).await?;
    Ok(FullMessage {
        text: String::from_utf8_lossy(&got.body).into_owned(),
        truncated: got.truncated,
        filename: attachment_filename(&got.headers).unwrap_or_default(),
    })
}

#[cfg(test)]
impl Embeds {
    /// A fetcher whose guard admits loopback servers on any port, for tests.
    pub fn open_for_test(dir: PathBuf) -> Self {
        crate::install_crypto_provider();
        let guard = Guard {
            own: Arc::new(HashSet::new()),
            open: true,
        };
        let client = client(&guard).unwrap();
        Self {
            dir,
            guard,
            client,
            full_message_hosts: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::test_support::tempdir;

    fn ip(s: &str) -> IpAddr {
        s.parse()
            .unwrap()
    }

    #[test]
    fn only_global_unicast_addresses_are_public() {
        for public in [
            "1.1.1.1",
            "8.8.4.4",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
            "64:ff9b::101:101",
            "2002:101:101::1",
        ] {
            assert!(is_public(ip(public)), "{public}");
        }
        for private in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.8",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "2002:7f00:1::1",
            "2002:c0a8:101::1",
            "2001::1",
            "2001:db8::1",
            "2001:10::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "100::1",
        ] {
            assert!(!is_public(ip(private)), "{private}");
        }
    }

    fn guard(own: &[&str]) -> Guard {
        Guard {
            own: Arc::new(
                own.iter()
                    .map(|s| ip(s))
                    .collect(),
            ),
            open: false,
        }
    }

    #[test]
    fn urls_are_checked_for_scheme_port_and_literal_hosts() {
        let g = guard(&["93.184.215.14"]);
        let check = |u: &str| {
            g.check_url(&Url::parse(u).unwrap())
                .map_err(|e| e.to_string())
        };
        assert!(check("https://example.org/").is_ok());
        assert!(check("http://example.org:80/x").is_ok());
        assert!(check("https://example.org:443/x").is_ok());
        assert!(check("https://1.1.1.1/").is_ok());
        for refused in [
            "ftp://example.org/",
            "https://example.org:8443/",
            "http://10.0.0.1/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[64:ff9b::a00:1]/",
            "http://0x7f.1/",
            "http://2130706433/",
            "https://93.184.215.14/",
        ] {
            assert!(check(refused).is_err(), "{refused} was allowed");
        }
    }

    #[tokio::test]
    async fn the_resolver_drops_non_public_addresses() {
        let err = guard(&[])
            .resolve(
                "localhost"
                    .parse()
                    .unwrap(),
            )
            .await
            .err()
            .expect("localhost resolved to an allowed address");
        assert!(
            err.to_string()
                .contains("no public address"),
            "{err}"
        );
    }

    /// Serves `responses` in order, one per connection, counting requests.
    async fn http_server(responses: Vec<Vec<u8>>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let addr = listener
            .local_addr()
            .unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut conn, _)) = listener
                    .accept()
                    .await
                else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let mut seen = Vec::new();
                while !seen
                    .windows(4)
                    .any(|w| w == b"\r\n\r\n")
                {
                    match conn
                        .read(&mut buf)
                        .await
                    {
                        Ok(0) | Err(_) => break,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                    }
                }
                if response.is_empty() {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                let _ = conn
                    .write_all(&response)
                    .await;
                let _ = conn
                    .shutdown()
                    .await;
            }
        });
        (format!("http://{addr}"), hits)
    }

    fn html_response(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    const PAGE: &str = r#"<html><head><title>Fallback</title>
<meta property="og:title" content="A &amp; B">
<meta property="og:site_name" content="Example">
<meta name="description" content="  spaced
   out  ">
<meta property="og:image" content="https://cdn.example.org/i.png">
</head><body>hi</body></html>"#;

    #[tokio::test]
    async fn a_page_gives_its_card_and_the_cache_answers_the_second_time() {
        let dir = tempdir();
        let (base, hits) = http_server(vec![html_response(PAGE), html_response(PAGE)]).await;
        let embeds = Embeds::open_for_test(
            dir.path()
                .to_owned(),
        );
        let url = Url::parse(&format!("{base}/page")).unwrap();
        let card = embeds
            .card(&url)
            .await;
        assert_eq!(
            card,
            Card {
                title: Some("A & B".into()),
                description: Some("spaced out".into()),
                site_name: Some("Example".into()),
                image: Some("https://cdn.example.org/i.png".into()),
            }
        );
        assert!(
            embeds
                .is_cached(&url, Kind::Card)
                .await
        );
        assert_eq!(
            embeds
                .card(&url)
                .await,
            card
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failures_are_cached_and_name_their_cause() {
        let dir = tempdir();
        let redirect = b"HTTP/1.1 302 Found\r\nlocation: http://10.0.0.1/x\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec();
        let image = b"HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: 1\r\nconnection: close\r\n\r\nx".to_vec();
        let (base, hits) = http_server(vec![redirect, image]).await;
        let embeds = Embeds::open_for_test(
            dir.path()
                .to_owned(),
        );

        let to_private = Url::parse(&format!("{base}/r")).unwrap();
        let err = fetch(&embeds.client, &embeds.guard, &to_private)
            .await
            .unwrap_err();
        assert!(
            error_text(&err).contains("10.0.0.1 is not a public address"),
            "{}",
            error_text(&err)
        );

        let png = Url::parse(&format!("{base}/i.png")).unwrap();
        assert_eq!(
            embeds
                .card(&png)
                .await,
            Card::default()
        );
        assert!(
            embeds
                .is_cached(&png, Kind::Card)
                .await,
            "a failure is cached too"
        );
        assert_eq!(
            embeds
                .card(&png)
                .await,
            Card::default()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    fn error_text(e: &FetchError) -> String {
        crate::znc::error_chain(e)
    }

    #[tokio::test]
    async fn the_body_is_read_up_to_its_cap() {
        let dir = tempdir();
        let late = format!(
            "<html><head><title>{}</title><meta property=\"og:title\" content=\"too late\"></head></html>",
            "x".repeat(MAX_BODY)
        );
        let (base, _) = http_server(vec![html_response(&late)]).await;
        let embeds = Embeds::open_for_test(
            dir.path()
                .to_owned(),
        );
        let card = fetch(&embeds.client, &embeds.guard, &Url::parse(&base).unwrap())
            .await
            .unwrap();
        assert_ne!(
            card.title
                .as_deref(),
            Some("too late")
        );
    }

    #[tokio::test]
    async fn a_stalled_site_times_out() {
        let dir = tempdir();
        let (base, _) = http_server(vec![Vec::new()]).await;
        let embeds = Embeds::open_for_test(
            dir.path()
                .to_owned(),
        );
        let err = fetch(&embeds.client, &embeds.guard, &Url::parse(&base).unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, FetchError::Timeout), "{err:?}");
    }

    #[test]
    fn cards_keep_only_https_images_on_named_hosts() {
        let base = Url::parse("https://example.org/a/b").unwrap();
        let image = |src: &str| {
            read_card(
                &format!(r#"<meta property="og:image" content="{src}">"#),
                &base,
            )
            .image
        };
        assert_eq!(
            image("c.png").as_deref(),
            Some("https://example.org/a/c.png")
        );
        assert_eq!(image("http://example.org/c.png"), None);
        assert_eq!(image("https://1.2.3.4/c.png"), None);
        assert_eq!(image("javascript:alert(1)"), None);
        // rust-lang.org sends an empty og:title; the <title> still counts.
        let empty = read_card(
            r#"<title> The
  Title </title><meta property="og:title" content=""><meta name="twitter:title" content="">"#,
            &base,
        );
        assert_eq!(
            empty
                .title
                .as_deref(),
            Some("The Title")
        );
        let long = read_card(
            &format!(
                r#"<meta property="og:description" content="{}">"#,
                "é".repeat(600)
            ),
            &base,
        );
        assert_eq!(
            long.description
                .unwrap()
                .chars()
                .count(),
            501
        );
    }

    async fn get(app: &axum::Router, uri: &str) -> (axum::http::StatusCode, String) {
        use tower::ServiceExt;
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::get(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn the_endpoint_serves_only_links_of_that_days_log() {
        use crate::parser::LogFormat;
        use crate::test_support::{add_channel, app, app_state};
        use crate::{ChannelDir, ChannelNode};

        let root = tempdir();
        let (base, hits) = http_server(vec![html_response(PAGE)]).await;
        let logs = root
            .path()
            .join("#c");
        std::fs::create_dir(&logs).unwrap();
        std::fs::write(
            logs.join("2026-01-01.log"),
            format!("[10:00:00] <a> see {base}/page and https://Example.org/a%7e\n"),
        )
        .unwrap();
        let cache = root
            .path()
            .join("cache");
        std::fs::create_dir(&cache).unwrap();
        let mut channels = ChannelNode::default();
        let dirs = vec![ChannelDir {
            path: logs,
            format: LogFormat::Znc,
        }];
        add_channel(&mut channels, &["#c"], dirs);
        let mut state = app_state(channels, "logs_dirs: []");
        state.embeds = Some(Embeds::open_for_test(cache));
        let app = app(Arc::new(state), "192.0.2.1:4000");
        let embed = |url: &str| {
            format!(
                "/%23c/2026-01-01/embed?url={}",
                percent_encoding::utf8_percent_encode(url, percent_encoding::NON_ALPHANUMERIC)
            )
        };

        let (status, body) = get(&app, &embed(&format!("{base}/page"))).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        let card: Card = serde_json::from_str(&body).unwrap();
        assert_eq!(
            card.title
                .as_deref(),
            Some("A & B")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // The normalised form of a logged link, another link, another day: none is served.
        for (uri, status) in [
            (embed("https://example.org/a~"), 404),
            (embed(&format!("{base}/other")), 404),
            (
                format!(
                    "/%23c/2026-01-02/embed?url={}",
                    percent_encoding::utf8_percent_encode(
                        &format!("{base}/page"),
                        percent_encoding::NON_ALPHANUMERIC
                    )
                ),
                404,
            ),
            ("/%23c/2026-01-01/embed".to_owned(), 400),
        ] {
            let (got, body) = get(&app, &uri).await;
            assert_eq!(got.as_u16(), status, "{uri}: {body}");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1, "a refused request fetched");
    }

    fn text_response(filename: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-disposition: inline; filename*=utf-8''{filename}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn attachment_names_tie_a_message_to_its_nick() {
        let named = |value: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::CONTENT_DISPOSITION,
                value
                    .parse()
                    .unwrap(),
            );
            attachment_filename(&headers)
        };
        assert_eq!(
            named("inline; filename*=utf-8''Bob%5Bm%5D_2026-11-12_11%3A49%3A47.txt").as_deref(),
            Some("Bob[m]_2026-11-12_11:49:47.txt")
        );
        assert_eq!(
            named("attachment; filename=\"a b.txt\"").as_deref(),
            Some("a b.txt")
        );
        assert_eq!(named("inline").as_deref(), None);
        let message = |filename: &str| FullMessage {
            text: String::new(),
            truncated: false,
            filename: filename.into(),
        };
        assert!(message("Bob[m]_2026.txt").sent_by("bob{M}"));
        assert!(!message("Bob[m]2026.txt").sent_by("Bob[m]"));
        assert!(!message("Bobby_2026.txt").sent_by("Bob"));
        assert!(!message("").sent_by("Bob"));
    }

    #[tokio::test]
    async fn a_long_message_is_cached_by_url_and_checked_against_each_nick() {
        let dir = tempdir();
        let body = "line 1\nline 2\n```\ncode\n```\n";
        let (base, hits) = http_server(vec![
            text_response("Bob%5Bm%5D_2026-11-12.txt", body),
            text_response("Bob%5Bm%5D_2026-11-12.txt", body),
            html_response(PAGE),
        ])
        .await;
        let embeds = Embeds::open_for_test(
            dir.path()
                .to_owned(),
        );
        let url = Url::parse(&format!("{base}/oftc/media/v1/media/download/Ab")).unwrap();
        assert!(embeds.expands(&url));
        // With embeds shown, the same link first gets a card attempt, which fails on text/plain.
        assert_eq!(
            embeds
                .card(&url)
                .await,
            Card::default()
        );
        let got = embeds
            .full_message(&url, "Bob[m]")
            .await
            .unwrap();
        assert_eq!(
            (
                got.text
                    .as_str(),
                got.truncated
            ),
            (body, false)
        );
        // Cached by URL; another nick linking the same upload does not get it.
        assert!(
            embeds
                .is_cached(&url, Kind::FullMessage)
                .await
        );
        assert_eq!(
            embeds
                .full_message(&url, "mallory")
                .await,
            None
        );
        assert_eq!(
            embeds
                .full_message(&url, "bob[m]")
                .await
                .map(|m| m.text),
            Some(body.to_owned())
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        let host_dir = dir
            .path()
            .join("127.0.0.1");
        assert_eq!(
            std::fs::read_dir(&host_dir)
                .unwrap()
                .count(),
            2,
            "a card and a message of one URL live apart, under their host"
        );

        let html = Url::parse(&format!("{base}/media/page")).unwrap();
        assert_eq!(
            embeds
                .full_message(&html, "Bob[m]")
                .await,
            None,
            "only text/plain is taken"
        );
    }

    #[test]
    fn only_a_trailing_notice_names_a_long_message() {
        use crate::templates::full_message_url;
        let url = "https://matrix.org/oftc/media/v1/media/download/AbC";
        assert_eq!(
            full_message_url(&format!(
                "Going to repost this... (full message at <{url}>)"
            )),
            Some(url)
        );
        assert_eq!(
            full_message_url(&format!("(full message at <{url}>) and more")),
            None
        );
        assert_eq!(full_message_url(&format!("see <{url}>")), None);
        assert_eq!(full_message_url("(full message at <not a url>)"), None);
    }

    #[tokio::test]
    async fn the_full_message_endpoint_serves_only_a_notice_of_that_day() {
        use crate::parser::LogFormat;
        use crate::test_support::{add_channel, app, app_state};
        use crate::{ChannelDir, ChannelNode};

        let root = tempdir();
        let (base, hits) =
            http_server(vec![text_response("Bob%5Bm%5D_x.txt", "the whole text")]).await;
        let notice = format!("{base}/media/v1/download/A");
        let plain = format!("{base}/media/v1/download/B");
        let logs = root
            .path()
            .join("#c");
        std::fs::create_dir(&logs).unwrap();
        std::fs::write(
            logs.join("2026-01-01.log"),
            format!(
                "[10:00:00] <Bob[m]> long... (full message at <{notice}>)\n[10:01:00] <eve> {plain}\n[10:02:00] <eve> fake (full message at <{notice}>)\n"
            ),
        )
        .unwrap();
        let cache = root
            .path()
            .join("cache");
        std::fs::create_dir(&cache).unwrap();
        let mut channels = ChannelNode::default();
        add_channel(
            &mut channels,
            &["#c"],
            vec![ChannelDir {
                path: logs,
                format: LogFormat::Znc,
            }],
        );
        let mut state = app_state(channels, "logs_dirs: []");
        state.embeds = Some(Embeds::open_for_test(cache));
        let app = app(Arc::new(state), "192.0.2.1:4000");
        let enc = |s: &str| {
            percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
        };
        let ask = |url: &str, nick: &str| {
            format!(
                "/%23c/2026-01-01/fullmsg?url={}&nick={}",
                enc(url),
                enc(nick)
            )
        };

        let (status, body) = get(&app, &ask(&notice, "Bob[m]")).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["text"], "the whole text");
        assert_eq!(body["host"], "127.0.0.1");
        // eve's notice links Bob's upload: served from the cache, refused by name.
        let (status, body) = get(&app, &ask(&notice, "eve")).await;
        assert_eq!((status.as_u16(), body.as_str()), (200, "{}"));
        for (uri, status) in [
            (ask(&plain, "eve"), 404),
            (ask(&notice, "mallory"), 404),
            (ask("https://matrix.org/other/media/x", "Bob[m]"), 404),
            (
                format!("/%23c/2026-01-01/fullmsg?url={}", enc(&notice)),
                400,
            ),
        ] {
            let (got, body) = get(&app, &uri).await;
            assert_eq!(got.as_u16(), status, "{uri}: {body}");
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
