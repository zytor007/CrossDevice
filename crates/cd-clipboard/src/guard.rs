use std::collections::BTreeMap;

use cd_proto::ClipboardAnnounce;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteDecision {
    Apply,
    Duplicate,
    Stale,
    Echo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    seq: u64,
    hash: String,
}

/// Prevents two failure modes:
/// - a peer resending an old sequence number,
/// - our own clipboard change being uploaded again after we applied remote data.
#[derive(Debug, Default)]
pub struct LoopGuard {
    seen: BTreeMap<String, Seen>,
    echo_hash: Option<String>,
    echo_until_ms: u64,
}

impl LoopGuard {
    pub const ECHO_WINDOW_MS: u64 = 10_000;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn evaluate_remote(&mut self, announce: &ClipboardAnnounce, now_ms: u64) -> RemoteDecision {
        if let Some(seen) = self.seen.get(&announce.origin_id) {
            if announce.seq <= seen.seq {
                return if announce.hash == seen.hash {
                    RemoteDecision::Duplicate
                } else {
                    RemoteDecision::Stale
                };
            }
        }

        let echo = self.echo_hash.as_deref() == Some(announce.hash.as_str())
            && now_ms <= self.echo_until_ms;
        self.seen.insert(
            announce.origin_id.clone(),
            Seen {
                seq: announce.seq,
                hash: announce.hash.clone(),
            },
        );
        if echo {
            RemoteDecision::Echo
        } else {
            RemoteDecision::Apply
        }
    }

    /// Call right after the remote snapshot has been written to the OS clipboard.
    pub fn note_applied_remote(&mut self, hash: impl Into<String>, now_ms: u64) {
        self.echo_hash = Some(hash.into());
        self.echo_until_ms = now_ms.saturating_add(Self::ECHO_WINDOW_MS);
    }

    /// Local clipboard readers call this before uploading a snapshot.
    pub fn should_publish_local(&self, hash: &str, now_ms: u64) -> bool {
        if self.echo_hash.as_deref() == Some(hash) && now_ms <= self.echo_until_ms {
            return false;
        }
        true
    }

    /// Remote ack received; the echo window can end early.
    pub fn acknowledge(&mut self) {
        self.echo_hash = None;
        self.echo_until_ms = 0;
    }

    pub fn is_suppressing(&self) -> bool {
        self.echo_hash.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announce(origin: &str, seq: u64, hash: &str) -> ClipboardAnnounce {
        ClipboardAnnounce {
            kind: cd_proto::ClipboardKind::Text,
            hash: hash.to_owned(),
            size: 1,
            origin_id: origin.to_owned(),
            seq,
        }
    }

    #[test]
    fn duplicate_and_stale_sequences_are_rejected() {
        let mut guard = LoopGuard::new();
        assert_eq!(
            guard.evaluate_remote(&announce("linux-4k", 1, "aa"), 0),
            RemoteDecision::Apply
        );
        assert_eq!(
            guard.evaluate_remote(&announce("linux-4k", 1, "aa"), 100),
            RemoteDecision::Duplicate
        );
        assert_eq!(
            guard.evaluate_remote(&announce("linux-4k", 1, "bb"), 100),
            RemoteDecision::Stale
        );
        assert_eq!(
            guard.evaluate_remote(&announce("linux-4k", 2, "bb"), 100),
            RemoteDecision::Apply
        );
    }

    #[test]
    fn applied_remote_hash_is_not_uploaded_back_inside_the_window() {
        let mut guard = LoopGuard::new();
        guard.note_applied_remote("aa", 1_000);
        assert!(!guard.should_publish_local("aa", 1_100));
        assert!(guard.should_publish_local("aa", 11_001));
        assert!(guard.should_publish_local("bb", 1_100));
    }

    #[test]
    fn remote_echo_of_applied_content_is_ignored() {
        let mut guard = LoopGuard::new();
        guard.note_applied_remote("aa", 1_000);
        assert_eq!(
            guard.evaluate_remote(&announce("macbook-air-13", 5, "aa"), 1_100),
            RemoteDecision::Echo
        );
        assert_eq!(
            guard.evaluate_remote(&announce("macbook-air-13", 6, "cc"), 1_100),
            RemoteDecision::Apply
        );
    }

    #[test]
    fn acknowledge_ends_echo_suppression_early() {
        let mut guard = LoopGuard::new();
        guard.note_applied_remote("aa", 1_000);
        assert!(guard.is_suppressing());
        guard.acknowledge();
        assert!(!guard.is_suppressing());
        assert!(guard.should_publish_local("aa", 1_001));
    }
}
