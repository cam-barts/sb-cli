//! `sb upgrade` — self-update from this project's GitHub releases.
//!
//! Flavor-aware: an `ai` build only ever fetches `sb-ai-*` assets and a slim
//! build only `sb-*` assets, so an upgrade never silently swaps flavors. Uses
//! the `self_update` crate (GitHub-releases backend) which verifies downloads
//! and replaces the running binary in place.
//!
//! NOTE: `self_update` uses a blocking HTTP client, so callers must invoke this
//! off the async runtime (the dispatcher runs it on a blocking task).

use self_update::backends::github::Update;
use self_update::cargo_crate_version;

use crate::error::{SbError, SbResult};
use crate::output;

const REPO_OWNER: &str = "cam-barts";
const REPO_NAME: &str = "sb-cli";

/// The release-asset identifier substring for the running build's flavor. Our
/// assets are `sb-vX-<target>` (slim) and `sb-ai-vX-<target>` (ai); `"sb-ai-"`
/// matches only the ai asset and `"sb-v"` matches only the slim one (release
/// tags always start with `v`), so the two never cross-match.
fn flavor_identifier() -> &'static str {
    if cfg!(feature = "ai") {
        "sb-ai-"
    } else {
        "sb-v"
    }
}

pub fn execute(check: bool, quiet: bool, color: bool) -> SbResult<()> {
    let current = cargo_crate_version!();
    let target = self_update::get_target();

    let update = Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name("sb")
        .target(target)
        .identifier(flavor_identifier())
        .current_version(current)
        // Agents/scripts (`--yes`, `--no-input`, or a pipe) must not be prompted
        // before the binary is replaced.
        .no_confirm(output::assume_yes() || output::no_input())
        .show_download_progress(!quiet && output::is_tty())
        .build()
        .map_err(map_err)?;

    if check {
        let latest = update.get_latest_release().map_err(map_err)?;
        let newer =
            self_update::version::bump_is_greater(current, &latest.version).map_err(map_err)?;
        if newer {
            output::print_success(
                &format!(
                    "update available: {current} -> {} (run `sb upgrade`)",
                    latest.version
                ),
                color,
                quiet,
            );
        } else {
            output::print_success(&format!("up to date ({current})"), color, quiet);
        }
        return Ok(());
    }

    let status = update.update().map_err(map_err)?;
    if status.updated() {
        output::print_success(&format!("upgraded to {}", status.version()), color, quiet);
    } else {
        output::print_success(&format!("already up to date ({current})"), color, quiet);
    }
    Ok(())
}

/// Map a `self_update` error onto our taxonomy. Network/HTTP failures surface as
/// general errors with the underlying message preserved for diagnosis.
fn map_err(e: self_update::errors::Error) -> SbError {
    SbError::Config {
        message: format!("upgrade failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flavor_identifier_matches_build() {
        // The identifier must disambiguate the two flavors: neither is a
        // substring of the other's asset stem.
        let id = flavor_identifier();
        if cfg!(feature = "ai") {
            assert_eq!(id, "sb-ai-");
        } else {
            assert_eq!(id, "sb-v");
            assert!(
                !"sb-ai-v1.0.0-x".contains(id),
                "slim id must not match ai assets"
            );
        }
    }
}
