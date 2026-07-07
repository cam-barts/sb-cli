//! `sb skills init` — generate agent instruction/skill files that teach coding
//! agents how to drive sb. AGENTS.md (the cross-tool baseline) is the default;
//! SKILL.md / Cursor / Copilot / Windsurf targets are opt-in.
//!
//! The command inventory is pulled from the live clap graph (same source as
//! `sb schema`) so it can't drift; the surrounding guidance is kept lean and
//! command-first on purpose — verbose auto-generated context files measurably
//! hurt agent performance, so this generates recipes and contracts, not prose.

use std::path::{Path, PathBuf};

use clap::CommandFactory;

use crate::cli::SkillsTarget;
use crate::error::{SbError, SbResult};
use crate::output;

/// A file the generator produces: destination path (relative to cwd) + contents.
struct Artifact {
    path: PathBuf,
    contents: String,
}

pub fn execute_init(target: SkillsTarget, quiet: bool, color: bool) -> SbResult<()> {
    let body = agents_body();
    let artifacts = artifacts_for(target, &body);

    let mut wrote = 0usize;
    for art in artifacts {
        // Never clobber a file the user may have hand-edited; report and skip.
        if art.path.exists() {
            output::print_warning(
                &format!("{} already exists — skipping", art.path.display()),
                color,
                quiet,
            );
            continue;
        }
        if let Some(parent) = art.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| SbError::Filesystem {
                    message: "failed to create directory".into(),
                    path: parent.display().to_string(),
                    source: Some(e),
                })?;
            }
        }
        std::fs::write(&art.path, &art.contents).map_err(|e| SbError::Filesystem {
            message: "failed to write skill file".into(),
            path: art.path.display().to_string(),
            source: Some(e),
        })?;
        output::print_success(&format!("wrote {}", art.path.display()), color, quiet);
        wrote += 1;
    }

    if wrote == 0 {
        output::print_warning("no files written (all targets already exist)", color, quiet);
    }
    Ok(())
}

/// Resolve the artifact set for a target. `All` is the union of every concrete
/// target; each concrete target wraps the shared body in the format that
/// ecosystem expects.
fn artifacts_for(target: SkillsTarget, body: &str) -> Vec<Artifact> {
    match target {
        SkillsTarget::Agents => vec![agents_md(body)],
        SkillsTarget::Claude => vec![claude_md(), skill_md(body)],
        SkillsTarget::Cursor => vec![cursor_mdc(body)],
        SkillsTarget::Copilot => vec![copilot_md(body)],
        SkillsTarget::Windsurf => vec![windsurf_md(body)],
        SkillsTarget::All => vec![
            agents_md(body),
            claude_md(),
            skill_md(body),
            cursor_mdc(body),
            copilot_md(body),
            windsurf_md(body),
        ],
    }
}

fn agents_md(body: &str) -> Artifact {
    Artifact {
        path: PathBuf::from("AGENTS.md"),
        contents: body.to_string(),
    }
}

/// CLAUDE.md is intentionally tiny — it just points at AGENTS.md so there's one
/// source of truth (Claude Code reads both).
fn claude_md() -> Artifact {
    Artifact {
        path: PathBuf::from("CLAUDE.md"),
        contents: "# Project memory\n\nSee [AGENTS.md](AGENTS.md) for how to drive the `sb` CLI.\n"
            .to_string(),
    }
}

fn skill_md(body: &str) -> Artifact {
    let front = "---\n\
name: sb-cli\n\
description: Drive the sb command-line client for a SilverBullet notes space — create/read/list pages, capture daily journal entries, run index queries, and sync. Use whenever the task involves SilverBullet notes via the sb CLI.\n\
---\n\n";
    Artifact {
        path: Path::new(".claude")
            .join("skills")
            .join("sb-cli")
            .join("SKILL.md"),
        contents: format!("{front}{body}"),
    }
}

fn cursor_mdc(body: &str) -> Artifact {
    let front = "---\n\
description: How to drive the sb CLI for SilverBullet notes\n\
alwaysApply: false\n\
---\n\n";
    Artifact {
        path: Path::new(".cursor").join("rules").join("sb-cli.mdc"),
        contents: format!("{front}{body}"),
    }
}

fn copilot_md(body: &str) -> Artifact {
    Artifact {
        path: Path::new(".github").join("copilot-instructions.md"),
        contents: body.to_string(),
    }
}

fn windsurf_md(body: &str) -> Artifact {
    Artifact {
        path: Path::new(".windsurf").join("rules").join("sb-cli.md"),
        contents: body.to_string(),
    }
}

/// Build the shared Markdown body: a lean machine-contract + env + command
/// inventory (from the live clap graph) + recipes + safety notes.
fn agents_body() -> String {
    let mut out = String::new();
    out.push_str(
        "# Driving the `sb` CLI (SilverBullet)\n\n\
`sb` is a command-line client for a SilverBullet notes space. Prefer it over \
editing note files directly. It is automation-safe: data goes to stdout, \
diagnostics to stderr, and it never blocks on prompts when run non-interactively.\n\n\
## Machine contract\n\
- Output: pass `--format json` for machine-readable output (auto-selected when \
stdout is not a TTY); human tables otherwise. `--fields a,b` trims JSON to named \
top-level fields.\n\
- Errors: `--format json` prints `{ \"error\", \"code\", \"remediation\" }` to \
stderr; stdout stays empty on failure.\n\
- Exit codes: 0 success, 1 general, 2 usage, 3 auth, 4 not-found, 5 conflict, \
6 confirmation-required.\n\
- Non-interactive: pass `--no-input` to disable pickers/confirmations/$EDITOR, \
and `--yes` (or `--force`) to approve destructive operations.\n\n\
## Configuration\n\
- Resolution order (highest wins): CLI flags > `SB_*` env vars > OS keychain > \
per-space `<space>/.sb/config.toml` > user `~/.config/sb/config.toml` > defaults. \
`sb config show` prints the resolved values and where each came from.\n\
- `SB_SERVER_URL` — SilverBullet server URL.\n\
- `SB_TOKEN` — auth token; preferred for headless/agent use (beats the keychain). \
Every config field has an `SB_`-prefixed variable.\n\n\
## Commands\n",
    );

    // Command inventory from the live clap graph (name — one-line about).
    let cmd = crate::cli::Cli::command();
    let mut names: Vec<(String, String)> = cmd
        .get_subcommands()
        .filter(|s| {
            let n = s.get_name();
            n != "help" && n != "completions"
        })
        .map(|s| {
            (
                s.get_name().to_string(),
                s.get_about().map(|a| a.to_string()).unwrap_or_default(),
            )
        })
        .collect();
    names.sort();
    for (name, about) in names {
        out.push_str(&format!("- `sb {name}` — {about}\n"));
    }

    out.push_str(
        "\n## Querying & scripting\n\
- `sb query` runs SilverBullet index queries (SLIQ) over indexed objects. Common \
tags: `page`, `task`, `tag`, `link`, plus any custom fenced data-object tag.\n\
  - `sb query 'from index.tag \"page\" order by name limit 20'`\n\
  - `sb query 'from index.tag \"task\" where done = false limit 50'`\n\
- `sb describe <tag>` samples objects of a tag and reports their observed fields \
— use it to learn a tag's shape before querying.\n\
- `sb lua` evaluates a Space Lua expression via the Runtime API, e.g. \
`sb lua 'return 1 + 1'`.\n\
- `query`/`lua`/`describe` (and template rendering) require SilverBullet's Runtime \
API, which is OFF by default. Enable it with `[runtime] available = true` in \
`.sb/config.toml` (or `SB_RUNTIME_AVAILABLE=1`); the server must be running with a \
headless browser. On a transient 5xx just after a space reload, wait and retry.\n\
\n## Recipes\n\
- Append to today's journal: `sb daily \"text\"`\n\
- Create or overwrite a page: `sb page create Name --content \"...\" --upsert`\n\
- Read a page as JSON: `sb page read Name --format json`\n\
- List page names only: `sb page list --fields name --format json`\n\
- Preview a destructive op first: `sb page delete Name --dry-run`\n\
- Sync with the server: `sb sync` (or `sb sync pull` / `push` / `status`)\n\n\
## Safety\n\
- `sb shell` runs commands on the server and is DISABLED by default — do not \
rely on it; it is destructive and gated.\n\
- `sb page delete` requires `--yes` (or `--force`) when non-interactive.\n\
- Run `sb schema` for the complete machine-readable command/flag surface.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_body_includes_contract_and_commands() {
        let body = agents_body();
        assert!(body.contains("Exit codes"), "should document exit codes");
        assert!(body.contains("SB_TOKEN"), "should mention SB_TOKEN");
        assert!(body.contains("`sb daily`"), "should list the daily command");
        assert!(body.contains("`sb page`"), "should list the page command");
        // Generic SilverBullet querying/scripting guidance (SLIQ + Space Lua).
        assert!(
            body.contains("index.tag"),
            "should show SLIQ query examples"
        );
        assert!(
            body.contains("sb lua"),
            "should mention Space Lua evaluation"
        );
        // Config location + the Runtime-API-off-by-default gotcha.
        assert!(
            body.contains(".sb/config.toml"),
            "should say where config lives"
        );
        assert!(
            body.contains("Runtime") && body.contains("OFF by default"),
            "should note the Runtime API is off by default"
        );
        // `sb shell` safety warning is required.
        assert!(body.contains("DISABLED by default"));
    }

    #[test]
    fn all_target_produces_every_ecosystem_file() {
        let arts = artifacts_for(SkillsTarget::All, &agents_body());
        let paths: Vec<String> = arts.iter().map(|a| a.path.display().to_string()).collect();
        assert!(paths.iter().any(|p| p == "AGENTS.md"));
        assert!(paths.iter().any(|p| p.ends_with("SKILL.md")));
        assert!(paths.iter().any(|p| p.ends_with("sb-cli.mdc")));
        assert!(paths.iter().any(|p| p.contains("copilot-instructions")));
    }

    #[test]
    fn skill_md_has_required_frontmatter() {
        let arts = artifacts_for(SkillsTarget::Claude, &agents_body());
        let skill = arts
            .iter()
            .find(|a| a.path.ends_with("SKILL.md"))
            .expect("SKILL.md present");
        assert!(skill.contents.starts_with("---\n"));
        assert!(skill.contents.contains("name: sb-cli"));
        assert!(skill.contents.contains("description:"));
    }
}
