use std::collections::BTreeMap;

use cd_geometry::{Layout, LayoutIssue};
use cd_proto::{Capabilities, Modifiers};

use crate::layout_state::LayoutState;
use crate::ownership::{OwnershipAction, OwnershipEvent, OwnershipMachine};

#[derive(Debug, Clone)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub capabilities: Capabilities,
    pub online: bool,
    pub last_seen_ms: Option<u64>,
}

impl Peer {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            capabilities: Capabilities::empty(),
            online: false,
            last_seen_ms: None,
        }
    }

    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn with_online(mut self, online: bool) -> Self {
        self.online = online;
        self
    }
}

/// Minimal session orchestration for the headless daemon. Transport, clipboard
/// and file pipelines plug in here; this layer only owns peers, layout and input
/// ownership.
#[derive(Debug, Clone)]
pub struct Session {
    local_id: String,
    layout: LayoutState,
    ownership: OwnershipMachine,
    peers: BTreeMap<String, Peer>,
}

impl Session {
    pub fn new(local_id: impl Into<String>, layout: Layout) -> Self {
        Self {
            local_id: local_id.into(),
            layout: LayoutState::new(layout),
            ownership: OwnershipMachine::new(),
            peers: BTreeMap::new(),
        }
    }

    pub fn local_id(&self) -> &str {
        &self.local_id
    }

    pub fn layout(&self) -> &LayoutState {
        &self.layout
    }

    pub fn ownership(&self) -> &OwnershipMachine {
        &self.ownership
    }

    pub fn ownership_mut(&mut self) -> &mut OwnershipMachine {
        &mut self.ownership
    }

    pub fn add_peer(&mut self, peer: Peer) {
        self.peers.insert(peer.id.clone(), peer);
    }

    pub fn peer(&self, id: &str) -> Option<&Peer> {
        self.peers.get(id)
    }

    pub fn peers(&self) -> impl Iterator<Item = &Peer> {
        self.peers.values()
    }

    pub fn apply_layout(&mut self, layout: Layout) -> Result<u64, Vec<LayoutIssue>> {
        self.layout.try_apply(layout)
    }

    pub fn handle(&mut self, event: OwnershipEvent) -> Vec<OwnershipAction> {
        self.ownership.handle(event)
    }

    /// Edge crossing only hands off to an online, known peer.
    pub fn handle_edge_crossing(
        &mut self,
        target: &str,
        modifiers: Modifiers,
        at_ms: u64,
    ) -> Vec<OwnershipAction> {
        let online = self.peers.get(target).is_some_and(|peer| peer.online);
        if !online {
            return Vec::new();
        }
        self.ownership.handle(OwnershipEvent::EdgeCrossed {
            target: target.to_owned(),
            modifiers,
            at_ms,
        })
    }

    /// Mark a peer offline/online. If it owned input, control returns locally.
    pub fn set_peer_online(&mut self, id: &str, online: bool, now_ms: u64) -> Vec<OwnershipAction> {
        let Some(peer) = self.peers.get_mut(id) else {
            return Vec::new();
        };
        peer.online = online;
        if online {
            peer.last_seen_ms = Some(now_ms);
        }

        if !online && self.ownership.current_peer() == Some(id) {
            self.ownership
                .handle(OwnershipEvent::PeerLost { at_ms: now_ms })
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ownership::{OwnershipAction, OwnershipState};
    use cd_geometry::presets;

    fn session() -> Session {
        let mut session = Session::new(
            "linux-4k",
            presets::compose_air(presets::macbook_air_13(), 0.0),
        );
        session.add_peer(Peer::new("macbook-air-13", "MacBook Air").with_online(true));
        session
    }

    #[test]
    fn crossing_requires_an_online_peer() {
        let mut session = Session::new(
            "linux-4k",
            presets::compose_air(presets::macbook_air_13(), 0.0),
        );
        session.add_peer(Peer::new("macbook-air-13", "MacBook Air"));
        assert!(session
            .handle_edge_crossing("macbook-air-13", Modifiers::empty(), 10)
            .is_empty());
        assert!(session.ownership().is_local());
    }

    #[test]
    fn crossing_to_online_peer_starts_handoff() {
        let mut session = session();
        let actions = session.handle_edge_crossing("macbook-air-13", Modifiers::CTRL, 10);
        assert!(actions.contains(&OwnershipAction::HideLocalCursor));
        assert!(matches!(
            session.ownership().state(),
            OwnershipState::HandoffOut { .. }
        ));
    }

    #[test]
    fn peer_going_offline_restores_local_control() {
        let mut session = session();
        session.handle_edge_crossing("macbook-air-13", Modifiers::empty(), 10);
        session
            .ownership_mut()
            .handle(OwnershipEvent::HandoffAccepted { at_ms: 20 });
        let actions = session.set_peer_online("macbook-air-13", false, 900);
        assert!(actions.contains(&OwnershipAction::ShowLocalCursor));
        assert!(session.ownership().is_local());
        assert!(!session.peer("macbook-air-13").unwrap().online);
    }
}
