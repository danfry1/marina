//! Cross-thread contracts (the seams). Three channels exist at runtime:
//!
//!   sampler ──SamplerMsg──► UI      snapshots (with any data-source error inside)
//!   UI      ──SamplerCtl──► sampler refresh-now / shutdown (by dropping the sender)
//!   verbs/logs ──UiEvent──► UI      results from background verb + discovery threads

use crate::model::Snapshot;
use std::path::PathBuf;
use std::sync::Arc;

/// sampler -> UI. Data-source errors ride inside the Snapshot (`snapshot.error`)
/// so they display persistently, not as a transient status.
pub enum SamplerMsg {
    Snapshot(Arc<Snapshot>),
}

/// UI -> sampler. Dropping the sender shuts the sampler thread down.
pub enum SamplerCtl {
    /// Rebuild a snapshot now (right after a verb) instead of waiting out the
    /// adaptive-cadence sleep — a killed row disappears immediately.
    Refresh,
}

/// Background verb / log-discovery threads -> UI. Drained every loop tick.
pub enum UiEvent {
    /// A line for the status bar (verb outcomes, permission errors, …).
    Status(String),
    /// Result of an off-thread log discovery for `project` (`T` verb) —
    /// `lsof` can stall, so it never runs on the UI thread.
    LogReady {
        project: String,
        path: Option<PathBuf>,
    },
}
