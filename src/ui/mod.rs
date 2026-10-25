//! Ratatui view, split by responsibility:
//! - `state`  — view state that survives snapshot swaps (selection, sort,
//!   filter, log pane, pending kill, flash/dying animation), plus all input
//!   handling logic (keys arrive via `main`, mouse via `App::on_mouse`).
//! - `render` — drawing from `(snapshot + view state)`; no state decisions
//!   beyond recording layout rects for mouse hit-testing.
//! - `format` — small display helpers.

mod agent_view;
mod control;
mod format;
mod render;
mod state;

pub use control::{Action, LandAction};

/// Load which agent replies have been read (`<state dir>/seen.json`).
pub fn load_seen(app: &mut App) {
    if let Some(dir) = crate::logs::state_dir() {
        app.ctl.seen = control::Seen::load(dir.join("seen.json"));
        app.ctl.aside = control::Seen::load_plain(dir.join("aside.json"));
        // once: with no agent hooked, point at what's left to set up
        let marker = dir.join("setup-tip-shown");
        let hooked = crate::hooks::installed()
            .iter()
            .any(|(_, h, _)| *h == crate::hooks::Hooked::Yes);
        if !hooked && !marker.exists() && std::fs::create_dir_all(&dir).is_ok() {
            let _ = std::fs::write(&marker, "");
            app.ctl.setup_tip = Some("new here? marina doctor shows what to set up".into());
        }
    }
}

/// Screen rows the preview reads.
pub fn control_rows() -> usize {
    control::SCREEN_ROWS
}

pub use render::render;
pub use state::{coalesce, App};

pub(crate) use agent_view::first_sentence;
pub(crate) use format::{fmt_age, fmt_uptime, tildify, unix_now};
