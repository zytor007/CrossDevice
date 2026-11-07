use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use cd_clipboard::{Content, History, Snapshot};
use cd_geometry::MonitorInfo;
use cd_store::TransferRecord;
use serde::Serialize;
use serde_json::json;
use tokio::sync::{broadcast, oneshot};

#[derive(Debug, Clone, Serialize)]
pub struct LocalInfo {
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub fingerprint: String,
    pub protocol: String,
    pub data_dir: String,
    pub quic_addr: String,
    pub http_addr: String,
    pub started_at_ms: i64,
    pub pair_mode: String,
    pub clipboard: bool,
    pub input_pump: bool,
    pub edge: String,
    pub pointer_speed: f64,
    pub edge_resistance: u8,
    pub file_conflict: String,
    pub layout_gap_mm: f64,
    pub monitors: Vec<cd_geometry::MonitorInfo>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PeerView {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub address: String,
    pub online: bool,
    pub paired: bool,
    pub rtt_ms: f64,
    pub last_seen_ms: Option<i64>,
    pub connected_at_ms: Option<i64>,
    pub input_owner: bool,
    pub monitors: Vec<MonitorInfo>,
    pub clipboard_enabled: bool,
    pub input_enabled: bool,
    pub features_known: bool,
    pub features_revision: u64,
    pub edge: String,
    pub pointer_speed: f64,
    pub edge_resistance: u8,
    pub settings_known: bool,
    pub settings_revision: u64,
    pub layout_known: bool,
    pub layout_revision: u64,
    #[serde(skip)]
    pub session: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ClipboardView {
    pub direction: String,
    pub origin: String,
    pub seq: u64,
    pub size: u64,
    pub hash: String,
    pub preview: String,
    pub updated_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClipboardHistoryView {
    pub hash: String,
    pub kind: String,
    pub preview: String,
    pub size: u64,
    pub origin: String,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferView {
    pub id: String,
    pub direction: String,
    pub peer_id: String,
    pub root_name: String,
    pub total_bytes: u64,
    pub received_bytes: u64,
    pub state: String,
    pub dest_path: String,
    pub updated_at_ms: i64,
}

impl TransferView {
    pub fn from_record(record: &TransferRecord) -> Self {
        Self {
            id: record.id.clone(),
            direction: record.direction.as_str().to_owned(),
            peer_id: record.peer_id.clone(),
            root_name: record.root_name.clone(),
            total_bytes: record.total_bytes,
            received_bytes: record.received_bytes,
            state: state_label(record.state),
            dest_path: record.dest_path.clone(),
            updated_at_ms: record.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OwnershipView {
    pub state: String,
    pub peer: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingPairView {
    pub id: String,
    pub direction: String,
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub address: String,
    /// Six-digit SAS code shown on both sides.
    pub code: String,
    pub fingerprint: String,
    pub requested_at_ms: i64,
    pub expires_at_ms: i64,
    pub monitors: Vec<MonitorInfo>,
}

struct PendingPair {
    view: PendingPairView,
    decision: Option<oneshot::Sender<bool>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingConflictView {
    pub id: String,
    pub task_id: String,
    pub peer_id: String,
    pub root_name: String,
    pub total_bytes: u64,
    pub destination: String,
    pub requested_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug)]
struct PendingConflict {
    view: PendingConflictView,
    decision: Option<oneshot::Sender<cd_files::ConflictPolicy>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredPeerView {
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub address: String,
    pub paired: bool,
    pub last_seen_ms: i64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct CountersView {
    pub input_events_in: u64,
    pub input_events_out: u64,
    pub clipboard_in: u64,
    pub clipboard_out: u64,
    pub heartbeats: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusSnapshot {
    pub local: LocalInfo,
    pub uptime_ms: u64,
    pub peers: Vec<PeerView>,
    pub clipboard: ClipboardView,
    pub transfers: Vec<TransferView>,
    pub ownership: OwnershipView,
    pub counters: CountersView,
    pub last_error: Option<String>,
    pub pending_pairs: Vec<PendingPairView>,
    pub pending_conflicts: Vec<PendingConflictView>,
    pub discovered: Vec<DiscoveredPeerView>,
    pub clipboard_history: Vec<ClipboardHistoryView>,
}

#[derive(Debug)]
struct Board {
    peers: BTreeMap<String, PeerView>,
    clipboard: ClipboardView,
    transfers: BTreeMap<String, TransferView>,
    ownership: OwnershipView,
    counters: CountersView,
    last_error: Option<String>,
    discovered: BTreeMap<String, DiscoveredPeerView>,
    conflicts: BTreeMap<String, PendingConflict>,
}

pub struct StatusBoard {
    local: Mutex<LocalInfo>,
    started: Instant,
    inner: Mutex<Board>,
    pending: Mutex<BTreeMap<String, PendingPair>>,
    history: Mutex<History>,
    events: broadcast::Sender<String>,
    next_session: AtomicU64,
    next_pair: AtomicU64,
    next_conflict: AtomicU64,
}

impl StatusBoard {
    pub fn new(local: LocalInfo) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            local: Mutex::new(local),
            started: Instant::now(),
            inner: Mutex::new(Board {
                peers: BTreeMap::new(),
                clipboard: ClipboardView::default(),
                transfers: BTreeMap::new(),
                ownership: OwnershipView {
                    state: "local_owned".to_owned(),
                    peer: None,
                },
                counters: CountersView::default(),
                last_error: None,
                discovered: BTreeMap::new(),
                conflicts: BTreeMap::new(),
            }),
            pending: Mutex::new(BTreeMap::new()),
            history: Mutex::new(History::new(20)),
            events,
            next_session: AtomicU64::new(1),
            next_pair: AtomicU64::new(1),
            next_conflict: AtomicU64::new(1),
        }
    }

    pub fn local(&self) -> LocalInfo {
        self.local
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    /// Registers a pairing request and returns the receiver that resolves when
    /// the local user approves or rejects it.
    pub fn request_pair(
        &self,
        mut view: PendingPairView,
    ) -> (PendingPairView, oneshot::Receiver<bool>) {
        if view.id.is_empty() {
            view.id = format!(
                "{:x}-{:x}",
                now_ms(),
                self.next_pair.fetch_add(1, Ordering::Relaxed)
            );
        }
        let (tx, rx) = oneshot::channel();
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        prune_pending(&mut pending, now_ms());
        view.requested_at_ms = now_ms();
        pending.insert(
            view.id.clone(),
            PendingPair {
                view: view.clone(),
                decision: Some(tx),
            },
        );
        drop(pending);
        self.emit_pairing();
        (view, rx)
    }

    pub fn resolve_pair(&self, id: &str, accepted: bool) -> bool {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(mut pair) = pending.remove(id) else {
            return false;
        };
        if let Some(decision) = pair.decision.take() {
            let _ = decision.send(accepted);
        }
        drop(pending);
        self.emit_pairing();
        true
    }

    pub fn pending_pairs(&self) -> Vec<PendingPairView> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        prune_pending(&mut pending, now_ms());
        pending.values().map(|pair| pair.view.clone()).collect()
    }

    fn emit_pairing(&self) {
        self.emit("pairing", &json!({ "pairs": self.pending_pairs() }));
    }

    /// Registers a same-name conflict and returns the receiver that resolves
    /// when the local user chooses rename/overwrite/skip.
    pub fn request_conflict(
        &self,
        mut view: PendingConflictView,
    ) -> (
        PendingConflictView,
        oneshot::Receiver<cd_files::ConflictPolicy>,
    ) {
        if view.id.is_empty() {
            view.id = format!(
                "c-{:x}-{:x}",
                now_ms(),
                self.next_conflict.fetch_add(1, Ordering::Relaxed)
            );
        }
        view.requested_at_ms = now_ms();
        view.expires_at_ms = view.requested_at_ms + 60_000;
        let (tx, rx) = oneshot::channel();
        let mut board = self.lock();
        prune_conflicts(&mut board.conflicts, now_ms());
        board.conflicts.insert(
            view.id.clone(),
            PendingConflict {
                view: view.clone(),
                decision: Some(tx),
            },
        );
        drop(board);
        self.emit_conflicts();
        (view, rx)
    }

    pub fn resolve_conflict(&self, id: &str, policy: cd_files::ConflictPolicy) -> bool {
        let mut board = self.lock();
        let Some(mut conflict) = board.conflicts.remove(id) else {
            return false;
        };
        if let Some(decision) = conflict.decision.take() {
            let _ = decision.send(policy);
        }
        drop(board);
        self.emit_conflicts();
        true
    }

    pub fn pending_conflicts(&self) -> Vec<PendingConflictView> {
        let mut board = self.lock();
        prune_conflicts(&mut board.conflicts, now_ms());
        board
            .conflicts
            .values()
            .map(|item| item.view.clone())
            .collect()
    }

    fn emit_conflicts(&self) {
        self.emit(
            "conflict",
            &json!({ "conflicts": self.pending_conflicts() }),
        );
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        let mut board = self.lock();
        prune_discovered(&mut board.discovered, now_ms());
        StatusSnapshot {
            local: self.local(),
            uptime_ms: self.started.elapsed().as_millis() as u64,
            peers: board.peers.values().cloned().collect(),
            clipboard: board.clipboard.clone(),
            transfers: board.transfers.values().cloned().collect(),
            ownership: board.ownership.clone(),
            counters: board.counters.clone(),
            last_error: board.last_error.clone(),
            pending_pairs: self.pending_pairs(),
            pending_conflicts: board
                .conflicts
                .values()
                .map(|item| item.view.clone())
                .collect(),
            discovered: board.discovered.values().cloned().collect(),
            clipboard_history: self.clipboard_history(),
        }
    }

    pub fn note_discovered(&self, view: DiscoveredPeerView) {
        let mut board = self.lock();
        if board.discovered.len() >= 64 && !board.discovered.contains_key(&view.device_id) {
            return;
        }
        board.discovered.insert(view.device_id.clone(), view);
        prune_discovered(&mut board.discovered, now_ms());
        let list = board.discovered.values().cloned().collect::<Vec<_>>();
        drop(board);
        self.emit("discovered", &json!({ "peers": list }));
    }

    pub fn set_features(&self, clipboard: bool, input: bool) {
        self.local
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clipboard = clipboard;
        self.local
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .input_pump = input;
        self.emit("local", &self.local());
    }

    pub fn set_pair_settings(&self, settings: &cd_proto::PairSettings) {
        {
            let mut local = self
                .local
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            local.edge = edge_label(settings.edge).to_owned();
            local.pointer_speed = settings.pointer_speed;
            local.edge_resistance = settings.edge_resistance;
        }
        let local = self.local();
        self.emit("local", &local);
    }

    pub fn peer_layout(&self, id: &str, revision: u64) {
        let mut board = self.lock();
        let Some(peer) = board.peers.get_mut(id) else {
            return;
        };
        peer.layout_known = true;
        peer.layout_revision = revision;
        let clone = peer.clone();
        drop(board);
        self.emit("peer", &clone);
    }

    pub fn set_layout_gap(&self, gap_mm: f64) {
        self.local
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .layout_gap_mm = gap_mm;
        let local = self.local();
        self.emit("local", &local);
    }

    pub fn set_file_conflict(&self, policy: &str) {
        self.local
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .file_conflict = policy.to_owned();
        let local = self.local();
        self.emit("local", &local);
    }

    pub fn peer_settings(&self, id: &str, settings: cd_proto::PairSettings) {
        let mut board = self.lock();
        let Some(peer) = board.peers.get_mut(id) else {
            return;
        };
        if peer.settings_known && settings.revision < peer.settings_revision {
            return;
        }
        peer.edge = edge_label(settings.edge).to_owned();
        peer.pointer_speed = settings.pointer_speed;
        peer.edge_resistance = settings.edge_resistance;
        peer.settings_known = true;
        peer.settings_revision = settings.revision;
        let clone = peer.clone();
        drop(board);
        self.emit("peer", &clone);
    }

    pub fn peer_features(&self, id: &str, clipboard: bool, input: bool, revision: u64) {
        let mut board = self.lock();
        let Some(peer) = board.peers.get_mut(id) else {
            return;
        };
        peer.clipboard_enabled = clipboard;
        peer.input_enabled = input;
        peer.features_known = true;
        peer.features_revision = revision;
        let clone = peer.clone();
        drop(board);
        self.emit("peer", &clone);
    }

    pub fn peer_connecting(
        &self,
        id: &str,
        name: &str,
        platform: &str,
        address: &str,
        paired: bool,
        monitors: Vec<MonitorInfo>,
    ) -> u64 {
        let session = self.next_session.fetch_add(1, Ordering::Relaxed);
        let peer = PeerView {
            id: id.to_owned(),
            name: name.to_owned(),
            platform: platform.to_owned(),
            address: address.to_owned(),
            online: false,
            paired,
            connected_at_ms: Some(now_ms()),
            monitors,
            session,
            ..PeerView::default()
        };
        self.lock().peers.insert(id.to_owned(), peer.clone());
        self.emit("peer", &peer);
        session
    }

    pub fn peer_online(&self, id: &str, session: u64, rtt_ms: f64) {
        let mut board = self.lock();
        let Some(peer) = board.peers.get_mut(id) else {
            return;
        };
        if peer.session != session {
            return;
        }
        peer.online = true;
        peer.paired = true;
        peer.rtt_ms = rtt_ms;
        peer.last_seen_ms = Some(now_ms());
        let clone = peer.clone();
        drop(board);
        self.emit("peer", &clone);
    }

    pub fn peer_seen(&self, id: &str, seq: u64) {
        let mut board = self.lock();
        let mut updated = None;
        if let Some(peer) = board.peers.get_mut(id) {
            peer.last_seen_ms = Some(now_ms());
            updated = Some(peer.clone());
        }
        board.counters.heartbeats = board.counters.heartbeats.saturating_add(seq.min(1));
        drop(board);
        if let Some(peer) = updated {
            self.emit("peer", &peer);
        }
    }

    pub fn peer_offline(&self, id: &str, session: u64, reason: &str) {
        let mut board = self.lock();
        let Some(peer) = board.peers.get_mut(id) else {
            return;
        };
        if peer.session != session {
            return;
        }
        peer.online = false;
        peer.input_owner = false;
        let clone = peer.clone();
        board.last_error = Some(reason.to_owned());
        drop(board);
        self.emit("peer", &clone);
    }

    pub fn set_ownership(&self, state: &str, peer: Option<&str>) {
        let mut board = self.lock();
        board.ownership.state = state.to_owned();
        board.ownership.peer = peer.map(str::to_owned);
        for item in board.peers.values_mut() {
            item.input_owner = peer == Some(item.id.as_str());
        }
        let view = board.ownership.clone();
        drop(board);
        self.emit("ownership", &view);
    }

    pub fn clipboard_local(&self, snapshot: &Snapshot) {
        let mut board = self.lock();
        board.clipboard = ClipboardView {
            direction: "out".to_owned(),
            origin: snapshot.origin_id.clone(),
            seq: snapshot.seq,
            size: snapshot.size,
            hash: snapshot.hash.clone(),
            preview: snapshot.content.preview(),
            updated_at_ms: Some(now_ms()),
        };
        board.counters.clipboard_out = board.counters.clipboard_out.saturating_add(1);
        let view = board.clipboard.clone();
        drop(board);
        self.record_history(snapshot);
        self.emit("clipboard", &view);
    }

    pub fn clipboard_remote(&self, snapshot: &Snapshot) {
        let mut board = self.lock();
        board.clipboard = ClipboardView {
            direction: "in".to_owned(),
            origin: snapshot.origin_id.clone(),
            seq: snapshot.seq,
            size: snapshot.size,
            hash: snapshot.hash.clone(),
            preview: snapshot.content.preview(),
            updated_at_ms: Some(now_ms()),
        };
        board.counters.clipboard_in = board.counters.clipboard_in.saturating_add(1);
        let view = board.clipboard.clone();
        drop(board);
        self.record_history(snapshot);
        self.emit("clipboard", &view);
    }

    fn record_history(&self, snapshot: &Snapshot) {
        let now = now_ms() as u64;
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        history.push(snapshot.clone(), now);
        drop(history);
        self.emit_history();
    }

    pub fn history_snapshot(&self, hash: &str) -> Option<Snapshot> {
        let history = self
            .history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        history
            .items()
            .iter()
            .find(|item| item.snapshot().hash == hash)
            .map(|item| item.snapshot().clone())
    }

    fn clipboard_history(&self) -> Vec<ClipboardHistoryView> {
        let history = self
            .history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        history
            .items()
            .iter()
            .map(|item| {
                let snapshot = item.snapshot();
                ClipboardHistoryView {
                    hash: snapshot.hash.clone(),
                    kind: kind_label(&snapshot.content).to_owned(),
                    preview: snapshot.content.preview(),
                    size: snapshot.size,
                    origin: snapshot.origin_id.clone(),
                    created_at_ms: item.created_at_ms(),
                }
            })
            .collect()
    }

    fn emit_history(&self) {
        self.emit(
            "clipboard_history",
            &json!({ "items": self.clipboard_history() }),
        );
    }

    pub fn transfer_started(&self, record: &TransferRecord) {
        self.publish_transfer(record);
    }

    pub fn transfer_progress(&self, id: &str, received_bytes: u64) {
        let mut board = self.lock();
        let updated = if let Some(view) = board.transfers.get_mut(id) {
            view.received_bytes = received_bytes;
            view.state = "running".to_owned();
            view.updated_at_ms = now_ms();
            Some(view.clone())
        } else {
            None
        };
        drop(board);
        if let Some(view) = updated {
            self.emit("transfer", &view);
        }
    }

    pub fn transfer_finished(&self, record: &TransferRecord) {
        self.publish_transfer(record);
    }

    fn publish_transfer(&self, record: &TransferRecord) {
        let view = TransferView::from_record(record);
        self.lock()
            .transfers
            .insert(record.id.clone(), view.clone());
        self.emit("transfer", &view);
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn input_in(&self, count: u64) {
        let mut board = self.lock();
        board.counters.input_events_in = board.counters.input_events_in.saturating_add(count);
        let counters = board.counters.clone();
        drop(board);
        self.emit("counters", &counters);
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn input_out(&self, count: u64) {
        let mut board = self.lock();
        board.counters.input_events_out = board.counters.input_events_out.saturating_add(count);
        let counters = board.counters.clone();
        drop(board);
        self.emit("counters", &counters);
    }

    pub fn bytes(&self, inbound: u64, outbound: u64) {
        let mut board = self.lock();
        board.counters.bytes_in = board.counters.bytes_in.saturating_add(inbound);
        board.counters.bytes_out = board.counters.bytes_out.saturating_add(outbound);
        let counters = board.counters.clone();
        drop(board);
        self.emit("counters", &counters);
    }

    pub fn note_error(&self, message: impl Into<String>) {
        let message = message.into();
        self.lock().last_error = Some(message.clone());
        self.emit("error", &serde_json::json!({ "message": message }));
    }

    fn emit<T: Serialize>(&self, kind: &str, body: &T) {
        let mut value = serde_json::to_value(body).unwrap_or(serde_json::Value::Null);
        if let serde_json::Value::Object(map) = &mut value {
            map.insert(
                "type".to_owned(),
                serde_json::Value::String(kind.to_owned()),
            );
        }
        if let Ok(text) = serde_json::to_string(&value) {
            let _ = self.events.send(text);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Board> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

fn state_label(state: cd_files::TransferState) -> String {
    match state {
        cd_files::TransferState::Queued => "queued",
        cd_files::TransferState::Running => "running",
        cd_files::TransferState::Paused => "paused",
        cd_files::TransferState::Completed => "completed",
        cd_files::TransferState::Skipped => "skipped",
        cd_files::TransferState::Cancelled => "cancelled",
        cd_files::TransferState::Failed => "failed",
    }
    .to_owned()
}

fn prune_pending(pending: &mut BTreeMap<String, PendingPair>, now_ms: i64) {
    let expired = pending
        .iter()
        .filter(|(_, pair)| pair.view.expires_at_ms <= now_ms)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in expired {
        if let Some(mut pair) = pending.remove(&id) {
            if let Some(decision) = pair.decision.take() {
                let _ = decision.send(false);
            }
        }
    }
}

fn prune_discovered(discovered: &mut BTreeMap<String, DiscoveredPeerView>, now_ms: i64) {
    discovered.retain(|_, peer| now_ms.saturating_sub(peer.last_seen_ms) <= 15_000);
}

fn prune_conflicts(conflicts: &mut BTreeMap<String, PendingConflict>, now_ms: i64) {
    let expired = conflicts
        .iter()
        .filter(|(_, conflict)| conflict.view.expires_at_ms <= now_ms)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    for id in expired {
        if let Some(mut conflict) = conflicts.remove(&id) {
            if let Some(decision) = conflict.decision.take() {
                let _ = decision.send(cd_files::ConflictPolicy::Rename);
            }
        }
    }
}

fn edge_label(edge: cd_proto::EdgeSide) -> &'static str {
    match edge {
        cd_proto::EdgeSide::Left => "left",
        cd_proto::EdgeSide::Right => "right",
    }
}

fn kind_label(content: &Content) -> &'static str {
    match content {
        Content::Text { .. } => "text",
        Content::Html { .. } => "html",
        Content::Image { .. } => "image",
        Content::Files { .. } => "files",
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
