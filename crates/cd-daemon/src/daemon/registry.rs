use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use cd_clipboard::Snapshot;
use cd_proto::{ControlMessage, InputEvent};
use tokio::sync::mpsc;

/// Work the daemon wants a session task to perform on its link. Sessions own
/// their `Link`; the registry only holds a queue.
#[derive(Debug, Clone)]
pub enum Outbound {
    Clipboard(Snapshot),
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Input(InputEvent),
    Control(ControlMessage),
    SendFile {
        path: PathBuf,
        clipboard: bool,
    },
    /// The console revoked this device from the whitelist; close the session.
    Disconnect,
}

#[derive(Clone)]
struct Channel {
    token: u64,
    tx: mpsc::UnboundedSender<Outbound>,
}

#[derive(Clone)]
pub struct PeerRegistry {
    peers: Arc<Mutex<BTreeMap<String, Channel>>>,
    next_token: Arc<AtomicU64>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self {
            peers: Arc::new(Mutex::new(BTreeMap::new())),
            next_token: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Returns a session token; a stale session cannot remove a newer channel
    /// registered by a reconnect.
    pub fn register(&self, id: &str, tx: mpsc::UnboundedSender<Outbound>) -> u64 {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(id.to_owned(), Channel { token, tx });
        token
    }

    pub fn unregister(&self, id: &str, token: u64) {
        let mut peers = self.lock();
        if peers.get(id).is_some_and(|channel| channel.token == token) {
            peers.remove(id);
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn first_peer(&self) -> Option<String> {
        self.lock().keys().next().cloned()
    }

    pub fn send_to(&self, id: &str, outbound: Outbound) -> bool {
        let channel = self.lock().get(id).map(|channel| channel.tx.clone());
        channel.is_some_and(|tx| tx.send(outbound).is_ok())
    }

    pub fn send_all(&self, outbound: Outbound) {
        let channels = self
            .lock()
            .values()
            .map(|channel| channel.tx.clone())
            .collect::<Vec<_>>();
        for tx in channels {
            let _ = tx.send(outbound.clone());
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn send_input(&self, id: &str, event: InputEvent) -> bool {
        self.send_to(id, Outbound::Input(event))
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Channel>> {
        self.peers
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}
