use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Iso8601,
    Znc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Time {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl Time {
    pub fn to_hms(self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }

    pub fn to_anchor(self) -> String {
        format!("T{:02}{:02}{:02}", self.hour, self.minute, self.second)
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineKind {
    Message {
        nick: String,
        text: String,
    },
    Action {
        nick: String,
        text: String,
    },
    Join {
        nick: String,
        userhost: String,
    },
    Quit {
        nick: String,
        userhost: String,
        reason: String,
    },
    Part {
        nick: String,
        userhost: String,
        reason: String,
    },
    NickChange {
        old_nick: String,
        new_nick: String,
    },
    /// `*** X was kicked by Y (reason)`.
    Kick {
        nick: String,
        by: String,
        reason: String,
    },
    /// `*** X changes topic to '<topic>'`.
    Topic {
        nick: String,
        topic: String,
    },
    /// `*** X sets mode: <modes> <args>`, X being a nick, a server or `Server`.
    Mode {
        nick: String,
        modes: String,
        args: Vec<String>,
    },
    /// Any other `***` server or channel notice.
    Notice {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub time: Time,
    pub kind: LineKind,
}

impl LogLine {
    pub fn is_event(&self) -> bool {
        !matches!(
            self.kind,
            LineKind::Message { .. } | LineKind::Action { .. }
        )
    }
}

pub fn detect_format(first_line: &str) -> LogFormat {
    if first_line.starts_with('[') {
        LogFormat::Znc
    } else {
        LogFormat::Iso8601
    }
}

fn parse_time(s: &str) -> Option<Time> {
    if s.len() < 8 {
        return None;
    }
    let hour = s
        .get(0..2)?
        .parse()
        .ok()?;
    let minute = s
        .get(3..5)?
        .parse()
        .ok()?;
    let second = s
        .get(6..8)?
        .parse()
        .ok()?;
    Some(Time {
        hour,
        minute,
        second,
    })
}

pub fn parse_line(line: &str, format: LogFormat) -> Option<LogLine> {
    match format {
        LogFormat::Iso8601 => parse_iso8601(line),
        LogFormat::Znc => parse_znc(line),
    }
}

fn parse_iso8601(line: &str) -> Option<LogLine> {
    // Format: 2025-02-01T12:18:17Z <nick> msg
    // Time is at bytes [11..19]
    if line.len() < 21 {
        return None;
    }
    let time = parse_time(line.get(11..19)?)?;
    let rest = line.get(21..)?; // skip "YYYY-MM-DDTHH:MM:SSZ "

    if let Some(rest) = rest.strip_prefix('<') {
        let end = rest.find('>')?;
        let nick = rest[..end].to_string();
        let text = rest[end + 1..]
            .strip_prefix(' ')
            .unwrap_or(&rest[end + 1..])
            .to_string();
        Some(LogLine {
            time,
            kind: LineKind::Message { nick, text },
        })
    } else if let Some(rest) = rest.strip_prefix("* ") {
        let space = rest.find(' ')?;
        let nick = rest[..space].to_string();
        let text = rest[space + 1..].to_string();
        Some(LogLine {
            time,
            kind: LineKind::Action { nick, text },
        })
    } else {
        None
    }
}

fn parse_znc(line: &str) -> Option<LogLine> {
    // Format: [HH:MM:SS] <nick> msg
    // Time is at bytes [1..9]
    if line.len() < 11 || !line.starts_with('[') {
        return None;
    }
    let time = parse_time(line.get(1..9)?)?;
    let rest = line.get(11..)?; // skip "[HH:MM:SS] "

    if let Some(rest) = rest.strip_prefix('<') {
        let end = rest.find('>')?;
        let nick = rest[..end].to_string();
        let text = rest[end + 1..]
            .strip_prefix(' ')
            .unwrap_or(&rest[end + 1..])
            .to_string();
        Some(LogLine {
            time,
            kind: LineKind::Message { nick, text },
        })
    } else if let Some(event_rest) = rest.strip_prefix("*** ") {
        parse_znc_event(time, event_rest).or_else(|| {
            Some(LogLine {
                time,
                kind: LineKind::Notice {
                    text: event_rest.to_string(),
                },
            })
        })
    } else if let Some(rest) = rest.strip_prefix("* ") {
        let space = rest.find(' ')?;
        let nick = rest[..space].to_string();
        let text = rest[space + 1..].to_string();
        Some(LogLine {
            time,
            kind: LineKind::Action { nick, text },
        })
    } else {
        None
    }
}

fn parse_znc_event(time: Time, rest: &str) -> Option<LogLine> {
    if let Some(rest) = rest.strip_prefix("Joins: ") {
        // nick (~user@host)
        let paren = rest.find(" (")?;
        let nick = rest[..paren].to_string();
        let end = rest.find(')')?;
        let userhost = rest[paren + 2..end].to_string();
        Some(LogLine {
            time,
            kind: LineKind::Join { nick, userhost },
        })
    } else if let Some(rest) = rest.strip_prefix("Quits: ") {
        // nick (~user@host) (reason)
        parse_quit_or_part(time, rest, true)
    } else if let Some(rest) = rest.strip_prefix("Parts: ") {
        // nick (~user@host) (reason)
        parse_quit_or_part(time, rest, false)
    } else if let Some(kind) = parse_nick_event(rest) {
        Some(LogLine { time, kind })
    } else if let Some(pos) = rest.find(" is now known as ") {
        let old_nick = rest[..pos].to_string();
        let new_nick = rest[pos + 17..].to_string();
        Some(LogLine {
            time,
            kind: LineKind::NickChange { old_nick, new_nick },
        })
    } else {
        None
    }
}

/// Kick, topic and mode lines, anchored on the leading nick, which has no space.
fn parse_nick_event(rest: &str) -> Option<LineKind> {
    let (nick, rest) = rest.split_once(' ')?;
    if nick.is_empty() {
        return None;
    }
    let nick = nick.to_string();
    if let Some(rest) = rest.strip_prefix("was kicked by ") {
        let (by, reason_part) = rest
            .split_once(' ')
            .unwrap_or((rest, ""));
        if by.is_empty() {
            return None;
        }
        let reason = if reason_part.is_empty() {
            ""
        } else {
            let reason_part = reason_part.strip_prefix('(')?;
            reason_part
                .strip_suffix(')')
                .unwrap_or(reason_part)
        };
        Some(LineKind::Kick {
            nick,
            by: by.to_string(),
            reason: reason.to_string(),
        })
    } else if let Some(rest) = rest.strip_prefix("changes topic to ") {
        let topic = rest
            .strip_prefix('\'')?
            .strip_suffix('\'')?;
        Some(LineKind::Topic {
            nick,
            topic: topic.to_string(),
        })
    } else if let Some(rest) = rest.strip_prefix("sets mode: ") {
        let mut words = rest
            .split(' ')
            .filter(|word| !word.is_empty());
        let modes = words
            .next()?
            .to_string();
        let args = words
            .map(str::to_string)
            .collect();
        Some(LineKind::Mode { nick, modes, args })
    } else {
        None
    }
}

fn parse_quit_or_part(time: Time, rest: &str, is_quit: bool) -> Option<LogLine> {
    // nick (~user@host) (reason)
    let paren = rest.find(" (")?;
    let nick = rest[..paren].to_string();
    let close = rest.find(')')?;
    let userhost = rest[paren + 2..close].to_string();

    let reason_part = &rest[close + 1..];
    let reason = if let Some(reason_part) = reason_part.strip_prefix(" (") {
        reason_part
            .strip_suffix(')')
            .unwrap_or(reason_part)
            .to_string()
    } else {
        String::new()
    };

    let kind = if is_quit {
        LineKind::Quit {
            nick,
            userhost,
            reason,
        }
    } else {
        LineKind::Part {
            nick,
            userhost,
            reason,
        }
    };
    Some(LogLine { time, kind })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn znc_kind(line: &str) -> LineKind {
        parse_line(line, LogFormat::Znc)
            .unwrap()
            .kind
    }

    fn mode(nick: &str, modes: &str, args: &[&str]) -> LineKind {
        LineKind::Mode {
            nick: nick.into(),
            modes: modes.into(),
            args: args
                .iter()
                .map(|arg| arg.to_string())
                .collect(),
        }
    }

    #[test]
    fn test_znc_unrecognised_notices_are_events() {
        for line in [
            "[15:32:11] *** Server sets mode: +nt ",
            "[12:37:25] *** larich.oftc.net sets mode: +o py1hon",
            "[01:02:03] *** Joins: malformed-without-host",
        ] {
            let parsed = parse_line(line, LogFormat::Znc).unwrap();
            assert!(parsed.is_event(), "{line}");
        }
        assert_eq!(
            znc_kind("[01:02:03] *** Joins: malformed-without-host"),
            LineKind::Notice {
                text: "Joins: malformed-without-host".into(),
            }
        );
        let message = parse_line("[01:02:03] <a> *** not a notice", LogFormat::Znc).unwrap();
        assert!(!message.is_event());
    }

    #[test]
    fn test_znc_mode() {
        assert_eq!(
            znc_kind("[15:32:11] *** Server sets mode: +nt "),
            mode("Server", "+nt", &[])
        );
        assert_eq!(
            znc_kind("[01:49:26] *** Server sets mode: +nst "),
            mode("Server", "+nst", &[])
        );
        assert_eq!(
            znc_kind("[12:37:25] *** larich.oftc.net sets mode: +o py1hon"),
            mode("larich.oftc.net", "+o", &["py1hon"])
        );
        assert_eq!(
            znc_kind("[10:00:00] *** ChanServ sets mode: +o-v alice bob"),
            mode("ChanServ", "+o-v", &["alice", "bob"])
        );
    }

    #[test]
    fn test_znc_topic() {
        assert_eq!(
            znc_kind(
                "[19:30:47] *** py1hon changes topic to 'https://evilpiepirate.org/forge/kent/consciousness | https://poc.bcachefs.org/paper.pdf'"
            ),
            LineKind::Topic {
                nick: "py1hon".into(),
                topic: "https://evilpiepirate.org/forge/kent/consciousness | https://poc.bcachefs.org/paper.pdf".into(),
            }
        );
        assert_eq!(
            znc_kind(
                "[10:00:00] *** op changes topic to ''quoted' (x) a was kicked by b (c) is now known as d'"
            ),
            LineKind::Topic {
                nick: "op".into(),
                topic: "'quoted' (x) a was kicked by b (c) is now known as d".into(),
            }
        );
    }

    #[test]
    fn test_znc_kick() {
        let kick = |by: &str, reason: &str| LineKind::Kick {
            nick: "spammer".into(),
            by: by.into(),
            reason: reason.into(),
        };
        assert_eq!(
            znc_kind("[10:00:00] *** spammer was kicked by ChanServ (Banned: spam)"),
            kick("ChanServ", "Banned: spam")
        );
        assert_eq!(
            znc_kind("[10:00:00] *** spammer was kicked by op ()"),
            kick("op", "")
        );
        assert_eq!(
            znc_kind("[10:00:00] *** spammer was kicked by op (flood (again))"),
            kick("op", "flood (again)")
        );
        assert_eq!(
            znc_kind("[10:00:00] *** spammer was kicked by op"),
            kick("op", "")
        );
    }

    #[test]
    fn test_znc_nick_events_need_the_prefix_shape() {
        assert_eq!(
            znc_kind("[01:02:03] <a> *** x sets mode: +o y"),
            LineKind::Message {
                nick: "a".into(),
                text: "*** x sets mode: +o y".into(),
            }
        );
        assert_eq!(
            znc_kind("[01:02:03] *** two words sets mode: +o y"),
            LineKind::Notice {
                text: "two words sets mode: +o y".into(),
            }
        );
    }

    #[test]
    fn test_znc_truncated_nick_events_are_notices() {
        for text in [
            "x sets mode:",
            "x sets mode: ",
            "x changes topic to '",
            "x changes topic to",
            "x was kicked by ",
            "x was kicked by op junk",
            "x",
        ] {
            let line = format!("[01:02:03] *** {text}");
            let parsed = parse_line(&line, LogFormat::Znc).unwrap();
            assert!(parsed.is_event(), "{line}");
            assert_eq!(parsed.kind, LineKind::Notice { text: text.into() });
        }
        assert_eq!(
            znc_kind("[01:02:03] *** x was kicked by op ("),
            LineKind::Kick {
                nick: "x".into(),
                by: "op".into(),
                reason: String::new(),
            }
        );
    }

    #[test]
    fn test_multibyte_in_timestamp_is_rejected() {
        for line in [
            "2025-02-01T12:1\u{e9}:17Z <a> b",
            "2025-02-01T\u{e9}2:18:1 <a> b",
            "2025-02-01T12:18:17Z\u{e9}<a> b",
        ] {
            assert_eq!(parse_line(line, LogFormat::Iso8601), None, "{line}");
        }
        for line in [
            "[12:3\u{e9}:56] <a> b",
            "[12:34:5\u{e9}] <a> b",
            "[12:34:56]\u{e9}<a> b",
        ] {
            assert_eq!(parse_line(line, LogFormat::Znc), None, "{line}");
        }
    }

    #[test]
    fn test_detect_format() {
        assert_eq!(detect_format("[12:34:56] <nick> hi"), LogFormat::Znc);
        assert_eq!(
            detect_format("2025-02-01T12:18:17Z <nick> hi"),
            LogFormat::Iso8601
        );
    }

    #[test]
    fn test_iso8601_message() {
        let line = "2025-02-01T12:18:17Z <py1hon> hello world";
        let parsed = parse_line(line, LogFormat::Iso8601).unwrap();
        assert_eq!(
            parsed.time,
            Time {
                hour: 12,
                minute: 18,
                second: 17
            }
        );
        assert_eq!(
            parsed.kind,
            LineKind::Message {
                nick: "py1hon".into(),
                text: "hello world".into(),
            }
        );
    }

    #[test]
    fn test_iso8601_action() {
        let line = "2025-02-01T12:18:17Z * py1hon waves";
        let parsed = parse_line(line, LogFormat::Iso8601).unwrap();
        assert_eq!(
            parsed.kind,
            LineKind::Action {
                nick: "py1hon".into(),
                text: "waves".into(),
            }
        );
    }

    #[test]
    fn test_znc_message() {
        let line = "[05:06:37] <LordKitsuna> hello world";
        let parsed = parse_line(line, LogFormat::Znc).unwrap();
        assert_eq!(
            parsed.time,
            Time {
                hour: 5,
                minute: 6,
                second: 37
            }
        );
        assert_eq!(
            parsed.kind,
            LineKind::Message {
                nick: "LordKitsuna".into(),
                text: "hello world".into(),
            }
        );
    }

    #[test]
    fn test_znc_join() {
        let line = "[00:07:04] *** Joins: lxdr046342 (~lxdr@51-15-4-54.rev.poneytelecom.eu)";
        let parsed = parse_line(line, LogFormat::Znc).unwrap();
        assert_eq!(
            parsed.kind,
            LineKind::Join {
                nick: "lxdr046342".into(),
                userhost: "~lxdr@51-15-4-54.rev.poneytelecom.eu".into(),
            }
        );
    }

    #[test]
    fn test_znc_quit() {
        let line = "[00:06:44] *** Quits: lxdr046342 (~lxdr@51-15-4-54.rev.poneytelecom.eu) (Remote host closed the connection)";
        let parsed = parse_line(line, LogFormat::Znc).unwrap();
        assert_eq!(
            parsed.kind,
            LineKind::Quit {
                nick: "lxdr046342".into(),
                userhost: "~lxdr@51-15-4-54.rev.poneytelecom.eu".into(),
                reason: "Remote host closed the connection".into(),
            }
        );
    }

    #[test]
    fn test_znc_part() {
        let line = "[02:28:40] *** Parts: dza (~dza@0002ef68.user.oftc.net) (Connection pool? That's gross. Someone might have peed in that.)";
        let parsed = parse_line(line, LogFormat::Znc).unwrap();
        assert_eq!(
            parsed.kind,
            LineKind::Part {
                nick: "dza".into(),
                userhost: "~dza@0002ef68.user.oftc.net".into(),
                reason: "Connection pool? That's gross. Someone might have peed in that.".into(),
            }
        );
    }

    #[test]
    fn test_znc_nick_change() {
        let line = "[04:43:20] *** therobin is now known as Guest2176";
        let parsed = parse_line(line, LogFormat::Znc).unwrap();
        assert_eq!(
            parsed.kind,
            LineKind::NickChange {
                old_nick: "therobin".into(),
                new_nick: "Guest2176".into(),
            }
        );
    }

    #[test]
    fn test_znc_action() {
        let line = "[12:34:56] * nick does something";
        let parsed = parse_line(line, LogFormat::Znc).unwrap();
        assert_eq!(
            parsed.kind,
            LineKind::Action {
                nick: "nick".into(),
                text: "does something".into(),
            }
        );
    }

    #[test]
    fn test_time_display() {
        let t = Time {
            hour: 5,
            minute: 6,
            second: 7,
        };
        assert_eq!(t.to_hms(), "05:06:07");
        assert_eq!(t.to_anchor(), "T050607");
    }

    #[test]
    fn test_is_event() {
        let msg = LogLine {
            time: Time {
                hour: 0,
                minute: 0,
                second: 0,
            },
            kind: LineKind::Message {
                nick: "a".into(),
                text: "b".into(),
            },
        };
        assert!(!msg.is_event());

        let join = LogLine {
            time: Time {
                hour: 0,
                minute: 0,
                second: 0,
            },
            kind: LineKind::Join {
                nick: "a".into(),
                userhost: "b".into(),
            },
        };
        assert!(join.is_event());
    }

    #[test]
    fn test_iso8601_url_message() {
        let line = "2026-02-10T00:06:39Z <py1hon> is anyone here a cachyos user? https://discuss.cachyos.org/t/bcachefs-nvme-drive-fstab-no-longer-exists-wont-boot/22666";
        let parsed = parse_line(line, LogFormat::Iso8601).unwrap();
        if let LineKind::Message { nick, text } = &parsed.kind {
            assert_eq!(nick, "py1hon");
            assert!(text.contains("https://"));
        } else {
            panic!("expected message");
        }
    }
}
