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
- Journaling: quick-entry, stdin, date-prefix routing, list/filter past entries (jrnl-flavoured)
- Space Lua evaluation, index queries, log streaming, screenshots, and tag introspection via the Runtime API (optional)
- OS keychain integration for token storage (optional)
- TTY-aware output: human-readable when interactive, JSON when piped
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

### Journaling

`sb daily` doubles as a journal in the spirit of [jrnl](https://jrnl.sh), but
each entry lands as a SilverBullet bullet with inline attributes
(`[time:: 14:32]`, `[starred:: true]`), so they are queryable from Space Lua and
`sb query` rather than living in opaque `.txt` files.

```sh
# Quick entry — joined positional text becomes a timestamped bullet
sb daily Finished the migration spike

# Stdin pipe — multi-line entries land as a single bullet with continuation indent
echo "two\nthoughts" | sb daily

# Route to another day with a small natural-date prefix
sb daily today: wrote the plan
sb daily yesterday: pretended to write the plan
sb daily 2026-05-15: backfilled

# Star an entry (sets [starred:: true])
sb daily --star celebrated finishing the cutover

# Override or omit the time attribute
sb daily --time 09:00 morning standup
sb daily --no-time todo: pick up groceries

# Listing/filtering past entries — any view flag switches read mode
sb daily -n 5
sb daily --from 2026-05-01 --contains migration
sb daily --tags engineering,ops --starred --short
sb daily --on 2026-05-15 --format json | jq .
```

Want to drop the `daily` suffix? Add `alias jrnl='sb daily'` to your shell rc;
all flags above work unchanged.

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
| `sb daily [ENTRY...]` | Journal today: write entry, pipe from stdin, or open in `$EDITOR`. Date flags: `--yesterday`, `--offset`, `--on`. Write flags: `--star`, `--time`, `--no-time`. Read flags: `-n`, `--from`, `--to`, `--contains`, `--tags`, `--starred`, `--short`. |
| `sb sync` | Bidirectional sync: pull then push (`--dry-run`) |
| `sb sync pull` | Pull changes from server (`--dry-run`) |
| `sb sync push` | Push local changes to server (`--dry-run`) |
| `sb sync status` | Show sync status (modified, new, deleted, conflicts) |
| `sb sync conflicts` | List files in conflict |
| `sb sync resolve <path>` | Resolve a conflict (`--keep-local`, `--keep-remote`, `--diff`) |
| `sb lua <expr>` | Evaluate a Space Lua expression via the Runtime API |
| `sb query <query>` | Run an index query via the Runtime API |
| `sb logs` | Stream client + server logs from the Runtime API (`--follow`, `--source client\|server\|both`) |
| `sb screenshot` | Save a PNG of the headless browser's current state (`--output PATH\|-`) |
| `sb describe <tag>` | Sample objects of a tag and report observed fields and types (`--limit N`) |
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
| `--format <human\|json>` | Output format. Defaults to `human` when stdout is a TTY and `json` when piped, so `sb page list \| jq ...` works without an explicit flag. |
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
timeFormat = "%H:%M"       # Time format used for [time:: ...] attributes (default: "%H:%M")
bulletStyle = "*"          # Bullet character for journal entries: "*" or "-" (default: "*")

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

## Testing & Coverage

Run the full test suite locally:

```sh
cargo test
```

Generate a coverage report (requires `cargo-llvm-cov`):

```sh
cargo install cargo-llvm-cov
cargo llvm-cov --lib --html  # writes target/llvm-cov/html/index.html
```

CI enforces a region-coverage floor of **80%** on every push and pull request
(`coverage` job in `.github/workflows/ci.yml`). Drops below the floor fail the
build. The threshold is intentionally a ratchet — raise it (in its own PR) when
the repo holds a new sustained high; don't lower it without an explicit waiver.

## Attribution

Built for [SilverBullet](https://silverbullet.md), created by [Zef Hemel](https://github.com/zefhemel).

Inspired by:

- [zk](https://github.com/zk-org/zk) -- plain text note-taking from the terminal (which I still love using)
- [nb](https://github.com/xwmx/nb) -- command line notebook, bookmarking, and knowledge base
- [jrnl](https://jrnl.sh) -- friction-free CLI journaling. `sb daily` borrows its quick-entry ergonomics (positional text, stdin, date-prefix routing, view flags) while keeping every entry as a SilverBullet bullet with inline attributes so it stays queryable

Most of this project was vibe coded with [Claude Opus 4.6](https://docs.anthropic.com/en/docs/about-claude/models) via [Claude Code](https://docs.anthropic.com/en/docs/claude-code) to meet one person's specific requirements (mine).
Credit to the enormous body of open source software the models were trained on, this project wouldn't exist without it.

## License

MIT -- see [LICENSE](LICENSE) for details.
