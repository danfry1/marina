//! The sampler<->UI message contract (the seam). See DESIGN.md.
//! Stubs for now — the sampler thread and action executor aren't wired yet.

use crate::model::{Snapshot, TargetKey};
use std::sync::Arc;

/// sampler -> UI
pub enum SamplerMsg {
    Snapshot(Arc<Snapshot>),
    /// Surfaced in a status line; never panics the UI.
    Error(String),
}

/// UI -> sampler / action thread
pub enum UiMsg {
    /// Drives adaptive idle cadence.
    SetFocused(bool),
    /// e.g. right after a verb, to reflect it quickly.
    RequestRefresh,
    Verb { target: TargetKey, verb: Verb },
    Shutdown,
}

/// One-keystroke action on the selected target.
/// Kill/Restart run off the UI thread; CopyUrl/Open are cheap and run inline.
#[derive(Clone, Copy)]
pub enum Verb {
    Kill,
    Restart,
    CopyUrl,
    Open,
    ToggleTail,
}
