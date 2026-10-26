#!/usr/bin/env bash
# Regenerate demo/marina.gif from `marina demo`. Requires: vhs (brew install vhs).
#
# Runs VHS from a throwaway directory, so nothing it writes lands in the repo
# except the GIF.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo build --release --manifest-path "$REPO/Cargo.toml"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

export MARINA_BIN="$REPO/target/release/marina"
( cd "$work" && vhs "$REPO/demo/demo.tape" )
mv "$work/marina.gif" "$REPO/demo/marina.gif"
echo "wrote $REPO/demo/marina.gif"
