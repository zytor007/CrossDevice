//! Wire protocol for the cross-device suite.
//!
//! - C0 control: JSON messages over a reliable QUIC stream.
//! - C1 input: fixed 20-byte little-endian events over QUIC DATAGRAM.
//! - C2 data: clipboard and file payloads over QUIC streams (framing lives in
//!   `cd-transport`).

pub mod c1;
pub mod control;
pub mod data;
pub mod error;
pub mod sas;
pub mod version;

pub use c1::{InputEvent, InputKind, C1_EVENT_LEN, FLAG_ABSOLUTE, FLAG_DOWN, FLAG_MICROMETER};
pub use control::{
    Capabilities, ClipboardAnnounce, ClipboardKind, ControlMessage, EdgeSide, FeatureSet,
    FileSendGrant, FileSendRelease, FileSendRequest, Heartbeat, Hello, InputOwner, LayoutSet,
    Modifiers, OwnerPhase, PairBegin, PairConfirm, PairSettings, ProtocolError, TransferCancel,
    TransferOffer, TransferPause, MAX_EDGE_RESISTANCE, MAX_POINTER_SPEED, MIN_POINTER_SPEED,
};
pub use data::{
    DataHeader, FileChunkHeader, FileChunkMapEntry, FileChunkMapHeader, FileChunkMeta,
    FileManifestEntry, FileManifestHeader, ResumeFile, ResumePlanBody, ResumePlanHeader,
};
pub use error::ErrorCode;
pub use version::{ProtocolVersion, VersionError};

/// Any C1 datagram must fit this budget on a LAN path.
pub const MAX_DATAGRAM_BYTES: usize = 1200;
