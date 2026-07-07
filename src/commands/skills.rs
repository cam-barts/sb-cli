//! `sb skills init` — generate agent instruction/skill files that teach coding
//! agents how to drive sb (AGENTS.md by default; SKILL.md / Cursor / Copilot /
//! Windsurf on request). Files are populated from `sb schema` so they stay in
//! sync with the real command surface, and are kept lean and command-first.
//!
//! NOTE: Phase 0 stub — real implementation lands in Phase 3.

use crate::cli::SkillsTarget;
use crate::error::{SbError, SbResult};

pub fn execute_init(_target: SkillsTarget, _quiet: bool, _color: bool) -> SbResult<()> {
    Err(SbError::NotImplemented)
}
