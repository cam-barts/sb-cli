//! Shared helpers for the integration tests.
use assert_cmd::cargo::CargoError;
use assert_cmd::Command;

/// The `sb` binary with every `SB_*` variable stripped. SB_SPACE outranks the
/// cwd and SB_SERVER_URL / SB_TOKEN outrank a space's config, so a dev shell
/// exporting any of them would aim a test at real data.
pub fn sb() -> Result<Command, CargoError> {
    Command::cargo_bin("sb").map(|mut cmd| {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("SB_") {
                cmd.env_remove(key);
            }
        }
        cmd
    })
}
