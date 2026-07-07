# sb

[![CI](https://github.com/cam-barts/sb-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/cam-barts/sb-cli/actions/workflows/ci.yml)
[![Coverage](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fcam-barts%2Fsb-cli%2Fbadges%2Fcoverage.json)](https://github.com/cam-barts/sb-cli/actions/workflows/ci.yml)
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

### Slim vs. AI builds

`sb` ships in two flavors that are otherwise identical — the AI build is a strict
superset (the `sb` command works the same either way):

- **slim** (default): the full notes/sync/journal CLI, zero AI surface, no heavy
  dependencies. This is what you want unless you drive `sb` with an AI agent.
- **ai**: adds `sb mcp serve` (a [Model Context Protocol](https://modelcontextprotocol.io)
  server) and `sb skills init` (generates agent instruction files). Opt-in via
  the `ai` Cargo feature. `sb version` reports which flavor you have (`features:`).

Because Cargo features are resolved at **compile time**, the flavor is chosen
when you install/build — a downloaded slim binary can't be toggled to AI in
place; you install the other artifact (or rebuild with `--features ai`).

### From GitHub Releases

Download the latest release for your platform from the [releases page](https://github.com/cam-barts/sb-cli/releases/latest).
Pick the `sb-*` archive for the slim flavor or the `sb-ai-*` archive for the AI flavor.

| Platform | Slim archive | AI archive |
|----------|--------------|------------|
| Linux (x86_64) | `sb-v*-x86_64-unknown-linux-gnu.tar.gz` | `sb-ai-v*-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (aarch64) | `sb-v*-aarch64-unknown-linux-gnu.tar.gz` | `sb-ai-v*-aarch64-unknown-linux-gnu.tar.gz` |
| macOS (Intel) | `sb-v*-x86_64-apple-darwin.tar.gz` | `sb-ai-v*-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `sb-v*-aarch64-apple-darwin.tar.gz` | `sb-ai-v*-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `sb-v*-x86_64-pc-windows-msvc.zip` | `sb-ai-v*-x86_64-pc-windows-msvc.zip` |

Example (Linux x86_64, slim):

```sh
curl -LO https://github.com/cam-barts/sb-cli/releases/latest/download/sb-v1.0.0-x86_64-unknown-linux-gnu.tar.gz
tar xzf sb-v1.0.0-x86_64-unknown-linux-gnu.tar.gz
sudo mv sb /usr/local/bin/
```

Once installed, `sb upgrade` self-updates to the latest release of the **same
flavor** (an AI build only pulls `sb-ai-*` assets, a slim build only `sb-*`);
`sb upgrade --check` reports whether a newer release is available without
installing it.

### From source

```sh
# Slim (default):
cargo install --git https://github.com/cam-barts/sb-cli
# AI build (adds `sb mcp serve` + `sb skills init`):
cargo install --git https://github.com/cam-barts/sb-cli --features ai
```

`cargo binstall sb-cli` fetches the prebuilt slim release tarball instead of
compiling. Or build locally:

```sh
git clone https://github.com/cam-barts/sb-cli
cd sb-cli
cargo build --release                 # slim
cargo build --release --features ai   # ai
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

# Capture the entry as a task (checkbox item): "* [ ] ... #task"
sb daily --task follow up with the vendor
sb daily --task-tag urgent patch the auth bug   # override the tag (implies --task)
sb daily --task --no-task-tag just a plain checkbox

# Listing/filtering past entries — any view flag switches read mode
sb daily -n 5
sb daily --from 2026-05-01 --contains migration
sb daily --tags engineering,ops --starred --short
sb daily --on 2026-05-15 --format json | jq .
```

Want to drop the `daily` suffix? Add `alias jrnl='sb daily'` to your shell rc;
all flags above work unchanged.

### Templates

SilverBullet marks a page template by tagging the page `meta/template/page`.
`sb` reuses that convention: `sb template list` reports those pages (from the
index when the Runtime API is available, otherwise by scanning local
frontmatter), and `sb template new` instantiates one.

```sh
# List available templates
sb template list

# Create from a named template (skips the picker)
sb template new "Projects/Work/Auth Rework" --template "Library/Personal/Templates/HLS"

# Omit --template to pick interactively:
#   uses fzf if it's installed, otherwise a numbered prompt
sb template new "Projects/Work/Auth Rework"

# Omit the name entirely to use the template's suggestedName:
#   confirm/complete it interactively (a trailing "/" prompts for the leaf),
#   or it's used directly when the template sets confirmName: false
sb template new --template "Library/Std/Page Templates/Quick Note"
```

The new page's name follows the template's `suggestedName`/`confirmName`
frontmatter, just like SilverBullet: `suggestedName` is rendered (so
`${os.date(...)}` resolves) and offered as the name; `confirmName: true` (SB's
default) confirms it interactively, while `confirmName: false` uses it directly.
An explicit `name` argument always overrides the suggestion.

After creating the page, `sb template new` opens it in `$EDITOR` (pass
`--no-edit` to skip, e.g. in scripts; it's also skipped automatically when not
attached to a terminal). `openIfExists: true` makes a name collision non-fatal —
instead of erroring, the existing page is opened rather than overwritten.
Templates' `description` frontmatter is shown in `sb template list`.

The UI-only template fields (`command`, `key`/`mac`, `priority`) have no
command-palette/keybinding analog in the CLI and are ignored.

Instantiation follows SilverBullet's page-template model: the template's nested
`frontmatter:` block becomes the new page's frontmatter, its own metadata
(`tags: meta/template/page`, `command`, `suggestedName`, …) is dropped, and the
`|^|` cursor marker is handled as an insertion point. When the Runtime API is
available the result is rendered so `${...}` Space Lua expressions (e.g.
`${os.date('%Y-%m-%d')}`) resolve; offline, `${...}` is left literal.
`sb page create --template <name>` uses the same instantiation path.

Piping content in fills the template's `|^|` marker (splice-then-render), so you
can drop text straight into the cursor slot:

```sh
echo "Reworks the auth subsystem." | sb template new "Projects/Work/Auth" --template "Library/Personal/Templates/HLS"
```

The piped text lands where `|^|` sits (appended after the body if the template
has no marker), and the whole page is rendered together.

### Shell completions

```sh
# Print a script (bash, zsh, fish, elvish, powershell)
sb completions zsh

# Install to the standard location for your shell (auto-detected from $SHELL
# when omitted). Prints any fpath/rc step the shell needs.
sb completions zsh --install
```

### MCP server (AI build)

The `ai` build exposes `sb` as a [Model Context Protocol](https://modelcontextprotocol.io)
server over the same core as the CLI, so MCP-native clients (Claude Desktop,
Cursor, VS Code, …) can call it with typed tools instead of shell strings. Two
transports:

```sh
# stdio (default): local subprocess, zero network. Configure your MCP client to
# launch this command; JSON-RPC flows over stdin/stdout, diagnostics over stderr.
sb mcp serve

# Streamable HTTP: for networked or multi-client use. Serves at <addr>/mcp.
sb mcp serve --http                       # binds 127.0.0.1:8787
sb mcp serve --http --addr 127.0.0.1:9000 # custom bind
```

It exposes a small, outcome-oriented tool set — `page_list`, `page_read`,
`query`, `server_ping` (read-only), plus `daily_append` and `page_create`
(additive) — each annotated so clients can auto-approve reads and gate writes.
The HTTP transport restricts the `Host` header to localhost by default. This
subcommand exists only in the `ai` build (`sb version` shows `features: …ai…`).

## Commands

Commands that take a page or template name (`page read`/`edit`/`delete`,
`template new`) let you omit it and pick interactively instead — via [`fzf`](https://github.com/junegunn/fzf)
when it's installed, otherwise a numbered prompt.

| Command | Description |
|---------|-------------|
| `sb init <url>` | Initialize a local space linked to a SilverBullet server |
| `sb page list` | List all pages (supports `--sort`, `--limit`) |
| `sb page read [name]` | Display page content (`--remote` to fetch from server). Omit `name` to pick with fzf |
| `sb page create <name>` | Create a page (`--content`, `--edit`, `--template`) |
| `sb page edit [name]` | Edit a page in `$EDITOR`. Omit `name` to pick with fzf |
| `sb page delete [name]` | Delete a page (`--force` to skip confirmation). Omit `name` to pick with fzf |
| `sb page append <name>` | Append content to a page (`--content`) |
| `sb page move <from> <to>` | Rename or move a page |
| `sb template list` | List pages tagged `meta/template/page` (index-backed when the Runtime API is on, else a local frontmatter scan) |
| `sb template new [name]` | Create a page from a template and open it in `$EDITOR` (use `--no-edit` to skip). `--template <name>` skips the picker; omitting it opens an fzf picker when `fzf` is installed, otherwise a numbered prompt. Omitting `name` uses the template's `suggestedName`/`confirmName` |
| `sb completions <shell>` | Print a shell completion script (bash, zsh, fish, elvish, powershell); `--install` writes it to the standard location |
| `sb daily [ENTRY...]` | Journal today: write entry, pipe from stdin, or open in `$EDITOR`. Date flags: `--yesterday`, `--offset`, `--on`. Write flags: `--star`, `--time`, `--no-time`, `--task`, `--task-tag`, `--no-task-tag`. Read flags: `-n`, `--from`, `--to`, `--contains`, `--tags`, `--starred`, `--short`. |
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
| `--no-input` | Never prompt: disables interactive pickers, confirmations, and `$EDITOR` launches (also implied when not attached to a TTY). |
| `--yes`, `-y` | Assume "yes" to confirmation prompts on destructive operations (required, or `--force`, to mutate non-interactively). |

Agent-oriented per-command flags include `--fields name,modified` (trim JSON
output on `page list`/`query`/`describe`), `--dry-run` (preview `page delete`/
`page move`/`template new` without changes), and `--upsert` (`page create`
overwrites instead of failing when the page exists).

### Exit codes

`sb` publishes a stable exit-code taxonomy so scripts and agents can branch on
failures without parsing stderr. These codes are a contract and are never
reshuffled:

| Code | Meaning |
|------|---------|
| `0` | success |
| `1` | general error |
| `2` | usage / invalid arguments |
| `3` | authentication error |
| `4` | not found |
| `5` | conflict / already exists |
| `6` | confirmation required (re-run with `--yes`) |
| *(other)* | `sb shell` passes through the remote process's own exit code |

Under `--format json`, failures also emit a structured object to **stderr**
(stdout stays empty), giving a parseable failure body alongside the exit code:

```console
$ sb page read missing --format json
# stderr:
{"error":"page not found: missing","code":"not_found","remediation":"Run `sb page list` to see available pages"}
```

The `code` string mirrors the exit-code category (`auth`, `not_found`,
`conflict`, `confirmation_required`, `usage`, `general`, `process_failed`), and
`remediation` is `null` when no actionable hint applies.

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
taskTag = "task"           # Tag applied to --task entries (default: "task")
taskTagMode = "auto"       # When to tag tasks: "auto" | "always" | "never" (default: "auto")

[shell]
enabled = false            # Enable shell endpoint access (default: false)

[auth]
keychain = false           # Use OS keychain for token storage (default: false)

[runtime]
available = false          # Runtime API available on server (default: false)
```

With `taskTagMode = "auto"` (the default), `sb daily --task` reads the space's
`index.task.all` setting via the Runtime API and applies `taskTag` only when task
indexing is tag-gated (`index.task.all = false`), leaving the task bare otherwise.
If the Runtime API is unavailable it falls back to tagging, since a tagged task is
indexed under either setting. Use `always`/`never` to pin the behaviour, or the
per-entry `--task-tag <TAG>` / `--no-task-tag` flags to override a single entry.

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

The coverage badge at the top of this README is auto-updated by CI. On every
push to `main`, the `coverage` job writes a [shields.io endpoint
JSON](https://shields.io/badges/endpoint-badge) to the `badges` branch with the
current region-coverage percentage and a colour bucket
(`brightgreen` ≥ 90, `green` ≥ 80, `yellow` ≥ 70, `orange` ≥ 60, otherwise
`red`). PRs intentionally don't update the badge — only merges to `main` do.

### Locking the `badges` branch to CI

Recommended one-time setup so the badge can only be moved by the workflow:

1. Settings → Branches → **Add branch protection rule** (or Settings → Rules →
   Rulesets → **New branch ruleset** for the newer flow).
2. Branch name pattern: `badges`.
3. Enable **Restrict who can push to matching branches** (classic) or
   **Restrict updates** (ruleset).
4. Allow only `github-actions[bot]` to push (classic: add it to the allow-list;
   ruleset: leave the rule rejecting everyone and add `github-actions[bot]` —
   or "Repository admin" if you want a manual override — as a bypass actor).
5. Optional but recommended: enable **Do not allow force pushes** and **Do not
   allow deletions** so the branch's history is append-only.

Same pattern works for `main` once you're ready to gate direct pushes:
require the `test`, `lint`, and `coverage` checks, restrict who can push, and
disable force-pushes.

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
