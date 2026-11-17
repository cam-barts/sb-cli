# sb-cli: agent guide

`sb`, a Rust CLI for SilverBullet. The README, CHANGELOG and PRs record *what* it does. This file
records the *why*: the rules a change has to stay inside. If the code and this file disagree, say
so rather than silently picking one.

## Governing design rules

### 1. AI is opt-in, and gating is by identity, not dependency weight

Cam, 2026-07-07: *"I wanted to avoid this becoming another AI specific tool… I don't want to force
AI on the users, but if a user happens to use AI alot, I want to be able to accomodate that."*
Mental model: Python extras, `pkg[ai]`.

The operative test: **the slim build's `--help` must show zero AI surface.** That is why
`sb skills init` sits behind the `ai` feature despite carrying no heavy dependencies. Weight is not
the criterion; how the tool *reads* to someone who doesn't care about agents is.

This rule erodes one convenience at a time. Each AI-adjacent addition looks harmless on its own
and the failure is only visible in `--help`, so run the check per release:

```bash
cargo run -- --help | rg '^  (mcp|skills|schema)'               # must print nothing
cargo run --features ai -- --help | rg '^  (mcp|skills|schema)' # must print all three
```

What the rule governs is the *command list*. Agent-facing wording inside a core flag's help text
(`--no-input`, `--yes`) is fine, because those flags are core by rule 3.

### 2. It is built for the SilverBullet community, not just Cam

Decided *against* a recommendation: a model argued the adoption and distribution work was waste
because "this is your personal CLI," and Cam rejected the premise: much of what makes the tool
opinionated "could be useful for a broader SilverBullet audience." That ruling is why the
10-artifact release matrix, `sb upgrade`, help-text polish and the README work all exist; every one
of them is something the counterargument said to skip.

Before writing "this is just for Cam," re-read this rule. Before coupling anything to his space
specifically, find the general version.

### 3. Machine-contract hardening is not AI

Exit-code taxonomy, JSON errors, stdout/stderr discipline, `--dry-run` / `--yes` / `--no-input` /
`--fields` all ship **unconditionally**, because they serve `jq`, cron and CI identically. Only
`sb mcp serve` and `sb skills init` are gated.

**The reusable test: if it also helps a shell script, it is core; if it only helps an agent, it is
gated.**

Hard invariant: the `ai` feature is purely **additive**. It may add surface; it may never change
existing behaviour.

### Also standing

- **Mirror SilverBullet's semantics, don't invent parallel ones.** `suggestedName`, `confirmName`,
  `openIfExists` mean something in SB, so they mean the same thing here.
- **The fzf convention is general, not per-command.** Any command taking a name lets you omit it
  and pick: fzf when installed, numbered prompt otherwise, suppressed by `--no-input` or a
  non-TTY.

## Machine contract (don't reshuffle)

Exit codes, from `ErrorCategory` in `src/error.rs`, which is the single source of truth behind both
the process exit code and the JSON `code` string so the two cannot drift:

| code | meaning | `code_str` |
|---|---|---|
| 0 | success | n/a |
| 1 | general / internal / network / io | `general` |
| 2 | usage, bad args or flags (matches clap's own) | `usage` |
| 3 | auth / token | `auth` |
| 4 | not found | `not_found` |
| 5 | already exists / conflicting state | `conflict` |
| 6 | mutation needs `--yes` | `confirmation_required` |

`sb shell` passes through the child's code. Callers branch on these, never on error text, so
adding an error variant means assigning it a category, not inventing a seventh code.

Interaction flags are process-global (`output::set_no_input` / `set_assume_yes`), not threaded
through every command signature. `no_input()` is also true when stdin isn't a terminal: an agent or
a pipe can't answer a prompt either way.

## Stack and dependency rules

Rust, clap (derive), tokio, reqwest, rusqlite, thiserror. Cam's Python preferences (cyclopts,
Pydantic, Textual) do not apply here. Minimal-dependency style throughout: table rendering and
frontmatter parsing are hand-rolled rather than pulling crates for them.

The `.claude/hookify.*.local.md` rules block these at edit time and cite this section:

- **`tracing`, never the `log` crate.** `use tracing::{debug, info, warn, error};`
- **`rustls-tls`, never `native-tls`.** `native-tls` drags in an OpenSSL C dependency that breaks
  cross-compilation for musl targets. Current form: `rustls-tls-native-roots` (rustls, system trust
  store).
- **`rusqlite` with `bundled`, never diesel or sqlx.** An ORM is overhead for a single-table sync
  state database.
- **`wiremock`, never `mockall`.** See testing below.

## Testing

Tests verify observable behaviour (inputs, outputs, side effects), not implementation details. If
refactoring internals breaks a test, the test was wrong.

- `wiremock` for HTTP: assert the right request went out and the right result came back.
- Real SQLite (`:memory:`), not a mocked database.
- `tempdir` plus `test_util::make_space` / `SbSpaceGuard`; verify actual files on disk.
- Never assert "this function was called with these args."
- After writing a test, ask: would it still pass if I rewrote the internals?

The failure mode this guards against is the congratulation machine: tests that verify mocks were
called correctly, pass forever, and prove nothing.

After implementing something non-trivial, review the diff adversarially. Brief the review with
"there are bugs, find them," not "review this," and point it at where the change is most likely
wrong. Then reproduce each claimed bug against the built binary before fixing; some findings won't
be real.

## Working in the repo

```bash
cargo build                                    # slim
cargo build --features ai                      # + mcp serve, skills init
cargo test                                     # also: --features ai
cargo fmt -- && cargo clippy --all-targets -- -D warnings   # what pre-commit runs
cz bump                                        # commitizen; version from Cargo.toml, tag v$version
```

Conventional commits (commitizen drives the version bump and CHANGELOG). Releases are reactive:
they happen when something annoying enough accumulates.

## Gotchas worth knowing before debugging

- **Weak ETags through Cloudflare.** Conditional writes (`If-Match`) need a *strong* revision.
  Cloudflare weakens `ETag`, so SilverBullet >= 2.12.1 also sends `x-silverbullet-revision`,
  which sb prefers. Against an older server behind Cloudflare, sync silently falls back to
  unconditional writes. `sb --verbose` reports which it got. Anyone chasing a lost write needs
  this first.
- **Runtime API vs `sb lua`.** Runtime commands post to `/.runtime/lua_script` and are gated on
  `config.runtime_available`; `sb query` fails with "Runtime API not available on this server" when
  the server's headless browser is down, while `sb lua` (the `/.runtime/lua` endpoint, a different
  scope) may still answer.
- **A 403 is read-only, not bad credentials.** Since SilverBullet 2.10.0 it means a read-only path
  (a bundled `Library/Std` page, or `SB_READ_ONLY`). Mapped to `SbError::ReadOnly`, not `AuthFailed`.
