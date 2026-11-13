# ZNC integration

How irc-log-viewer talks to ZNC and reads its logs, as verified against ZNC 1.10.1 (source tag znc-1.10.1) on OpenWrt 25.12. The README covers configuration; this file holds the protocol and ZNC behaviour the code depends on.

## What the viewer needs from ZNC

- The log module's per-channel files, read from `logs_dirs`, for message, join, part, quit, kick, nick, mode and topic lines.
- The network's status window (`<network>/status/YYYY-MM-DD.log`), read but never served, for `Connected to IRC (<server>)`.
- A short client session to learn who is in each channel right now, since the log module never logs NAMES.

## Log module output

- Channel files get `*** Joins:`, `*** Parts:`, `*** Quits:` (one copy per channel the nick shared with the bouncer), `*** X was kicked by Y (reason)`, `*** X is now known as Y`, `*** X sets mode: <modes> <args>` (`Server sets mode: +nst ` with a trailing space when the server sets it), and `*** X changes topic to '<topic>'`.
- The bouncer's own connect and disconnect go only to the status window: `Connected to IRC (irc.oftc.net +6697)` on 001, `Disconnected from IRC (...)` on a clean disconnect. An unclean drop, such as a reboot, logs no Disconnected, so Connected is the only reliable marker.
- After a reconnect the bouncer's own `*** Joins: <nick>` appears in each channel file, spread over seconds by FloodRate and FloodBurst.
- A client logging in or out writes nothing to channel or status files.

## Log timestamps follow the ZNC process's time zone

The log module formats times in the zone the ZNC process runs in. procd passes no TZ, so ZNC started by a procd init logs UTC; ZNC started some other way picks up the system zone. Moving between the two shifts every later line, and the day file in use at the switch holds both. Check a known event, such as a topic change, against the server's 333 epoch to find which zone a stretch of the archive is in.

## Setting up a viewer login on OpenWrt

1. Install the modules: `apk add znc-mod-certauth znc-mod-controlpanel`.
2. Give ZNC write access to its own data directory. SaveConfig writes a temporary file next to `znc.conf`, and global modules keep their data in `<datadir>/moddata/`. A root-owned directory gives *Error while trying to write config.*, and certauth's key is then lost at the next restart: `chown znc:znc /etc/znc /etc/znc/configs`.
3. From an IRC client logged in as an admin:

```
/msg *status LoadMod --type=global certauth
/msg *status AddPort +7778 ipv6 irc ::1
/msg *certauth Add <client cert SHA-256>
/msg *status SaveConfig
```

4. Restart ZNC. certauth asks listeners for a client certificate only in its OnBoot hook (modules/certauth.cpp, OnBoot), which runs at ZNC startup. Loading the module at runtime, or adding a listener after it, leaves TLS clients unasked, and SASL then answers *904 No client cert presented*.

`+7778` means TLS; `ipv6 ... ::1` binds the loopback only; `irc` keeps the web interface off this port. ZNC 1.10 also accepts `AddPort unix:[ssl,mode=NNN,group=foo]:/path/to/socket irc`.

## certauth keys

- The key is the SHA-256 of the client certificate's DER encoding, as colon-separated hex. Add lowercases it, so the output of `openssl x509 -in client.pem -noout -fingerprint -sha256` can be pasted as is.
- Keys are stored per user in `<datadir>/moddata/certauth/.registry`. Add saves only when the key is new.
- `/msg *certauth List` and `Del <n>` manage them; `Show` prints the key of the connection you are on.

## The viewer's session

Over TLS to the loopback listener, presenting the client certificate and pinning the server certificate's SHA-256 (ZNC's default certificate is self-signed with no subjectAltName, which standard verification rejects):

```
CAP LS 302
CAP REQ :sasl multi-prefix
AUTHENTICATE EXTERNAL
AUTHENTICATE <base64 of user/network>
CAP END
NICK <any>
USER <user>/<network> 0 * :<any>
```

What comes back:

- `900 ... :You are now logged in as <user>` and `903` on success. With no certificate: `904 :No client cert presented`, then `464 :Password required`. With an unknown certificate: `904 :Client cert not recognized`.
- ZNC renames the client to the bouncer's nick (`:<any> NICK :<bouncer nick>`).
- The upstream 001 to 005, LUSERS and MOTD, replayed by ZNC.
- For each attached channel: `JOIN`, `332` and `333` when a topic is set (`333` carries the setter's full mask and an epoch), the `353` lines with every prefix under multi-prefix, and `366`. Detached channels get nothing.
- ZNC sends `PING :ZNC` to the client, which has to be answered.

ZNC writes the channel bursts during login, before it reads more client input, so a `PING` sent on receiving `376` (or `422` when there is no MOTD) is answered by ZNC itself after the last `366`. Its `PONG` marks the end of the burst. The viewer's own session confirmed this against ZNC 1.10.1 on the router: every attached channel's burst arrived before the `PONG`.

The viewer sends no `MODE #chan`. ZNC answers it locally, to the asking client only, only when it is on the channel and already knows the modes (src/Client.cpp, CClient::OnModeMessage). Otherwise it forwards the query, and the server's `324` and `329` reach every client of the user: another client attached to the same network displays them. Channel modes come from the log instead, since ZNC's own mode query after each join is logged as `*** Server sets mode: ...`.

## Side effects of the session

- It counts as a client of the user. With AutoClearChanBuffer on, ZNC replays and then clears each channel's buffer, so the viewer consumes buffered lines another client would otherwise have seen on attach.
- No notice reached the user's other client (Halloy) when the session logged in or out.
