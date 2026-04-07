# sb

[![CI](https://github.com/cam-barts/sb-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/cam-barts/sb-cli/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Latest Release](https://img.shields.io/github/v/release/cam-barts/sb-cli)](https://github.com/cam-barts/sb-cli/releases/latest)

A CLI for [SilverBullet](https://silverbullet.md), the self-hosted Markdown note-taking platform.

Work with your SilverBullet notes from the terminal. Pages live on your local filesystem and sync to your server when you tell them to. One binary, no runtime dependencies.

## Features

- Page operations are local-first; sync is explicit
- Bidirectional sync with conflict detection and resolution
- Page CRUD: create, read, edit, delete, append, move, list
- Daily notes with configurable templates and date offsets
- Space Lua evaluation and index queries via the Runtime API (optional)
- OS keychain integration for token storage (optional)
- `--format json` on any structured command
- Single static binary

## Installation

### From GitHub Releases

Download the latest release for your platform from the [releases page](https://github.com/cam-barts/sb-cli/releases/latest).

| Platform | Archive |
|----------|---------|
| Linux (x86_64) | `sb-v*-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (aarch64) | `sb-v*-aarch64-unknown-linux-gnu.tar.gz` |
| macOS (Intel) | `sb-v*-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `sb-v*-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `sb-v*-x86_64-pc-windows-msvc.zip` |

Example (Linux x86_64):

```sh
curl -LO https://github.com/cam-barts/sb-cli/releases/latest/download/sb-v1.0.0-x86_64-unknown-linux-gnu.tar.gz
tar xzf sb-v1.0.0-x86_64-unknown-linux-gnu.tar.gz
sudo mv sb /usr/local/bin/
```

### From source

```sh
cargo install --git https://github.com/cam-barts/sb-cli
```

Or build locally:

```sh
git clone https://github.com/cam-barts/sb-cli
cd sb-cli
cargo build --release
# Binary at target/release/sb
```

### Requirements

- A running [SilverBullet](https://silverbullet.md) v2.x server

## Quick Start

```sh
# Point sb at your server
sb init https://sb.example.com

# Create a page
sb page create "Meeting Notes" --content "# Meeting Notes"

# List pages
sb page list

# Pull from server
sb sync pull

# Open today's daily note in $EDITOR
sb daily
```

## Commands

| Command | Description |
|---------|-------------|
| `sb init <url>` | Initialize a local space linked to a SilverBullet server |
| `sb page list` | List all pages (supports `--sort`, `--limit`) |
| `sb page read <name>` | Display page content (`--remote` to fetch from server) |
| `sb page create <name>` | Create a page (`--content`, `--edit`, `--template`) |
| `sb page edit <name>` | Edit a page in `$EDITOR` |
| `sb page delete <name>` | Delete a page (`--force` to skip confirmation) |
| `sb page append <name>` | Append content to a page (`--content`) |
| `sb page move <from> <to>` | Rename or move a page |
| `sb daily` | Open today's daily note (`--yesterday`, `--offset`, `--append`) |
| `sb sync` | Bidirectional sync: pull then push (`--dry-run`) |
| `sb sync pull` | Pull changes from server (`--dry-run`) |
| `sb sync push` | Push local changes to server (`--dry-run`) |
| `sb sync status` | Show sync status (modified, new, deleted, conflicts) |
| `sb sync conflicts` | List files in conflict |
| `sb sync resolve <path>` | Resolve a conflict (`--keep-local`, `--keep-remote`, `--diff`) |
| `sb lua <expr>` | Evaluate a Space Lua expression via the Runtime API |
| `sb query <query>` | Run an index query via the Runtime API |
| `sb shell <cmd>` | Execute a command on the server (opt-in, disabled by default) |
| `sb auth set` | Set auth token (`--token` or interactive prompt) |
| `sb config show` | Display resolved configuration (`--reveal` to unmask tokens) |
| `sb server ping` | Check server connectivity and response time |
| `sb server config` | Display server configuration |
| `sb version` | Show version, commit hash, build date, and target |

### Global Flags

| Flag | Description |
|------|-------------|
| `--quiet` | Suppress all informational output |
| `--verbose` | Enable debug logging to stderr |
| `--no-color` | Disable colored output |
| `--format <human\|json>` | Output format (default: `human`) |
| `--token <TOKEN>` | Auth token override (highest precedence) |

## Configuration

Configuration lives in `.sb/config.toml` inside your initialized space directory.

```toml
server_url = "https://sb.example.com"

[sync]
dir = "space"              # Local directory name (default: "space")
workers = 4                # Concurrent sync workers (default: 4)
attachments = false        # Sync non-Markdown files (default: false)
exclude = ["_plug/*"]      # Glob patterns to exclude (default: ["_plug/*"])
include = []               # Force-include patterns (default: [])

[daily]
path = "Journal/{{date}}"  # Daily note path template (default: "Journal/{{date}}")
dateFormat = "%Y-%m-%d"    # Date format string (default: "%Y-%m-%d")
template = "Daily"         # Template page name (optional)

[shell]
enabled = false            # Enable shell endpoint access (default: false)

[auth]
keychain = false           # Use OS keychain for token storage (default: false)

[runtime]
available = false          # Runtime API available on server (default: false)
```

### Precedence (highest wins)

1. CLI flags (e.g., `--token`)
2. Environment variables (`SB_SERVER_URL`, `SB_TOKEN`, `SB_SYNC_DIR`, etc.)
3. OS keychain (auth token only, when `auth.keychain = true`)
4. Config file (`.sb/config.toml`)
5. Built-in defaults

Every config field has a `SB_` environment variable. `sync.workers` maps to `SB_SYNC_WORKERS`, and so on.

## Attribution

Built for [SilverBullet](https://silverbullet.md), created by [Zef Hemel](https://github.com/zefhemel).

Inspired by:

- [zk](https://github.com/zk-org/zk) -- plain text note-taking from the terminal (which I still love using)
- [nb](https://github.com/xwmx/nb) -- command line notebook, bookmarking, and knowledge base

Most of this project was vibe coded with [Claude Opus 4.6](https://docs.anthropic.com/en/docs/about-claude/models) via [Claude Code](https://docs.anthropic.com/en/docs/claude-code) to meet one person's specific requirements (mine).
Credit to the enormous body of open source software the models were trained on, this project wouldn't exist without it.

## License

MIT -- see [LICENSE](LICENSE) for details.
