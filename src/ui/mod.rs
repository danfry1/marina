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

pub use control::Action;

/// Load which agent replies have been read (`<state dir>/seen.json`).
pub fn load_seen(app: &mut App) {
    if let Some(dir) = crate::logs::state_dir() {
        app.ctl.seen = control::Seen::load(dir.join("seen.json"));
    }
}

/// Screen rows the preview reads.
pub fn control_rows() -> usize {
    control::SCREEN_ROWS
}

pub use render::render;
pub use state::App;

pub(crate) use agent_view::first_sentence;
pub(crate) use format::{fmt_age, fmt_uptime, tildify};
