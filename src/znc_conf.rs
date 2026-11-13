//! `-z/--znc <znc.conf>`: prints the `znc:` section for this viewer and
//! creates the client certificate it names.
//!
//! # Reading znc.conf
//!
//! The reader follows ZNC 1.10's CConfig::Parse: `<Tag name>` blocks,
//! `</Tag>`, `key = value` lines, and `#`, `//` and `/* */` comments, with
//! tags and keys case-insensitive. It keeps only the keys it names: the
//! global SSLCertFile and the legacy `Listen*`/`Listener*` lines, a
//! Listener block's Port, Host, SSL, AllowIRC, IPv4 and IPv6, and the
//! names of User and Network blocks. Every other value, Pass blocks and
//! password hashes included, is dropped as it is read and never printed.
//!
//! # What is printed
//!
//! The listener is the first with IRC and SSL on, on a loopback host before
//! one bound to all addresses; the session always uses TLS. Each user and
//! network pair gets a section, the first live and the rest commented. The
//! channel path is the channel tree's prefix ending in a segment named like
//! the network; every match is listed, the first live. The fingerprint is
//! the SHA-256 of the first certificate in SSLCertFile (ZNC's default is
//! `znc.pem` in its data directory, the parent of `configs/`).
//!
//! # The client certificate
//!
//! An ECDSA P-256 key and a self-signed certificate valid until 2100 are
//! written to `znc.client_cert`, or `znc-client.pem` beside the viewer's
//! config, created with mode 0600. An existing file is never overwritten:
//! its certificate is reused and its fingerprint printed.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

/// One listener, from a Listener block or a legacy line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub port: u16,
    pub host: String,
    pub ssl: bool,
    pub irc: bool,
    pub ipv4: bool,
    pub ipv6: bool,
}

/// One ZNC user and the networks under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub networks: Vec<String>,
}

/// The parts of znc.conf `-z` uses.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ZncConf {
    pub listeners: Vec<Listener>,
    pub ssl_cert_file: Option<String>,
    pub users: Vec<User>,
}

/// Why `-z` cannot produce a section.
#[derive(Debug, thiserror::Error)]
pub enum ZncConfError {
    #[error("cannot read {path}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}, line {line}: {problem}")]
    Syntax {
        path: String,
        line: usize,
        problem: String,
    },
    #[error("{0}: no Listener with AllowIRC and SSL; the viewer connects over TLS")]
    NoListener(String),
    #[error("{0}: no User block with a Network block")]
    NoNetwork(String),
    #[error("cannot read the certificate in {path}")]
    ReadCert {
        path: String,
        #[source]
        source: rustls::pki_types::pem::Error,
    },
    #[error("cannot create the client certificate {path}")]
    WriteCert {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot generate the client certificate")]
    Generate(#[source] rcgen::Error),
}

fn truthy(value: &str) -> bool {
    ["true", "yes", "on", "1"]
        .iter()
        .any(|t| t.eq_ignore_ascii_case(value.trim()))
}

/// A legacy `Listen = [irc_only ][host ][+]port` line.
fn legacy_listener(key: &str, value: &str) -> Option<Listener> {
    let mut value = value.trim();
    let mut irc = true;
    if let Some(rest) = value.strip_prefix("irc_only ") {
        value = rest;
    } else if let Some(rest) = value.strip_prefix("web_only ") {
        value = rest;
        irc = false;
    }
    let v4_only = ["listen", "listen4", "listener4"]
        .iter()
        .any(|k| k.eq_ignore_ascii_case(key));
    let v6_only = key.eq_ignore_ascii_case("listener6");
    let replaced;
    if v4_only {
        replaced = value.replace(':', " ");
        value = &replaced;
    }
    let (host, port) = match value.split_once(' ') {
        Some((host, port)) => (host, port.trim()),
        None => ("", value),
    };
    let (ssl, port) = match port.strip_prefix('+') {
        Some(port) => (true, port),
        None => (false, port),
    };
    Some(Listener {
        port: port
            .parse()
            .ok()?,
        host: host.into(),
        ssl,
        irc,
        ipv4: !v6_only,
        ipv6: !v4_only,
    })
}

/// Reads the keys `-z` uses from znc.conf text; `path` names it in errors.
pub fn parse(text: &str, path: &str) -> Result<ZncConf, ZncConfError> {
    let mut conf = ZncConf::default();
    let mut stack: Vec<(String, String)> = Vec::new();
    let mut listener: Option<Listener> = None;
    let mut commented = false;
    for (index, raw) in text
        .lines()
        .enumerate()
    {
        let line_no = index + 1;
        let syntax = |problem: String| ZncConfError::Syntax {
            path: path.into(),
            line: line_no,
            problem,
        };
        let line = raw.trim_start();
        if commented || line.starts_with("/*") {
            commented = !line.ends_with("*/");
            continue;
        }
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        if let Some(inner) = line
            .strip_prefix('<')
            .and_then(|l| l.strip_suffix('>'))
        {
            let inner = inner.trim();
            let (tag, name) = inner
                .split_once(char::is_whitespace)
                .map_or((inner, ""), |(t, n)| (t, n.trim()));
            if let Some(closing) = tag.strip_prefix('/') {
                let Some((open, _)) = stack.pop() else {
                    return Err(syntax(format!("closing tag {closing} which is not open")));
                };
                if !open.eq_ignore_ascii_case(closing) {
                    return Err(syntax(format!(
                        "closing tag {closing} while {open} is open"
                    )));
                }
                if open == "listener" {
                    if let Some(l) = listener.take() {
                        conf.listeners
                            .push(l);
                    }
                }
                continue;
            }
            if name.is_empty() {
                return Err(syntax(format!("block {tag} has no name")));
            }
            let tag = tag.to_ascii_lowercase();
            let depth: Vec<&str> = stack
                .iter()
                .map(|(t, _)| t.as_str())
                .collect();
            match (depth.as_slice(), tag.as_str()) {
                ([], "listener") => {
                    listener = Some(Listener {
                        port: 0,
                        host: String::new(),
                        ssl: false,
                        irc: true,
                        ipv4: true,
                        ipv6: true,
                    });
                }
                ([], "user") => conf
                    .users
                    .push(User {
                        name: name.into(),
                        networks: Vec::new(),
                    }),
                (["user"], "network") => {
                    if let Some(user) = conf
                        .users
                        .last_mut()
                    {
                        user.networks
                            .push(name.into());
                    }
                }
                _ => {}
            }
            stack.push((tag, name.into()));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(syntax("not a key = value line".into()));
        };
        let key = key.trim();
        let value = value
            .strip_prefix(' ')
            .unwrap_or(value);
        let depth: Vec<&str> = stack
            .iter()
            .map(|(t, _)| t.as_str())
            .collect();
        match depth.as_slice() {
            [] if key.eq_ignore_ascii_case("sslcertfile") => {
                conf.ssl_cert_file = Some(
                    value
                        .trim()
                        .into(),
                );
            }
            [] if key
                .to_ascii_lowercase()
                .starts_with("listen") =>
            {
                if let Some(l) = legacy_listener(key, value) {
                    conf.listeners
                        .push(l);
                }
            }
            ["listener"] => {
                let Some(l) = listener.as_mut() else { continue };
                match key
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "port" => {
                        l.port = value
                            .trim()
                            .parse()
                            .map_err(|_| {
                                syntax(format!("Port {:?} is not a port", value.trim()))
                            })?;
                    }
                    "host" => {
                        l.host = value
                            .trim()
                            .into()
                    }
                    "ssl" => l.ssl = truthy(value),
                    "allowirc" => l.irc = truthy(value),
                    "ipv4" => l.ipv4 = truthy(value),
                    "ipv6" => l.ipv6 = truthy(value),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if let Some((open, _)) = stack.last() {
        return Err(ZncConfError::Syntax {
            path: path.into(),
            line: text
                .lines()
                .count(),
            problem: format!("block {open} is never closed"),
        });
    }
    Ok(conf)
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "::1" | "127.0.0.1" | "localhost")
}

/// The listener the viewer should use, and the address to reach it at.
fn pick_listener(listeners: &[Listener]) -> Option<String> {
    let usable = || {
        listeners
            .iter()
            .filter(|l| l.irc && l.ssl && l.port != 0)
    };
    let chosen = usable()
        .find(|l| is_loopback(&l.host))
        .or_else(|| {
            usable().find(|l| {
                l.host
                    .is_empty()
            })
        })
        .or_else(|| usable().next())?;
    let host = match chosen
        .host
        .as_str()
    {
        "" if chosen.ipv6 => "::1",
        "" => "127.0.0.1",
        host => host,
    };
    Some(if host.contains(':') {
        format!("[{host}]:{}", chosen.port)
    } else {
        format!("{host}:{}", chosen.port)
    })
}

/// Tree prefixes holding channels whose last segment names `network`.
fn channel_paths(network: &str, channel_keys: &[String]) -> Vec<String> {
    let mut paths: Vec<String> = channel_keys
        .iter()
        .filter_map(|key| {
            key.rsplit_once('/')
                .map(|(prefix, _)| prefix)
        })
        .filter(|prefix| {
            prefix
                .rsplit('/')
                .next()
                .is_some_and(|last| last.eq_ignore_ascii_case(network))
        })
        .map(str::to_owned)
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// The SHA-256 of a DER certificate, as colon-separated lowercase hex.
pub fn fingerprint(der: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, der);
    let mut out = String::new();
    for (i, b) in digest
        .as_ref()
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

fn cert_fingerprint(path: &Path) -> Result<String, rustls::pki_types::pem::Error> {
    CertificateDer::pem_file_iter(path)?
        .next()
        .ok_or(rustls::pki_types::pem::Error::NoItemsFound)?
        .map(|der| fingerprint(&der))
}

/// Creates the client certificate at `path` unless a file is there; returns
/// its fingerprint and whether it was created.
fn ensure_client_cert(path: &Path) -> Result<(String, bool), ZncConfError> {
    let shown = || {
        path.display()
            .to_string()
    };
    let mut params =
        rcgen::CertificateParams::new(Vec::<String>::new()).map_err(ZncConfError::Generate)?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "irc-log-viewer");
    params.not_after = rcgen::date_time_ymd(2100, 1, 1);
    let key = rcgen::KeyPair::generate().map_err(ZncConfError::Generate)?;
    let cert = params
        .self_signed(&key)
        .map_err(ZncConfError::Generate)?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path);
    let mut file = match file {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let fp = cert_fingerprint(path).map_err(|source| ZncConfError::ReadCert {
                path: shown(),
                source,
            })?;
            return Ok((fp, false));
        }
        Err(source) => {
            return Err(ZncConfError::WriteCert {
                path: shown(),
                source,
            });
        }
    };
    file.write_all((cert.pem() + &key.serialize_pem()).as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|source| ZncConfError::WriteCert {
            path: shown(),
            source,
        })?;
    Ok((fingerprint(cert.der()), true))
}

/// Reads `conf_path`, creates the client certificate at `client_cert`, and
/// returns the text to print.
pub fn generate(
    conf_path: &Path,
    client_cert: &Path,
    channel_keys: &[String],
) -> Result<String, ZncConfError> {
    let shown = conf_path
        .display()
        .to_string();
    let text = std::fs::read_to_string(conf_path).map_err(|source| ZncConfError::Read {
        path: shown.clone(),
        source,
    })?;
    let conf = parse(&text, &shown)?;
    let address =
        pick_listener(&conf.listeners).ok_or_else(|| ZncConfError::NoListener(shown.clone()))?;
    let pairs: Vec<(&str, &str)> = conf
        .users
        .iter()
        .flat_map(|u| {
            u.networks
                .iter()
                .map(|n| {
                    (
                        u.name
                            .as_str(),
                        n.as_str(),
                    )
                })
        })
        .collect();
    if pairs.is_empty() {
        return Err(ZncConfError::NoNetwork(shown));
    }
    let server_pem = match &conf.ssl_cert_file {
        Some(file) => PathBuf::from(file),
        None => conf_path
            .parent()
            .and_then(Path::parent)
            .unwrap_or(Path::new("."))
            .join("znc.pem"),
    };
    let server_fp = cert_fingerprint(&server_pem).map_err(|source| ZncConfError::ReadCert {
        path: server_pem
            .display()
            .to_string(),
        source,
    })?;
    let (client_fp, created) = ensure_client_cert(client_cert)?;

    let mut out = String::new();
    let _ = writeln!(out, "# znc: section generated from {shown}");
    for (i, (user, network)) in pairs
        .iter()
        .enumerate()
    {
        let c = if i == 0 { "" } else { "#" };
        if i == 1 {
            let _ = writeln!(out, "# Other users and networks in {shown}:");
        }
        let _ = writeln!(out, "{c}znc:");
        let _ = writeln!(out, "{c}  address: \"{address}\"");
        let _ = writeln!(out, "{c}  user: {user}");
        let _ = writeln!(out, "{c}  network: {network}");
        let paths = channel_paths(network, channel_keys);
        match paths.split_first() {
            Some((first, rest)) => {
                let _ = writeln!(out, "{c}  channel_path: {first}");
                for other in rest {
                    let _ = writeln!(out, "#  channel_path: {other}");
                }
            }
            None => {
                let _ = writeln!(
                    out,
                    "#  channel_path: no channel directory under logs_dirs ends in {network}; set it by hand"
                );
            }
        }
        let _ = writeln!(out, "{c}  fingerprint: {server_fp}");
        let _ = writeln!(out, "{c}  client_cert: {}", client_cert.display());
    }
    let _ = writeln!(
        out,
        "#\n# {} {}\n# Enrol it from an IRC client logged in to ZNC as an admin:\n#   /msg *status LoadMod --type=global certauth\n#   /msg *certauth Add {client_fp}\n#   /msg *status SaveConfig\n# then restart ZNC: certauth asks for client certificates only on\n# listeners present when ZNC starts.",
        if created {
            "Created"
        } else {
            "Reusing the existing"
        },
        client_cert.display()
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tempdir;

    /// A ZNC 1.10 config in the shape `znc --makeconf` and webadmin write,
    /// with values made up for the test.
    const CONF: &str = "\
// WARNING
/* a block comment
   <User ghost>
*/
Version = 1.10.1
<Listener listener0>
\tAllowIRC = true
\tAllowWeb = true
\tIPv4 = true
\tIPv6 = true
\tPort = 7777
\tSSL = true
\tURIPrefix = /
</Listener>
<Listener listener1>
\tAllowIRC = true
\tAllowWeb = false
\tHost = ::1
\tIPv4 = false
\tIPv6 = true
\tPort = 7778
\tSSL = true
</Listener>
Listen4 = irc_only 127.0.0.1 +7000
LoadModule = webadmin
SSLCertFile = /var/lib/znc/znc.pem
<User alice>
\tAdmin = true
\tNick = alice
\tAltNick = alice_
\t<Pass password>
\t\tHash = 0000secret-hash
\t\tMethod = SHA256
\t\tSalt = secret-salt
\t</Pass>
\t<Network libera>
\t\tServer = irc.libera.chat +6697
\t\t<Chan #rust>
\t\t</Chan>
\t</Network>
\t<Network OFTC>
\t\tServer = irc.oftc.net +6697 secret-server-pass
\t</Network>
</User>
";

    #[test]
    fn reads_only_the_keys_it_uses() {
        let conf = parse(CONF, "znc.conf").unwrap();
        assert_eq!(
            conf.ssl_cert_file
                .as_deref(),
            Some("/var/lib/znc/znc.pem")
        );
        assert_eq!(
            conf.users,
            [User {
                name: "alice".into(),
                networks: vec!["libera".into(), "OFTC".into()],
            }]
        );
        assert_eq!(
            conf.listeners,
            [
                Listener {
                    port: 7777,
                    host: String::new(),
                    ssl: true,
                    irc: true,
                    ipv4: true,
                    ipv6: true
                },
                Listener {
                    port: 7778,
                    host: "::1".into(),
                    ssl: true,
                    irc: true,
                    ipv4: false,
                    ipv6: true
                },
                Listener {
                    port: 7000,
                    host: "127.0.0.1".into(),
                    ssl: true,
                    irc: true,
                    ipv4: true,
                    ipv6: false
                },
            ]
        );
        assert_eq!(
            pick_listener(&conf.listeners).as_deref(),
            Some("[::1]:7778")
        );
    }

    #[test]
    fn syntax_errors_name_the_line() {
        let err = parse("<User a>\n\tNick = a\n</Network>\n", "znc.conf").unwrap_err();
        assert_eq!(
            err.to_string(),
            "znc.conf, line 3: closing tag Network while user is open"
        );
        let err = parse("<User a>\n", "znc.conf").unwrap_err();
        assert_eq!(
            err.to_string(),
            "znc.conf, line 1: block user is never closed"
        );
        let err = parse("<Listener l>\nPort = x\n</Listener>\n", "znc.conf").unwrap_err();
        assert_eq!(
            err.to_string(),
            "znc.conf, line 2: Port \"x\" is not a port"
        );
    }

    #[test]
    fn listeners_fall_back_from_loopback_to_all_addresses() {
        let all = Listener {
            port: 7777,
            host: String::new(),
            ssl: true,
            irc: true,
            ipv4: true,
            ipv6: true,
        };
        let v4 = Listener {
            ipv6: false,
            ..all.clone()
        };
        let plain = Listener {
            port: 6667,
            host: "::1".into(),
            ssl: false,
            ..all.clone()
        };
        let web = Listener {
            port: 8443,
            host: "::1".into(),
            irc: false,
            ..all.clone()
        };
        assert_eq!(
            pick_listener(&[plain.clone(), web.clone(), all]).as_deref(),
            Some("[::1]:7777")
        );
        assert_eq!(pick_listener(&[v4]).as_deref(), Some("127.0.0.1:7777"));
        assert_eq!(pick_listener(&[plain, web]), None);
    }

    #[test]
    fn generates_the_section_and_a_private_client_cert() {
        let dir = tempdir();
        let server = rcgen::generate_simple_self_signed(vec!["host.unknown".to_owned()]).unwrap();
        let server_pem = dir
            .path()
            .join("znc.pem");
        std::fs::write(
            &server_pem,
            server
                .signing_key
                .serialize_pem()
                + &server
                    .cert
                    .pem(),
        )
        .unwrap();
        let conf_path = dir
            .path()
            .join("znc.conf");
        std::fs::write(
            &conf_path,
            CONF.replace(
                "/var/lib/znc/znc.pem",
                &server_pem
                    .display()
                    .to_string(),
            ),
        )
        .unwrap();
        let client = dir
            .path()
            .join("znc-client.pem");
        let keys = [
            "OFTC/#bcache".to_owned(),
            "OFTC/#c".to_owned(),
            "old/oftc/#x".to_owned(),
            "libera/#y".to_owned(),
        ];

        let out = generate(&conf_path, &client, &keys).unwrap();
        for secret in ["secret-hash", "secret-salt", "secret-server-pass"] {
            assert!(!out.contains(secret), "{secret} leaked: {out}");
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&client)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let client_fp = cert_fingerprint(&client).unwrap();
        assert!(
            out.contains(&format!("/msg *certauth Add {client_fp}")),
            "{out}"
        );
        assert!(out.contains("Created"), "{out}");

        // The live section parses as the viewer's config and pins the server certificate.
        let live: String = out
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| format!("{l}\n"))
            .collect();
        let config = crate::parse_config(&format!("logs_dirs: []\n{live}")).unwrap();
        let znc = config
            .znc
            .unwrap();
        assert_eq!(znc.address, "[::1]:7778");
        assert_eq!(
            (
                znc.user
                    .as_str(),
                znc.network
                    .as_str()
            ),
            ("alice", "libera")
        );
        assert_eq!(znc.channel_path(), ["libera"]);
        assert_eq!(
            znc.fingerprint,
            fingerprint(
                server
                    .cert
                    .der()
            )
        );
        assert_eq!(
            znc.client_cert
                .as_deref(),
            Some(client.as_path())
        );
        assert!(
            out.contains("#  network: OFTC\n#  channel_path: OFTC\n#  channel_path: old/oftc\n"),
            "{out}"
        );
        crate::znc::Setup::new(&znc).expect("the generated section and cert load");

        // A second run reuses the certificate instead of replacing it.
        let again = generate(&conf_path, &client, &keys).unwrap();
        assert_eq!(cert_fingerprint(&client).unwrap(), client_fp);
        assert!(again.contains("Reusing the existing"), "{again}");
        assert!(
            again.contains(&format!("/msg *certauth Add {client_fp}")),
            "{again}"
        );
    }
}
