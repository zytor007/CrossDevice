use cd_proto::sas::sas_digits;
use thiserror::Error;

/// Pairing code lifetime; both sides must confirm inside this window.
pub const PAIRING_TTL_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingState {
    AwaitingPeer,
    CodeReady {
        code: String,
        local_confirmed: bool,
        peer_confirmed: bool,
    },
    Paired {
        code: String,
        paired_at_ms: u64,
    },
    Expired,
    Failed {
        reason: String,
    },
}

impl PairingState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::AwaitingPeer => "AWAITING_PEER",
            Self::CodeReady { .. } => "CODE_READY",
            Self::Paired { .. } => "PAIRED",
            Self::Expired => "EXPIRED",
            Self::Failed { .. } => "FAILED",
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PairingError {
    #[error("pairing session is not awaiting a peer")]
    NotAwaitingPeer,
    #[error("pairing code is not ready")]
    CodeNotReady,
    #[error("pairing session expired")]
    Expired,
    #[error("local confirmation already recorded")]
    LocalAlreadyConfirmed,
    #[error("peer confirmation already recorded")]
    PeerAlreadyConfirmed,
    #[error("public key must not be empty")]
    EmptyKey,
    #[error("nonce must be at least 16 bytes")]
    ShortNonce,
    #[error("pairing session is already {0}")]
    Inactive(&'static str),
}

/// Both peers derive the same six-digit code from
/// `blake3(sorted(pubA, pubB) || nonce)`, display it, and confirm independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingSession {
    local_key: Vec<u8>,
    local_fingerprint: String,
    remote_key: Option<Vec<u8>>,
    remote_fingerprint: Option<String>,
    nonce: Vec<u8>,
    started_at_ms: u64,
    expires_at_ms: u64,
    state: PairingState,
}

impl PairingSession {
    pub fn start(
        local_key: Vec<u8>,
        local_fingerprint: impl Into<String>,
        nonce: Vec<u8>,
        now_ms: u64,
    ) -> Result<Self, PairingError> {
        if local_key.is_empty() {
            return Err(PairingError::EmptyKey);
        }
        if nonce.len() < 16 {
            return Err(PairingError::ShortNonce);
        }
        Ok(Self {
            local_key,
            local_fingerprint: local_fingerprint.into(),
            remote_key: None,
            remote_fingerprint: None,
            nonce,
            started_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(PAIRING_TTL_MS),
            state: PairingState::AwaitingPeer,
        })
    }

    pub fn state(&self) -> &PairingState {
        &self.state
    }

    pub fn code(&self) -> Option<&str> {
        match &self.state {
            PairingState::CodeReady { code, .. } | PairingState::Paired { code, .. } => Some(code),
            _ => None,
        }
    }

    pub fn is_paired(&self) -> bool {
        matches!(self.state, PairingState::Paired { .. })
    }

    pub fn local_fingerprint(&self) -> &str {
        &self.local_fingerprint
    }

    pub fn remote_fingerprint(&self) -> Option<&str> {
        self.remote_fingerprint.as_deref()
    }

    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }

    /// Receives the peer public key and computes the code both sides display.
    pub fn receive_peer(
        &mut self,
        remote_key: Vec<u8>,
        remote_fingerprint: impl Into<String>,
        now_ms: u64,
    ) -> Result<String, PairingError> {
        if remote_key.is_empty() {
            return Err(PairingError::EmptyKey);
        }
        self.ensure_active(now_ms)?;
        match &self.state {
            PairingState::AwaitingPeer => {}
            other => {
                return Err(PairingError::Inactive(other.label()));
            }
        }

        let code = sas_digits(&self.local_key, &remote_key, &self.nonce);
        self.remote_key = Some(remote_key);
        self.remote_fingerprint = Some(remote_fingerprint.into());
        self.state = PairingState::CodeReady {
            code: code.clone(),
            local_confirmed: false,
            peer_confirmed: false,
        };
        Ok(code)
    }

    pub fn confirm_local(&mut self, now_ms: u64) -> Result<(), PairingError> {
        self.ensure_active(now_ms)?;
        let PairingState::CodeReady {
            code,
            local_confirmed,
            peer_confirmed,
        } = self.state.clone()
        else {
            return Err(PairingError::CodeNotReady);
        };
        if local_confirmed {
            return Err(PairingError::LocalAlreadyConfirmed);
        }
        if peer_confirmed {
            self.state = PairingState::Paired {
                code,
                paired_at_ms: now_ms,
            };
        } else {
            self.state = PairingState::CodeReady {
                code,
                local_confirmed: true,
                peer_confirmed,
            };
        }
        Ok(())
    }

    pub fn confirm_peer(&mut self, now_ms: u64) -> Result<(), PairingError> {
        self.ensure_active(now_ms)?;
        let PairingState::CodeReady {
            code,
            local_confirmed,
            peer_confirmed,
        } = self.state.clone()
        else {
            return Err(PairingError::CodeNotReady);
        };
        if peer_confirmed {
            return Err(PairingError::PeerAlreadyConfirmed);
        }
        if local_confirmed {
            self.state = PairingState::Paired {
                code,
                paired_at_ms: now_ms,
            };
        } else {
            self.state = PairingState::CodeReady {
                code,
                local_confirmed,
                peer_confirmed: true,
            };
        }
        Ok(())
    }

    /// Advances the expiry timer; returns true when the session just expired.
    pub fn tick(&mut self, now_ms: u64) -> bool {
        if matches!(
            self.state,
            PairingState::Paired { .. } | PairingState::Expired | PairingState::Failed { .. }
        ) {
            return false;
        }
        if now_ms >= self.expires_at_ms {
            self.state = PairingState::Expired;
            return true;
        }
        false
    }

    pub fn abort(&mut self, reason: impl Into<String>) {
        self.state = PairingState::Failed {
            reason: reason.into(),
        };
    }

    fn ensure_active(&mut self, now_ms: u64) -> Result<(), PairingError> {
        match &self.state {
            PairingState::Expired => return Err(PairingError::Expired),
            PairingState::Failed { .. } => {
                return Err(PairingError::Inactive("FAILED"));
            }
            PairingState::Paired { .. } => {
                return Err(PairingError::Inactive("PAIRED"));
            }
            _ => {}
        }
        if now_ms >= self.expires_at_ms {
            self.state = PairingState::Expired;
            return Err(PairingError::Expired);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonce() -> Vec<u8> {
        (0..16).collect()
    }

    fn sessions() -> (PairingSession, PairingSession) {
        let first = PairingSession::start(vec![1; 32], "aa".repeat(32), nonce(), 0).unwrap();
        let second = PairingSession::start(vec![2; 32], "bb".repeat(32), nonce(), 0).unwrap();
        (first, second)
    }

    #[test]
    fn both_sides_derive_the_same_code_and_pair() {
        let (mut a, mut b) = sessions();
        let a_code = a.receive_peer(vec![2; 32], "bb".repeat(32), 10).unwrap();
        let b_code = b.receive_peer(vec![1; 32], "aa".repeat(32), 10).unwrap();
        assert_eq!(a_code, b_code);
        assert_eq!(a_code.len(), 6);

        a.confirm_local(20).unwrap();
        b.confirm_peer(30).unwrap();
        a.confirm_peer(30).unwrap();
        b.confirm_local(40).unwrap();
        assert!(a.is_paired());
        assert!(b.is_paired());
        assert_eq!(a.remote_fingerprint(), Some("bb".repeat(32).as_str()));
    }

    #[test]
    fn different_keys_produce_a_different_code() {
        let mut a = PairingSession::start(vec![1; 32], "aa".repeat(32), nonce(), 0).unwrap();
        let code_a = a.receive_peer(vec![2; 32], "bb".repeat(32), 0).unwrap();
        let mut c = PairingSession::start(vec![3; 32], "cc".repeat(32), nonce(), 0).unwrap();
        let code_c = c.receive_peer(vec![4; 32], "dd".repeat(32), 0).unwrap();
        assert_ne!(code_a, code_c);
    }

    #[test]
    fn code_requires_both_confirmations() {
        let (mut a, _) = sessions();
        a.receive_peer(vec![2; 32], "bb".repeat(32), 0).unwrap();
        a.confirm_local(10).unwrap();
        assert!(!a.is_paired());
        assert_eq!(a.state().label(), "CODE_READY");
        a.confirm_peer(20).unwrap();
        assert!(a.is_paired());
    }

    #[test]
    fn session_expires_after_sixty_seconds() {
        let (mut a, _) = sessions();
        a.receive_peer(vec![2; 32], "bb".repeat(32), 0).unwrap();
        assert!(!a.tick(59_999));
        assert!(a.tick(60_000));
        assert_eq!(a.state().label(), "EXPIRED");
        assert_eq!(a.confirm_local(60_001), Err(PairingError::Expired));
    }

    #[test]
    fn invalid_sequences_are_rejected() {
        let (mut a, _) = sessions();
        assert_eq!(a.confirm_local(0), Err(PairingError::CodeNotReady));
        a.receive_peer(vec![2; 32], "bb".repeat(32), 0).unwrap();
        assert!(matches!(
            a.receive_peer(vec![3; 32], "cc".repeat(32), 0),
            Err(PairingError::Inactive(_))
        ));
        a.confirm_local(0).unwrap();
        assert_eq!(a.confirm_local(0), Err(PairingError::LocalAlreadyConfirmed));
    }

    #[test]
    fn nonce_and_key_validation() {
        assert!(matches!(
            PairingSession::start(vec![1; 32], "aa", vec![0; 8], 0),
            Err(PairingError::ShortNonce)
        ));
        assert!(matches!(
            PairingSession::start(vec![], "aa", nonce(), 0),
            Err(PairingError::EmptyKey)
        ));
    }

    #[test]
    fn abort_marks_the_session_failed() {
        let (mut a, _) = sessions();
        a.abort("user rejected");
        assert_eq!(a.state().label(), "FAILED");
        assert!(a.confirm_local(0).is_err());
    }
}
