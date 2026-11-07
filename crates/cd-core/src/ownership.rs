use cd_proto::Modifiers;

/// 250 ms without a handoff acknowledgement is "slow"; 750 ms is a failure and
/// input must return to the local machine.
pub const HANDOFF_SLOW_MS: u64 = 250;
pub const HANDOFF_TIMEOUT_MS: u64 = 750;
pub const HANDOFF_BACK_TIMEOUT_MS: u64 = 250;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuspendReason {
    LockScreen,
    Sleep,
    PermissionLost,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipState {
    LocalOwned,
    HandoffOut {
        target: String,
        since_ms: u64,
    },
    RemoteOwned {
        owner: String,
        since_ms: u64,
    },
    HandoffBack {
        from: String,
        since_ms: u64,
    },
    Suspended {
        reason: SuspendReason,
        since_ms: u64,
    },
}

impl OwnershipState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::LocalOwned => "LOCAL_OWNED",
            Self::HandoffOut { .. } => "HANDOFF_OUT",
            Self::RemoteOwned { .. } => "REMOTE_OWNED",
            Self::HandoffBack { .. } => "HANDOFF_BACK",
            Self::Suspended { .. } => "SUSPENDED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipEvent {
    EdgeCrossed {
        target: String,
        modifiers: Modifiers,
        at_ms: u64,
    },
    /// Peer accepted the handoff, or finished releasing input.
    HandoffAccepted {
        at_ms: u64,
    },
    HandoffFailed {
        at_ms: u64,
    },
    ReturnHome {
        at_ms: u64,
    },
    /// This machine saw physical input while a peer owned the pointer.
    LocalInputWhileRemote {
        at_ms: u64,
    },
    PeerLost {
        at_ms: u64,
    },
    Suspend {
        reason: SuspendReason,
        at_ms: u64,
    },
    Resume {
        at_ms: u64,
    },
    /// Watchdog tick; the machine itself has no timer ownership requirement.
    Tick {
        at_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipAction {
    HideLocalCursor,
    ShowLocalCursor,
    InformPeerHandoff {
        target: String,
        modifiers: Modifiers,
    },
    InformPeerRelease {
        target: String,
    },
    NotifyHandoffSlow {
        target: String,
        elapsed_ms: u64,
    },
    NotifyHandoffTimeout {
        target: String,
        elapsed_ms: u64,
    },
    NotifyPeerLost {
        target: String,
    },
    NotifySuspended {
        reason: SuspendReason,
    },
    NotifyResumed,
}

/// Source-side ownership state machine. Every transition is pure; the caller
/// executes the returned actions against OS/UI/transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnershipMachine {
    state: OwnershipState,
    slow_notified: bool,
}

impl Default for OwnershipMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl OwnershipMachine {
    pub fn new() -> Self {
        Self {
            state: OwnershipState::LocalOwned,
            slow_notified: false,
        }
    }

    pub fn state(&self) -> &OwnershipState {
        &self.state
    }

    pub fn is_local(&self) -> bool {
        matches!(self.state, OwnershipState::LocalOwned)
    }

    /// Device currently receiving injected input, if any.
    pub fn current_peer(&self) -> Option<&str> {
        match &self.state {
            OwnershipState::HandoffOut { target, .. } => Some(target),
            OwnershipState::RemoteOwned { owner, .. } => Some(owner),
            OwnershipState::HandoffBack { from, .. } => Some(from),
            OwnershipState::LocalOwned | OwnershipState::Suspended { .. } => None,
        }
    }

    pub fn handle(&mut self, event: OwnershipEvent) -> Vec<OwnershipAction> {
        match event {
            OwnershipEvent::EdgeCrossed {
                target,
                modifiers,
                at_ms,
            } => {
                if !matches!(self.state, OwnershipState::LocalOwned) {
                    return Vec::new();
                }
                self.slow_notified = false;
                self.state = OwnershipState::HandoffOut {
                    target: target.clone(),
                    since_ms: at_ms,
                };
                vec![
                    OwnershipAction::HideLocalCursor,
                    OwnershipAction::InformPeerHandoff { target, modifiers },
                ]
            }

            OwnershipEvent::HandoffAccepted { at_ms } => match &self.state {
                OwnershipState::HandoffOut { target, .. } => {
                    self.state = OwnershipState::RemoteOwned {
                        owner: target.clone(),
                        since_ms: at_ms,
                    };
                    Vec::new()
                }
                OwnershipState::HandoffBack { .. } => {
                    self.state = OwnershipState::LocalOwned;
                    Vec::new()
                }
                _ => Vec::new(),
            },

            OwnershipEvent::HandoffFailed { .. } => match &self.state {
                OwnershipState::HandoffOut { target, .. } => {
                    let target = target.clone();
                    self.state = OwnershipState::LocalOwned;
                    vec![
                        OwnershipAction::ShowLocalCursor,
                        OwnershipAction::InformPeerRelease { target },
                    ]
                }
                _ => Vec::new(),
            },

            OwnershipEvent::ReturnHome { at_ms }
            | OwnershipEvent::LocalInputWhileRemote { at_ms } => match &self.state {
                OwnershipState::HandoffOut { target, .. }
                | OwnershipState::RemoteOwned { owner: target, .. } => {
                    let from = target.clone();
                    self.state = OwnershipState::HandoffBack {
                        from: from.clone(),
                        since_ms: at_ms,
                    };
                    vec![
                        OwnershipAction::ShowLocalCursor,
                        OwnershipAction::InformPeerRelease { target: from },
                    ]
                }
                _ => Vec::new(),
            },

            OwnershipEvent::PeerLost { .. } => {
                if let Some(target) = self.current_peer().map(str::to_owned) {
                    self.state = OwnershipState::LocalOwned;
                    vec![
                        OwnershipAction::ShowLocalCursor,
                        OwnershipAction::NotifyPeerLost { target },
                    ]
                } else {
                    Vec::new()
                }
            }

            OwnershipEvent::Suspend { reason, at_ms } => {
                if matches!(self.state, OwnershipState::Suspended { .. }) {
                    return Vec::new();
                }
                let peer = self.current_peer().map(str::to_owned);
                self.state = OwnershipState::Suspended {
                    reason: reason.clone(),
                    since_ms: at_ms,
                };
                let mut actions = vec![
                    OwnershipAction::ShowLocalCursor,
                    OwnershipAction::NotifySuspended { reason },
                ];
                if let Some(target) = peer {
                    actions.insert(1, OwnershipAction::InformPeerRelease { target });
                }
                actions
            }

            OwnershipEvent::Resume { .. } => {
                if matches!(self.state, OwnershipState::Suspended { .. }) {
                    self.state = OwnershipState::LocalOwned;
                    vec![OwnershipAction::NotifyResumed]
                } else {
                    Vec::new()
                }
            }

            OwnershipEvent::Tick { at_ms } => self.on_tick(at_ms),
        }
    }

    fn on_tick(&mut self, at_ms: u64) -> Vec<OwnershipAction> {
        match &self.state {
            OwnershipState::HandoffOut { target, since_ms } => {
                let elapsed = at_ms.saturating_sub(*since_ms);
                if elapsed >= HANDOFF_TIMEOUT_MS {
                    let target = target.clone();
                    self.state = OwnershipState::LocalOwned;
                    vec![
                        OwnershipAction::ShowLocalCursor,
                        OwnershipAction::InformPeerRelease {
                            target: target.clone(),
                        },
                        OwnershipAction::NotifyHandoffTimeout {
                            target,
                            elapsed_ms: elapsed,
                        },
                    ]
                } else if elapsed >= HANDOFF_SLOW_MS && !self.slow_notified {
                    self.slow_notified = true;
                    vec![OwnershipAction::NotifyHandoffSlow {
                        target: target.clone(),
                        elapsed_ms: elapsed,
                    }]
                } else {
                    Vec::new()
                }
            }
            OwnershipState::HandoffBack { since_ms, .. } => {
                let elapsed = at_ms.saturating_sub(*since_ms);
                if elapsed >= HANDOFF_BACK_TIMEOUT_MS {
                    self.state = OwnershipState::LocalOwned;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crossing() -> OwnershipEvent {
        OwnershipEvent::EdgeCrossed {
            target: "macbook-air-13".to_owned(),
            modifiers: Modifiers::CTRL,
            at_ms: 100,
        }
    }

    fn remote_machine() -> OwnershipMachine {
        let mut machine = OwnershipMachine::new();
        machine.handle(crossing());
        machine.handle(OwnershipEvent::HandoffAccepted { at_ms: 120 });
        machine
    }

    #[test]
    fn happy_path_hides_then_hands_over() {
        let mut machine = OwnershipMachine::new();
        let actions = machine.handle(crossing());
        assert_eq!(
            actions,
            vec![
                OwnershipAction::HideLocalCursor,
                OwnershipAction::InformPeerHandoff {
                    target: "macbook-air-13".to_owned(),
                    modifiers: Modifiers::CTRL,
                }
            ]
        );
        assert!(matches!(machine.state(), OwnershipState::HandoffOut { .. }));
        assert_eq!(machine.current_peer(), Some("macbook-air-13"));

        machine.handle(OwnershipEvent::HandoffAccepted { at_ms: 150 });
        assert!(matches!(
            machine.state(),
            OwnershipState::RemoteOwned { .. }
        ));
    }

    #[test]
    fn crossing_is_ignored_while_a_peer_owns_input() {
        let mut machine = remote_machine();
        let actions = machine.handle(crossing());
        assert!(actions.is_empty());
        assert_eq!(machine.state().label(), "REMOTE_OWNED");
    }

    #[test]
    fn slow_ack_notifies_once_then_times_out_at_750ms() {
        let mut machine = OwnershipMachine::new();
        machine.handle(crossing());

        assert!(machine
            .handle(OwnershipEvent::Tick { at_ms: 200 })
            .is_empty());

        let slow = machine.handle(OwnershipEvent::Tick { at_ms: 400 });
        assert_eq!(
            slow,
            vec![OwnershipAction::NotifyHandoffSlow {
                target: "macbook-air-13".to_owned(),
                elapsed_ms: 300,
            }]
        );
        assert!(machine
            .handle(OwnershipEvent::Tick { at_ms: 500 })
            .is_empty());
        assert!(matches!(machine.state(), OwnershipState::HandoffOut { .. }));

        let timeout = machine.handle(OwnershipEvent::Tick { at_ms: 900 });
        assert!(timeout.contains(&OwnershipAction::NotifyHandoffTimeout {
            target: "macbook-air-13".to_owned(),
            elapsed_ms: 800,
        }));
        assert!(timeout.contains(&OwnershipAction::ShowLocalCursor));
        assert!(machine.is_local());
    }

    #[test]
    fn return_home_releases_the_peer_and_settles_after_timeout() {
        let mut machine = remote_machine();
        let actions = machine.handle(OwnershipEvent::ReturnHome { at_ms: 500 });
        assert_eq!(
            actions,
            vec![
                OwnershipAction::ShowLocalCursor,
                OwnershipAction::InformPeerRelease {
                    target: "macbook-air-13".to_owned(),
                },
            ]
        );
        assert!(matches!(
            machine.state(),
            OwnershipState::HandoffBack { .. }
        ));

        machine.handle(OwnershipEvent::Tick { at_ms: 600 });
        assert!(matches!(
            machine.state(),
            OwnershipState::HandoffBack { .. }
        ));
        machine.handle(OwnershipEvent::Tick { at_ms: 800 });
        assert!(machine.is_local());
    }

    #[test]
    fn local_input_while_remote_takes_control_back() {
        let mut machine = remote_machine();
        let actions = machine.handle(OwnershipEvent::LocalInputWhileRemote { at_ms: 300 });
        assert!(actions.contains(&OwnershipAction::ShowLocalCursor));
        assert!(matches!(
            machine.state(),
            OwnershipState::HandoffBack { .. }
        ));
    }

    #[test]
    fn peer_loss_immediately_restores_the_local_cursor() {
        let mut machine = remote_machine();
        let actions = machine.handle(OwnershipEvent::PeerLost { at_ms: 900 });
        assert_eq!(
            actions,
            vec![
                OwnershipAction::ShowLocalCursor,
                OwnershipAction::NotifyPeerLost {
                    target: "macbook-air-13".to_owned(),
                },
            ]
        );
        assert!(machine.is_local());
    }

    #[test]
    fn suspend_and_resume_are_idempotent_and_release_the_peer() {
        let mut machine = remote_machine();
        let actions = machine.handle(OwnershipEvent::Suspend {
            reason: SuspendReason::LockScreen,
            at_ms: 400,
        });
        assert!(actions.contains(&OwnershipAction::InformPeerRelease {
            target: "macbook-air-13".to_owned(),
        }));
        assert!(actions.contains(&OwnershipAction::NotifySuspended {
            reason: SuspendReason::LockScreen,
        }));
        assert!(machine
            .handle(OwnershipEvent::Suspend {
                reason: SuspendReason::Sleep,
                at_ms: 500,
            })
            .is_empty());

        let resumed = machine.handle(OwnershipEvent::Resume { at_ms: 900 });
        assert_eq!(resumed, vec![OwnershipAction::NotifyResumed]);
        assert!(machine.is_local());
    }

    #[test]
    fn handoff_failure_returns_control_immediately() {
        let mut machine = OwnershipMachine::new();
        machine.handle(crossing());
        let actions = machine.handle(OwnershipEvent::HandoffFailed { at_ms: 130 });
        assert!(actions.contains(&OwnershipAction::ShowLocalCursor));
        assert!(actions.contains(&OwnershipAction::InformPeerRelease {
            target: "macbook-air-13".to_owned(),
        }));
        assert!(machine.is_local());
    }
}
