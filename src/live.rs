//! Who is in a channel right now: a snapshot from a ZNC session's burst,
//! kept current by the log lines the tail reads after it.
//!
//! # A snapshot and its deltas
//!
//! A [`LiveChannel`] starts from one channel's burst (353 names with every
//! prefix under multi-prefix, 332 and 333 for the topic) and is moved forward
//! by [`LiveChannel::apply`] for each later log line: a join adds a member, a
//! part, quit or kick removes one, a nick change renames one and keeps its
//! prefixes, a mode change adds or drops prefixes, a topic line replaces the
//! topic. Every delta is idempotent given the order of the lines, so
//! replaying a line the burst already reflected leaves the same state. The
//! presence actor relies on that to replay lines that arrived during a
//! session on top of the burst.
//!
//! # Modes follow the network's ISUPPORT
//!
//! Which mode letters are member prefixes, and which channel modes take an
//! argument, come from the 005 PREFIX and CHANMODES tokens of the session
//! that produced the snapshot ([`ModeSpec`]). Without them the RFC 2811
//! defaults apply. Walking a mode line with the wrong table pairs arguments
//! with the wrong letters, so a delta never guesses: a mode letter the table
//! does not know stops the walk for that line, logged.
//!
//! # Keys
//!
//! Members are keyed by the nick under rfc1459 casemapping, as archive users
//! are, so the endpoint can join the two.

use std::collections::HashMap;

use jiff::Timestamp;

use crate::parser::LineKind;
use crate::presence::{Topic, casemap};

/// The member prefixes and the argument-taking channel modes of a network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeSpec {
    /// Prefix modes from highest rank to lowest, as (mode letter, prefix char).
    pub prefixes: Vec<(char, char)>,
    /// CHANMODES type A (list) and B (always an argument).
    pub always_arg: String,
    /// CHANMODES type C (an argument only when set).
    pub set_arg: String,
    /// CHANMODES type D (never an argument).
    pub no_arg: String,
}

impl Default for ModeSpec {
    /// RFC 2811: `(ov)@+`, and CHANMODES `beI,k,l,imnpst`.
    fn default() -> Self {
        Self {
            prefixes: vec![('o', '@'), ('v', '+')],
            always_arg: "beIk".into(),
            set_arg: "l".into(),
            no_arg: "imnpst".into(),
        }
    }
}

impl ModeSpec {
    /// Applies a 005 token to the table; returns false when it is not one
    /// this cares about or does not parse.
    pub fn apply_isupport(&mut self, token: &str) -> bool {
        if let Some(value) = token.strip_prefix("PREFIX=") {
            let Some((modes, chars)) = value
                .strip_prefix('(')
                .and_then(|v| v.split_once(')'))
            else {
                return false;
            };
            if modes
                .chars()
                .count()
                != chars
                    .chars()
                    .count()
            {
                return false;
            }
            self.prefixes = modes
                .chars()
                .zip(chars.chars())
                .collect();
            return true;
        }
        if let Some(value) = token.strip_prefix("CHANMODES=") {
            let groups: Vec<&str> = value
                .split(',')
                .collect();
            if groups.len() < 4 {
                return false;
            }
            self.always_arg = format!("{}{}", groups[0], groups[1]);
            self.set_arg = groups[2].into();
            self.no_arg = groups[3].into();
            return true;
        }
        false
    }

    fn rank(&self, prefix: char) -> Option<usize> {
        self.prefixes
            .iter()
            .position(|&(_, p)| p == prefix)
    }

    fn prefix_for_mode(&self, mode: char) -> Option<char> {
        self.prefixes
            .iter()
            .find(|&&(m, _)| m == mode)
            .map(|&(_, p)| p)
    }

    /// Splits a 353 entry such as `@+nick` into its prefixes and nick.
    pub fn split_names_entry<'a>(&self, entry: &'a str) -> (String, &'a str) {
        let nick = entry.trim_start_matches(|c| {
            self.rank(c)
                .is_some()
        });
        let prefixes = &entry[..entry.len() - nick.len()];
        (self.sorted(prefixes.chars()), nick)
    }

    /// Prefix chars deduplicated and in rank order.
    fn sorted(&self, chars: impl Iterator<Item = char>) -> String {
        let mut ranked: Vec<(usize, char)> = chars
            .filter_map(|c| {
                self.rank(c)
                    .map(|r| (r, c))
            })
            .collect();
        ranked.sort_unstable();
        ranked.dedup();
        ranked
            .into_iter()
            .map(|(_, c)| c)
            .collect()
    }
}

/// One member of a live channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Display nick.
    pub nick: String,
    /// Prefix chars in rank order, such as `@+`.
    pub prefixes: String,
}

/// A channel's live membership and topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveChannel {
    /// Members keyed by rfc1459-casemapped nick.
    pub members: HashMap<String, Member>,
    /// Topic from 332 and 333, then from topic lines.
    pub topic: Option<Topic>,
    /// When the burst this started from was received.
    pub snapshot_at: Timestamp,
    /// Whether the bouncer may have left the channel since: its session
    /// failed, its network reconnected, or the last session had no JOIN for it.
    pub stale: bool,
    /// The table mode lines are read with.
    pub modes: ModeSpec,
}

impl LiveChannel {
    /// A snapshot from one burst: 353 `names` entries with their prefixes.
    pub fn from_burst<'a>(
        names: impl IntoIterator<Item = &'a str>,
        topic: Option<Topic>,
        snapshot_at: Timestamp,
        modes: ModeSpec,
    ) -> Self {
        let mut members = HashMap::new();
        for entry in names {
            let (prefixes, nick) = modes.split_names_entry(entry);
            if nick.is_empty() {
                continue;
            }
            members.insert(
                casemap(nick),
                Member {
                    nick: nick.to_owned(),
                    prefixes,
                },
            );
        }
        Self {
            members,
            topic,
            snapshot_at,
            stale: false,
            modes,
        }
    }

    /// Moves the snapshot forward by one log line stamped `at`.
    pub fn apply(&mut self, at: Timestamp, kind: &LineKind) {
        match kind {
            LineKind::Join { nick, .. } => {
                self.members
                    .entry(casemap(nick))
                    .or_insert_with(|| Member {
                        nick: nick.clone(),
                        prefixes: String::new(),
                    });
            }
            LineKind::Part { nick, .. }
            | LineKind::Quit { nick, .. }
            | LineKind::Kick { nick, .. } => {
                self.members
                    .remove(&casemap(nick));
            }
            LineKind::NickChange { old_nick, new_nick } => {
                if let Some(mut member) = self
                    .members
                    .remove(&casemap(old_nick))
                {
                    new_nick.clone_into(&mut member.nick);
                    self.members
                        .insert(casemap(new_nick), member);
                }
            }
            LineKind::Mode { modes, args, .. } => self.apply_modes(modes, args),
            LineKind::Topic { nick, topic } => {
                self.topic = Some(Topic {
                    nick: nick.clone(),
                    text: topic.clone(),
                    at,
                });
            }
            LineKind::Message { .. } | LineKind::Action { .. } | LineKind::Notice { .. } => {}
        }
    }

    fn apply_modes(&mut self, modes: &str, args: &[String]) {
        let mut adding = true;
        let mut args = args.iter();
        for mode in modes.chars() {
            match mode {
                '+' => adding = true,
                '-' => adding = false,
                _ => {
                    if let Some(prefix) = self
                        .modes
                        .prefix_for_mode(mode)
                    {
                        let Some(target) = args.next() else {
                            return;
                        };
                        self.set_prefix(target, prefix, adding);
                    } else if self
                        .modes
                        .always_arg
                        .contains(mode)
                        || (adding
                            && self
                                .modes
                                .set_arg
                                .contains(mode))
                    {
                        args.next();
                    } else if !self
                        .modes
                        .no_arg
                        .contains(mode)
                        && !self
                            .modes
                            .set_arg
                            .contains(mode)
                    {
                        eprintln!(
                            "live: mode {mode:?} of {modes:?} is not in the network's CHANMODES or PREFIX; ignoring the rest of the line"
                        );
                        return;
                    }
                }
            }
        }
    }

    fn set_prefix(&mut self, nick: &str, prefix: char, adding: bool) {
        let Some(member) = self
            .members
            .get_mut(&casemap(nick))
        else {
            return;
        };
        let current = member
            .prefixes
            .chars()
            .filter(|&c| c != prefix);
        member.prefixes = if adding {
            self.modes
                .sorted(current.chain([prefix]))
        } else {
            current.collect()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse()
            .unwrap()
    }

    fn snapshot(names: &[&str]) -> LiveChannel {
        let mut modes = ModeSpec::default();
        assert!(modes.apply_isupport("PREFIX=(qaohv)~&@%+"));
        assert!(modes.apply_isupport("CHANMODES=beI,kfL,lj,psmntirRcOAQKVCuzNSMTGZ"));
        LiveChannel::from_burst(
            names
                .iter()
                .copied(),
            None,
            ts("2026-01-01T00:00:00Z"),
            modes,
        )
    }

    fn prefixes(live: &LiveChannel) -> Vec<(String, String)> {
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

    fn mode(modes: &str, args: &[&str]) -> LineKind {
        LineKind::Mode {
            nick: "op".into(),
            modes: modes.into(),
            args: args
                .iter()
                .map(|a| a.to_string())
                .collect(),
        }
    }

    #[test]
    fn burst_entries_keep_every_prefix_in_rank_order() {
        let live = snapshot(&["+@Alice", "~bob", "carol", "%+[dave]"]);
        assert_eq!(
            prefixes(&live),
            [
                ("Alice".into(), "@+".into()),
                ("[dave]".into(), "%+".into()),
                ("bob".into(), "~".into()),
                ("carol".into(), String::new()),
            ]
        );
        assert!(
            live.members
                .contains_key("{dave}")
        );
    }

    #[test]
    fn mode_lines_consume_arguments_by_chanmodes_type() {
        let mut live = snapshot(&["alice", "bob", "carol"]);
        let at = ts("2026-01-01T00:00:01Z");
        // b, k, l (when set) and f take an argument, so each target lands on the right nick.
        live.apply(
            at,
            &mode("+bokvlh", &["*!*@x", "alice", "key", "bob", "10", "carol"]),
        );
        live.apply(at, &mode("-l+fv-o", &["3:5", "alice", "alice"]));
        assert_eq!(
            prefixes(&live),
            [
                ("alice".into(), "+".into()),
                ("bob".into(), "+".into()),
                ("carol".into(), "%".into()),
            ]
        );
        // An unknown letter stops the walk rather than misplace the arguments.
        live.apply(at, &mode("+Xo", &["?", "carol"]));
        assert_eq!(live.members["carol"].prefixes, "%");
    }

    #[test]
    fn deltas_track_membership_and_replay_idempotently() {
        let at = ts("2026-01-01T00:00:02Z");
        let lines = [
            LineKind::Join {
                nick: "Eve".into(),
                userhost: "~e@h".into(),
            },
            mode("+v", &["eve"]),
            LineKind::NickChange {
                old_nick: "EVE".into(),
                new_nick: "Eve2".into(),
            },
            LineKind::Quit {
                nick: "alice".into(),
                userhost: "~a@h".into(),
                reason: String::new(),
            },
            LineKind::Kick {
                nick: "bob".into(),
                by: "op".into(),
                reason: String::new(),
            },
            LineKind::Part {
                nick: "nobody".into(),
                userhost: "~n@h".into(),
                reason: String::new(),
            },
            LineKind::Topic {
                nick: "op".into(),
                topic: "new".into(),
            },
        ];
        let mut once = snapshot(&["alice", "@bob", "carol"]);
        for line in &lines {
            once.apply(at, line);
        }
        assert_eq!(
            prefixes(&once),
            [("Eve2".into(), "+".into()), ("carol".into(), String::new()),]
        );
        assert_eq!(
            once.topic,
            Some(Topic {
                nick: "op".into(),
                text: "new".into(),
                at,
            })
        );
        let mut twice = once.clone();
        for line in &lines {
            twice.apply(at, line);
        }
        assert_eq!(twice, once);
    }

    #[test]
    fn malformed_isupport_tokens_keep_the_defaults() {
        let mut modes = ModeSpec::default();
        assert!(!modes.apply_isupport("PREFIX=(ov)@"));
        assert!(!modes.apply_isupport("PREFIX=ov@+"));
        assert!(!modes.apply_isupport("CHANMODES=b,k,l"));
        assert!(!modes.apply_isupport("NETWORK=OFTC"));
        assert_eq!(modes, ModeSpec::default());
    }
}
