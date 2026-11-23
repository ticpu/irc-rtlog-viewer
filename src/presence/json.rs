//! The presence endpoint's JSON body.

use std::collections::{BTreeMap, HashMap};

use jiff::Timestamp;
use serde::Serialize;

use super::{ChannelPresence, Topic};
use crate::live::{LiveChannel, ModeSpec};

#[derive(Serialize)]
struct PresenceJson<'a> {
    scanning: bool,
    snapshot_at: Option<Timestamp>,
    stale: bool,
    topic: Option<TopicJson<'a>>,
    /// Channel modes such as `+lnt 50`, the +k key left out.
    modes: Option<String>,
    users: BTreeMap<&'a str, UserJson<'a>>,
}

#[derive(Serialize)]
struct TopicJson<'a> {
    source: &'static str,
    #[serde(flatten)]
    topic: &'a Topic,
}

#[derive(Serialize)]
struct UserJson<'a> {
    /// In the channel now; `None` without a snapshot.
    live: Option<bool>,
    prefixes: &'a str,
    online: Option<Timestamp>,
    offline: Option<Timestamp>,
    spoke: Option<Timestamp>,
}

/// The presence endpoint's JSON body for one channel: archive stamps for
/// every nick in either source, live status when a snapshot exists, and the
/// live topic when known, else the archive's.
pub fn json_body(
    scanning: bool,
    archive: Option<&ChannelPresence>,
    live: Option<&LiveChannel>,
) -> serde_json::Result<String> {
    let mut users: HashMap<&str, (&str, UserJson)> = HashMap::new();
    for (key, user) in archive
        .into_iter()
        .flat_map(|c| &c.users)
    {
        let member = live.and_then(|l| {
            l.members
                .get(key)
        });
        users.insert(
            key,
            (
                member.map_or(&user.nick, |m| &m.nick),
                UserJson {
                    live: live.map(|_| member.is_some()),
                    prefixes: member.map_or("", |m| &m.prefixes),
                    online: user.online,
                    offline: user.offline,
                    spoke: user.spoke,
                },
            ),
        );
    }
    for (key, member) in live
        .into_iter()
        .flat_map(|l| &l.members)
    {
        users
            .entry(key)
            .or_insert((
                &member.nick,
                UserJson {
                    live: Some(true),
                    prefixes: &member.prefixes,
                    online: None,
                    offline: None,
                    spoke: None,
                },
            ));
    }
    let live_topic = live.and_then(|l| {
        l.topic
            .as_ref()
            .map(|topic| TopicJson {
                source: "live",
                topic,
            })
    });
    let archive_topic = archive.and_then(|c| {
        c.topic
            .as_ref()
            .map(|topic| TopicJson {
                source: "archive",
                topic,
            })
    });
    serde_json::to_string(&PresenceJson {
        scanning,
        snapshot_at: live.map(|l| l.snapshot_at),
        stale: live.is_some_and(|l| l.stale),
        topic: live_topic.or(archive_topic),
        modes: archive.and_then(|c| {
            let spec = live.map_or_else(ModeSpec::default, |l| {
                l.modes
                    .clone()
            });
            c.modes
                .baseline
                .as_ref()
                .map(|base| {
                    spec.channel_modes(
                        base,
                        &c.modes
                            .changes,
                    )
                })
        }),
        users: users
            .into_values()
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::ModeSpec;
    use crate::presence::test_helpers::*;
    use crate::presence::*;

    #[test]
    fn json_joins_archive_and_live_users() {
        let archive = file_stamps(&[
            ("2026-01-01T00:00:01Z", says("Alice")),
            ("2026-01-01T00:00:02Z", quits("gone")),
            ("2026-01-01T00:00:03Z", topic("op", "archived")),
        ]);
        let mut snapshot = LiveChannel::from_burst(
            ["@alice", "+new"],
            None,
            ts("2026-01-01T01:00:00Z"),
            ModeSpec::default(),
        );
        let body = |live: Option<&LiveChannel>| {
            serde_json::from_str::<serde_json::Value>(
                &json_body(false, Some(&archive), live).unwrap(),
            )
            .unwrap()
        };
        assert_eq!(
            body(None),
            serde_json::json!({
                "scanning": false,
                "snapshot_at": null,
                "stale": false,
                "modes": null,
                "topic": {"source": "archive", "nick": "op", "text": "archived", "at": "2026-01-01T00:00:03Z"},
                "users": {
                    "Alice": {"live": null, "prefixes": "", "online": "2026-01-01T00:00:01Z", "offline": null, "spoke": "2026-01-01T00:00:01Z"},
                    "gone": {"live": null, "prefixes": "", "online": null, "offline": "2026-01-01T00:00:02Z", "spoke": null},
                },
            })
        );
        snapshot.topic = Some(Topic {
            nick: "op2".into(),
            text: "live".into(),
            at: ts("2026-01-01T00:30:00Z"),
        });
        snapshot.stale = true;
        assert_eq!(
            body(Some(&snapshot)),
            serde_json::json!({
                "scanning": false,
                "snapshot_at": "2026-01-01T01:00:00Z",
                "stale": true,
                "modes": null,
                "topic": {"source": "live", "nick": "op2", "text": "live", "at": "2026-01-01T00:30:00Z"},
                "users": {
                    "alice": {"live": true, "prefixes": "@", "online": "2026-01-01T00:00:01Z", "offline": null, "spoke": "2026-01-01T00:00:01Z"},
                    "gone": {"live": false, "prefixes": "", "online": null, "offline": "2026-01-01T00:00:02Z", "spoke": null},
                    "new": {"live": true, "prefixes": "+", "online": null, "offline": null, "spoke": null},
                },
            })
        );
    }
}
