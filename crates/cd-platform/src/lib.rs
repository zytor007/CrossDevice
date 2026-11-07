//! Platform shims. Pure parsing lives here so it can be tested without a live
//! X server; the command wrappers are thin.

pub mod clipboard;
pub mod clipboard_files;
mod detect;
pub mod xrandr;

pub use clipboard::{ClipboardError, ClipboardImage, SystemClipboard};
pub use clipboard_files::{encode_uri_list, parse_uri_list, FileClipboard};
pub use detect::{detect, Detection, PlatformError};
