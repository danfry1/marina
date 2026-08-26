//! Ratatui view, split by responsibility:
//! - `state`  — view state that survives snapshot swaps (selection, sort,
//!   filter, log pane, pending kill, flash/dying animation), plus all input
//!   handling logic (keys arrive via `main`, mouse via `App::on_mouse`).
//! - `render` — drawing from `(snapshot + view state)`; no state decisions
//!   beyond recording layout rects for mouse hit-testing.
//! - `format` — small display helpers.

mod format;
mod render;
mod state;

pub use render::render;
pub use state::App;

pub(crate) use format::tildify;
