//! Clipboard pipeline primitives. OS clipboard access plugs in at the edges;
//! every rule here is pure and testable.

mod content;
mod error;
mod guard;
mod history;
mod policy;

pub use content::{Content, FileRef, Snapshot};
pub use error::ClipboardError;
pub use guard::{LoopGuard, RemoteDecision};
pub use history::{History, HistoryItem, PushOutcome};
pub use policy::{
    ClipboardPolicy, DEFAULT_AUTO_FETCH_BYTES, MAX_AUTO_FETCH_BYTES, MIN_AUTO_FETCH_BYTES,
};
