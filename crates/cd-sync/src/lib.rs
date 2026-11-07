//! High-level sync protocols layered on cd-transport.

mod cancel;
mod clipboard;
mod error;
mod files;
mod pairing;

pub use cancel::CancelToken;
pub use clipboard::{receive_clipboard, send_clipboard};
pub use error::SyncError;
pub use files::{
    receive_skipped_from_manifest, receive_tree, receive_tree_from_manifest,
    receive_tree_from_manifest_with_side, receive_tree_from_manifest_with_side_and_cancel,
    receive_tree_with_cache, receive_tree_with_progress, send_file, send_file_with_side,
    send_file_with_side_and_cancel, send_tree, send_tree_with_config, send_tree_with_side,
    send_tree_with_side_and_cancel, ReceiveReport, SendReport, CHUNK_BYTES,
};
pub use pairing::{
    pair, pair_begin_initiator, pair_begin_responder, pair_finish_initiator, pair_finish_responder,
    sha256_bytes, sha256_hex, PairOutcome, PairPreview,
};
