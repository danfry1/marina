#!/usr/bin/env bash
# Tear down the demo scene started by scene-up.sh.
BASE="${MARINA_DEMO_DIR:-$HOME/.cache/marina-demo}"
pkill -f "$BASE" 2>/dev/null || true
rm -rf "$BASE"
