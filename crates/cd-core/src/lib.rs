//! Session orchestration: layout lifecycle and input ownership.

mod layout_state;
mod ownership;
mod pairing;
mod session;

pub use layout_state::LayoutState;
pub use ownership::{
    OwnershipAction, OwnershipEvent, OwnershipMachine, OwnershipState, SuspendReason,
    HANDOFF_BACK_TIMEOUT_MS, HANDOFF_SLOW_MS, HANDOFF_TIMEOUT_MS,
};
pub use pairing::{PairingError, PairingSession, PairingState, PAIRING_TTL_MS};
pub use session::{Peer, Session};
