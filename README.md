# irc-log-viewer

Web-based IRC log viewer with real-time tail, full-text search, and optional AI-powered natural language search via the Anthropic API.

Supports ZNC and ISO 8601 log formats, zstd-compressed archives, and multiple log directories merged into a unified channel tree.

## Building

```sh
cargo build --release
```

### Cross-compilation (e.g. OpenWrt aarch64)

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc \
    cargo build --release --target aarch64-unknown-linux-musl
```

## Usage

```sh
irc-log-viewer -c config.yaml
```

On first run, if the config file doesn't exist, a default one is created and the program exits. `-z <znc.conf>` prints a `znc:` section for that ZNC and exits; see ZNC integration below.

## Configuration

All configuration is in a single YAML file. Example with all options:

```yaml
bind: "[::]:8080"
title: IRC Logs
search_limit: 10000
base_path: /irc
logs_dirs:
- /mnt/data/irc-log-archive
- /mnt/data/znc/log
trusted_proxies:
- 127.0.0.1
- ::1
search:
  max_concurrent: 2
  per_ip_per_minute: 30
  max_query_len: 200
  max_days: 365
log_timezone: EST5EDT,M3.2.0,M11.1.0
presence:
  index_path: /var/lib/irc-logs/presence.yaml
znc:
  address: "[::1]:7778"
  user: Tiky
  network: OFTC
  fingerprint: 3f:a1:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00
  client_cert: /etc/irc-log-viewer/znc-client.pem
ai:
  api_key: sk-ant-api03-...
  model: claude-haiku-5-5
  output_dir: /var/lib/irc-logs/ask
  max_concurrent: 1
  max_tool_calls: 100
  max_query_len: 500
  requests_per_day_per_client: 100
  requests_per_day_per_ip: 250
  system_prompt: |
    Custom system prompt here.
    The channel list is always appended automatically.
```

### General options

| Option | Default | Description |
|--------|---------|-------------|
| `bind` | `[::]:8080` | Address and port to listen on; `[::]` accepts IPv4 too unless net.ipv6.bindv6only is set. Quote it in YAML. |
| `title` | `IRC Logs` | Page title shown in the sidebar and browser tab |
| `search_limit` | `10000` | Maximum number of matching lines a search returns |
| `logs_dirs` | `[./logs]` | List of directories containing IRC log channels |
| `base_path` | *(empty)* | URL prefix for reverse proxy subpath deployments (e.g. `/irc`) |
| `trusted_proxies` | `[127.0.0.1, ::1]` | Peers whose `X-Real-IP` header is taken as the client address for per-address limits; from any other peer the TCP address is used |
| `log_timezone` | `UTC` | Time zone of the times in ZNC-format logs: `UTC`, an IANA name from the system tzdb (`America/Toronto`), or a POSIX TZ string (`EST5EDT,M3.2.0,M11.1.0`). It is the zone ZNC ran in when it wrote them. ISO 8601 logs are always UTC. Under the OpenWrt ujail there is no zoneinfo, so only `UTC` and POSIX strings resolve there; an IANA name needs `TZDIR` or a zoneinfo mount. |

### Presence

The `presence` section is optional. When set, the viewer scans the archive in the background for each nick's last online (join, message, action, nick change to it), last offline (quit, part, kick, nick change from it) and last spoke (message, action), and for each channel's latest topic change. `GET <channel>/presence` returns them as JSON: `scanning` (true until the startup scan is done); `snapshot_at` and `stale` describing the live snapshot (null and false without one, see ZNC below); `topic` (`source` `live` or `archive`, `nick`, `text`, `at`) or null; and `users`, mapping each nick to `live` (in the channel now, or null without a snapshot), `prefixes` (such as `@+`), `online`, `offline` and `spoke`, null when never seen. Stamps are RFC 3339 UTC. Without the section the route answers 404.

The index file is a disposable cache: at startup only new or changed log files are read, and deleting the index makes the next start rescan everything. Stamps only move forward, so a deleted or rewritten log keeps its contributions until the index is deleted. The index's directory must be writable by the viewer.

| Option | Default | Description |
|--------|---------|-------------|
| `presence.index_path` | *(required)* | YAML file caching the stamps between runs, written after the scan, every 10 minutes when something changed, and at shutdown |

### ZNC

The `znc` section is optional and needs `presence`. ZNC never logs NAMES, so the archive cannot tell who is in a channel now; with this section the viewer opens a short client session to ZNC to read each channel's member list and topic, then keeps them current from the log lines it tails.

| Option | Default | Description |
|--------|---------|-------------|
| `znc.address` | *(required)* | `host:port` of a ZNC listener with SSL and IRC enabled, such as `"[::1]:7778"`; quote it in YAML |
| `znc.user` | *(required)* | ZNC user |
| `znc.network` | *(required)* | ZNC network of that user |
| `znc.channel_path` | `network` | Path of the network's channel directories under `logs_dirs`, such as `OFTC` for `OFTC/#chan`; a channel ZNC joins with no directory there is ignored |
| `znc.fingerprint` | *(required)* | SHA-256 of the listener's certificate, colon-separated hex; the output of `openssl x509 -noout -fingerprint -sha256` works as is |
| `znc.client_cert` | | PEM file with the client certificate and its key, enrolled with ZNC's certauth module |
| `znc.password` | | The ZNC user's password, instead of `client_cert`; set exactly one of the two |
| `znc.timeout_secs` | `30` | Time one session may take from connect to the end of the burst |

### Search options

Per-address limits key IPv4 clients by address and IPv6 clients by their /64. Counters live in memory and reset on restart.

| Option | Default | Description |
|--------|---------|-------------|
| `search.max_concurrent` | `2` | Searches running at once; further requests get 503 |
| `search.per_ip_per_minute` | `30` | Searches one client address may start per minute; further requests get 429 with Retry-After |
| `search.max_query_len` | `200` | Longest accepted query, in characters |
| `search.max_days` | `365` | Days scanned, newest first; the results page says when older days were skipped |

### Log directory structure

Each path in `logs_dirs` is scanned recursively. Channels are identified by directories containing `YYYY-MM-DD.log` or `YYYY-MM-DD.log.zst` files. The directory tree structure becomes the channel path (e.g. `logs/OFTC/#channel/` becomes `OFTC/#channel`).

Only directories whose name starts with `#` become channels; anything else, such as a ZNC private query log, is never served or searched.

Multiple `logs_dirs` entries are merged: if the same channel path exists in multiple directories, their logs are combined.

### AI options

The `ai` section is optional. When omitted, the "ask" feature is disabled and no AI-related routes are registered.

| Option | Default | Description |
|--------|---------|-------------|
| `ai.api_key` | *(required)* | Anthropic API key |
| `ai.model` | `claude-haiku-5-5` | Model ID to use |
| `ai.output_dir` | *(required)* | Directory where output markdown files are written |
| `ai.max_concurrent` | `1` | Maximum concurrent AI sessions; a further ask is refused with a message on the page |
| `ai.max_tool_calls` | `100` | Maximum API round-trips per session before stopping |
| `ai.max_query_len` | `500` | Longest accepted question, in characters |
| `ai.requests_per_day_per_client` | `100` | Anthropic API requests one browser may make per day, tracked by an `ask_id` cookie the ask page sets |
| `ai.requests_per_day_per_ip` | `250` | Anthropic API requests one client address may make per day |
| `ai.system_prompt` | *(built-in)* | Override the system prompt sent to the model. The available channel list is always appended regardless. |

The built-in system prompt instructs the model to search logs using the provided tools, compile relevant excerpts, format output as markdown, and always produce a result document via the `done` tool.

The daily budgets are rolling: a full allowance of N refills one request every 24h/N. A browser can drop its cookie for a fresh client budget, so the address budget is the binding one. A session that runs out mid-way stops with an error on the page.

## ZNC integration

### What the session does and when

At startup, and each time the bouncer reconnects to IRC, the viewer logs in to ZNC as a client of the configured user and network, reads the burst ZNC sends for every attached channel (JOIN, topic, names with all prefixes), and disconnects. A reconnect is noticed from the `Connected to IRC` line in the network's `status/` log, which is read but never served; the session then waits until the bouncer's own joins have been quiet for 10 seconds. A failed session is logged and not retried until the next reconnect or restart.

The certificate pin is the SHA-256 of the listener's certificate: expiry and host name are not checked, since ZNC's default certificate is self-signed with no subject alternative name. If the certificate changes, update `znc.fingerprint`. Check it with:

```sh
openssl s_client -connect '[::1]:7778' </dev/null 2>/dev/null | openssl x509 -noout -fingerprint -sha256
```

### Setting up certauth

The quickest way is to let the viewer read ZNC's own config:

```sh
irc-log-viewer -c config.yaml -z /etc/znc/configs/znc.conf
```

It prints a `znc:` section to paste into the config: the first listener with IRC and SSL on (a loopback one first), each user and network (the first live, the others commented), the channel path found under `logs_dirs`, and the SHA-256 of ZNC's SSLCertFile. It creates the client certificate at `znc.client_cert`, or `znc-client.pem` beside the config, with mode 0600, and prints the certauth commands that enrol it. An existing certificate file is reused, never overwritten. It reads no other value from znc.conf, so passwords and hashes never reach its output. Run it as a user that can read znc.conf and SSLCertFile, then make the client certificate readable by the user the viewer runs as.

To do the same by hand, generate the client certificate (here with openssl; any self-signed certificate works) and enrol its SHA-256 with certauth from an IRC client logged in as a ZNC admin:

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 36500 -subj /CN=irc-log-viewer -keyout znc-client.pem -out znc-client.pem
openssl x509 -in znc-client.pem -noout -fingerprint -sha256
```

```
/msg *status LoadMod --type=global certauth
/msg *status AddPort +7778 ipv6 irc ::1
/msg *certauth Add <the client certificate's SHA-256>
/msg *status SaveConfig
```

Then restart ZNC: certauth asks for client certificates only on listeners present when ZNC starts. docs/znc.md has the details, including the directory permissions SaveConfig needs on OpenWrt. With `znc.password` instead, no certauth is needed.

### Side effects

Each session is a ZNC client of the user. With AutoClearChanBuffer on, ZNC replays each channel's buffer to the session and then clears it, so lines buffered while no other client was attached are consumed. Channels detached in ZNC get no burst.

### What pages show when ZNC is unreachable

A `znc` section that cannot be used at startup (no `presence` section, an unreadable `client_cert`, a malformed `fingerprint`, both or neither of `client_cert` and `password`) is logged with its cause and live status stays off; the rest of the viewer runs. Hover stamps and the archive topic keep working. The live snapshot from the last successful session is kept and marked stale (`stale: true` in the JSON); with no session ever succeeding, `live` is null for every nick and no status is shown.

## Reverse proxy

### Subdomain (recommended)

```nginx
server {
    listen 443 ssl;
    server_name irc.example.com;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;

        # SSE support
        proxy_buffering off;
        proxy_cache off;
        proxy_set_header Connection '';
    }
}
```

No `base_path` needed in the config.

### Subpath

Set `base_path: /irc` in the config, then:

```nginx
server {
    listen 443 ssl;
    server_name example.com;

    location /irc/ {
        proxy_pass http://127.0.0.1:8080/irc/;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;

        # SSE support
        proxy_buffering off;
        proxy_cache off;
        proxy_set_header Connection '';
    }
}
```

The `base_path` value is normalized on startup: `irc`, `/irc`, and `/irc/` all resolve to `/irc`.

## Features

- **Real-time tail**: today's log page auto-updates via SSE as new messages arrive
- **Full-text search**: substring search across all dates for a channel
- **AI search** (optional): natural language queries powered by Claude, with regex log search, markdown output, and permanent result links
- **Compressed logs**: transparent reading of `.log.zst` files
- **Multiple log dirs**: merge channels from different sources (e.g. archive + live ZNC)
- **Dark theme**: terminal-style dark UI

## OpenWrt

An example procd init script is provided in `openwrt/irc-log-viewer.init`. It parses the YAML config for jail mount paths and runs the binary under ujail with memory limits.

## License

GPL-3.0-only
