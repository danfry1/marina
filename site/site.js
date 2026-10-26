// The page's small interactions: copy buttons, tabs and the harbour. The
// servers demo is demo.js.
(function () {
  "use strict";

  var reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  function esc(s) { return String(s).replace(/[&<>"]/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]; }); }

  // Copy buttons copy exactly the commands shown, without the prompts
  var status = document.getElementById("copy-status");
  document.querySelectorAll(".copy").forEach(function (b) {
    var timer;
    b.addEventListener("click", function () {
      var pre = b.parentNode.querySelector("pre").cloneNode(true);
      pre.querySelectorAll(".p").forEach(function (p) { p.remove(); });
      navigator.clipboard.writeText(pre.textContent.trim()).then(function () {
        b.classList.add("done");
        status.textContent = "Copied";
        clearTimeout(timer);
        timer = setTimeout(function () { b.classList.remove("done"); status.textContent = ""; }, 1600);
      });
    });
  });

  // Tabs, each list on its own, with arrow keys between them
  document.querySelectorAll('[role="tablist"]').forEach(function (list) {
    var tabs = Array.prototype.slice.call(list.querySelectorAll('[role="tab"]'));
    function select(tab) {
      tabs.forEach(function (t) {
        var on = t === tab;
        t.setAttribute("aria-selected", on);
        t.tabIndex = on ? 0 : -1;
        document.getElementById(t.getAttribute("aria-controls")).hidden = !on;
      });
      tab.focus({ preventScroll: true });
    }
    tabs.forEach(function (t, i) {
      t.addEventListener("click", function () { select(t); });
      t.addEventListener("keydown", function (e) {
        var d = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
        if (d) { e.preventDefault(); select(tabs[(i + d + tabs.length) % tabs.length]); }
      });
    });
  });

  // Plays an element's entrance once, when it first comes into view
  function onView(el, fn, ratio) {
    if (!el) return;
    if (!("IntersectionObserver" in window)) { fn(); return; }
    var io = new IntersectionObserver(function (es) {
      es.forEach(function (e) { if (e.isIntersecting) { io.disconnect(); fn(); } });
    }, { threshold: ratio || 0.4 });
    io.observe(el);
  }

  // ---- The harbour: servers berthed at their ports -------------------------

  var ships = [
    { port: 3000, project: "checkout", cmd: "next dev", via: "claude·checkout", ls: "claude", url: "http://localhost:3000", cpu: "1.2%", mem: "380MB", up: "2h" },
    { port: 5432, project: "checkout", cmd: "postgres", via: "docker", ls: "—", url: "postgres://localhost:5432", cpu: "—", mem: "—", up: "6d", container: "checkout-db-1" },
    { port: 8080, project: "api", cmd: "cargo run", via: "codex·api", ls: "codex", url: "http://localhost:8080", cpu: "0.4%", mem: "42MB", up: "40m", lan: true },
    { port: 5173, project: "web", cmd: "vite", via: "claude·web", ls: "claude", url: "http://localhost:5173", cpu: "0.8%", mem: "210MB", up: "5h" },
    { port: 4321, project: "docs", cmd: "astro", via: "claude·ended", ls: "claude·ended", url: "http://localhost:4321", cpu: "0.2%", mem: "120MB", up: "1d", orphan: true },
    { port: 6006, project: "search", cmd: "storybook", via: "codex·search", ls: "codex", url: "http://localhost:6006", cpu: "164.0%", mem: "2970MB", up: "3d", heavy: true },
  ];
  var berths = document.getElementById("berths");
  var out = document.getElementById("harbour-out");
  ships.forEach(function (s, i) {
    var flag = s.heavy ? '<span class="flag">164% cpu</span>'
      : s.orphan ? '<span class="flag">session ended</span>' : "";
    var b = document.createElement("div");
    b.className = "berth" + (s.orphan ? " adrift" : "");
    b.innerHTML =
      '<span class="num">:' + s.port + '</span><span class="rope" aria-hidden="true"></span>' +
      '<div class="boat' + (s.heavy ? " heavy" : "") + (s.lan ? " lan" : "") + '" style="--i:' + i + '">' +
      '<div class="proj">' + esc(s.project) + '</div><div class="cmdl">' + esc(s.cmd) + "</div>" +
      '<span class="via' + (s.via === "docker" ? " docker" : "") + '">' + esc(s.via) + "</span><br>" + flag + "</div>" +
      '<span class="empty">free</span>';
    s.el = b;
    berths.appendChild(b);
  });

  function pad(s, n) { s = String(s); return s.length >= n ? s + " " : s + " ".repeat(n - s.length); }
  function lpad(s, n) { s = String(s); return s.length >= n ? " " + s : " ".repeat(n - s.length) + s; }
  function here() { return ships.filter(function (s) { return !s.gone; }); }
  function line(cmd, lines) {
    var html = '<span class="p">$</span> <span class="o">' + esc(cmd) + "</span>\n" + lines.join("\n");
    out.innerHTML = (out.innerHTML ? out.innerHTML + "\n" : "") + html;
    out.scrollTop = out.scrollHeight;
  }
  function sink(s) { s.gone = true; s.el.classList.add("gone"); }
  var runs = {
    ls: function () {
      var rows = here();
      if (!rows.length) return line("marina ls", ["no dev servers running"]);
      // like the real table: sorted by port, CPU and MEM right-aligned
      rows = rows.slice().sort(function (a, b) { return a.port - b.port; });
      line("marina ls", ['<span class="o">' + pad("PROJECT", 9) + pad("COMMAND", 10) + pad("PORT", 7) + lpad("CPU", 7) + lpad("MEM", 8) + "  " + pad("VIA", 13) + "URL</span>"]
        .concat(rows.map(function (s) {
          var t = pad(s.project, 9) + pad(s.cmd, 10) + pad(":" + s.port + (s.lan ? "!" : ""), 7) + lpad(s.cpu, 7) + lpad(s.mem, 8) + "  " + pad(s.ls, 13) + (s.url || "");
          return esc(t);
        })));
    },
    ended: function () {
      var lost = here().filter(function (s) { return s.orphan; });
      if (!lost.length) return line("marina kill --ended", ["nothing to kill (no servers left running by ended agent sessions)"]);
      line("marina kill --ended", lost.map(function (s) { return "stopping " + s.project + " · " + s.cmd + " :" + s.port; })
        .concat(["stopped " + lost.length + " server" + (lost.length === 1 ? "" : "s")]));
      lost.forEach(sink);
    },
    free: function () {
      var s = ships[5];
      if (s.gone) return line("marina free 6006", [":6006  already free"]);
      line("marina free 6006", [":6006  freed — stopped search · storybook"]);
      sink(s);
    },
    reset: function () {
      ships.forEach(function (s) { s.gone = false; s.el.classList.remove("gone"); });
      out.innerHTML = "";
      runs.ls();
    },
  };
  document.querySelectorAll("[data-run]").forEach(function (b) {
    b.addEventListener("click", function () { runs[b.dataset.run](); });
  });
  onView(berths, function () { runs.ls(); }, 0.3);

})();
