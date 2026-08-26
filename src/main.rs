//! marina — a developer-process cockpit.

mod cli;
mod config;
mod docker;
mod logs;
mod model;
mod msg;
mod resolve;
mod sampler;
mod sources;
mod ui;
mod verbs;

use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::Duration;

use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;

use crate::msg::{SamplerCtl, SamplerMsg, UiEvent};

fn main() -> io::Result<()> {
    // Headless smoke test: build a couple of snapshots and print them. Useful
    // for verifying the data layer without a TTY.
    if std::env::args().any(|a| a == "--dump") {
        return dump();
    }
    if let Some(pos) = std::env::args().position(|a| a == "--logtest") {
        let pids: Vec<u32> = std::env::args()
            .skip(pos + 1)
            .filter_map(|a| a.parse().ok())
            .collect();
        return logtest(pids);
    }

    // CLI subcommands (ls/kill/restart/url/version/help). Unknown args error
    // there rather than falling through; only a bare `marina` reaches the TUI.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = cli::dispatch(&args) {
        std::process::exit(code);
    }

    let (rx, ctl) = sampler::spawn();
    let (ev_tx, ev_rx) = mpsc::channel::<UiEvent>();
    let mut terminal = ratatui::init();
    let _ = execute!(io::stdout(), EnableMouseCapture);
    // ratatui::init installed a panic hook that restores the terminal; chain
    // ours in front so mouse capture is released first.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        prev_hook(info);
    }));
    let mut app = ui::App::new();
    let result = run(&mut terminal, &mut app, rx, ctl, ev_tx, ev_rx);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut ui::App,
    rx: Receiver<SamplerMsg>,
    ctl: Sender<SamplerCtl>,
    ev_tx: Sender<UiEvent>,
    ev_rx: Receiver<UiEvent>,
) -> io::Result<()> {
    loop {
        // Draw only when something changed (or an animation is running) — an
        // idle cockpit does no per-frame work, which is the whole point.
        if app.dirty || app.has_transient() {
            terminal.draw(|frame| ui::render(frame, app))?;
            app.dirty = false;
        }

        // ~100ms input poll keeps the UI responsive; the sampler feeds data on
        // its own cadence over the channel, so this loop never blocks on I/O.
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // Ctrl+C always quits, whatever mode we're in.
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        return Ok(());
                    }
                    if app.help_open() {
                        // Any key dismisses the help overlay.
                        app.close_help();
                    } else if app.is_filtering() {
                        // Filter input mode captures keystrokes.
                        match key.code {
                            KeyCode::Esc => app.filter_cancel(),
                            KeyCode::Enter => app.filter_commit(),
                            KeyCode::Backspace => app.filter_backspace(),
                            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                                app.filter_push(c)
                            }
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('q') => return Ok(()),
                            KeyCode::Char('j') | KeyCode::Down => app.select_next(),
                            KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
                            KeyCode::Char('g') => app.jump_top(),
                            KeyCode::Char('G') => app.jump_bottom(),
                            KeyCode::Enter => app.toggle_collapse(),
                            KeyCode::Char('/') => app.start_filter(),
                            KeyCode::Char('s') => app.cycle_sort(),
                            KeyCode::Char('i') => app.toggle_inspect(),
                            KeyCode::Char('?') => app.toggle_help(),
                            KeyCode::Char('K') => verb_kill(app, &ev_tx, &ctl),
                            KeyCode::Char('u') => app.undo_kill(),
                            KeyCode::Char('R') => verb_restart(app, &ev_tx, &ctl),
                            KeyCode::Char('Y') => verb_copy(app),
                            KeyCode::Char('O') => verb_open(app),
                            KeyCode::Char('T') => verb_tail(app, &ev_tx),
                            KeyCode::Char('[') => app.log_scroll_up(5),
                            KeyCode::Char(']') => app.log_scroll_down(5),
                            KeyCode::Char('+') | KeyCode::Char('=') => app.log_grow(),
                            KeyCode::Char('-') => app.log_shrink(),
                            // Esc: close log pane → inspect → clear filter.
                            KeyCode::Esc => app.escape(),
                            _ => {}
                        }
                    }
                }
                Event::Mouse(m) => app.on_mouse(m),
                Event::Resize(_, _) => app.dirty = true,
                _ => {}
            }
        }

        // Drain snapshots; a disconnect means the sampler thread died — the
        // display would silently freeze otherwise, so say so.
        loop {
            match rx.try_recv() {
                Ok(SamplerMsg::Snapshot(snap)) => app.apply(snap),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    app.sampler_died();
                    break;
                }
            }
        }

        // Results from background verb / log-discovery threads.
        while let Ok(ev) = ev_rx.try_recv() {
            match ev {
                UiEvent::Status(s) => app.set_status(s),
                UiEvent::LogReady { project, path } => on_log_ready(app, project, path),
            }
        }

        // New log lines; expire status / kill-undo window / row flashes.
        app.pump_log();
        app.tick();
    }
}

fn dump() -> io::Result<()> {
    let mut s = sampler::Sampler::new();
    let _ = s.build(); // prime CPU deltas
    std::thread::sleep(Duration::from_millis(600));
    let snap = s.build();
    println!("snapshot seq={} — {} targets", snap.seq, snap.targets.len());
    if let Some(e) = &snap.error {
        println!("  ⚠ {e}");
    }
    for t in &snap.targets {
        let port = match t.ports.first() {
            Some(p) if t.exposed => format!(":{p}!"),
            Some(p) => format!(":{p}"),
            None => "—".into(),
        };
        println!(
            "  {:<22} {:<18} {:<8} {:>6.1}%  {:>6}MB  pids={} anchor={} cwd={} {}",
            t.project,
            t.command_label,
            port,
            t.cpu_pct,
            t.mem_bytes / (1024 * 1024),
            t.pids.len(),
            t.anchor.pid,
            t.cwd.display(),
            t.url.as_ref().map(|u| u.value.as_str()).unwrap_or(""),
        );
    }
    Ok(())
}

fn logtest(pids: Vec<u32>) -> io::Result<()> {
    match logs::discover(&pids, std::path::Path::new("/"), "") {
        Some(path) => {
            println!("found log: {}", path.display());
            let stop = Arc::new(AtomicBool::new(false));
            let rx = logs::tail(path, stop);
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_secs(2) {
                while let Ok(line) = rx.try_recv() {
                    println!("| {line}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        None => println!("no log file found for pids {pids:?}"),
    }
    Ok(())
}

// --- verbs (orchestrated here; the work runs off the UI thread) --------------

fn verb_kill(app: &mut ui::App, ev: &Sender<UiEvent>, ctl: &Sender<SamplerCtl>) {
    let (pid_starts, containers, keys) = {
        let ts = app.selected_targets();
        let mut ps: Vec<verbs::PidStart> = Vec::new();
        let mut cs: Vec<String> = Vec::new();
        let mut keys = Vec::new();
        for t in &ts {
            keys.push(t.key.clone());
            match &t.container {
                Some(c) => cs.push(c.clone()),
                None => ps.extend(&t.pid_starts),
            }
        }
        (ps, cs, keys)
    };
    if pid_starts.is_empty() && containers.is_empty() {
        app.set_status("nothing to kill");
        return;
    }
    let label = app.selection_label();
    app.mark_marina_action(&keys);
    if !containers.is_empty() {
        let n = containers.len();
        verbs::docker("stop", containers, ev.clone(), ctl.clone());
        app.set_status(format!(
            "stopping {n} container{}…",
            if n == 1 { "" } else { "s" }
        ));
    }
    if !pid_starts.is_empty() {
        let n = pid_starts.len();
        match verbs::kill(pid_starts, ev.clone(), ctl.clone()) {
            Ok(cancel) => {
                app.note_pending_kill(cancel, &label, keys);
                app.set_status(format!(
                    "killing {label} ({n} processes) — SIGTERM sent, SIGKILL in 4s · u cancels the SIGKILL"
                ));
            }
            Err(e) => app.set_status(format!("kill {label}: {e}")),
        }
    }
    let _ = ctl.send(SamplerCtl::Refresh);
}

fn verb_restart(app: &mut ui::App, ev: &Sender<UiEvent>, ctl: &Sender<SamplerCtl>) {
    let label = app.selection_label();
    let (plans, containers, keys) = {
        let ts = app.selected_targets();
        let mut plans = Vec::new();
        let mut cs = Vec::new();
        let mut keys = Vec::new();
        for t in &ts {
            keys.push(t.key.clone());
            match &t.container {
                Some(c) => cs.push(c.clone()),
                None if !t.anchor_argv.is_empty() => plans.push(verbs::RestartPlan {
                    project: t.project.clone(),
                    argv: t.anchor_argv.clone(),
                    cwd: t.cwd.clone(),
                    ports: t.ports.clone(),
                    pid_starts: t.pid_starts.clone(),
                }),
                None => {}
            }
        }
        (plans, cs, keys)
    };
    if plans.is_empty() && containers.is_empty() {
        app.set_status(format!("can't restart {label}: command not captured"));
        return;
    }
    app.mark_marina_action(&keys);
    if !containers.is_empty() {
        verbs::docker("restart", containers, ev.clone(), ctl.clone());
    }
    for plan in plans {
        verbs::restart(plan, ev.clone(), ctl.clone());
    }
    app.set_status(format!("restarting {label}…"));
}

fn verb_copy(app: &mut ui::App) {
    if app.selected_target().is_none() {
        app.set_status("select a service (not a group) to copy a URL");
        return;
    }
    let url = app.selected_target().and_then(|t| t.url.clone());
    match url {
        Some(u) => match verbs::copy_url(&u.value) {
            Ok(()) => app.set_status(format!("copied {}", u.value)),
            Err(e) => app.set_status(format!("copy failed: {e}")),
        },
        None => app.set_status("no URL for this target"),
    }
}

fn verb_open(app: &mut ui::App) {
    if app.selected_target().is_none() {
        app.set_status("select a service (not a group) to open");
        return;
    }
    let url = app.selected_target().and_then(|t| t.url.clone());
    match url {
        Some(u) if u.scheme.is_web() => {
            let _ = verbs::open_url(&u.value);
            app.set_status(format!("opening {}", u.value));
        }
        Some(u) => app.set_status(format!("{} is not a web URL", u.value)),
        None => app.set_status("no URL for this target"),
    }
}

/// `T`: docker targets tail `docker logs -f` directly; native targets get an
/// off-thread discovery (`lsof` can stall) whose result comes back as a
/// `UiEvent::LogReady`.
fn verb_tail(app: &mut ui::App, ev: &Sender<UiEvent>) {
    if app.log_open() {
        app.close_log();
        return;
    }
    let Some(t) = app.selected_target() else {
        app.set_status("select a service (not a group) to tail");
        return;
    };
    if let Some(c) = &t.container {
        let mut cmd = std::process::Command::new("docker");
        cmd.args(["logs", "-f", "--tail", "200", c]);
        let stop = Arc::new(AtomicBool::new(false));
        let rx = logs::tail_cmd(cmd, Arc::clone(&stop));
        app.open_log(format!("docker logs {c}"), rx, stop);
        return;
    }
    if app.log_pending {
        return; // a discovery is already running
    }
    let (pids, cwd, project) = (t.pids.clone(), t.cwd.clone(), t.project.clone());
    app.log_pending = true;
    app.set_status(format!("looking for {project} logs…"));
    let ev = ev.clone();
    std::thread::spawn(move || {
        let path = logs::discover(&pids, &cwd, &project);
        let _ = ev.send(UiEvent::LogReady { project, path });
    });
}

fn on_log_ready(app: &mut ui::App, project: String, path: Option<std::path::PathBuf>) {
    app.log_pending = false;
    if app.log_open() {
        return; // something else opened meanwhile (e.g. a docker tail)
    }
    match path {
        Some(p) => {
            let title = ui::tildify(&p.display().to_string());
            let stop = Arc::new(AtomicBool::new(false));
            let rx = logs::tail(p, Arc::clone(&stop));
            app.open_log(title, rx, stop);
        }
        None => app.set_status(format!(
            "no log file found for {project} — logs may go to stdout (R restarts with capture)"
        )),
    }
}
