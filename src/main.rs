//! marina — a developer-process cockpit.

mod cli;
mod config;
mod demo;
mod docker;
mod exec;
mod git;
mod hosts;
mod launcher;
mod logs;
mod model;
mod msg;
mod opencode;
mod ports;
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

use crate::model::TargetKey;
use crate::msg::{SamplerCtl, SamplerMsg, UiEvent};
use crate::ui::{Entry, Verb};

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Hidden debug commands, only in first position — anywhere else they're
    // unknown flags like any other. `--dump`: build a couple of snapshots and
    // print them, to check the data layer without a TTY.
    match args.first().map(String::as_str) {
        Some("--dump") => return dump(),
        Some("--logtest") => {
            return logtest(args[1..].iter().filter_map(|a| a.parse().ok()).collect());
        }
        _ => {}
    }

    // CLI subcommands (ls, kill, …, demo, version, help). Unknown args error
    // there rather than falling through; only a bare `marina` reaches the TUI.
    let demo = args.first().is_some_and(|a| a == "demo");
    if demo && args.len() > 1 {
        eprintln!("marina: demo takes no arguments");
        std::process::exit(2);
    }
    if !demo {
        if let Some(code) = cli::dispatch(&args) {
            std::process::exit(code);
        }
    }

    let (rx, ctl) = if demo {
        demo::spawn()
    } else {
        sampler::spawn()
    };
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
    app.demo = demo;
    let notify = config::load().notify;
    let desktop_notify = notify.desktop && !demo;
    let result = run(
        &mut terminal,
        &mut app,
        rx,
        ctl,
        ev_tx,
        ev_rx,
        desktop_notify,
    );
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
    desktop_notify: bool,
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
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    // Ctrl+C always quits, whatever mode we're in.
                    if ctrl && key.code == KeyCode::Char('c') {
                        return Ok(());
                    }
                    if app.help_open() {
                        // Any key dismisses the help overlay.
                        app.close_help();
                    } else if app.confirm.is_some() {
                        on_confirm_key(app, key.code, &ev_tx, &ctl);
                    } else if app.is_filtering() {
                        // Filter input mode captures keystrokes; the arrows
                        // move through what it leaves.
                        match key.code {
                            KeyCode::Esc => app.filter_cancel(),
                            KeyCode::Enter => app.filter_commit(),
                            KeyCode::Backspace => app.filter_backspace(),
                            KeyCode::Up => app.select_prev(),
                            KeyCode::Down => app.select_next(),
                            KeyCode::Char('u') if ctrl => app.filter_clear_input(),
                            KeyCode::Char(c) if !ctrl => app.filter_push(c),
                            _ => {}
                        }
                    } else if ctrl {
                        match key.code {
                            KeyCode::Char('d') => app.half_page(true),
                            KeyCode::Char('u') => app.half_page(false),
                            _ => {}
                        }
                    } else {
                        match key.code {
                            // a pane open: q closes it, like less or a pager
                            KeyCode::Char('q') if app.close_top() => {}
                            KeyCode::Char('q') => return Ok(()),
                            KeyCode::Char('j') | KeyCode::Down => app.select_next(),
                            KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
                            KeyCode::Char('g') | KeyCode::Home => app.jump_top(),
                            KeyCode::Char('G') | KeyCode::End => app.jump_bottom(),
                            KeyCode::PageDown if app.log_open() => app.log_page(true),
                            KeyCode::PageUp if app.log_open() => app.log_page(false),
                            KeyCode::PageDown => app.page(true),
                            KeyCode::PageUp => app.page(false),
                            KeyCode::Char('h') | KeyCode::Left => app.fold_or_parent(),
                            KeyCode::Char('l') | KeyCode::Right => app.set_fold(true),
                            KeyCode::Enter => on_enter(app, &ev_tx),
                            KeyCode::Char('d') => verb_diff(app),
                            KeyCode::Char('/') => app.start_filter(),
                            KeyCode::Char('s') => app.cycle_sort(),
                            KeyCode::Char('S') => app.reverse_sort(),
                            KeyCode::Char('i') => app.toggle_inspect(),
                            KeyCode::Char('v') => app.toggle_group_by(),
                            KeyCode::Char('?') => app.toggle_help(),
                            KeyCode::Char('X') => verb_kill(app, &ev_tx, &ctl, false),
                            KeyCode::Char('R') => verb_restart(app, &ev_tx, &ctl, false),
                            KeyCode::Char('y') => verb_copy(app),
                            KeyCode::Char('o') => verb_open(app),
                            KeyCode::Char('t') => verb_tail(app, &ev_tx),
                            KeyCode::Char('[') => app.log_scroll_up(5),
                            KeyCode::Char(']') => app.log_scroll_down(5),
                            KeyCode::Char('+') | KeyCode::Char('=') => app.log_grow(),
                            KeyCode::Char('-') => app.log_shrink(),
                            // Esc: close log pane → inspect → clear filter.
                            KeyCode::Esc => app.escape(),
                            // a habit from another tool shouldn't do nothing
                            // silently
                            KeyCode::Char(c @ ('K' | 'x')) => app.set_status(format!(
                                "{c} isn't a key here — X (Shift-x) kills · ? lists them"
                            )),
                            KeyCode::Char(c) if c.is_ascii_alphanumeric() => {
                                app.set_status(format!("{c} isn't a key here · ? lists them"))
                            }
                            _ => {}
                        }
                    }
                }
                // clicking the selected row again does what Enter does
                Event::Mouse(m) if app.on_mouse(m) => on_enter(app, &ev_tx),
                Event::Mouse(_) => {}
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

        // Orphaned servers; agent sessions finished or waiting on you.
        for msg in app.take_notifications() {
            if desktop_notify {
                verbs::notify("marina", &msg);
            }
        }

        // Results from background verb / log-discovery threads.
        while let Ok(ev) = ev_rx.try_recv() {
            match ev {
                UiEvent::Status(s) => app.set_status(s),
                UiEvent::LogReady { project, path } => on_log_ready(app, project, path),
            }
        }

        // New log lines; expire status / held rows / row flashes.
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
    println!("{} agent sessions", snap.agents.len());
    for a in &snap.agents {
        let kids: Vec<String> = a
            .children
            .iter()
            .map(|c| format!("{}{}", if c.shell { "$ " } else { "" }, c.label))
            .collect();
        println!(
            "  {:<9} pid={:<6} {:<18} {:>6.1}%  {:>6}MB  pids={} session={} [{}]",
            a.name,
            a.pid,
            a.project,
            a.cpu_pct,
            a.mem_bytes / (1024 * 1024),
            a.pids.len(),
            a.session.as_deref().unwrap_or("—"),
            kids.join(", "),
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

/// Verbs act on servers only. On an agent row or the agents section, say
/// why nothing happens instead of a bare "nothing to kill": marina never
/// stops an agent.
fn on_agent_row(app: &mut ui::App, verb: &str) -> bool {
    let stops = matches!(verb, "kill" | "restart");
    let msg = match (app.selected_agent(), &app.selected) {
        (Some(a), _) if stops => format!(
            "exit {} (pid {}) from its terminal — marina doesn't {verb} agent sessions; its servers are listed under it",
            a.name, a.pid
        ),
        (Some(a), _) => format!("select one of its servers — a {} session has no {verb}", a.name),
        (None, Some(Entry::Agents)) if stops => {
            format!("select a server — marina doesn't {verb} agent sessions")
        }
        (None, Some(Entry::Agents)) => format!("select a server — the agents section has no {verb}"),
        _ => return false,
    };
    app.set_status(msg);
    true
}

/// On the placeholder for a row marina just stopped or restarted, say what
/// became of it rather than act on whatever slid into its place.
fn on_held_row(app: &mut ui::App) -> bool {
    let Some(h) = app.held_placeholder() else {
        return false;
    };
    let msg = format!("{} is {}", h.label, h.state.trim_end_matches('…'));
    app.set_status(msg);
    true
}

/// The panes have squeezed the table out: don't act on a cursor nobody can
/// see.
fn table_hidden(app: &mut ui::App) -> bool {
    if app.page_rows > 0 {
        return false;
    }
    app.set_status("table hidden — Esc closes panes");
    true
}

/// Enter, or clicking the selected row: fold a header, jump to an agent
/// session, inspect a server.
fn on_enter(app: &mut ui::App, ev: &Sender<UiEvent>) {
    match app.selected {
        Some(Entry::Agent(_)) => verb_jump(app, ev),
        Some(Entry::Group(_) | Entry::Agents) => app.toggle_collapse(),
        Some(Entry::Member(_) | Entry::Target(_)) => app.toggle_inspect(),
        None => {}
    }
}

/// A key while `X`/`R` on a header waits for confirmation: its key again or
/// Enter goes ahead, anything else cancels.
fn on_confirm_key(
    app: &mut ui::App,
    code: KeyCode,
    ev: &Sender<UiEvent>,
    ctl: &Sender<SamplerCtl>,
) {
    let again = match app.confirm.as_ref().map(|c| c.verb) {
        Some(Verb::Kill) => code == KeyCode::Char('X'),
        Some(Verb::Restart) => code == KeyCode::Char('R'),
        None => false,
    };
    if !(again || code == KeyCode::Enter) {
        app.confirm = None;
        app.set_status("cancelled");
        return;
    }
    match app.take_confirm() {
        Some(Verb::Kill) => verb_kill(app, ev, ctl, true),
        Some(Verb::Restart) => verb_restart(app, ev, ctl, true),
        None => app.set_status("cancelled — the selection moved"),
    }
}

/// `2 servers in client-portal`, or the one server's name.
fn servers_label(app: &ui::App, n: usize) -> String {
    let label = app.selection_label();
    match app.selected {
        Some(Entry::Group(_)) => format!("{n} server{} in {label}", if n == 1 { "" } else { "s" }),
        _ => label,
    }
}

/// Enter on an agent row: focus its pane in the multiplexer that hosts it
/// (herdr, tmux). With no drivable host, copy a command that gets back into
/// the session instead.
fn verb_jump(app: &mut ui::App, ev: &Sender<UiEvent>) {
    let Some(a) = app.selected_agent() else {
        return;
    };
    if app.demo {
        let place = a
            .host
            .as_ref()
            .map(|h| format!("the {}", h.describe()))
            .unwrap_or_else(|| "its terminal".into());
        app.set_status(format!("demo: Enter would jump to {place}"));
        return;
    }
    match a.host.clone() {
        Some(host) => {
            let ev = ev.clone();
            let agent = a.name.clone();
            app.set_status(format!("jumping to the {}…", host.describe()));
            std::thread::spawn(move || {
                let msg = hosts::jump(&host, &agent).unwrap_or_else(|e| e);
                let _ = ev.send(UiEvent::Status(msg));
            });
        }
        None => {
            let resume = a
                .as_launcher()
                .resume_hint()
                .unwrap_or_else(|| a.name.clone());
            let cmd = format!(
                "cd {} && {resume}",
                shell_quote(&a.cwd.display().to_string())
            );
            // long enough to read and paste
            match verbs::copy_url(&cmd) {
                Ok(()) => app.set_notice(format!(
                    "paste it in a terminal — copied `{cmd}` (no terminal marina can drive)"
                )),
                Err(e) => app.set_notice(format!("run: {cmd} — no pane to jump to ({e})")),
            }
        }
    }
}

/// Single-quote for a POSIX shell.
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-~".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// `d`: the uncommitted diff of the selected agent's (or server's) work
/// tree, in the pane below; again closes it.
fn verb_diff(app: &mut ui::App) {
    if app.pane_kind() == Some(ui::PaneKind::Diff) {
        app.close_log();
        return;
    }
    let dir = match (app.selected_agent(), app.selected_target()) {
        (Some(a), _) => a.cwd.clone(),
        (None, Some(t)) if !t.cwd.as_os_str().is_empty() => t.cwd.clone(),
        _ => {
            app.set_status("select an agent session or a server to see its diff");
            return;
        }
    };
    let (tx, rx) = mpsc::channel();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let demo = app.demo;
    let title = ui::tildify(&dir.display().to_string());
    std::thread::spawn(move || {
        let lines = if demo {
            let project = dir.file_name().map(|n| n.to_string_lossy().into_owned());
            demo::diff_lines(&project.unwrap_or_default())
        } else {
            git::diff_lines(&dir)
        };
        for line in lines {
            if tx.send(line).is_err() {
                break; // pane closed
            }
        }
    });
    app.open_diff(title, rx, stop);
}

/// `X`: SIGTERM the selection; on a server that's still stopping, SIGKILL.
/// A header asks first when it would touch more than one server.
fn verb_kill(app: &mut ui::App, ev: &Sender<UiEvent>, ctl: &Sender<SamplerCtl>, confirmed: bool) {
    if on_agent_row(app, "kill") || on_held_row(app) || table_hidden(app) {
        return;
    }
    let group = matches!(app.selected, Some(Entry::Group(_)));
    // (key, fingerprints, container) of each server it touches
    let (plan, force) = {
        let ts = app.selected_targets();
        // all of them got SIGTERM already: this press escalates
        let force = !ts.is_empty() && ts.iter().all(|t| app.is_dying(&t.key));
        let plan: Vec<(TargetKey, Vec<verbs::PidStart>, Option<String>)> = ts
            .iter()
            .filter(|t| force || !app.is_dying(&t.key))
            .map(|t| (t.key.clone(), t.pid_starts.clone(), t.container.clone()))
            .collect();
        (plan, force)
    };
    let n = plan.len();
    if n == 0 {
        app.set_status("nothing to kill");
        return;
    }
    let what = servers_label(app, n);
    if group && n > 1 && !confirmed {
        let verb = if force { "force-kill" } else { "kill" };
        app.ask(
            Verb::Kill,
            format!("{verb} {what}? X again or Enter · any other key cancels"),
        );
        return;
    }
    let keys: Vec<TargetKey> = plan.iter().map(|p| p.0.clone()).collect();
    // the demo's servers have no processes to signal
    if app.demo {
        app.mark_marina_action(&keys);
        app.hold_selection(Verb::Kill);
        let _ = ctl.send(SamplerCtl::Demo(msg::DemoAct::Stop(keys)));
        app.set_status(format!("stopped {what} (demo: nothing was signalled)"));
        return;
    }
    if force {
        let pids: Vec<verbs::PidStart> = plan.iter().flat_map(|p| p.1.clone()).collect();
        if pids.is_empty() {
            app.set_status(format!("docker stop is still running for {what}"));
        } else {
            app.set_status(format!("force-killing {what}…"));
            verbs::force_kill(pids, what, ev.clone(), ctl.clone());
        }
        return;
    }
    let containers: Vec<String> = plan.iter().filter_map(|p| p.2.clone()).collect();
    let pids: Vec<verbs::PidStart> = plan
        .iter()
        .filter(|p| p.2.is_none())
        .flat_map(|p| p.1.clone())
        .collect();
    if !pids.is_empty() {
        if let Err(e) = verbs::kill(pids, what.clone(), ev.clone(), ctl.clone()) {
            app.set_status(format!("kill {what}: {e}"));
            return;
        }
    }
    if !containers.is_empty() {
        verbs::docker("stop", containers, ev.clone(), ctl.clone());
    }
    app.hold_selection(Verb::Kill);
    app.note_stopping(plan.into_iter().map(|(k, p, _)| (k, p)).collect());
    app.set_status(format!("stopping {what} — SIGTERM sent"));
    let _ = ctl.send(SamplerCtl::Refresh);
}

fn verb_restart(
    app: &mut ui::App,
    ev: &Sender<UiEvent>,
    ctl: &Sender<SamplerCtl>,
    confirmed: bool,
) {
    if on_agent_row(app, "restart") || on_held_row(app) || table_hidden(app) {
        return;
    }
    let group = matches!(app.selected, Some(Entry::Group(_)));
    let first = !app.restart_noted;
    let (mut plans, containers, keys) = {
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
                    note_env: false,
                }),
                None => {}
            }
        }
        (plans, cs, keys)
    };
    let n = keys.len();
    if n == 0 {
        app.set_status("nothing to restart");
        return;
    }
    let what = servers_label(app, n);
    if group && n > 1 && !confirmed {
        app.ask(
            Verb::Restart,
            format!("restart {what}? R again or Enter · any other key cancels"),
        );
        return;
    }
    if app.demo {
        app.mark_marina_action(&keys);
        let _ = ctl.send(SamplerCtl::Demo(msg::DemoAct::Restart(keys)));
        app.set_status(format!("restarted {what} (demo: nothing was run)"));
        return;
    }
    if plans.is_empty() && containers.is_empty() {
        app.set_status(format!("can't restart {what}: its command wasn't captured"));
        return;
    }
    app.mark_marina_action(&keys);
    app.hold_selection(Verb::Restart);
    if !containers.is_empty() {
        verbs::docker("restart", containers, ev.clone(), ctl.clone());
    }
    // marina can't capture a running process's environment: say so once
    if let Some(p) = plans.first_mut().filter(|_| first) {
        p.note_env = true;
        app.restart_noted = true;
    }
    for plan in plans {
        verbs::restart(plan, ev.clone(), ctl.clone());
    }
    app.set_status(format!("restarting {what}…"));
}

/// `o`/`y` on a session or a group header: the URL of one of its web
/// servers (the next on a repeat press), or why there's none.
fn row_url(app: &mut ui::App, verb: char) -> Option<Result<(String, String), String>> {
    let (row, label, urls) = match (app.selected_agent(), &app.selected) {
        (Some(a), _) => (
            format!("agent:{}", a.pid),
            app.agent_label(a),
            app.agent_urls(a),
        ),
        (None, Some(Entry::Group(g))) => (format!("group:{g}"), g.clone(), app.group_urls(g)),
        _ => return None,
    };
    if urls.is_empty() {
        return Some(Err(match app.selected_agent() {
            Some(_) => format!("{label} hasn't started a web server"),
            None => format!("{label} has no web server"),
        }));
    }
    let i = app.next_url(verb, &row, urls.len());
    let more = if urls.len() > 1 {
        format!(" ({} of {} — again for the next)", i + 1, urls.len())
    } else {
        String::new()
    };
    Some(Ok((urls[i].clone(), more)))
}

/// `o`/`y` on a server: its URL, or why there's none.
fn server_url(app: &mut ui::App, verb: char) -> Option<crate::model::Url> {
    if app.selected == Some(Entry::Agents) {
        app.set_status("the agents section has no URL — select a session or a server");
        return None;
    }
    if on_held_row(app) {
        return None;
    }
    let Some(t) = app.selected_target() else {
        let doing = if verb == 'y' { "copy" } else { "open" };
        app.set_status(format!("select a server to {doing} its URL"));
        return None;
    };
    let url = t.url.clone();
    if url.is_none() {
        let label = app.selection_label();
        app.set_status(format!("{label} has no URL"));
    }
    url
}

fn verb_copy(app: &mut ui::App) {
    let (url, more) = match row_url(app, 'y') {
        Some(Ok(found)) => found,
        Some(Err(why)) => return app.set_status(why),
        None => match server_url(app, 'y') {
            Some(u) => (u.value, String::new()),
            None => return,
        },
    };
    // the demo's URLs are made up — leave the real clipboard alone
    if app.demo {
        return app.set_status(format!("demo: y would copy {url}{more}"));
    }
    match verbs::copy_url(&url) {
        Ok(()) => app.set_status(format!("copied {url}{more}")),
        Err(e) => app.set_status(format!("copy failed: {e}")),
    }
}

fn verb_open(app: &mut ui::App) {
    match row_url(app, 'o') {
        Some(Ok((url, more))) => {
            if app.demo {
                app.set_status(format!("demo: o would open {url}{more}"));
                return;
            }
            let _ = verbs::open_url(&url);
            app.set_status(format!("opening {url}{more}"));
            return;
        }
        Some(Err(why)) => {
            app.set_status(why);
            return;
        }
        None => {}
    }
    match server_url(app, 'o') {
        Some(u) if u.scheme.is_web() && app.demo => {
            app.set_status(format!("demo: o would open {}", u.value));
        }
        Some(u) if u.scheme.is_web() => {
            let _ = verbs::open_url(&u.value);
            app.set_status(format!("opening {}", u.value));
        }
        Some(u) => app.set_status(format!("{} is not a web URL", u.value)),
        None => {}
    }
}

/// `t`: the selected server's log in the pane below; again closes it.
/// Docker targets tail `docker logs -f` directly; native targets get an
/// off-thread discovery (`lsof` can stall) whose result comes back as a
/// `UiEvent::LogReady`.
fn verb_tail(app: &mut ui::App, ev: &Sender<UiEvent>) {
    if app.pane_kind() == Some(ui::PaneKind::Log) {
        app.close_log();
        return;
    }
    if on_agent_row(app, "log") || on_held_row(app) {
        return;
    }
    let Some(t) = app.selected_target() else {
        app.set_status("select a server to see its log");
        return;
    };
    if app.demo {
        let lines = demo::log_lines(&t.project, &t.command_label, t.ports.first().copied());
        let title = format!("{} · {} (demo)", t.project, t.command_label);
        let (tx, rx) = mpsc::channel();
        for l in lines {
            let _ = tx.send(l);
        }
        app.open_log(title, rx, Arc::new(AtomicBool::new(false)));
        return;
    }
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
    if app.pane_kind() == Some(ui::PaneKind::Log) {
        return; // another log opened meanwhile (e.g. a docker tail)
    }
    match path {
        Some(p) => {
            let title = ui::tildify(&p.display().to_string());
            let stop = Arc::new(AtomicBool::new(false));
            let rx = logs::tail(p, Arc::clone(&stop));
            app.open_log(title, rx, stop);
        }
        None => app.set_status(format!(
            "R restarts {project} with its output captured — no log file found (it may log to stdout)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn shell_quote_leaves_plain_paths_and_quotes_the_rest() {
        assert_eq!(shell_quote("/Users/me/dev/app"), "/Users/me/dev/app");
        assert_eq!(shell_quote("/tmp/my app"), "'/tmp/my app'");
        assert_eq!(shell_quote("/tmp/it's"), "'/tmp/it'\\''s'");
    }
}
