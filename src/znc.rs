//! Who is in each channel right now, learned from a short client session to
//! ZNC and handed to presence as [`SessionEvent`]s.
//!
//! docs/znc.md records the ZNC behaviour this relies on, verified against
//! ZNC 1.10.1.
//!
//! # The session
//!
//! Over TLS, presenting the client certificate: `CAP LS 302` and
//! `CAP REQ :sasl multi-prefix` sent together (registration waits for
//! `CAP END`, so the LS reply needs no parsing), `NICK`, `USER`, then
//! SASL EXTERNAL with `user/network` as the authorization identity. With a
//! password instead, `PASS user/network:password` and only multi-prefix.
//! ZNC renames the client to the bouncer's nick, replays 001 to 005 and the
//! MOTD, then sends for each attached channel `JOIN`, 332 and 333 when a
//! topic is set, the 353 lines and 366; each 366 publishes that channel.
//! ZNC writes these bursts before it reads further client input, so the
//! `PING` sent on 376 (or 422, no MOTD) is answered after the last 366, and
//! its `PONG` ends the session with `QUIT`. No `MODE` is sent: ZNC forwards
//! a mode query it cannot answer, and the reply then reaches the user's
//! other clients. `PING` from ZNC is answered. One timeout bounds the whole
//! session.
//!
//! # Side effects
//!
//! The session is a client of the ZNC user. With AutoClearChanBuffer on,
//! ZNC replays each channel's buffer to it and clears it, so lines buffered
//! for a detached client are consumed. Detached channels get no burst and
//! their snapshots go stale.
//!
//! # The certificate pin
//!
//! The listener's certificate is accepted only when the SHA-256 of its DER
//! equals `znc.fingerprint`; intermediates are ignored, and expiry and name
//! are not checked, by design: ZNC's default certificate is self-signed with
//! no subjectAltName. Handshake signatures are still verified. This verifier
//! is used only here.
//!
//! # When a session runs
//!
//! At startup, then after each reconnect of the bouncer: a
//! `Connected to IRC` line in the network's `status/` log arms the trigger
//! and marks every snapshot stale, and the session runs 10 s after the last
//! `JOIN` of the bouncer's own nick (learned from the previous session's
//! rename) in a channel of the network. Until a session has succeeded the
//! nick is unknown and any join counts. A failed session is logged with the
//! error chain and not retried: the next trigger or a restart runs the next.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use irc_proto::{CapSubCommand, Command, Message};
use jiff::Timestamp;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsConnector;

use crate::live::{LiveChannel, ModeSpec};
use crate::presence::{Presence, SessionEvent, Topic, casemap};

/// Quiet time after the bouncer's last join before a session runs.
const DEBOUNCE: Duration = Duration::from_secs(10);
/// Longest line accepted from ZNC; IRCv3 allows 8191 bytes of tags plus 512.
const MAX_LINE: u64 = 8704;
const SENTINEL: &str = "irc-log-viewer-end-of-burst";

/// The `znc:` section.
#[derive(Debug, Serialize, Deserialize)]
pub struct ZncConfig {
    /// `host:port` of a ZNC listener with SSL and AllowIRC, e.g. `[::1]:7778`.
    pub address: String,
    /// ZNC user.
    pub user: String,
    /// ZNC network of that user.
    pub network: String,
    /// The channel tree's path to the network's channels; defaults to `network`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_path: Option<String>,
    /// SHA-256 of the listener certificate's DER, colon-separated hex.
    pub fingerprint: String,
    /// PEM file holding the client certificate and its key, enrolled with certauth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<std::path::PathBuf>,
    /// The ZNC user's password, used instead of `client_cert`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Seconds the whole session may take.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    30
}

impl ZncConfig {
    /// Path segments of the network's channels in the channel tree.
    pub fn channel_path(&self) -> Vec<String> {
        self.channel_path
            .as_deref()
            .unwrap_or(&self.network)
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

/// Why the `znc:` section cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("set exactly one of znc.client_cert and znc.password")]
    Credentials,
    #[error("znc.fingerprint is not 32 colon-separated hex bytes")]
    Fingerprint,
    #[error("znc.address {0:?} is not host:port")]
    Address(String),
    #[error("cannot read znc.client_cert {path}")]
    ClientCert {
        path: String,
        #[source]
        source: rustls::pki_types::pem::Error,
    },
    #[error("znc.client_cert {0} holds no certificate")]
    NoCertificate(String),
    #[error("cannot use the client certificate")]
    Tls(#[source] rustls::Error),
}

/// Why a session failed.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("cannot connect")]
    Connect(#[source] std::io::Error),
    #[error("TLS handshake failed")]
    Handshake(#[source] std::io::Error),
    #[error("connection failed")]
    Io(#[source] std::io::Error),
    #[error("no end of burst within {0} s")]
    Timeout(u64),
    #[error("connection closed before the end of the burst")]
    Closed,
    #[error("ZNC sent a line longer than {MAX_LINE} bytes")]
    LineTooLong,
    #[error("ZNC refused the login: {0}")]
    Refused(String),
    #[error("ZNC did not grant {0}")]
    Capability(&'static str),
    #[error("ZNC closed the session: {0}")]
    Error(String),
}

/// Everything a session needs, checked once at startup.
pub struct Setup {
    address: String,
    server_name: ServerName<'static>,
    tls: TlsConnector,
    user: String,
    network: String,
    password: Option<String>,
    channel_path: Vec<String>,
    timeout: Duration,
}

impl Setup {
    /// Validates `config` and loads the client certificate.
    pub fn new(config: &ZncConfig) -> Result<Self, SetupError> {
        let pin = parse_fingerprint(&config.fingerprint).ok_or(SetupError::Fingerprint)?;
        let host = host_of(&config.address).ok_or_else(|| {
            SetupError::Address(
                config
                    .address
                    .clone(),
            )
        })?;
        let server_name = ServerName::try_from(host.to_owned()).map_err(|_| {
            SetupError::Address(
                config
                    .address
                    .clone(),
            )
        })?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(SetupError::Tls)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCert { pin, provider }));
        let tls = match (&config.client_cert, &config.password) {
            (Some(path), None) => {
                let (certs, key) = load_client_cert(path)?;
                builder
                    .with_client_auth_cert(certs, key)
                    .map_err(SetupError::Tls)?
            }
            (None, Some(_)) => builder.with_no_client_auth(),
            _ => return Err(SetupError::Credentials),
        };
        Ok(Self {
            address: config
                .address
                .clone(),
            server_name,
            tls: TlsConnector::from(Arc::new(tls)),
            user: config
                .user
                .clone(),
            network: config
                .network
                .clone(),
            password: config
                .password
                .clone(),
            channel_path: config.channel_path(),
            timeout: Duration::from_secs(config.timeout_secs),
        })
    }

    /// Whether `segments`, a log directory's path under logs_dirs, is the
    /// network's status window.
    pub fn is_status_dir(&self, segments: &[String]) -> bool {
        segments.len()
            == self
                .channel_path
                .len()
                + 1
            && segments.starts_with(&self.channel_path)
            && segments
                .last()
                .is_some_and(|s| s == "status")
    }

    /// The channel key the JOIN echo of `name` maps to, among `known`.
    fn channel_key<'a>(&self, name: &str, known: &'a [String]) -> Option<&'a String> {
        let want = casemap(&format!(
            "{}/{name}",
            self.channel_path
                .join("/")
        ));
        known
            .iter()
            .find(|key| casemap(key) == want)
    }
}

/// `host` of `host:port` or `[v6]:port`.
fn host_of(address: &str) -> Option<&str> {
    let (host, port) = address.rsplit_once(':')?;
    port.parse::<u16>()
        .ok()?;
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.strip_suffix(']')?,
        None if host.contains(':') => return None,
        None => host,
    };
    (!host.is_empty()).then_some(host)
}

/// 32 bytes from colon-separated hex, accepting openssl's
/// `sha256 Fingerprint=` prefix.
fn parse_fingerprint(text: &str) -> Option<[u8; 32]> {
    let hex = text
        .rsplit_once('=')
        .map_or(text, |(_, h)| h)
        .trim();
    let bytes: Vec<u8> = hex
        .split(':')
        .map(|b| {
            (b.len() == 2)
                .then(|| u8::from_str_radix(b, 16).ok())
                .flatten()
        })
        .collect::<Option<_>>()?;
    bytes
        .try_into()
        .ok()
}

fn hex_fingerprint(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (i, b) in bytes
        .iter()
        .enumerate()
    {
        if i > 0 {
            out.push(':');
        }
        // Writing to a String cannot fail.
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn load_client_cert(
    path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), SetupError> {
    let shown = || {
        path.display()
            .to_string()
    };
    let certs = CertificateDer::pem_file_iter(path)
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .map_err(|source| SetupError::ClientCert {
            path: shown(),
            source,
        })?;
    if certs.is_empty() {
        return Err(SetupError::NoCertificate(shown()));
    }
    let key = PrivateKeyDer::from_pem_file(path).map_err(|source| SetupError::ClientCert {
        path: shown(),
        source,
    })?;
    Ok((certs, key))
}

/// Accepts the server certificate whose DER hashes to the pin.
#[derive(Debug)]
struct PinnedCert {
    pin: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        if got.as_ref() == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "ZNC certificate SHA-256 {} does not match znc.fingerprint {}",
                hex_fingerprint(got.as_ref()),
                hex_fingerprint(&self.pin)
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self
                .provider
                .signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self
                .provider
                .signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// What a successful session learned.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The bouncer's nick, from ZNC's rename of the session.
    pub own_nick: Option<String>,
    /// Channel keys that got a burst.
    pub bursted: Vec<String>,
}

/// Runs session `epoch` and reports it to `presence`, `known` being the
/// channel keys of the tree.
pub async fn run_session(
    setup: &Setup,
    known: &[String],
    epoch: u64,
    presence: &Presence,
) -> Result<Outcome, SessionError> {
    presence
        .session(SessionEvent::Started { epoch })
        .await;
    let result = tokio::time::timeout(setup.timeout, session(setup, known, epoch, presence))
        .await
        .unwrap_or({
            Err(SessionError::Timeout(
                setup
                    .timeout
                    .as_secs(),
            ))
        });
    presence
        .session(SessionEvent::Ended {
            epoch,
            ok: result.is_ok(),
        })
        .await;
    result
}

/// One channel's burst being collected.
#[derive(Default)]
struct Pending {
    topic: Option<String>,
    setter: Option<(String, Timestamp)>,
    names: Vec<String>,
}

async fn session(
    setup: &Setup,
    known: &[String],
    epoch: u64,
    presence: &Presence,
) -> Result<Outcome, SessionError> {
    let tcp = TcpStream::connect(&setup.address)
        .await
        .map_err(SessionError::Connect)?;
    let tls = setup
        .tls
        .connect(
            setup
                .server_name
                .clone(),
            tcp,
        )
        .await
        .map_err(SessionError::Handshake)?;
    let (read, mut write) = tokio::io::split(tls);
    let mut read = BufReader::new(read);

    let login = format!("{}/{}", setup.user, setup.network);
    let mut hello = String::new();
    match &setup.password {
        Some(password) => {
            hello += &format!("PASS {login}:{password}\r\nCAP LS 302\r\nCAP REQ :multi-prefix\r\n");
        }
        None => hello += "CAP LS 302\r\nCAP REQ :sasl multi-prefix\r\n",
    }
    hello += &format!("NICK irc-log-viewer\r\nUSER {login} 0 * :irc-log-viewer\r\n");
    send(&mut write, &hello).await?;

    let mut own_nick = "irc-log-viewer".to_owned();
    let mut renamed = false;
    let mut modes = ModeSpec::default();
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut bursted = Vec::new();
    let mut unmapped = Vec::new();
    loop {
        let line = read_line(&mut read).await?;
        let Ok(msg) = line
            .trim_end()
            .parse::<Message>()
        else {
            continue;
        };
        let from_me = msg
            .source_nickname()
            .is_some_and(|n| casemap(n) == casemap(&own_nick));
        let (numeric, args) = numeric(&msg.command);
        let end_of_burst = matches!(
            &msg.command,
            Command::PONG(server, token) if token.as_deref().unwrap_or(server) == SENTINEL
        );
        match (&msg.command, numeric) {
            (Command::PING(server, _), _) => {
                send(&mut write, &format!("PONG :{server}\r\n")).await?;
            }
            _ if end_of_burst => {
                send(&mut write, "QUIT :done\r\n").await?;
                if !unmapped.is_empty() {
                    eprintln!(
                        "znc: session {epoch}: no log directory under {} for {}; ignored",
                        setup
                            .channel_path
                            .join("/"),
                        unmapped.join(", ")
                    );
                }
                return Ok(Outcome {
                    own_nick: renamed.then_some(own_nick),
                    bursted,
                });
            }
            (Command::CAP(_, CapSubCommand::ACK, a, b), _) => {
                let caps = b
                    .as_deref()
                    .or(a.as_deref())
                    .unwrap_or_default();
                let has = |cap| {
                    caps.split_whitespace()
                        .any(|c| c == cap)
                };
                if setup
                    .password
                    .is_some()
                {
                    send(&mut write, "CAP END\r\n").await?;
                } else if has("sasl") {
                    send(&mut write, "AUTHENTICATE EXTERNAL\r\n").await?;
                } else {
                    return Err(SessionError::Capability("sasl"));
                }
            }
            (Command::CAP(_, CapSubCommand::NAK, _, _), _) => {
                return Err(SessionError::Capability("sasl multi-prefix"));
            }
            (Command::AUTHENTICATE(data), _) if data == "+" => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(&login);
                send(&mut write, &format!("AUTHENTICATE {encoded}\r\n")).await?;
            }
            (Command::NICK(new), _) if from_me => {
                own_nick = new.clone();
                renamed = true;
            }
            (Command::JOIN(name, _, _), _) if from_me => match setup.channel_key(name, known) {
                Some(_) => {
                    pending.insert(casemap(name), Pending::default());
                }
                None => unmapped.push(name.clone()),
            },
            (Command::ERROR(text), _) => return Err(SessionError::Error(text.clone())),
            (_, Some(903)) => send(&mut write, "CAP END\r\n").await?,
            (_, Some(902 | 904 | 905 | 906 | 464 | 465)) => {
                return Err(SessionError::Refused(
                    args.last()
                        .cloned()
                        .unwrap_or_default(),
                ));
            }
            (_, Some(5)) => {
                for token in args
                    .iter()
                    .skip(1)
                {
                    modes.apply_isupport(token);
                }
            }
            (_, Some(376 | 422)) => {
                send(&mut write, &format!("PING :{SENTINEL}\r\n")).await?;
            }
            (_, Some(332)) => {
                if let (Some(chan), Some(text)) = (args.get(1), args.get(2)) {
                    if let Some(p) = pending.get_mut(&casemap(chan)) {
                        p.topic = Some(text.clone());
                    }
                }
            }
            (_, Some(333)) => {
                if let (Some(chan), Some(setter), Some(at)) =
                    (args.get(1), args.get(2), args.get(3))
                {
                    let at = at
                        .parse::<i64>()
                        .ok()
                        .and_then(|s| Timestamp::from_second(s).ok());
                    if let (Some(p), Some(at)) = (pending.get_mut(&casemap(chan)), at) {
                        let nick = setter
                            .split('!')
                            .next()
                            .unwrap_or(setter);
                        p.setter = Some((nick.to_owned(), at));
                    }
                }
            }
            (_, Some(353)) => {
                if let (Some(chan), Some(names)) = (args.get(2), args.get(3)) {
                    if let Some(p) = pending.get_mut(&casemap(chan)) {
                        p.names
                            .extend(
                                names
                                    .split_whitespace()
                                    .map(str::to_owned),
                            );
                    }
                }
            }
            (_, Some(366)) => {
                let Some(chan) = args.get(1) else { continue };
                let Some(p) = pending.remove(&casemap(chan)) else {
                    continue;
                };
                let Some(key) = setup.channel_key(chan, known) else {
                    continue;
                };
                let now = Timestamp::now();
                let topic = p
                    .topic
                    .map(|text| {
                        let (nick, at) = p
                            .setter
                            .unwrap_or_else(|| (String::new(), now));
                        Topic { nick, text, at }
                    });
                let live = LiveChannel::from_burst(
                    p.names
                        .iter()
                        .map(String::as_str),
                    topic,
                    now,
                    modes.clone(),
                );
                presence
                    .session(SessionEvent::Burst {
                        epoch,
                        channel: key.clone(),
                        live: Box::new(live),
                    })
                    .await;
                bursted.push(key.clone());
            }
            _ => {}
        }
    }
}

/// The numeric of a reply, and its parameters.
fn numeric(command: &Command) -> (Option<u16>, &[String]) {
    match command {
        Command::Response(resp, args) => (Some(*resp as u16), args),
        Command::Raw(cmd, args) => (
            cmd.parse()
                .ok(),
            args,
        ),
        _ => (None, &[]),
    }
}

async fn send<W: AsyncWriteExt + Unpin>(write: &mut W, text: &str) -> Result<(), SessionError> {
    write
        .write_all(text.as_bytes())
        .await
        .map_err(SessionError::Io)?;
    write
        .flush()
        .await
        .map_err(SessionError::Io)
}

async fn read_line<R: AsyncBufRead + Unpin>(read: &mut R) -> Result<String, SessionError> {
    let mut buf = Vec::new();
    let n = read
        .take(MAX_LINE + 1)
        .read_until(b'\n', &mut buf)
        .await
        .map_err(|e| match e.kind() {
            // rustls reports a close without close_notify this way.
            std::io::ErrorKind::UnexpectedEof => SessionError::Closed,
            _ => SessionError::Io(e),
        })?;
    if n == 0 {
        return Err(SessionError::Closed);
    }
    if buf.last() != Some(&b'\n') {
        return Err(if n as u64 > MAX_LINE {
            SessionError::LineTooLong
        } else {
            SessionError::Closed
        });
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Formats an error with its source chain.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let _ = write!(out, ": {s}");
        source = s.source();
    }
    out
}

/// What the tail reports from the network's logs.
#[derive(Debug)]
pub enum Trigger {
    /// The bouncer connected to IRC.
    Connected,
    /// Someone joined one of the network's channels.
    Join(String),
}

/// Decides when the next session runs.
#[derive(Debug, Default)]
struct Triggers {
    armed: bool,
    own_nick: Option<String>,
    due: Option<Instant>,
}

impl Triggers {
    fn on(&mut self, trigger: Trigger, now: Instant) {
        match trigger {
            Trigger::Connected => {
                self.armed = true;
                self.due = None;
            }
            Trigger::Join(nick) => {
                let ours = self
                    .own_nick
                    .as_ref()
                    .is_none_or(|own| casemap(own) == casemap(&nick));
                if self.armed && ours {
                    self.due = Some(now + DEBOUNCE);
                }
            }
        }
    }

    fn ran(&mut self, own_nick: Option<String>) {
        self.armed = false;
        self.due = None;
        if own_nick.is_some() {
            self.own_nick = own_nick;
        }
    }
}

/// The tail's handle on the session runner.
pub struct Znc {
    triggers: mpsc::Sender<Trigger>,
    setup: Arc<Setup>,
}

impl Znc {
    /// A handle for network `OFTC` whose triggers land in the returned receiver.
    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::Receiver<Trigger>) {
        let setup = Setup::new(&ZncConfig {
            address: "[::1]:1".into(),
            user: "u".into(),
            network: "OFTC".into(),
            channel_path: None,
            fingerprint: "00:".repeat(31) + "00",
            client_cert: None,
            password: Some("p".into()),
            timeout_secs: 1,
        })
        .unwrap();
        let (tx, rx) = mpsc::channel(16);
        (
            Self {
                triggers: tx,
                setup: Arc::new(setup),
            },
            rx,
        )
    }

    /// Whether `segments` is the network's status window.
    pub fn is_status_dir(&self, segments: &[String]) -> bool {
        self.setup
            .is_status_dir(segments)
    }

    /// Whether the channel keyed `key` belongs to the network.
    pub fn owns_channel(&self, key: &str) -> bool {
        let prefix = self
            .setup
            .channel_path
            .join("/");
        key.strip_prefix(&prefix)
            .is_some_and(|rest| rest.starts_with('/') && !rest[1..].contains('/'))
    }

    /// Passes a trigger to the runner. Blocks while its queue is full; call
    /// it from a blocking thread.
    pub fn trigger(&self, trigger: Trigger) {
        // Fails only once the runtime is shutting down and the runner is gone.
        let _ = self
            .triggers
            .blocking_send(trigger);
    }
}

/// Whether a status window line is ZNC's "Connected to IRC" on 001.
pub fn is_connected_line(line: &str) -> bool {
    line.split_once("] ")
        .is_some_and(|(_, text)| text.starts_with("Connected to IRC ("))
}

/// Starts the runner, which runs a session now and then on each trigger.
pub fn start(setup: Setup, known: Vec<String>, presence: Arc<Presence>) -> Znc {
    let (tx, rx) = mpsc::channel(256);
    let setup = Arc::new(setup);
    tokio::spawn(run(Arc::clone(&setup), known, presence, rx));
    Znc {
        triggers: tx,
        setup,
    }
}

async fn run(
    setup: Arc<Setup>,
    known: Vec<String>,
    presence: Arc<Presence>,
    mut rx: mpsc::Receiver<Trigger>,
) {
    let mut triggers = Triggers::default();
    let mut epoch = 0u64;
    let mut run_now = true;
    loop {
        if run_now {
            epoch += 1;
            let started = Instant::now();
            match run_session(&setup, &known, epoch, &presence).await {
                Ok(outcome) => {
                    eprintln!(
                        "znc: session {epoch} to {}: {} channels, bouncer nick {}, in {:.1?}",
                        setup.address,
                        outcome
                            .bursted
                            .len(),
                        outcome
                            .own_nick
                            .as_deref()
                            .unwrap_or("unknown"),
                        started.elapsed()
                    );
                    triggers.ran(outcome.own_nick);
                }
                Err(e) => {
                    eprintln!(
                        "znc: session {epoch} to {} failed: {}; live status stays stale until the next reconnect or restart",
                        setup.address,
                        error_chain(&e)
                    );
                    triggers.ran(None);
                }
            }
            run_now = false;
        }
        let due = triggers.due;
        tokio::select! {
            trigger = rx.recv() => {
                let Some(trigger) = trigger else { return };
                if matches!(trigger, Trigger::Connected) {
                    eprintln!("znc: the bouncer reconnected; a session runs {} s after its last join", DEBOUNCE.as_secs());
                    presence.session(SessionEvent::Stale).await;
                }
                triggers.on(trigger, Instant::now());
            }
            () = sleep_until(due) => run_now = true,
        }
    }
}

async fn sleep_until(due: Option<Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(due.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
    use rustls::{DistinguishedName, ServerConfig};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use super::*;
    use crate::ChannelNode;
    use crate::test_support::tempdir;

    /// Asks for a client certificate and accepts any, so a test can see
    /// which one the session presented.
    #[derive(Debug)]
    struct AnyClient(Arc<CryptoProvider>);

    impl ClientCertVerifier for AnyClient {
        fn root_hint_subjects(&self) -> &[DistinguishedName] {
            &[]
        }

        fn client_auth_mandatory(&self) -> bool {
            false
        }

        fn verify_client_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _now: UnixTime,
        ) -> Result<ClientCertVerified, rustls::Error> {
            Ok(ClientCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls12_signature(
                message,
                cert,
                dss,
                &self
                    .0
                    .signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &self
                    .0
                    .signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    /// What one scripted connection saw.
    #[derive(Debug, Default)]
    struct Seen {
        lines: Vec<String>,
        client_cert: Option<Vec<u8>>,
    }

    /// Replies to one received line; `None` closes the connection.
    type Script = fn(&str) -> Option<Vec<String>>;

    /// A TLS listener with a self-signed certificate, like ZNC's default.
    struct FakeZnc {
        addr: SocketAddr,
        fingerprint: String,
        seen: tokio::task::JoinHandle<Seen>,
    }

    async fn fake_znc(script: Script) -> FakeZnc {
        let cert = rcgen::generate_simple_self_signed(vec!["host.unknown".to_owned()]).unwrap();
        let fingerprint = hex_fingerprint(
            ring::digest::digest(
                &ring::digest::SHA256,
                cert.cert
                    .der(),
            )
            .as_ref(),
        );
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let key = PrivateKeyDer::from_pem_slice(
            cert.signing_key
                .serialize_pem()
                .as_bytes(),
        )
        .unwrap();
        let config = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_client_cert_verifier(Arc::new(AnyClient(provider)))
            .with_single_cert(
                vec![
                    cert.cert
                        .der()
                        .clone(),
                ],
                key,
            )
            .unwrap();
        let listener = TcpListener::bind("[::1]:0")
            .await
            .unwrap();
        let addr = listener
            .local_addr()
            .unwrap();
        let seen = tokio::spawn(async move {
            let mut seen = Seen::default();
            let (tcp, _) = listener
                .accept()
                .await
                .unwrap();
            let Ok(tls) = TlsAcceptor::from(Arc::new(config))
                .accept(tcp)
                .await
            else {
                return seen;
            };
            seen.client_cert = tls
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .map(|c| c.to_vec());
            let (read, mut write) = tokio::io::split(tls);
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines
                .next_line()
                .await
            {
                let replies = script(&line);
                seen.lines
                    .push(line);
                let Some(replies) = replies else { break };
                for reply in replies {
                    if write
                        .write_all(format!("{reply}\r\n").as_bytes())
                        .await
                        .is_err()
                    {
                        return seen;
                    }
                }
            }
            seen
        });
        FakeZnc {
            addr,
            fingerprint,
            seen,
        }
    }

    fn lines(texts: &[&str]) -> Option<Vec<String>> {
        Some(
            texts
                .iter()
                .map(|t| t.to_string())
                .collect(),
        )
    }

    const BURST: &[&str] = &[
        ":irc.znc.in 001 irc-log-viewer :Welcome to IRC",
        ":irc-log-viewer!x@irc.znc.in NICK :TiCPU",
        ":irc.oftc.net 005 TiCPU PREFIX=(ov)@+ CHANMODES=eIbq,k,flj,CFLMPQScgimnprstuz :are supported by this server",
        ":irc.znc.in PING :ZNC",
        ":irc.oftc.net 375 TiCPU :- irc.oftc.net Message of the Day -",
        ":irc.oftc.net 376 TiCPU :End of /MOTD command.",
        ":TiCPU!~t@h JOIN :#bcache",
        ":irc.oftc.net 332 TiCPU #bcache :the topic",
        ":irc.oftc.net 333 TiCPU #bcache py1hon!~p@h 1770412381",
        ":irc.oftc.net 353 TiCPU = #bcache :@+TiCPU alice",
        ":irc.oftc.net 353 TiCPU = #bcache :+bob",
        ":irc.oftc.net 366 TiCPU #bcache :End of /NAMES list.",
        ":TiCPU!~t@h JOIN :#unlogged",
        ":irc.oftc.net 353 TiCPU = #unlogged :TiCPU",
        ":irc.oftc.net 366 TiCPU #unlogged :End of /NAMES list.",
    ];

    fn sasl_znc(line: &str) -> Option<Vec<String>> {
        match line {
            "CAP REQ :sasl multi-prefix" => {
                lines(&[":irc.znc.in CAP irc-log-viewer ACK :sasl multi-prefix"])
            }
            "AUTHENTICATE EXTERNAL" => lines(&["AUTHENTICATE +"]),
            "AUTHENTICATE VGlreS9PRlRD" => lines(&[
                ":irc.znc.in 900 irc-log-viewer irc-log-viewer!x@y Tiky :You are now logged in as Tiky",
                ":irc.znc.in 903 irc-log-viewer :SASL authentication successful",
            ]),
            "CAP END" => lines(BURST),
            "PING :irc-log-viewer-end-of-burst" => {
                lines(&[":irc.znc.in PONG irc.znc.in :irc-log-viewer-end-of-burst"])
            }
            "QUIT :done" => None,
            _ => lines(&[]),
        }
    }

    fn setup(znc: &FakeZnc, dir: &Path, password: bool, timeout_secs: u64) -> Setup {
        let client = rcgen::generate_simple_self_signed(vec!["irc-log-viewer".to_owned()]).unwrap();
        let pem_path = dir.join("client.pem");
        std::fs::write(
            &pem_path,
            client
                .cert
                .pem()
                + &client
                    .signing_key
                    .serialize_pem(),
        )
        .unwrap();
        Setup::new(&ZncConfig {
            address: znc
                .addr
                .to_string(),
            user: "Tiky".into(),
            network: "OFTC".into(),
            channel_path: None,
            fingerprint: znc
                .fingerprint
                .to_uppercase(),
            client_cert: (!password).then_some(pem_path),
            password: password.then(|| "secret".to_owned()),
            timeout_secs,
        })
        .unwrap()
    }

    fn known() -> Vec<String> {
        vec![
            "OFTC/#bcache".into(),
            "OFTC/#detached".into(),
            "other/#bcache".into(),
        ]
    }

    fn presence(dir: &Path) -> Arc<Presence> {
        crate::presence::start(
            dir.join("presence.yaml"),
            jiff::tz::TimeZone::UTC,
            &ChannelNode::default(),
        )
    }

    /// The live snapshot of `key` once published, within 5 s.
    async fn published(
        presence: &Presence,
        key: &str,
        check: impl Fn(&LiveChannel) -> bool,
    ) -> Arc<LiveChannel> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(live) = presence.live(key) {
                if check(&live) {
                    return live;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{key} not published as expected within 5 s: {:?}",
                presence.live(key)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn members(live: &LiveChannel) -> Vec<(String, String)> {
        let mut out: Vec<_> = live
            .members
            .values()
            .map(|m| {
                (
                    m.nick
                        .clone(),
                    m.prefixes
                        .clone(),
                )
            })
            .collect();
        out.sort();
        out
    }

    #[tokio::test]
    async fn sasl_session_publishes_each_burst_and_learns_the_nick() {
        let dir = tempdir();
        let znc = fake_znc(sasl_znc).await;
        let setup = setup(&znc, dir.path(), false, 10);
        let presence = presence(dir.path());

        let outcome = run_session(&setup, &known(), 1, &presence)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            Outcome {
                own_nick: Some("TiCPU".into()),
                bursted: vec!["OFTC/#bcache".into()],
            }
        );
        let seen = znc
            .seen
            .await
            .unwrap();
        assert_eq!(
            seen.lines,
            [
                "CAP LS 302",
                "CAP REQ :sasl multi-prefix",
                "NICK irc-log-viewer",
                "USER Tiky/OFTC 0 * :irc-log-viewer",
                "AUTHENTICATE EXTERNAL",
                "AUTHENTICATE VGlreS9PRlRD",
                "CAP END",
                "PONG :ZNC",
                "PING :irc-log-viewer-end-of-burst",
                "QUIT :done",
            ]
        );
        assert!(
            seen.client_cert
                .is_some(),
            "the client certificate was not presented"
        );

        let live = published(&presence, "OFTC/#bcache", |_| true).await;
        assert_eq!(
            members(&live),
            [
                ("TiCPU".into(), "@+".into()),
                ("alice".into(), String::new()),
                ("bob".into(), "+".into()),
            ]
        );
        assert_eq!(
            live.topic,
            Some(Topic {
                nick: "py1hon".into(),
                text: "the topic".into(),
                at: Timestamp::from_second(1770412381).unwrap(),
            })
        );
        assert!(!live.stale);
        assert!(
            presence
                .live("other/#bcache")
                .is_none()
        );
        presence
            .shutdown()
            .await;
    }

    fn password_znc(line: &str) -> Option<Vec<String>> {
        match line {
            "CAP REQ :multi-prefix" => lines(&[":irc.znc.in CAP irc-log-viewer ACK :multi-prefix"]),
            "CAP END" => lines(BURST),
            other => sasl_znc(other),
        }
    }

    #[tokio::test]
    async fn password_session_sends_pass_and_no_sasl() {
        let dir = tempdir();
        let znc = fake_znc(password_znc).await;
        let setup = setup(&znc, dir.path(), true, 10);
        let presence = presence(dir.path());
        run_session(&setup, &known(), 1, &presence)
            .await
            .unwrap();
        let seen = znc
            .seen
            .await
            .unwrap();
        assert_eq!(
            seen.lines[..4],
            [
                "PASS Tiky/OFTC:secret",
                "CAP LS 302",
                "CAP REQ :multi-prefix",
                "NICK irc-log-viewer"
            ]
        );
        assert!(
            !seen
                .lines
                .iter()
                .any(|l| l.starts_with("AUTHENTICATE")),
            "{:?}",
            seen.lines
        );
        assert!(
            seen.client_cert
                .is_none()
        );
        presence
            .shutdown()
            .await;
    }

    fn refusing_znc(line: &str) -> Option<Vec<String>> {
        match line {
            "AUTHENTICATE EXTERNAL" => lines(&[
                ":irc.znc.in 904 irc-log-viewer :No client cert presented",
                ":irc.znc.in 464 irc-log-viewer :Password required",
            ]),
            other => sasl_znc(other),
        }
    }

    #[tokio::test]
    async fn a_refused_login_is_an_error_naming_znc_reason() {
        let dir = tempdir();
        let znc = fake_znc(refusing_znc).await;
        let setup = setup(&znc, dir.path(), false, 10);
        let presence = presence(dir.path());
        let err = run_session(&setup, &known(), 1, &presence)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "ZNC refused the login: No client cert presented"
        );
        presence
            .shutdown()
            .await;
    }

    fn stalling_znc(line: &str) -> Option<Vec<String>> {
        match line {
            "CAP END" => lines(&BURST[..6]),
            "PING :irc-log-viewer-end-of-burst" => lines(&[]),
            other => sasl_znc(other),
        }
    }

    fn dropping_znc(line: &str) -> Option<Vec<String>> {
        match line {
            "CAP END" => lines(&BURST[..10]),
            "PING :irc-log-viewer-end-of-burst" => None,
            other => sasl_znc(other),
        }
    }

    #[tokio::test]
    async fn a_failed_session_leaves_the_last_snapshot_stale() {
        let dir = tempdir();
        let presence = presence(dir.path());
        let znc = fake_znc(sasl_znc).await;
        run_session(&setup(&znc, dir.path(), false, 10), &known(), 1, &presence)
            .await
            .unwrap();
        published(&presence, "OFTC/#bcache", |l| !l.stale).await;

        // ZNC drops the connection after the first 353 of #bcache.
        let znc = fake_znc(dropping_znc).await;
        let err = run_session(&setup(&znc, dir.path(), false, 10), &known(), 2, &presence)
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::Closed), "{err:?}");
        let live = published(&presence, "OFTC/#bcache", |l| l.stale).await;
        assert_eq!(members(&live).len(), 3, "the old snapshot is kept");

        let znc = fake_znc(stalling_znc).await;
        let err = run_session(&setup(&znc, dir.path(), false, 1), &known(), 3, &presence)
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::Timeout(1)), "{err:?}");
        presence
            .shutdown()
            .await;
    }

    #[tokio::test]
    async fn a_certificate_other_than_the_pinned_one_is_refused() {
        let dir = tempdir();
        let znc = fake_znc(sasl_znc).await;
        let mut wrong = setup(&znc, dir.path(), false, 10);
        let other = fake_znc(sasl_znc).await;
        wrong.address = other
            .addr
            .to_string();
        let presence = presence(dir.path());
        let err = run_session(&wrong, &known(), 1, &presence)
            .await
            .unwrap_err();
        let text = error_chain(&err);
        assert!(matches!(err, SessionError::Handshake(_)), "{text}");
        assert!(
            text.contains(&other.fingerprint) && text.contains(&znc.fingerprint),
            "{text}"
        );
        presence
            .shutdown()
            .await;
    }

    #[test]
    fn fingerprints_and_addresses_parse_as_configured() {
        let hex = "ab:".repeat(31) + "ab";
        assert_eq!(parse_fingerprint(&hex), Some([0xab; 32]));
        assert_eq!(
            parse_fingerprint(&format!("sha256 Fingerprint={}", hex.to_uppercase())),
            Some([0xab; 32])
        );
        assert_eq!(parse_fingerprint(&hex[3..]), None);
        assert_eq!(parse_fingerprint(&hex.replace(':', "")), None);
        assert_eq!(host_of("[::1]:7778"), Some("::1"));
        assert_eq!(host_of("127.0.0.1:7778"), Some("127.0.0.1"));
        assert_eq!(host_of("znc.example:6697"), Some("znc.example"));
        assert_eq!(host_of("::1:7778"), None);
        assert_eq!(host_of("[::1]"), None);
    }

    #[test]
    fn a_session_runs_after_the_bouncers_own_joins_settle() {
        let t0 = Instant::now();
        let mut triggers = Triggers::default();
        triggers.on(Trigger::Join("TiCPU".into()), t0);
        assert_eq!(triggers.due, None, "not armed yet");

        triggers.on(Trigger::Connected, t0);
        // Before any session, the nick is unknown and any join counts.
        triggers.on(Trigger::Join("someone".into()), t0);
        assert_eq!(triggers.due, Some(t0 + DEBOUNCE));
        triggers.ran(Some("TiCPU".into()));
        assert_eq!(triggers.due, None);

        triggers.on(Trigger::Join("ticpu".into()), t0);
        assert_eq!(triggers.due, None, "disarmed by the session");
        triggers.on(Trigger::Connected, t0);
        triggers.on(Trigger::Join("someone".into()), t0);
        assert_eq!(triggers.due, None, "not the bouncer");
        let later = t0 + Duration::from_secs(3);
        triggers.on(Trigger::Join("ticpu".into()), t0);
        triggers.on(Trigger::Join("TiCPU".into()), later);
        assert_eq!(
            triggers.due,
            Some(later + DEBOUNCE),
            "debounced from the last join"
        );

        // A failed session keeps the nick it had.
        triggers.ran(None);
        assert_eq!(
            triggers
                .own_nick
                .as_deref(),
            Some("TiCPU")
        );
    }

    #[test]
    fn status_lines_mark_only_connects() {
        assert!(is_connected_line(
            "[01:49:26] Connected to IRC (irc.oftc.net +6697)"
        ));
        assert!(!is_connected_line(
            "[01:49:20] Disconnected from IRC (irc.oftc.net +6697)"
        ));
        assert!(!is_connected_line("[01:49:26] <x> Connected to IRC (fake)"));
    }
}
