//! port-manager — a developer-process cockpit (working name).
#![allow(dead_code, unused_imports)] // some contract types are defined ahead of being wired up

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

use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};

use crate::model::Target;
use crate::msg::SamplerMsg;

fn main() -> std::io::Result<()> {
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

    // CLI subcommands (ls/kill/restart/url/help); falls through to the TUI.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = cli::dispatch(&args) {
        std::process::exit(code);
    }

    let rx = sampler::spawn();
    let mut terminal = ratatui::init();
    let mut app = ui::App::new();
    let result = run(&mut terminal, &mut app, rx);
    ratatui::restore();
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut ui::App,
    rx: std::sync::mpsc::Receiver<SamplerMsg>,
) -> std::io::Result<()> {
    loop {
        terminal.draw(|frame| ui::render(frame, app))?;

        // ~100ms input poll keeps the UI responsive; the sampler feeds data on
        // its own cadence over the channel, so this loop never blocks on I/O.
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if app.is_filtering() {
                        // Filter input mode captures keystrokes.
                        match key.code {
                            KeyCode::Esc => app.filter_cancel(),
                            KeyCode::Enter => app.filter_commit(),
                            KeyCode::Backspace => app.filter_backspace(),
                            KeyCode::Char(c) => app.filter_push(c),
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
                            KeyCode::Char('K') => verb_kill(app),
                            KeyCode::Char('u') => app.undo_kill(),
                            KeyCode::Char('R') => verb_restart(app),
                            KeyCode::Char('Y') => verb_copy(app),
                            KeyCode::Char('O') => verb_open(app),
                            KeyCode::Char('T') => app.toggle_log(),
                            KeyCode::Esc => app.close_log(),
                            _ => {}
                        }
                    }
                }
            }
        }

        // Drain any snapshots the sampler produced since the last frame.
        while let Ok(msg) = rx.try_recv() {
            match msg {
                SamplerMsg::Snapshot(snap) => app.apply(snap),
                SamplerMsg::Error(e) => app.set_status(e),
            }
        }

        // Drain any new log lines; expire the kill-undo window + stale status.
        app.pump_log();
        app.expire_pending();
        app.expire_status();
    }
}

fn dump() -> std::io::Result<()> {
    let mut s = sampler::Sampler::new();
    let _ = s.build(); // prime CPU deltas
    std::thread::sleep(Duration::from_millis(600));
    let snap = s.build();
    println!("snapshot seq={} — {} targets", snap.seq, snap.targets.len());
    for t in &snap.targets {
        let port = t
            .ports
            .first()
            .map(|p| format!(":{p}"))
            .unwrap_or_else(|| "—".into());
        println!(
            "  {:<22} {:<18} {:<7} {:>6.1}%  {:>6}MB  pids={} anchor={} cwd={} {}",
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

fn logtest(pids: Vec<u32>) -> std::io::Result<()> {
    match logs::discover(&pids, std::path::Path::new("/"), "") {
        Some(path) => {
            println!("found log: {}", path.display());
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
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

fn verb_kill(app: &mut ui::App) {
    let pids: Vec<u32> = {
        let ts = app.selected_targets();
        ts.iter().flat_map(|t| t.pids.iter().copied()).collect()
    };
    if pids.is_empty() {
        app.set_status("nothing to kill");
        return;
    }
    let label = app.selection_label();
    let n = pids.len();
    match verbs::kill(pids) {
        Ok(cancel) => {
            app.note_pending_kill(cancel, &label);
            app.set_status(format!(
                "killing {label} ({n} processes) — SIGTERM now, SIGKILL in 4s · u to undo"
            ));
        }
        Err(e) => app.set_status(format!("kill failed: {e}")),
    }
}

fn verb_restart(app: &mut ui::App) {
    let label = app.selection_label();
    let plans: Vec<(Vec<String>, std::path::PathBuf, Vec<u32>)> = app
        .selected_targets()
        .iter()
        .filter(|t| !t.anchor_argv.is_empty())
        .map(|t| (t.anchor_argv.clone(), t.cwd.clone(), t.pids.clone()))
        .collect();
    if plans.is_empty() {
        app.set_status(format!("can't restart {label}: command not captured"));
        return;
    }
    for (argv, cwd, pids) in plans {
        verbs::restart(argv, cwd, pids);
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
