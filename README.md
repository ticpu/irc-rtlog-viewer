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

On first run, if the config file doesn't exist, a default one is created and the program exits.

## Configuration

All configuration is in a single YAML file. Example with all options:

```yaml
bind: 0.0.0.0:8080
title: IRC Logs
search_limit: 10000
base_path: /irc
logs_dirs:
- /mnt/data/irc-log-archive
- /mnt/data/znc/log
ai:
  api_key: sk-ant-api03-...
  model: claude-haiku-4-5-20251001
  output_dir: /var/lib/irc-logs/ask
  max_concurrent: 1
  max_tool_calls: 100
  system_prompt: |
    Custom system prompt here.
    The channel list is always appended automatically.
```

### General options

| Option | Default | Description |
|--------|---------|-------------|
| `bind` | `0.0.0.0:8080` | Address and port to listen on |
| `title` | `IRC Logs` | Page title shown in the sidebar and browser tab |
| `search_limit` | `10000` | Maximum number of lines to scan per channel during search |
| `logs_dirs` | `[./logs]` | List of directories containing IRC log channels |
| `base_path` | *(empty)* | URL prefix for reverse proxy subpath deployments (e.g. `/irc`) |

### Log directory structure

Each path in `logs_dirs` is scanned recursively. Channels are identified by directories containing `YYYY-MM-DD.log` or `YYYY-MM-DD.log.zst` files. The directory tree structure becomes the channel path (e.g. `logs/OFTC/#channel/` becomes `OFTC/#channel`).

When sibling directories include any `#`-prefixed name, non-`#` directories are filtered out (this excludes ZNC private query logs).

Multiple `logs_dirs` entries are merged: if the same channel path exists in multiple directories, their logs are combined.

### AI options

The `ai` section is optional. When omitted, the "ask" feature is disabled and no AI-related routes are registered.

| Option | Default | Description |
|--------|---------|-------------|
| `ai.api_key` | *(required)* | Anthropic API key |
| `ai.model` | `claude-haiku-4-5-20251001` | Model ID to use |
| `ai.output_dir` | *(required)* | Directory where output markdown files are written |
| `ai.max_concurrent` | `1` | Maximum concurrent AI sessions (returns 503 when full) |
| `ai.max_tool_calls` | `100` | Maximum API round-trips per session before stopping |
| `ai.system_prompt` | *(built-in)* | Override the system prompt sent to the model. The available channel list is always appended regardless. |

The built-in system prompt instructs the model to search logs using the provided tools, compile relevant excerpts, format output as markdown, and always produce a result document via the `done` tool.

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
