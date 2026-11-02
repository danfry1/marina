// A small marina in the browser: `marina demo`'s made-up machine (src/demo.rs)
// in the servers view, with the keys that matter (j/k, g/G, Enter, X, R, t,
// d with Tab { } n N z, o, y, s, v, ?). It copies how marina looks and behaves; it doesn't run
// marina, and nothing it does leaves the page.
(function () {
  "use strict";

  var screen = document.getElementById("demo-screen");
  var term = document.getElementById("demo");
  var keys = document.getElementById("demo-keys");
  if (!screen || !term) return;

  var touchOnly = window.matchMedia("(hover: none) and (pointer: coarse)").matches;
  var reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  function esc(s) { return String(s).replace(/[&<>"]/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]; }); }
  function now() { return Math.floor(Date.now() / 1000); }
  function age(s) {
    s = Math.max(0, s);
    return s < 60 ? s + "s" : s < 3600 ? Math.floor(s / 60) + "m" : s < 86400 ? Math.floor(s / 3600) + "h" : Math.floor(s / 86400) + "d";
  }
  function mem(mb) { return mb >= 1024 ? (mb / 1024).toFixed(1) + "GB" : Math.round(mb) + "MB"; }
  function sp(cls, text) { return '<span class="' + cls + '">' + esc(text) + "</span>"; }

  // ---- The made-up machine (src/demo.rs) -----------------------------------

  var S, tick = 0;
  function agents() {
    return [
      { id: "claude·checkout", agent: "claude", project: "checkout", branch: "saved-cards", tree: "●3", shell: "pnpm test checkout", host: "herdr·ghostty", place: "herdr pane w0:p1 in Ghostty", quiet: 60 },
      { id: "claude·web", agent: "claude", project: "web", branch: "main", tree: "●2", shell: "pnpm vitest run", host: "herdr·ghostty", place: "herdr pane w2:p1 in Ghostty", quiet: 20 },
      { id: "codex·api", agent: "codex", project: "api", branch: "rate-limit", tree: "●2", shell: null, host: "tmux·iterm", place: "tmux pane work:1.0 in iTerm2", quiet: 180 },
      { id: "codex·search", agent: "codex", project: "search", branch: "pgvector-spike", tree: "✓", shell: null, host: "ghostty", place: "Ghostty terminal in ~/dev/search", quiet: 3 * 86400 },
    ];
  }
  function servers(t) {
    return [
      { project: "checkout", cmd: "next dev", port: 3000, cpu: 1.2, mem: 380, up: t - 7200, branch: "saved-cards", by: "claude·checkout" },
      { project: "checkout", cmd: "postgres", tag: "db", port: 5432, cpu: 0, mem: 0, up: t - 6 * 86400, branch: "—", by: null, container: "checkout-db-1" },
      { project: "docs", cmd: "astro", port: 4321, cpu: 0.2, mem: 120, up: t - 86400, branch: "main", by: "claude·ended", orphan: true },
      { project: "web", cmd: "vite", port: 5173, cpu: 0.8, mem: 210, up: t - 5 * 3600, branch: "main", by: "claude·web" },
      { project: "search", cmd: "storybook", port: 6006, cpu: 164, mem: 2970, up: t - 3 * 86400, branch: "pgvector-spike", by: "codex·search" },
      { project: "api", cmd: "cargo run", port: 8080, cpu: 0.4, mem: 42, up: t - 2400, branch: "rate-limit", by: "codex·api", lan: true },
    ];
  }

  // ---- What `d` shows: each session's work tree (src/demo.rs) ---------------

  var PATCH = {
      "checkout": "diff --git a/src/checkout/index.tsx b/src/checkout/index.tsx\n--- a/src/checkout/index.tsx\n+++ b/src/checkout/index.tsx\n@@ -1,4 +1,5 @@\n import { CardForm } from './CardForm'\n+import { SavedCards } from './SavedCards'\n import { useFlags } from '../lib/flags'\n \n type Props = { user: User }\n@@ -12,9 +13,13 @@ export function Checkout({ user }: Props) {\n   const flags = useFlags()\n-  const cards = user.cards\n+  const cards = user.savedCards ?? []\n \n-  return <CardForm user={user} />\n+  if (flags.savedCards && cards.length > 0) {\n+    return <SavedCards cards={cards} onNew={() => setAdding(true)} />\n+  }\n+\n+  return <CardForm user={user} />\n }\n \n export default Checkout\ndiff --git a/src/lib/flags.ts b/src/lib/flags.ts\n--- a/src/lib/flags.ts\n+++ b/src/lib/flags.ts\n@@ -3,5 +3,6 @@\n export const defaults = {\n-  payments: false,\n+  payments: true,\n+  savedCards: true,\n   search: true,\n }\n",
      "api": "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,6 +1,8 @@\n mod routes;\n+mod limit;\n \n use axum::Router;\n+use limit::RateLimit;\n \n #[tokio::main]\n async fn main() {\n@@ -18,7 +20,8 @@ async fn main() {\n     let app = Router::new()\n         .nest(\"/v1\", routes::public())\n-        .nest(\"/admin\", routes::admin());\n+        .nest(\"/admin\", routes::admin())\n+        .layer(RateLimit::per_minute(120));\n \n     serve(app).await\n }\n",
      "web": "diff --git a/src/lib/date.test.ts b/src/lib/date.test.ts\n--- a/src/lib/date.test.ts\n+++ b/src/lib/date.test.ts\n@@ -1,10 +1,15 @@\n-import { describe, expect, it } from 'vitest'\n+import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'\n import { relative } from './date'\n \n describe('relative', () => {\n+  beforeEach(() => {\n+    vi.useFakeTimers()\n+    vi.setSystemTime(new Date('2026-03-01T12:00:00Z'))\n+  })\n+  afterEach(() => vi.useRealTimers())\n+\n   it('says yesterday', () => {\n-    const d = new Date(Date.now() - 86_400_000)\n+    const d = new Date('2026-02-28T12:00:00Z')\n     expect(relative(d)).toBe('yesterday')\n   })\n })\ndiff --git a/src/lib/date.ts b/src/lib/date.ts\n--- a/src/lib/date.ts\n+++ b/src/lib/date.ts\n@@ -4,5 +4,5 @@ const DAY = 86_400_000\n export function relative(d: Date): string {\n-  const days = Math.floor((Date.now() - d.getTime()) / DAY)\n+  const days = Math.round((Date.now() - d.getTime()) / DAY)\n   if (days === 1) return 'yesterday'\n   return `${days} days ago`\n }\n"
  };
  var UNTRACKED = {
      "checkout": [
          [
              "src/checkout/SavedCards.tsx",
              "import type { Card } from '../lib/types'\n\ntype Props = { cards: Card[]; onNew: () => void }\n\nexport function SavedCards({ cards, onNew }: Props) {\n  return (\n    <ul className=\"saved-cards\">\n      {cards.map((c) => (\n        <li key={c.id}>•••• {c.last4} · {c.brand}</li>\n      ))}\n      <li><button onClick={onNew}>Use a new card</button></li>\n    </ul>\n  )\n}\n"
          ]
      ],
      "api": [
          [
              "src/limit.rs",
              "//! A token bucket per API key.\n\nuse std::time::{Duration, Instant};\n\npub struct RateLimit {\n    per: Duration,\n    burst: u32,\n}\n\nimpl RateLimit {\n    pub fn per_minute(n: u32) -> Self {\n        RateLimit { per: Duration::from_secs(60) / n, burst: n }\n    }\n}\n"
          ]
      ]
  };

  // `git diff` text → files: { path, status, added, removed, lines }
  function parsePatch(text) {
    var files = [], f = null, hunk = false;
    text.split("\n").forEach(function (l) {
      var m = /^diff --git a\/(.*) b\/(.*)$/.exec(l);
      if (m) { f = { path: m[2], status: "M", added: 0, removed: 0, lines: [] }; files.push(f); hunk = false; return; }
      if (!f) return;
      if (l.indexOf("@@") === 0) { hunk = true; f.lines.push(l); return; }
      if (!hunk) return;
      if (l === "") return; // the patch's last newline
      if (l[0] === "+") f.added++;
      if (l[0] === "-") f.removed++;
      f.lines.push(l);
    });
    return files;
  }
  function diffFiles(project) {
    var files = parsePatch(PATCH[project] || "");
    (UNTRACKED[project] || []).forEach(function (u) {
      var body = u[1].replace(/\n$/, "").split("\n").map(function (l) { return "+" + l; });
      files.push({ path: u[0], status: "?", added: body.length, removed: 0, lines: ["@@ -0,0 +1," + body.length + " @@"].concat(body) });
    });
    return files;
  }
  function treeSummary(project) {
    var f = diffFiles(project), t = f.filter(function (x) { return x.status !== "?"; }), u = f.length - t.length;
    if (!f.length) return "clean";
    var a = 0, r = 0;
    t.forEach(function (x) { a += x.added; r += x.removed; });
    return t.length + " file" + (t.length === 1 ? "" : "s") + " +" + a + " −" + r + (u ? " · " + u + " untracked" : "");
  }

  function init() {
    S = {
      servers: servers(now()), agents: agents(), sel: 0, sort: "port", by: "project",
      agentsOpen: false, folded: {}, log: null, zoom: false, paneH: 7, help: false, notice: null, confirm: null,
      touched: S ? S.touched : false, hint: false,
    };
    tell("⚠ /ended to review · 1 server left running by ended agent sessions", "yellow", true);
  }

  function via(s) { return s.orphan ? "claude·ended" : s.by ? s.by.split("·")[0] : "—"; }
  function cpuOf(s) {
    return s.cpu >= 50 ? s.cpu * (1 + Math.sin(tick * 0.7) * 0.15) : s.cpu * (1 + Math.sin(tick * 0.9 + s.port) * 0.12);
  }
  function agentCpu(a) { return a.shell ? 5.5 + Math.sin(tick + a.quiet) : 0.8; }
  function sorted(list) {
    var key = S.sort;
    return list.slice().sort(function (a, b) {
      return key === "cpu" ? cpuOf(b) - cpuOf(a) : key === "mem" ? b.mem - a.mem : a.port - b.port;
    });
  }

  // ---- The rows: what's selectable, in order --------------------------------

  // Each row: { kind: "group" | "server" | "agent" | "fold", ... }, as the
  // project view (grouped by project, agents folded at the bottom) or the
  // session view (grouped by who started each server) lays them out.
  function entries() {
    var out = [], byName = {}, names = [];
    function add(name, row) {
      if (!byName[name]) { byName[name] = []; names.push(name); }
      byName[name].push(row);
    }
    if (S.by === "project") {
      sorted(S.servers).forEach(function (s) { add(s.project, { kind: "server", s: s, child: true }); });
    } else {
      S.agents.forEach(function (a) { add(a.id, { kind: "agent", a: a, child: true }); });
      sorted(S.servers).forEach(function (s) { add(s.by || "unknown", { kind: "server", s: s, child: true }); });
    }
    names.forEach(function (n) {
      var g = byName[n];
      if (S.by === "project" && g.length === 1) { out.push({ kind: "server", s: g[0].s }); return; }
      out.push({ kind: "group", name: n, list: g });
      if (!S.folded[n]) out = out.concat(g);
    });
    if (S.by === "project") {
      out.push({ kind: "fold" });
      if (S.agentsOpen) S.agents.forEach(function (a) { out.push({ kind: "agent", a: a }); });
    }
    return out;
  }
  function current() { var e = entries(); return e[Math.min(S.sel, e.length - 1)]; }
  function serversOf(e) {
    if (!e) return [];
    if (e.kind === "server") return [e.s];
    if (e.kind === "group") return e.list.filter(function (r) { return r.kind === "server"; }).map(function (r) { return r.s; });
    return [];
  }

  // ---- Keys ----------------------------------------------------------------

  var noticeTimer;
  function tell(text, cls, sticky) {
    S.notice = { text: text, cls: cls || "dim" };
    clearTimeout(noticeTimer);
    noticeTimer = setTimeout(function () { S.notice = null; render(); }, sticky ? 20000 : 5000);
  }

  function handle(k) {
    if (S.help) { if (!/^(Shift|Control|Alt|Meta|CapsLock|Tab)$/.test(k)) { S.help = false; render(); } return k !== "Tab"; }
    if (S.confirm) {
      var c = S.confirm;
      S.confirm = null;
      if (k === c.key || k === "Enter") c.run(); else tell("cancelled");
      render();
      return true;
    }
    var n = entries().length, e = current(), dv = S.log && S.log.kind === "diff" ? S.log : null;
    if (k === "Tab" && !dv) return false; // the page's own Tab
    if (S.log && S.zoom && /^(j|k|g|G|ArrowDown|ArrowUp|Home|End)$/.test(k)) {
      paneMove(k);
      render();
      return true;
    }
    switch (k) {
      case "j": case "ArrowDown": S.sel = Math.min(n - 1, S.sel + 1); break;
      case "k": case "ArrowUp": S.sel = Math.max(0, S.sel - 1); break;
      case "g": case "Home": S.sel = 0; break;
      case "G": case "End": S.sel = n - 1; break;
      case "?": S.help = true; break;
      case "Escape": if (S.zoom) S.zoom = false; else closePane(); break;
      case "Tab": diffToggle(dv); break;
      case "{": case "}": if (dv) diffStep(dv, k === "}" ? 1 : -1); break;
      case "[": case "]":
        if (dv && dv.mode === "files") diffStep(dv, k === "]" ? 1 : -1);
        else if (S.log) scrollPane(k === "]" ? 5 : -5);
        break;
      case "n": case "N":
        if (dv) diffHunk(dv, k === "n");
        else tell(k + " moves between diff hunks · d opens a diff");
        break;
      case "z":
        if (S.log) S.zoom = !S.zoom;
        else tell("z zooms an open log or diff — t or d opens one");
        break;
      case "s":
        S.sort = S.sort === "port" ? "cpu" : S.sort === "cpu" ? "mem" : "port";
        break;
      case "v":
        S.by = S.by === "project" ? "session" : "project";
        S.sel = 0;
        tell(S.by === "session" ? "grouped by who started them" : "grouped by project");
        break;
      case "Enter": enter(e); break;
      case "X": kill(e); break;
      case "R": restart(e); break;
      case "t": logs(e); break;
      case "d": diff(e); break;
      case "o": case "y": url(e, k); break;
      case "K": case "x": tell(k + " isn't a key here — X (Shift-x) kills · ? lists them"); break;
      case "q": if (S.log) closePane(); else tell("q quits marina — this one stays"); break;
      default:
        if (!/^[a-zA-Z0-9]$/.test(k)) return false;
        // marina's other keys (fold, inspect, reverse sort) aren't emulated here
        tell("hliS".indexOf(k) >= 0 ? k + " works in marina, not in this demo · ? lists what does" : k + " isn't a key here · ? lists them");
    }
    followDiff();
    render();
    return true;
  }

  function enter(e) {
    if (!e) return;
    if (e.kind === "fold") { S.agentsOpen = !S.agentsOpen; return; }
    if (e.kind === "group") { S.folded[e.name] = !S.folded[e.name]; return; }
    if (e.kind === "agent") { tell("jumping to the " + e.a.place + "… (in the demo, nothing moves)", "cyan"); return; }
    tell("Enter inspects a server in marina — here, try X, R or t on it");
  }

  function what(e, list) {
    return e.kind === "group" ? list.length + " server" + (list.length === 1 ? "" : "s") + " in " + e.name : list[0].project + " · " + list[0].cmd;
  }
  // a verb on several servers asks first: its key again or Enter goes ahead
  function ask(key, prompt, run) { S.confirm = { key: key, run: run }; tell(prompt + "? " + key + " again or Enter · any other key cancels", "yellow", true); }

  function kill(e, confirmed) {
    if (e && e.kind === "agent") {
      tell("exit " + e.a.agent + " from its terminal — marina doesn't kill agent sessions; its servers are listed under it", "yellow");
      return;
    }
    var list = serversOf(e);
    if (!list.length) { tell(e && e.kind === "fold" ? "select a server — marina doesn't kill agent sessions" : "select a server or a group to kill"); return; }
    var label = what(e, list);
    if (list.length > 1 && !confirmed) { ask("X", "kill " + label, function () { kill(e, true); }); return; }
    list.forEach(function (s) { s.dying = true; });
    tell("stopping " + label + " — SIGTERM sent", "red");
    setTimeout(function () {
      S.servers = S.servers.filter(function (s) { return list.indexOf(s) < 0; });
      render();
    }, reduced ? 0 : 1200);
  }

  function restart(e, confirmed) {
    if (e && e.kind === "agent") { tell("exit " + e.a.agent + " from its terminal — marina doesn't restart agent sessions; its servers are listed under it", "yellow"); return; }
    var list = serversOf(e);
    if (!list.length) { tell(e && e.kind === "fold" ? "select a server — marina doesn't restart agent sessions" : "select a server or a group to restart"); return; }
    var label = what(e, list);
    if (list.length > 1 && !confirmed) { ask("R", "restart " + label, function () { restart(e, true); }); return; }
    tell("restarting " + label + "…", "yellow");
    setTimeout(function () {
      list.forEach(function (s) { s.up = now(); s.born = Date.now(); });
      tell(list.length === 1 && list[0].container ? "docker restart " + list[0].container + ": done" : "t shows its output · restarted " + list[0].project, "green");
      render();
    }, reduced ? 0 : 900);
  }

  function logLines(s) {
    if (s.cmd === "postgres") return ["LOG:  database system is ready to accept connections", "LOG:  checkpoint starting: time", "LOG:  checkpoint complete: wrote 12 buffers (0.1%)"];
    if (s.cmd === "cargo run") return ["   Compiling api v0.3.0 (/Users/you/dev/api)", "    Finished `dev` profile in 4.21s", "     Running `target/debug/api` — listening on 0.0.0.0:8080", "GET /v1/search 200 12ms", "GET /v1/search 429 1ms  rate limited (key …9f2c)"];
    if (s.cmd === "storybook") return ["info => Starting manager..", "info => Storybook started on http://localhost:6006", "WARN Rebuilding: 418 stories changed", "WARN Rebuilding: 418 stories changed", "WARN Rebuilding: 418 stories changed"];
    return ["> " + s.project + "@0.1.0 dev", "> " + s.cmd, "", "  ➜  Local:   http://localhost:" + s.port + "/", "  ✓ Ready in 1.8s", " GET / 200 in 142ms", " GET /" + s.project + " 200 in 88ms"];
  }
  function logs(e) {
    if (S.log && S.log.kind === "log") { S.log = null; return; }
    if (!e || e.kind !== "server") { tell(e && e.kind === "agent" ? "select one of its servers — a " + e.a.agent + " session has no log" : e && e.kind === "fold" ? "select a server — the agents section has no log" : "select a server to see its log"); return; }
    var s = e.s;
    S.log = { kind: "log", title: s.container ? "docker logs " + s.container : "~/dev/" + s.project + "/.dev.log", lines: logLines(s) };
    tell("tailing " + s.project + " · " + s.cmd);
  }
  // the work tree d diffs for a row: an agent's, or a server's (a container has none)
  function treeOf(e) {
    if (!e) return null;
    if (e.kind === "agent") return e.a.project;
    if (e.kind === "server" && !e.s.container) return e.s.project;
    return null;
  }
  function diff(e) {
    var p = treeOf(e);
    if (S.log && S.log.kind === "diff") { closePane(); return; }
    if (!p) { tell("select an agent session or a server to see its diff"); return; }
    openDiff(p);
  }
  function openDiff(p) {
    S.log = { kind: "diff", project: p, title: "~/dev/" + p, files: diffFiles(p), cursor: 0, mode: "files", scroll: 0 };
  }
  function closePane() {
    if (S.log) tell(S.log.kind === "diff" ? "closed the diff" : "closed the log");
    S.log = null;
    S.zoom = false;
  }
  // with the diff open, a row in another work tree shows that tree's diff
  function followDiff() {
    if (!S.log || S.log.kind !== "diff") return;
    var p = treeOf(current());
    if (p && p !== S.log.project) openDiff(p);
  }
  function diffToggle(d) {
    if (d.mode === "files" && !d.files.length) return;
    d.mode = d.mode === "files" ? "patch" : "files";
    d.scroll = 0;
  }
  function diffStep(d, by) {
    var to = Math.max(0, Math.min(d.files.length - 1, d.cursor + by));
    if (to !== d.cursor) { d.cursor = to; d.scroll = 0; }
  }
  function isHunk(l) { return l.indexOf("@@") === 0; }
  // n / N: the next / previous hunk, running on into the next / previous file
  function diffHunk(d, fwd) {
    if (d.mode === "files") { diffStep(d, fwd ? 1 : -1); return; }
    var lines = d.files[d.cursor].lines, i;
    if (fwd) { for (i = d.scroll + 1; i < lines.length; i++) if (isHunk(lines[i])) { d.scroll = i; return; } }
    else { for (i = Math.min(d.scroll, lines.length) - 1; i >= 0; i--) if (isHunk(lines[i])) { d.scroll = i; return; } }
    if (fwd && d.cursor + 1 < d.files.length) { d.cursor++; d.scroll = 0; return; }
    if (!fwd && d.cursor > 0) {
      d.cursor--;
      var prev = d.files[d.cursor].lines;
      for (i = prev.length - 1; i > 0 && !isHunk(prev[i]); i--);
      d.scroll = Math.max(0, i);
      return;
    }
    tell(fwd ? "last hunk" : "first hunk");
  }
  function paneRows() {
    var l = S.log;
    if (l.kind === "log") return l.lines.length;
    return l.mode === "files" ? l.files.length : l.files[l.cursor].lines.length;
  }
  function scrollPane(by) {
    var l = S.log, max = Math.max(0, paneRows() - S.paneH);
    l.scroll = Math.max(0, Math.min(max, (l.scroll || 0) + by));
  }
  // j k g G while zoomed: through the pane
  function paneMove(k) {
    var l = S.log, files = l.kind === "diff" && l.mode === "files";
    var down = k === "j" || k === "ArrowDown", up = k === "k" || k === "ArrowUp", top = k === "g" || k === "Home";
    if (files) {
      if (down || up) diffStep(l, down ? 1 : -1);
      else l.cursor = top ? 0 : l.files.length - 1;
    } else if (down || up) scrollPane(down ? 1 : -1);
    else l.scroll = top ? 0 : Math.max(0, paneRows() - S.paneH);
  }
  function url(e, k) {
    var s = e && e.kind === "server" ? e.s : null;
    if (e && e.kind === "agent") s = S.servers.filter(function (x) { return x.by === e.a.id; })[0];
    if (e && e.kind === "group") s = serversOf(e).filter(function (x) { return x.cmd !== "postgres"; })[0];
    if (!s) { tell(e && e.kind === "agent" ? e.a.id + " hasn't started a web server" : e && e.kind === "fold" ? "the agents section has no URL — select a session or a server" : "select a server to open its URL"); return; }
    var u = (s.cmd === "postgres" ? "postgres://localhost:" : "http://localhost:") + s.port;
    if (k === "y") {
      tell("demo: y would copy " + u, "green");
    } else tell(s.cmd === "postgres" ? u + " is not a web URL" : "demo: o would open " + u, "cyan");
  }

  // ---- Drawing ---------------------------------------------------------------

  function cells(c) { return c.map(function (x) { return "<div" + (x[1] ? ' class="' + x[1] + '"' : "") + ">" + x[0] + "</div>"; }).join(""); }
  function rowHtml(e, i, t) {
    var sel = i === S.sel, bar = sel ? sp("mag", "▌") + " " : "  ";
    var cls = "tr" + (sel ? " sel" : "");
    var open = '<div class="' + cls + '" data-row="' + i + '">';
    if (e.kind === "fold") {
      var working = S.agents.filter(function (a) { return a.shell; }).length, stale = S.agents.filter(function (a) { return a.quiet > 86400; }).length;
      var acpu = S.agents.reduce(function (n, a) { return n + agentCpu(a); }, 0);
      return open + cells([[bar + (S.agentsOpen ? "▾" : "▸") + " agents (" + S.agents.length + ")", "dim"], [working + " working", "yellow"], [""],
        [acpu.toFixed(1) + "%", "dim"], [mem(520 * S.agents.length), "dim"], [stale + " stale", "dim c-via"], ["", "c-up"], ["", "c-br"]]) + "</div>";
    }
    if (e.kind === "group") {
      var gc = 0, gm = 0;
      e.list.forEach(function (r) {
        if (r.kind === "agent") { gc += agentCpu(r.a); gm += 520; } else { gc += cpuOf(r.s); gm += r.s.mem; }
      });
      return open + cells([[bar + (S.folded[e.name] ? "▸ " : "▾ ") + "<b>" + esc(e.name) + "</b>" + sp("dim", " (" + e.list.length + ")")], [""], [""],
        [gc.toFixed(1) + "%"], [mem(gm)], ["", "c-via"], ["", "c-up"], ["", "c-br"]]) + "</div>";
    }
    var lead = bar + (e.child ? (S.by === "session" && e.kind === "server" ? "  " : "  ↳ ") : "");
    if (e.kind === "agent") {
      var a = e.a;
      return open + cells([[lead + (e.child ? "◆ " + esc(a.agent) : esc(a.id)), "mag"], [a.shell ? esc("$ " + a.shell) : "idle " + age(a.quiet), a.shell ? "yellow" : "dim"], ["—", "dim"],
        [agentCpu(a).toFixed(1) + "%"], ["520MB"], ["—", "dim c-via"], [a.quiet > 86400 ? "3d" : "4h", "c-up"],
        [sp(a.tree === "✓" ? "green" : "yellow", a.tree) + " " + esc(a.branch), "c-br"]]) + "</div>";
    }
    var s = e.s, c = cpuOf(s), born = s.born && Date.now() - s.born < 1300;
    open = '<div class="' + cls + (s.dying ? " dying" : born ? " born" : "") + '" data-row="' + i + '">';
    return open + cells([[lead + (e.child && S.by !== "session" ? "" : esc(s.project))], [s.dying ? "stopping…" : esc(s.cmd) + (s.tag ? sp("dim", " ·" + s.tag) : "")],
      [":" + s.port + (s.lan ? sp("red", "!") : "")], [s.container ? "—" : c.toFixed(1) + "%"], [s.container ? "—" : mem(s.mem)],
      [esc(via(s)), (s.orphan ? "yellow" : s.by ? "mag" : "dim") + " c-via"], [s.container ? "—" : age(t - s.up), "c-up"], [esc(s.branch), "dim c-br"]]) + "</div>";
  }

  function detailLine(e) {
    if (!e) return "";
    if (e.kind === "fold") return S.agents.length + " agent sessions · 2 working · 1 idle and open >1d (520MB) · helpers: chrome-devtools-mcp ×4 192MB · Enter folds/unfolds";
    if (e.kind === "group") { var l = serversOf(e); return e.name + " · " + l.length + " server" + (l.length === 1 ? "" : "s") + " · X " + (S.by === "session" ? "stops its servers" : "kills all"); }
    if (e.kind === "agent") return e.a.id + " · " + (e.a.shell ? "$ " + e.a.shell : "idle " + age(e.a.quiet)) + " · " + treeSummary(e.a.project) + " · Enter jumps (" + e.a.host + ") · d its diff";
    var s = e.s;
    return s.project + " · " + s.cmd + " · " + (s.cmd === "postgres" ? "postgres://" : "http://") + "localhost:" + s.port + " · ~/dev/" + s.project +
      (s.lan ? " · LAN-exposed (0.0.0.0)" : "") + (s.orphan ? " · via claude (session 5e551000 ended)" : s.by ? " · via " + s.by : s.container ? " · docker " + s.container : "");
  }

  function ln(html, cls) { return '<div class="ln ' + (cls || "") + '">' + (html || "") + "</div>"; }

  // ---- The diff pane ---------------------------------------------------------

  function counts(f) {
    var a = f.added ? sp("green", "+" + f.added) : "", r = f.removed ? sp("red", "−" + f.removed) : "";
    return a + (a && r ? " " : "") + r;
  }
  function countsLen(f) { return (f.added ? String(f.added).length + 1 : 0) + (f.removed ? String(f.removed).length + 1 : 0) + (f.added && f.removed ? 1 : 0); }
  function pad(s, w) { return s + new Array(Math.max(0, w - s.length) + 1).join(" "); }
  function fileRows(d) {
    if (!d.files.length) return [ln(sp("dim", "  no uncommitted changes"))];
    var pw = Math.max.apply(null, d.files.map(function (f) { return f.path.length; }));
    var cw = Math.max.apply(null, d.files.map(countsLen));
    var top = Math.max(0, d.cursor - (S.paneH - 1));
    return d.files.slice(top).map(function (f, j) {
      var i = top + j, st = f.status === "?" || f.status === "A" ? "green" : f.status === "D" ? "red" : "yellow";
      return '<div class="ln file' + (i === d.cursor ? " fsel" : "") + '" data-file="' + i + '">' + (i === d.cursor ? sp("mag", "▌") + " " : "  ") +
        sp(st, f.status) + " " + esc(pad(f.path, pw)) + "  " + pad("", cw - countsLen(f)) + counts(f) + "</div>";
    });
  }
  // the changed middle of a replaced line, widened to whole words; null when
  // the two share too little for that to mean anything
  function changedMiddle(a, b) {
    var pre = 0, n = Math.min(a.length, b.length);
    while (pre < n && a[pre] === b[pre]) pre++;
    var suf = 0;
    while (suf < a.length - pre && suf < b.length - pre && a[a.length - 1 - suf] === b[b.length - 1 - suf]) suf++;
    var word = /[A-Za-z0-9_]/;
    while (pre > 0 && word.test(a[pre - 1])) pre--;
    function joined(s) { return suf > 0 && word.test(s[s.length - suf - 1] || "") && word.test(s[s.length - suf]); }
    while (suf > 0 && (joined(a) || joined(b))) suf--;
    var common = pre + suf, longer = Math.max(a.length, b.length);
    if (common === longer || common * 3 < longer) return null;
    return [[pre, a.length - suf], [pre, b.length - suf]];
  }
  function patchRows(f) {
    var lines = f.lines, nums = [], emph = [], old = 0, nw = 0, top = 0, i = 0;
    while (i < lines.length) {
      var l = lines[i], c = l[0];
      if (c === "@") {
        var m = /^@@ -(\d+)(?:,\d+)? \+(\d+)/.exec(l);
        old = m ? +m[1] : 0; nw = m ? +m[2] : 0;
        nums.push([null, null]); i++;
      } else if (c === "-") {
        var rm = i;
        while (i < lines.length && lines[i][0] === "-") { nums.push([old++, null]); i++; }
        var ad = i;
        while (i < lines.length && lines[i][0] === "+") { nums.push([null, nw++]); i++; }
        for (var k = 0; k < Math.min(ad - rm, i - ad); k++) {
          var cm = changedMiddle(lines[rm + k].slice(1), lines[ad + k].slice(1));
          if (cm) { emph[rm + k] = cm[0]; emph[ad + k] = cm[1]; }
        }
      } else if (c === "+") { nums.push([null, nw++]); i++; }
      else { nums.push([old++, nw++]); i++; }
      top = Math.max(top, old, nw);
    }
    var w = String(Math.max(1, top)).length;
    function num(x) { return x === null ? pad("", w) : pad("", w - String(x).length) + x; }
    return lines.map(function (l, j) {
      var c = l[0], gutter = sp("dim", num(nums[j][0]) + " " + num(nums[j][1]) + " │ ");
      if (c === "@") {
        var parts = l.split("@@");
        return ln(sp("dim", pad("", w) + " " + pad("", w) + " ┊ ") + sp("cyan", parts[1].trim() + " ") + "<b>" + sp("cyan", (parts[2] || "").trim()) + "</b>");
      }
      if (c === "+" || c === "-") {
        var cls = c === "+" ? "green" : "red", t = l.slice(1), e = emph[j];
        var body = e ? sp(cls, t.slice(0, e[0])) + sp("rev-" + cls, t.slice(e[0], e[1])) + sp(cls, t.slice(e[1])) : sp(cls, t);
        return ln(gutter + sp(cls, c + " ") + body);
      }
      return ln(gutter + esc("  " + l.slice(1)));
    });
  }
  function diffPane(d) {
    var z = S.zoom ? "z unzoom" : "z zoom", mv = S.zoom ? "j k" : "[ ]";
    if (d.mode === "patch" && d.files[d.cursor]) {
      var f = d.files[d.cursor];
      return { title: " diff: " + d.title + " · " + f.path + " (" + (d.cursor + 1) + "/" + d.files.length + ") — Tab files · n N hunk · { } file · " + mv + " scroll · " + z + " · q close ",
        rows: patchRows(f).slice(d.scroll) };
    }
    var a = 0, r = 0;
    d.files.forEach(function (x) { a += x.added; r += x.removed; });
    var n = d.files.length;
    return { title: n ? " diff: " + d.title + " · " + n + " file" + (n === 1 ? "" : "s") + " +" + a + " −" + r + " — Tab/click opens · " + mv + " move · " + z + " · q close " : " diff: " + d.title + " — " + z + " · q close ",
      rows: fileRows(d) };
  }

  function helpHtml() {
    var rows = [["j / k", "move · g / G top · bottom"], ["Enter", "fold a group · on an agent: jump to it"], ["X", "kill (again: force-kill)"],
      ["R", "restart, output captured to a log"], ["", "X and R on a group ask first"], ["t · d", "its log · its uncommitted diff"], ["Tab { }", "diff: files ↔ patch · next file"], ["n N · z", "diff: hunks · zoom the pane"], ["o · y", "open · copy its URL"],
      ["s", "sort: port · cpu · mem"], ["v", "group by project / by who started them"]];
    return '<div class="help"><div class="help-box">' + ln("<b>" + sp("cyan", "marina — keys") + "</b>") + ln("") +
      rows.map(function (r) { return '<div class="help-row">' + sp("mag", r[0]) + sp("white", r[1]) + "</div>"; }).join("") +
      ln("") + ln(sp("dim", "any key closes · the real thing also filters with /")) + "</div></div>";
  }

  // the keys that do something on this row
  function hints(e) {
    if (!e) return "v by session · ? help";
    if (e.kind === "group") { var n = serversOf(e).length; return "Enter fold · X kill " + n + " · R restart " + n + " · o open · ? help"; }
    if (e.kind === "fold") return "Enter fold/unfold · v by session · ? help";
    if (e.kind === "agent") return "Enter jump · d diff · o open its server · ? help";
    return "Enter inspect · o open · y copy · t logs · R restart · X kill · ? help";
  }

  function render() {
    var list = entries();
    if (S.sel >= list.length) S.sel = Math.max(0, list.length - 1);
    var t = now(), e = list[S.sel];
    var title = " DEMO · marina · " + S.servers.length + " servers · " + S.agents.length + " agents 2.0GB · sort:" + S.sort + (S.by === "session" ? " · by:session" : "") + " ";
    var head = cells([["  PROJECT", "th"], ["COMMAND", "th"], ["PORT" + (S.sort === "port" ? " ▾" : ""), "th"], ["CPU" + (S.sort === "cpu" ? " ▾" : ""), "th"],
      ["MEM" + (S.sort === "mem" ? " ▾" : ""), "th"], ["VIA", "th c-via"], ["UP", "th c-up"], ["BRANCH", "th c-br"]]);
    var zoomed = S.log && S.zoom, html = "";
    if (!zoomed) {
      html += '<div class="box main"><div class="box-title"><span>' + esc(title) + '</span></div><div class="box-body"><div class="tbl">' +
        head + list.map(function (x, i) { return rowHtml(x, i, t); }).join("") + "</div></div></div>";
    }
    if (S.log) {
      var pane = S.log.kind === "diff" ? diffPane(S.log)
        : { title: " log: " + S.log.title + " — [ ] scroll · " + (S.zoom ? "z unzoom" : "z zoom") + " · q close ", rows: S.log.lines.slice(S.log.scroll || 0).map(function (l) { return ln(esc(l)); }) };
      html += '<div class="box logs' + (S.log.kind === "diff" ? " diff" : "") + (zoomed ? " zoom" : "") + '"><div class="box-title"><span>' + esc(pane.title) +
        '</span></div><div class="box-body">' + pane.rows.join("") + "</div></div>";
    }
    html += '<div class="status">' + ln(esc(detailLine(e))) +
      ln(S.notice ? sp(S.notice.cls, S.notice.text) : sp("dim", hints(e))) + "</div>";
    if (S.help) html += helpHtml();
    if (S.hint) {
      html += '<div class="invite-hint"><b>Your turn.</b> ' + (touchOnly
        ? "Tap the keys below: X kills, t shows logs, d shows an agent's diff."
        : "Press <kbd>j</kbd> <kbd>k</kbd> to move, <kbd>X</kbd> to kill, <kbd>t</kbd> for logs, <kbd>d</kbd> on an agent for its diff.") + "</div>";
    }
    screen.innerHTML = html;
    // how many pane lines fit, for paging and keeping the file cursor in view
    var body = screen.querySelector(".box.logs .box-body"), one = screen.querySelector(".box.logs .ln");
    if (body && one && one.offsetHeight) {
      var h = Math.max(1, Math.floor(body.clientHeight / one.offsetHeight));
      if (h !== S.paneH) { S.paneH = h; render(); }
    }
  }

  // ---- Input ---------------------------------------------------------------

  function flashKey(k) {
    var b = document.querySelector('.keycaps [data-key="' + k + '"]');
    if (!b) return;
    b.classList.remove("hit"); void b.offsetWidth; b.classList.add("hit");
  }
  function touch() {
    if (S.touched) return;
    S.touched = true; S.hint = false;
    term.classList.remove("invite");
    render();
  }
  term.addEventListener("keydown", function (e) {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    touch();
    if (handle(e.key)) { e.preventDefault(); flashKey(e.key); }
  });
  // Clicking a row selects it; clicking it again is Enter
  term.addEventListener("pointerdown", function (e) {
    touch();
    var file = e.target.closest("[data-file]");
    if (file && S.log && S.log.kind === "diff") {
      S.log.cursor = +file.dataset.file; S.log.mode = "patch"; S.log.scroll = 0;
      render();
      return;
    }
    var row = e.target.closest("[data-row]");
    if (!row || !term.contains(row)) return;
    var i = +row.dataset.row;
    if (S.sel === i) handle("Enter"); else { S.sel = i; render(); }
  });

  // Keys come in through a hidden text box (see .demo-keys), so browser
  // extensions with vim keys leave them alone, except on touch screens,
  // where focusing it would open the on-screen keyboard.
  function focusDemo() { (keys && !touchOnly ? keys : term).focus({ preventScroll: true }); }
  function focused() { return term.contains(document.activeElement); }
  term.addEventListener("click", focusDemo);
  term.addEventListener("focus", function () { if (keys && !touchOnly) keys.focus({ preventScroll: true }); });
  term.addEventListener("focusin", function () { term.classList.add("live"); });
  term.addEventListener("focusout", function () { setTimeout(function () { if (!focused()) term.classList.remove("live"); }, 0); });
  if (keys) keys.addEventListener("input", function () { keys.value = ""; });

  // While the demo is on screen, its keys work from anywhere on the page
  // (not Space or Enter, which the page needs), so you can just start typing.
  var pageKeys = "jkgGXRtdoysvKx?nNz{}[]";
  var onScreen = false;
  document.addEventListener("keydown", function (e) {
    if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey) return;
    if (!onScreen || focused() || term.contains(e.target)) return;
    var el = document.activeElement;
    if (el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT|BUTTON|A)$/.test(el.tagName))) return;
    if (e.key.length !== 1 || pageKeys.indexOf(e.key) < 0) return;
    e.preventDefault();
    focusDemo();
    touch();
    handle(e.key);
    flashKey(e.key);
  });
  document.querySelectorAll(".keycaps [data-key]").forEach(function (b) {
    // Pressing a key with a mouse or finger leaves focus on the demo, so it
    // stays live (and keeps its keys) through the press.
    b.addEventListener("pointerdown", function (e) { if (focused()) e.preventDefault(); });
    b.addEventListener("click", function () {
      touch();
      handle(b.dataset.key);
      flashKey(b.dataset.key);
      if (!touchOnly) focusDemo();
    });
  });

  // ---- Showing that it's playable -------------------------------------------
  // The first time the demo is on screen it moves to the server an ended
  // session left behind and kills it, then groups by agent session, with the
  // keycaps going down, then hands over. Any input stops it.

  var script = [["j", 1400], ["j", 500], ["j", 500], ["X", 1100], ["v", 2600]];
  var shown = false;
  function attract() {
    if (shown || S.touched) return;
    shown = true;
    init();
    render();
    if (reduced) { invite(); return; }
    var i = 0;
    (function next() {
      if (S.touched) return;
      if (i >= script.length) { setTimeout(invite, 1800); return; }
      var k = script[i][0], wait = script[i][1];
      i++;
      setTimeout(function () {
        if (S.touched) return;
        handle(k);
        flashKey(k);
        next();
      }, wait);
    })();
  }
  function invite() {
    if (S.touched) return;
    S.hint = true;
    term.classList.add("invite");
    render();
  }
  if ("IntersectionObserver" in window) {
    new IntersectionObserver(function (es) {
      es.forEach(function (e) {
        onScreen = e.intersectionRatio >= 0.4;
        if (e.intersectionRatio >= 0.6) setTimeout(attract, 500);
      });
    }, { threshold: [0, 0.4, 0.6] }).observe(term);
  }

  // ---- Time ------------------------------------------------------------------

  setInterval(function () {
    tick++;
    if (!document.hidden && !S.help) render();
  }, 1000);

  init();
  render();
})();
