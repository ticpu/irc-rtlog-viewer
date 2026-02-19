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
    Message { nick: String, text: String },
    Action { nick: String, text: String },
    Join { nick: String, userhost: String },
    Quit { nick: String, userhost: String, reason: String },
    Part { nick: String, userhost: String, reason: String },
    NickChange { old_nick: String, new_nick: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub time: Time,
    pub kind: LineKind,
}

impl LogLine {
    pub fn is_event(&self) -> bool {
        !matches!(self.kind, LineKind::Message { .. } | LineKind::Action { .. })
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
    let hour = s[0..2].parse().ok()?;
    let minute = s[3..5].parse().ok()?;
    let second = s[6..8].parse().ok()?;
    Some(Time { hour, minute, second })
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
    let time = parse_time(&line[11..19])?;
    let rest = &line[21..]; // skip "YYYY-MM-DDTHH:MM:SSZ "

    if let Some(rest) = rest.strip_prefix('<') {
        let end = rest.find('>')?;
        let nick = rest[..end].to_string();
        let text = rest[end + 1..].strip_prefix(' ').unwrap_or(&rest[end + 1..]).to_string();
        Some(LogLine { time, kind: LineKind::Message { nick, text } })
    } else if let Some(rest) = rest.strip_prefix("* ") {
        let space = rest.find(' ')?;
        let nick = rest[..space].to_string();
        let text = rest[space + 1..].to_string();
        Some(LogLine { time, kind: LineKind::Action { nick, text } })
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
    let time = parse_time(&line[1..9])?;
    let rest = &line[11..]; // skip "[HH:MM:SS] "

    if let Some(rest) = rest.strip_prefix('<') {
        let end = rest.find('>')?;
        let nick = rest[..end].to_string();
        let text = rest[end + 1..].strip_prefix(' ').unwrap_or(&rest[end + 1..]).to_string();
        Some(LogLine { time, kind: LineKind::Message { nick, text } })
    } else if let Some(event_rest) = rest.strip_prefix("*** ") {
        parse_znc_event(time, event_rest)
    } else if let Some(rest) = rest.strip_prefix("* ") {
        let space = rest.find(' ')?;
        let nick = rest[..space].to_string();
        let text = rest[space + 1..].to_string();
        Some(LogLine { time, kind: LineKind::Action { nick, text } })
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
        Some(LogLine { time, kind: LineKind::Join { nick, userhost } })
    } else if let Some(rest) = rest.strip_prefix("Quits: ") {
        // nick (~user@host) (reason)
        parse_quit_or_part(time, rest, true)
    } else if let Some(rest) = rest.strip_prefix("Parts: ") {
        // nick (~user@host) (reason)
        parse_quit_or_part(time, rest, false)
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

fn parse_quit_or_part(time: Time, rest: &str, is_quit: bool) -> Option<LogLine> {
    // nick (~user@host) (reason)
    let paren = rest.find(" (")?;
    let nick = rest[..paren].to_string();
    let close = rest.find(')')?;
    let userhost = rest[paren + 2..close].to_string();

    let reason_part = &rest[close + 1..];
    let reason = if let Some(reason_part) = reason_part.strip_prefix(" (") {
        reason_part.strip_suffix(')').unwrap_or(reason_part).to_string()
    } else {
        String::new()
    };

    let kind = if is_quit {
        LineKind::Quit { nick, userhost, reason }
    } else {
        LineKind::Part { nick, userhost, reason }
    };
    Some(LogLine { time, kind })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_format() {
        assert_eq!(detect_format("[12:34:56] <nick> hi"), LogFormat::Znc);
        assert_eq!(detect_format("2025-02-01T12:18:17Z <nick> hi"), LogFormat::Iso8601);
    }

    #[test]
    fn test_iso8601_message() {
        let line = "2025-02-01T12:18:17Z <py1hon> hello world";
        let parsed = parse_line(line, LogFormat::Iso8601).unwrap();
        assert_eq!(parsed.time, Time { hour: 12, minute: 18, second: 17 });
        assert_eq!(parsed.kind, LineKind::Message {
            nick: "py1hon".into(),
            text: "hello world".into(),
        });
    }

    #[test]
    fn test_iso8601_action() {
        let line = "2025-02-01T12:18:17Z * py1hon waves";
        let parsed = parse_line(line, LogFormat::Iso8601).unwrap();
        assert_eq!(parsed.kind, LineKind::Action {
            nick: "py1hon".into(),
            text: "waves".into(),
        });
    }

    #[test]
    fn test_znc_message() {
        let line = "[05:06:37] <LordKitsuna> hello world";
        let parsed = parse_line(line, LogFormat::Znc).unwrap();
        assert_eq!(parsed.time, Time { hour: 5, minute: 6, second: 37 });
        assert_eq!(parsed.kind, LineKind::Message {
            nick: "LordKitsuna".into(),
            text: "hello world".into(),
        });
    }

    #[test]
    fn test_znc_join() {
        let line = "[00:07:04] *** Joins: lxdr046342 (~lxdr@51-15-4-54.rev.poneytelecom.eu)";
        let parsed = parse_line(line, LogFormat::Znc).unwrap();
        assert_eq!(parsed.kind, LineKind::Join {
            nick: "lxdr046342".into(),
            userhost: "~lxdr@51-15-4-54.rev.poneytelecom.eu".into(),
        });
    }

    #[test]
    fn test_znc_quit() {
        let line = "[00:06:44] *** Quits: lxdr046342 (~lxdr@51-15-4-54.rev.poneytelecom.eu) (Remote host closed the connection)";
        let parsed = parse_line(line, LogFormat::Znc).unwrap();
        assert_eq!(parsed.kind, LineKind::Quit {
            nick: "lxdr046342".into(),
            userhost: "~lxdr@51-15-4-54.rev.poneytelecom.eu".into(),
            reason: "Remote host closed the connection".into(),
        });
    }

    #[test]
    fn test_znc_part() {
        let line = "[02:28:40] *** Parts: dza (~dza@0002ef68.user.oftc.net) (Connection pool? That's gross. Someone might have peed in that.)";
        let parsed = parse_line(line, LogFormat::Znc).unwrap();
        assert_eq!(parsed.kind, LineKind::Part {
            nick: "dza".into(),
            userhost: "~dza@0002ef68.user.oftc.net".into(),
            reason: "Connection pool? That's gross. Someone might have peed in that.".into(),
        });
    }

    #[test]
    fn test_znc_nick_change() {
        let line = "[04:43:20] *** therobin is now known as Guest2176";
        let parsed = parse_line(line, LogFormat::Znc).unwrap();
        assert_eq!(parsed.kind, LineKind::NickChange {
            old_nick: "therobin".into(),
            new_nick: "Guest2176".into(),
        });
    }

    #[test]
    fn test_znc_action() {
        let line = "[12:34:56] * nick does something";
        let parsed = parse_line(line, LogFormat::Znc).unwrap();
        assert_eq!(parsed.kind, LineKind::Action {
            nick: "nick".into(),
            text: "does something".into(),
        });
    }

    #[test]
    fn test_time_display() {
        let t = Time { hour: 5, minute: 6, second: 7 };
        assert_eq!(t.to_hms(), "05:06:07");
        assert_eq!(t.to_anchor(), "T050607");
    }

    #[test]
    fn test_is_event() {
        let msg = LogLine {
            time: Time { hour: 0, minute: 0, second: 0 },
            kind: LineKind::Message { nick: "a".into(), text: "b".into() },
        };
        assert!(!msg.is_event());

        let join = LogLine {
            time: Time { hour: 0, minute: 0, second: 0 },
            kind: LineKind::Join { nick: "a".into(), userhost: "b".into() },
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
