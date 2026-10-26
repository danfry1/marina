#!/usr/bin/env bash
# Regenerate demo/marina.gif. Requires: vhs (brew install vhs) and node.
#
# Runs VHS from a throwaway directory so VHS's own helper processes (ttyd, the
# headless renderer) have a cwd outside any dev project and aren't picked up by
# marina as targets — keeping the demo to just the scripted scene.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cargo build --release --manifest-path "$REPO/Cargo.toml"

work="$(mktemp -d)"
trap 'rm -rf "$work"; pkill -f "$HOME/.cache/marina-demo" 2>/dev/null || true; rm -rf "$HOME/.cache/marina-demo"' EXIT

export MARINA_BIN="$REPO/target/release/marina"
export MARINA_SCENE="$REPO/demo"
( cd "$work" && vhs "$REPO/demo/demo.tape" )
mv "$work/marina.gif" "$REPO/demo/marina.gif"
echo "wrote $REPO/demo/marina.gif"
