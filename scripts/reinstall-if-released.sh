#!/usr/bin/env bash
# Reinstall the local `sb` when main carries a version the installed binary
# doesn't, i.e. right after a release lands. Local only: run by the opt-in
# pre-commit `post-merge` hook (see .pre-commit-config.yaml), or by hand.
#
# Installs with `cargo install` (into $CARGO_HOME/bin) rather than pointing at
# target/, so `cargo clean` can never leave `sb` dangling. Keeps whatever
# feature set the installed binary was built with (slim or `ai`).
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || exit 0

want=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
info=$(sb version 2>/dev/null || true)
have=$(printf '%s\n' "$info" | awk 'NR==1 {print $2}')
[ "$want" = "$have" ] && exit 0

features=()
installed_features=$(printf '%s\n' "$info" | awk -F': *' '/^features:/ {print $2}')
if [[ " ${installed_features//,/ } " == *" ai "* ]]; then
    features=(--features ai)
fi

echo "sb ${have:-<not installed>} -> $want: reinstalling ${features[*]:-(slim)}"
cargo install --path . --locked "${features[@]}"
