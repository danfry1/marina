#!/usr/bin/env bash
# Spin up a few throwaway dev servers so the demo GIF has a populated scene.
# Requires node. Torn down by scene-down.sh. Used only by demo/demo.tape.
set -e
BASE="${MARINA_DEMO_DIR:-$HOME/.cache/marina-demo}"
rm -rf "$BASE"
mkdir -p "$BASE/client-portal/bin" "$BASE/billing-api/bin" "$BASE/analytics/bin"
printf '{"name":"client-portal"}\n' >"$BASE/client-portal/package.json"
printf '{"name":"billing-api"}\n' >"$BASE/billing-api/package.json"
printf '{"name":"analytics"}\n' >"$BASE/analytics/package.json"

# A vite-style server on :3000 that writes a live log (for the tail demo) and
# does a little periodic work (so the CPU column isn't all zeros).
cat >"$BASE/client-portal/bin/vite.js" <<'JS'
const fs = require("fs");
const fd = fs.openSync(__dirname + "/../dev.log", "a");
require("http").createServer((_, r) => r.end("ok")).listen(3000, () => {});
setInterval(() => fs.writeSync(fd, `[${new Date().toISOString()}] GET /api/products 200 12ms\n`), 700);
setInterval(() => { let x = 0; for (let i = 0; i < 3e6; i++) x += i; }, 250);
JS
printf 'setInterval(()=>{},1000);\n' >"$BASE/client-portal/noop.js" # a port-less watcher
cat >"$BASE/billing-api/bin/uvicorn" <<'JS'
require("http").createServer((_, r) => r.end("ok")).listen(8000, () => {});
JS
cat >"$BASE/analytics/bin/redis-server.js" <<'JS'
require("http").createServer((_, r) => r.end("ok")).listen(6399, () => {});
JS

# Run each from its project dir (so marina resolves the project name), but with
# an absolute script path (so scene-down can find them by path).
( cd "$BASE/client-portal" && exec node "$BASE/client-portal/bin/vite.js" ) &
( cd "$BASE/client-portal" && exec node --watch "$BASE/client-portal/noop.js" ) &
( cd "$BASE/billing-api" && exec node "$BASE/billing-api/bin/uvicorn" ) &
( cd "$BASE/analytics" && exec node "$BASE/analytics/bin/redis-server.js" ) &
sleep 1
