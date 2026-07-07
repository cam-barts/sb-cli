//! `sb mcp serve` — run sb as a Model Context Protocol server over the same core
//! as the CLI (one source of truth, two interfaces). Defaults to stdio (local
//! subprocess, zero network); Streamable HTTP is available behind `--http`.
//!
//! CRITICAL: in stdio mode nothing may be written to stdout except JSON-RPC —
//! all diagnostics must go to stderr, or the protocol stream is corrupted.
//!
//! NOTE: Phase 0 stub — real implementation (rmcp 2.x) lands in Phase 4.

use crate::error::{SbError, SbResult};

pub async fn execute_serve(_http: bool, _quiet: bool, _color: bool) -> SbResult<()> {
    Err(SbError::NotImplemented)
}
