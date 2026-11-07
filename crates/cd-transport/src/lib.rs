//! QUIC transport: self-signed identity, C0 control stream and C1 datagrams.

mod error;
pub mod identity;
pub mod link;

pub use error::TransportError;
pub use identity::Identity;
pub use link::{DataReceiver, DataSender, DataTransfer, Link, QuicTransport};
