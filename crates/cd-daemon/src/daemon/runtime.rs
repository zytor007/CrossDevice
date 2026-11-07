use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cd_clipboard::{Content, LoopGuard, RemoteDecision, Snapshot};
use cd_core::{OwnershipEvent, OwnershipMachine};
use cd_files::{BlockCache, ConflictPolicy};
use cd_geometry::{presets, Layout, MonitorInfo, Side, MAX_GAP_MM};
use cd_proto::{
    Capabilities, ClipboardAnnounce, ControlMessage, EdgeSide, ErrorCode, FeatureSet,
    FileSendGrant, FileSendRelease, FileSendRequest, Heartbeat, Hello, InputOwner, LayoutSet,
    OwnerPhase, PairSettings, ProtocolError, ProtocolVersion, TransferCancel,
};
use cd_store::{DeviceRecord, Store, TransferDirection, TransferRecord};
use cd_sync::{
    pair, pair_begin_initiator, pair_begin_responder, pair_finish_initiator, pair_finish_responder,
    send_clipboard, sha256_bytes, CancelToken, PairPreview, SyncError,
};
use cd_transport::{Identity, Link, QuicTransport};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use super::clipboard::{self, ClipboardHandle, Command as ClipboardCommand};
use super::config::{Config, DaemonPaths, PairMode};
use super::input::{self, InputHandle};
use super::registry::{Outbound, PeerRegistry};
use super::status::{LocalInfo, PendingConflictView, PendingPairView, StatusBoard};

const SERVER_NAME: &str = "crossdev.local";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(35);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const PAIR_APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_CLIPBOARD_BYTES: u64 = cd_clipboard::DEFAULT_AUTO_FETCH_BYTES;
const MAX_IMAGE_CLIPBOARD_BYTES: u64 = 10 * 1024 * 1024;
const FILE_GRANT_TIMEOUT: Duration = Duration::from_secs(5);
/// Retry spacing after a mutually denied file-channel request. The smaller
/// device id retries first so two crossing requests cannot livelock.
const FILE_RETRY_FAST: Duration = Duration::from_millis(250);
const FILE_RETRY_SLOW: Duration = Duration::from_millis(900);

pub struct Shared {
    pub config: Arc<Config>,
    pub paths: Arc<DaemonPaths>,
    pub identity: Arc<Identity>,
    pub store: Arc<Mutex<Store>>,
    pub status: Arc<StatusBoard>,
    pub registry: PeerRegistry,
    pub guard: Arc<Mutex<LoopGuard>>,
    pub ownership: Arc<Mutex<OwnershipMachine>>,
    pub input: Arc<Mutex<Option<InputHandle>>>,
    pub clipboard: Arc<Mutex<Option<std::sync::mpsc::Sender<ClipboardCommand>>>>,
    pub clipboard_handle: Arc<Mutex<Option<ClipboardHandle>>>,
    pub clipboard_out: mpsc::UnboundedSender<Snapshot>,
    pub clipboard_system: bool,
    pub features: Arc<Features>,
    /// Pair-level settings mirrored to peers; the input pump reads them live.
    pub settings: Arc<Mutex<PairSettings>>,
    /// Receiver-side policy when an incoming root already exists.
    pub file_conflict: Arc<Mutex<ConflictPolicy>>,
    /// Dead-zone gap between the local and peer screens, in millimetres.
    pub layout_gap_mm: Arc<Mutex<f64>>,
    pub cache: Arc<BlockCache>,
    pub monitor: MonitorInfo,
    pub remote_input: Arc<AtomicBool>,
    /// Cancellation tokens for active file transfers.
    pub cancels: Cancels,
}

#[derive(Debug)]
pub struct Features {
    clipboard: AtomicBool,
    input: AtomicBool,
    revision: AtomicU64,
}

impl Features {
    fn new(clipboard: bool, input: bool) -> Self {
        Self {
            clipboard: AtomicBool::new(clipboard),
            input: AtomicBool::new(input),
            revision: AtomicU64::new(1),
        }
    }

    pub fn clipboard(&self) -> bool {
        self.clipboard.load(Ordering::Relaxed)
    }

    pub fn input(&self) -> bool {
        self.input.load(Ordering::Relaxed)
    }

    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    fn next_revision(&self) -> u64 {
        self.revision.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// One bidirectional file channel per daemon session. A C0 token handshake
/// prevents both peers from opening file manifests at the same time, which
/// would otherwise make each side wait for the other's resume plan.
#[derive(Debug)]
pub struct FileChannel {
    busy: AtomicBool,
    /// True while the local side holds the channel for an outbound transfer.
    outbound: AtomicBool,
    next_token: AtomicU64,
    active_token: AtomicU64,
    /// The current inbound transfer was triggered by a file-list clipboard.
    active_clipboard: AtomicBool,
    waiters: Mutex<BTreeMap<u64, oneshot::Sender<bool>>>,
}

impl FileChannel {
    pub fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            outbound: AtomicBool::new(false),
            next_token: AtomicU64::new(1),
            active_token: AtomicU64::new(0),
            active_clipboard: AtomicBool::new(false),
            waiters: Mutex::new(BTreeMap::new()),
        }
    }

    fn try_reserve(&self) -> Option<u64> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        self.outbound.store(true, Ordering::Release);
        self.active_clipboard.store(false, Ordering::Release);
        self.active_token.store(token, Ordering::Release);
        Some(token)
    }

    fn granted(&self, token: u64, clipboard: bool) -> bool {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.outbound.store(false, Ordering::Release);
            self.active_clipboard.store(clipboard, Ordering::Release);
            self.active_token.store(token, Ordering::Release);
            true
        } else {
            false
        }
    }

    fn register_waiter(&self, token: u64) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(token, tx);
        rx
    }

    fn deliver_grant(&self, token: u64, granted: bool) {
        let waiter = self
            .waiters
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&token);
        if let Some(waiter) = waiter {
            let _ = waiter.send(granted);
        }
    }

    /// Frees `token` and returns whether it was still the active holder.
    fn release(&self, token: u64) -> bool {
        self.waiters
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&token);
        if self.active_token.load(Ordering::Acquire) == token {
            self.active_token.store(0, Ordering::Release);
            self.outbound.store(false, Ordering::Release);
            self.active_clipboard.store(false, Ordering::Release);
            self.busy.store(false, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Token of an inbound transfer; zero while the channel is free or when
    /// the local side reserved it for an outbound send.
    fn inbound_token(&self) -> u64 {
        if self.outbound.load(Ordering::Acquire) {
            0
        } else {
            self.active_token.load(Ordering::Acquire)
        }
    }

    fn inbound_clipboard(&self) -> bool {
        !self.outbound.load(Ordering::Acquire) && self.active_clipboard.load(Ordering::Acquire)
    }
}

/// Tracks cancellation tokens per transfer task. Entries survive briefly after
/// a transfer finishes so a `TransferCancel` that crossed with completion is
/// still acknowledged instead of being silently dropped.
#[derive(Debug, Default)]
pub struct Cancels {
    entries: Mutex<BTreeMap<String, CancelEntry>>,
}

#[derive(Debug)]
struct CancelEntry {
    token: CancelToken,
    active: bool,
    cancelled: bool,
    paused: bool,
    updated: Instant,
}

const CANCEL_TOMBSTONE: Duration = Duration::from_secs(120);

impl Cancels {
    pub fn new() -> Self {
        Self::default()
    }

    fn prune(entries: &mut BTreeMap<String, CancelEntry>) {
        let now = Instant::now();
        entries.retain(|_, entry| {
            entry.active || now.saturating_duration_since(entry.updated) < CANCEL_TOMBSTONE
        });
    }

    /// Registers a live transfer and returns a token that a later
    /// [`Cancels::cancel`] can flip. A cancel that arrived before registration
    /// is applied immediately.
    pub fn register(&self, task_id: &str) -> CancelToken {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Self::prune(&mut entries);
        let now = Instant::now();
        if let Some(entry) = entries.get_mut(task_id) {
            entry.active = true;
            entry.updated = now;
            if entry.cancelled {
                entry.token.cancel();
            } else if entry.paused {
                entry.token.pause();
            }
            return entry.token.clone();
        }
        let token = CancelToken::new();
        entries.insert(
            task_id.to_owned(),
            CancelEntry {
                token: token.clone(),
                active: true,
                cancelled: false,
                paused: false,
                updated: now,
            },
        );
        token
    }

    /// Cancels a transfer. Returns true when a live transfer was waiting.
    /// Unknown ids leave a short-lived tombstone so a manifest that is still
    /// in flight is cancelled as soon as it registers.
    pub fn cancel(&self, task_id: &str) -> bool {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Self::prune(&mut entries);
        let now = Instant::now();
        if let Some(entry) = entries.get_mut(task_id) {
            entry.cancelled = true;
            entry.updated = now;
            entry.token.cancel();
            return entry.active;
        }
        let token = CancelToken::new();
        token.cancel();
        entries.insert(
            task_id.to_owned(),
            CancelEntry {
                token,
                active: false,
                cancelled: true,
                paused: false,
                updated: now,
            },
        );
        false
    }

    /// Pauses a live transfer. A pause that arrives before the transfer
    /// registers is remembered and applied on registration.
    pub fn pause(&self, task_id: &str) -> bool {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Self::prune(&mut entries);
        let now = Instant::now();
        if let Some(entry) = entries.get_mut(task_id) {
            if entry.cancelled {
                return false;
            }
            entry.paused = true;
            entry.updated = now;
            entry.token.pause();
            return entry.active;
        }
        let token = CancelToken::new();
        token.pause();
        entries.insert(
            task_id.to_owned(),
            CancelEntry {
                token,
                active: false,
                cancelled: false,
                paused: true,
                updated: now,
            },
        );
        false
    }

    /// Resumes a paused transfer. Unknown ids are ignored.
    pub fn resume(&self, task_id: &str) -> bool {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        Self::prune(&mut entries);
        let now = Instant::now();
        if let Some(entry) = entries.get_mut(task_id) {
            if entry.cancelled {
                return false;
            }
            entry.paused = false;
            entry.updated = now;
            entry.token.resume();
            return entry.active;
        }
        false
    }

    /// Clears the active flag after a transfer loop exits. Uncancelled entries
    /// are removed immediately; cancelled ones linger as tombstones.
    pub fn finish(&self, task_id: &str) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let now = Instant::now();
        if let Some(entry) = entries.get_mut(task_id) {
            entry.active = false;
            entry.updated = now;
            if !entry.cancelled && !entry.paused {
                entries.remove(task_id);
            }
        }
    }
}

impl Shared {
    pub fn local_id(&self) -> &str {
        &self.config.device_id
    }

    pub fn store_device(&self, record: DeviceRecord) -> Result<(), String> {
        let store = self
            .store
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        store
            .upsert_device(&record)
            .map_err(|error| error.to_string())
    }

    pub fn feature_set(&self) -> FeatureSet {
        FeatureSet {
            clipboard: self.features.clipboard(),
            input: self.features.input(),
            revision: self.features.revision(),
        }
    }

    pub fn pair_settings(&self) -> PairSettings {
        self.settings
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Updates the pair-level edge/speed/resistance knobs, persists them and
    /// mirrors them to every online peer.
    pub fn set_pair_settings(
        &self,
        edge: EdgeSide,
        pointer_speed: f64,
        edge_resistance: u8,
    ) -> Result<PairSettings, String> {
        if !pointer_speed.is_finite()
            || !(cd_proto::MIN_POINTER_SPEED..=cd_proto::MAX_POINTER_SPEED).contains(&pointer_speed)
        {
            return Err(format!(
                "指针速度必须在 {} 到 {} 之间",
                cd_proto::MIN_POINTER_SPEED,
                cd_proto::MAX_POINTER_SPEED
            ));
        }
        if edge_resistance > cd_proto::MAX_EDGE_RESISTANCE {
            return Err(format!(
                "边缘阻力不能超过 {}",
                cd_proto::MAX_EDGE_RESISTANCE
            ));
        }
        let snapshot = {
            let mut settings = self
                .settings
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            settings.edge = edge;
            settings.pointer_speed = pointer_speed;
            settings.edge_resistance = edge_resistance;
            settings.revision = settings.revision.saturating_add(1);
            settings.clone()
        };
        self.status.set_pair_settings(&snapshot);
        self.persist_config(|config| {
            config.edge = edge;
            config.pointer_speed = pointer_speed;
            config.edge_resistance = edge_resistance;
        })?;
        self.registry
            .send_all(Outbound::Control(ControlMessage::PairSettings(
                snapshot.clone(),
            )));
        Ok(snapshot)
    }

    pub fn conflict_policy(&self) -> ConflictPolicy {
        *self
            .file_conflict
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Pauses or resumes a live transfer on both the data loop and the status
    /// board. Returns false when no live token exists.
    pub fn set_transfer_paused(&self, task_id: &str, paused: bool) -> bool {
        let active = if paused {
            self.cancels.pause(task_id)
        } else {
            self.cancels.resume(task_id)
        };
        let record = self
            .store
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .transfer(task_id)
            .ok()
            .flatten();
        if let Some(mut record) = record {
            let state = if paused {
                cd_files::TransferState::Paused
            } else {
                cd_files::TransferState::Running
            };
            if record.state != state {
                record.state = state;
                record.updated_at = now_ms();
                self.save_transfer(&record);
                self.status.transfer_finished(&record);
            }
        }
        active
    }

    /// Changes what happens when an incoming root already exists. `ask`
    /// pauses the transfer until the console picks a policy.
    pub fn set_conflict_policy(&self, policy: ConflictPolicy) -> Result<(), String> {
        if matches!(policy, ConflictPolicy::Merge) {
            return Err("同名文件策略支持 rename / overwrite / skip / ask".to_owned());
        }
        {
            let mut slot = self
                .file_conflict
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            *slot = policy;
        }
        self.status.set_file_conflict(conflict_label(policy));
        self.persist_config(|config| config.file_conflict = policy)
    }

    pub fn layout_gap(&self) -> f64 {
        *self
            .layout_gap_mm
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Places the peer on the requested side of the local screen, validates the
    /// millimetre layout, switches the handoff edge, persists the gap and
    /// mirrors the layout to peers over C0.
    pub fn apply_layout(&self, edge: EdgeSide, gap_mm: f64) -> Result<serde_json::Value, String> {
        if !gap_mm.is_finite() || !(0.0..=MAX_GAP_MM).contains(&gap_mm) {
            return Err(format!("屏幕间隙必须在 0 到 {MAX_GAP_MM} mm 之间"));
        }
        let snapshot = self.status.snapshot();
        let peer = snapshot
            .peers
            .iter()
            .find(|peer| peer.online && !peer.monitors.is_empty())
            .ok_or_else(|| "没有在线对端或对端未上报屏幕".to_owned())?;
        let mut peer_monitor = peer.monitors[0].clone();
        // X11 output ids can repeat across machines; namespace the peer's screen.
        peer_monitor.id = format!("{}::{}", peer.id, peer_monitor.id);
        let local = self.monitor.clone();
        let local_id = local.id.clone();
        let peer_id = peer_monitor.id.clone();
        let mut layout = Layout::new(vec![local.clone(), peer_monitor.clone()]);
        let (left, right) = match edge {
            EdgeSide::Left => (peer_id.clone(), local_id.clone()),
            EdgeSide::Right => (local_id.clone(), peer_id.clone()),
        };
        layout.place_right_of(&right, &left, gap_mm);
        layout.auto_center_vertical(&right, &left);
        let issues = layout.validate();
        if !issues.is_empty() {
            return Err(format!("布局无效: {issues:?}"));
        }
        let pairs = layout.pair_edges();
        let local_side = if edge == EdgeSide::Left {
            Side::Left
        } else {
            Side::Right
        };
        let pair = pairs
            .iter()
            .find(|pair| pair.segment_of(&local_id, local_side).is_some())
            .ok_or_else(|| "间隙过大或高度重叠不足，未形成共享边".to_owned())?;
        let local_segment = pair
            .segment_of(&local_id, local_side)
            .ok_or_else(|| "共享边缺少本机段".to_owned())?;
        let comparison = cd_geometry::compare_devices(&local, &peer_monitor);

        {
            let mut gap = self
                .layout_gap_mm
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            *gap = gap_mm;
        }
        self.status.set_layout_gap(gap_mm);
        let current = self.pair_settings();
        self.set_pair_settings(edge, current.pointer_speed, current.edge_resistance)?;
        self.registry
            .send_all(Outbound::Control(ControlMessage::LayoutSet(LayoutSet {
                revision: layout.revision,
                screens: layout.screens.clone(),
                edge_pairs: pairs.clone(),
            })));

        Ok(serde_json::json!({
            "side": edge,
            "gap_mm": gap_mm,
            "coverage": pair.coverage_of(local_segment),
            "overlap_mm": pair.overlap_mm(),
            "edge_length_mm": local_segment.len(),
            "speed_multiplier": comparison.speed_multiplier_x,
            "screens": layout.screens,
            "edge_pairs": pairs,
        }))
    }

    /// Persists a copy of the startup config with the current runtime state
    /// overlaid, so feature toggles and pair settings cannot clobber each
    /// other on disk.
    fn persist_config(&self, apply: impl FnOnce(&mut Config)) -> Result<(), String> {
        let mut config = (*self.config).clone();
        config.clipboard = self.features.clipboard();
        config.input_pump = self.features.input();
        let settings = self.pair_settings();
        config.edge = settings.edge;
        config.pointer_speed = settings.pointer_speed;
        config.edge_resistance = settings.edge_resistance;
        config.file_conflict = self.conflict_policy();
        config.layout_gap_mm = self.layout_gap();
        apply(&mut config);
        config.save(&self.paths.config)
    }

    pub fn set_features(&self, clipboard: bool, input: bool) -> Result<(), String> {
        let clipboard_changed = clipboard != self.features.clipboard();
        let input_changed = input != self.features.input();
        self.features.clipboard.store(clipboard, Ordering::Relaxed);
        self.features.input.store(input, Ordering::Relaxed);
        if clipboard_changed {
            if clipboard {
                self.start_clipboard_worker();
            } else {
                self.stop_clipboard_worker();
            }
        }
        if input_changed {
            if input {
                self.start_input_pump();
            } else {
                self.stop_input_pump();
            }
        }
        self.features.next_revision();
        self.status.set_features(clipboard, input);
        self.persist_config(|config| {
            config.clipboard = clipboard;
            config.input_pump = input;
        })?;
        self.registry
            .send_all(Outbound::Control(ControlMessage::FeatureSet(
                self.feature_set(),
            )));
        Ok(())
    }

    pub fn start_clipboard_worker(&self) {
        let mut handle = self
            .clipboard_handle
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if handle.is_some() {
            return;
        }
        let worker = clipboard::spawn(
            self.config.device_id.clone(),
            Arc::clone(&self.guard),
            self.clipboard_out.clone(),
            self.config.excluded_apps.clone(),
            self.monitor.clone(),
            Arc::clone(&self.settings),
        );
        let sender = worker.command_sender();
        *handle = Some(worker);
        drop(handle);
        *self
            .clipboard
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(sender);
    }

    pub fn stop_clipboard_worker(&self) {
        let sender = self
            .clipboard
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        drop(sender);
        let worker = self
            .clipboard_handle
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        drop(worker);
    }

    pub fn start_input_pump(&self) {
        let mut slot = self
            .input
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if slot.is_some() {
            return;
        }
        *slot = input::spawn(
            self.config.device_id.clone(),
            self.monitor.clone(),
            self.registry.clone(),
            Arc::clone(&self.ownership),
            Arc::clone(&self.status),
            Arc::clone(&self.settings),
        );
    }

    pub fn stop_input_pump(&self) {
        let handle = self
            .input
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        drop(handle);
    }

    /// Snapshot of everything support needs to diagnose a daemon: runtime
    /// status, whitelist metadata (fingerprints only), transfer records,
    /// block-cache usage and non-sensitive settings.
    pub fn diagnostics(&self) -> serde_json::Value {
        let status = self.status.snapshot();
        let devices = {
            let store = self
                .store
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            store.devices().unwrap_or_default()
        };
        let device_views = devices
            .into_iter()
            .map(|device| {
                serde_json::json!({
                    "id": device.id,
                    "name": device.name,
                    "fingerprint": hex_bytes(&device.cert_fingerprint),
                    "capabilities": device.capabilities,
                    "paired_at": device.paired_at,
                    "last_seen": device.last_seen,
                })
            })
            .collect::<Vec<_>>();
        let (blocks, cache_bytes) = cache_usage(&self.paths.blocks);
        let settings = self.pair_settings();
        serde_json::json!({
            "app": "crossdev",
            "version": env!("CARGO_PKG_VERSION"),
            "generated_at_ms": now_ms(),
            "status": status,
            "devices": device_views,
            "cache": {
                "blocks": blocks,
                "bytes": cache_bytes,
                "limit_bytes": self.config.cache_limit_bytes,
                "unlimited": self.config.cache_limit_bytes == 0,
            },
            "config": {
                "pair_mode": self.config.pair_mode,
                "clipboard": self.features.clipboard(),
                "input_pump": self.features.input(),
                "edge": settings.edge,
                "pointer_speed": settings.pointer_speed,
                "edge_resistance": settings.edge_resistance,
                "cache_limit_bytes": self.config.cache_limit_bytes,
                "discovery": self.config.discovery,
                "file_conflict": self.conflict_policy(),
                "layout_gap_mm": self.layout_gap(),
                "excluded_apps": self.config.excluded_apps,
            },
        })
    }

    fn save_transfer(&self, record: &TransferRecord) {
        let store = self
            .store
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Err(error) = store.save_transfer(record) {
            self.status.note_error(format!("保存传输任务失败: {error}"));
        }
    }
}

pub struct RunningDaemon {
    pub shared: Arc<Shared>,
    pub quic_addr: SocketAddr,
    pub http_addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl RunningDaemon {
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        self.shared.stop_clipboard_worker();
        self.shared.stop_input_pump();
        for task in self.tasks.drain(..) {
            let _ = tokio::time::timeout(Duration::from_secs(3), task).await;
        }
    }
}

pub async fn start(paths: DaemonPaths, config: Config) -> Result<RunningDaemon, String> {
    config.validate()?;
    paths.ensure()?;
    config.save(&paths.config)?;

    let identity = Arc::new(
        Identity::load_or_generate(&paths.identity, SERVER_NAME)
            .map_err(|error| error.to_string())?,
    );
    let store = Store::open(&paths.database).map_err(|error| error.to_string())?;
    let cache_limit = if config.cache_limit_bytes == 0 {
        u64::MAX
    } else {
        config.cache_limit_bytes
    };
    let cache = BlockCache::open_with_limit(&paths.blocks, cache_limit)
        .map_err(|error| error.to_string())?;
    let monitor = detect_monitor();
    let quic_bind = config
        .bind
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid QUIC bind: {error}"))?;
    let server = QuicTransport::server(quic_bind, &identity).map_err(|error| error.to_string())?;
    let quic_addr = server.local_addr().map_err(|error| error.to_string())?;

    let http_bind = config
        .http_bind
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid HTTP bind: {error}"))?;
    let listener = tokio::net::TcpListener::bind(http_bind)
        .await
        .map_err(|error| format!("HTTP bind {http_bind}: {error}"))?;
    let http_addr = listener.local_addr().map_err(|error| error.to_string())?;

    let local = LocalInfo {
        device_id: config.device_id.clone(),
        device_name: config.device_name.clone(),
        platform: config.platform.clone(),
        fingerprint: identity.fingerprint_hex(),
        protocol: ProtocolVersion::CURRENT.to_string(),
        data_dir: paths.root.display().to_string(),
        quic_addr: quic_addr.to_string(),
        http_addr: http_addr.to_string(),
        started_at_ms: now_ms(),
        pair_mode: match config.pair_mode {
            PairMode::Auto => "auto".to_owned(),
            PairMode::Deny => "deny".to_owned(),
            PairMode::Confirm => "confirm".to_owned(),
        },
        clipboard: config.clipboard,
        input_pump: config.input_pump,
        edge: edge_label(config.edge).to_owned(),
        pointer_speed: config.pointer_speed,
        edge_resistance: config.edge_resistance,
        file_conflict: conflict_label(config.file_conflict).to_owned(),
        layout_gap_mm: config.layout_gap_mm,
        monitors: vec![monitor.clone()],
    };
    let status = Arc::new(StatusBoard::new(local));
    let guard = Arc::new(Mutex::new(LoopGuard::new()));
    let ownership = Arc::new(Mutex::new(OwnershipMachine::new()));
    let registry = PeerRegistry::new();
    let input_slot = Arc::new(Mutex::new(None));
    let (clip_tx, clip_rx) = tokio::sync::mpsc::unbounded_channel::<Snapshot>();
    let clipboard_enabled = config.clipboard;
    let clipboard_system = config.clipboard_system;
    let input_enabled = config.input_pump;
    let initial_edge = config.edge;
    let initial_pointer_speed = config.pointer_speed;
    let initial_edge_resistance = config.edge_resistance;
    let initial_conflict = config.file_conflict;
    let initial_gap = config.layout_gap_mm;
    let (connect_tx, connect_rx) = mpsc::unbounded_channel();

    let shared = Arc::new(Shared {
        config: Arc::new(config),
        paths: Arc::new(paths),
        identity: Arc::clone(&identity),
        store: Arc::new(Mutex::new(store)),
        status: Arc::clone(&status),
        registry: registry.clone(),
        guard,
        ownership,
        input: Arc::clone(&input_slot),
        clipboard: Arc::new(Mutex::new(None)),
        clipboard_handle: Arc::new(Mutex::new(None)),
        clipboard_out: clip_tx,
        clipboard_system,
        features: Arc::new(Features::new(clipboard_enabled, input_enabled)),
        settings: Arc::new(Mutex::new(PairSettings {
            edge: initial_edge,
            pointer_speed: initial_pointer_speed,
            edge_resistance: initial_edge_resistance,
            revision: 1,
        })),
        file_conflict: Arc::new(Mutex::new(initial_conflict)),
        layout_gap_mm: Arc::new(Mutex::new(initial_gap)),
        cache: Arc::new(cache),
        monitor: monitor.clone(),
        remote_input: Arc::new(AtomicBool::new(false)),
        cancels: Cancels::new(),
    });

    if clipboard_enabled {
        shared.start_clipboard_worker();
    }
    if input_enabled {
        shared.start_input_pump();
    }

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut tasks = Vec::new();

    let web_state = super::web::WebState {
        status: Arc::clone(&status),
        store: Arc::clone(&shared.store),
        input: Arc::clone(&shared.input),
        shared: Arc::clone(&shared),
        connector: connect_tx.clone(),
    };
    tasks.push(tokio::spawn(super::web::serve(
        listener,
        web_state,
        shutdown_rx.clone(),
    )));

    {
        let clip_shared = Arc::clone(&shared);
        let mut clip_rx = clip_rx;
        tasks.push(tokio::spawn(async move {
            while let Some(snapshot) = clip_rx.recv().await {
                publish_local_clipboard(&clip_shared, snapshot);
            }
        }));
    }

    if let Some(peer) = shared.config.peer.clone() {
        match peer.parse::<SocketAddr>() {
            Ok(addr) => {
                println!("  主动对端 {addr}（断线自动退避重连）");
                let _ = connect_tx.send(addr);
            }
            Err(error) => {
                eprintln!("忽略无效对端地址 `{peer}`: {error}");
            }
        }
    }
    tasks.push(tokio::spawn(connector_loop(
        Arc::clone(&shared),
        connect_rx,
        shutdown_rx.clone(),
    )));
    if shared.config.discovery {
        tasks.push(tokio::spawn(super::discovery::run(
            Arc::clone(&shared),
            shutdown_rx.clone(),
        )));
    }

    tasks.push(tokio::spawn(accept_loop(
        server,
        Arc::clone(&shared),
        shutdown_rx,
    )));

    println!(
        "crossdev 已启动 · {} ({})",
        status.local().device_name,
        status.local().device_id
    );
    println!("  QUIC  {quic_addr}");
    println!("  HTTP  http://{http_addr}");
    println!(
        "  身份  {} · 已配对设备 {}",
        short(&status.local().fingerprint),
        paired_count(&shared)
    );

    Ok(RunningDaemon {
        shared,
        quic_addr,
        http_addr,
        shutdown: shutdown_tx,
        tasks,
    })
}

async fn accept_loop(
    server: QuicTransport,
    shared: Arc<Shared>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut sessions = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    break;
                }
            }
            accepted = server.accept() => {
                match accepted {
                    Ok(link) => {
                        let session_shared = Arc::clone(&shared);
                        sessions.spawn(async move {
                            session(session_shared, link).await;
                        });
                    }
                    Err(error) => {
                        shared.status.note_error(format!("QUIC 接受失败: {error}"));
                    }
                }
            }
            Some(result) = sessions.join_next(), if !sessions.is_empty() => {
                if let Err(error) = result {
                    shared.status.note_error(format!("会话任务异常: {error}"));
                }
            }
        }
    }
    server.close();
    sessions.abort_all();
}

async fn session(shared: Arc<Shared>, link: Link) {
    let link = Arc::new(link);
    if let Err(error) = server_handshake(&shared, &link).await {
        shared.status.note_error(error);
    }
}

async fn server_handshake(shared: &Arc<Shared>, link: &Arc<Link>) -> Result<(), String> {
    if let Err(error) = link
        .send_control(&ControlMessage::Hello(local_hello(shared)))
        .await
    {
        return Err(format!("发送 Hello 失败: {error}"));
    }
    let hello = recv_hello(link).await?;
    let peer_fingerprint = validate_hello(shared, link, &hello).await?;

    let known = lookup_device(shared, &hello.device_id);
    let mut paired = known
        .as_ref()
        .is_some_and(|record| record.cert_fingerprint == peer_fingerprint);
    if known.is_some() && !paired {
        let error = ProtocolError::new(ErrorCode::CertificateMismatch);
        let _ = link.send_control(&ControlMessage::Error(error)).await;
        return Err(format!("{} 的设备指纹与白名单不一致", hello.device_name));
    }

    let status_token = shared.status.peer_connecting(
        &hello.device_id,
        &hello.device_name,
        &hello.platform,
        &link.remote_address().to_string(),
        paired,
        hello.monitors.clone(),
    );

    if !paired {
        match shared.config.pair_mode {
            PairMode::Auto => match pair(link, &shared.identity, false).await {
                Ok(outcome) => {
                    println!(
                        "配对 {} · SAS {} · 已自动接受（trust-on-first-use）",
                        hello.device_name, outcome.code
                    );
                    remember_peer(shared, &hello, outcome.peer_certificate)?;
                    paired = true;
                }
                Err(error) => return Err(format!("配对失败: {error}")),
            },
            PairMode::Confirm => {
                let preview = tokio::time::timeout(
                    PAIR_APPROVAL_TIMEOUT,
                    pair_begin_responder(link, &shared.identity),
                )
                .await
                .map_err(|_| "等待 PairBegin 超时".to_owned())?
                .map_err(|error| format!("配对失败: {error}"))?;
                let accepted = approve_pair(shared, link, &hello, &preview, "incoming").await?;
                let outcome = tokio::time::timeout(
                    PAIR_APPROVAL_TIMEOUT,
                    pair_finish_responder(link, preview, accepted),
                )
                .await
                .map_err(|_| "等待对端确认超时".to_owned())?
                .map_err(|error| format!("配对失败: {error}"))?;
                if accepted {
                    remember_peer(shared, &hello, outcome.peer_certificate)?;
                    paired = true;
                }
            }
            PairMode::Deny => {
                let error = ProtocolError::new(ErrorCode::PairRequired);
                let _ = link.send_control(&ControlMessage::Error(error)).await;
                return Err(format!(
                    "{} 未配对；daemon 当前为 deny 模式",
                    hello.device_name
                ));
            }
        }
    }
    if !paired {
        return Err("配对未完成".to_owned());
    }
    run_authenticated(Arc::clone(shared), Arc::clone(link), hello, status_token).await;
    Ok(())
}

async fn client_handshake(shared: &Arc<Shared>, addr: SocketAddr) -> Result<(), String> {
    let mut transport = QuicTransport::client().map_err(|error| error.to_string())?;
    let link = Arc::new(
        transport
            .connect_unverified(addr, SERVER_NAME, &shared.identity)
            .await
            .map_err(|error| format!("连接 {addr} 失败: {error}"))?,
    );
    link.send_control(&ControlMessage::Hello(local_hello(shared)))
        .await
        .map_err(|error| format!("发送 Hello 失败: {error}"))?;
    let hello = recv_hello(&link).await?;
    let peer_fingerprint = validate_hello(shared, &link, &hello).await?;

    let known = lookup_device(shared, &hello.device_id);
    let mut paired = known
        .as_ref()
        .is_some_and(|record| record.cert_fingerprint == peer_fingerprint);
    if known.is_some() && !paired {
        let error = ProtocolError::new(ErrorCode::CertificateMismatch);
        let _ = link.send_control(&ControlMessage::Error(error)).await;
        return Err(format!("{} 的证书与白名单不一致", hello.device_name));
    }

    let status_token = shared.status.peer_connecting(
        &hello.device_id,
        &hello.device_name,
        &hello.platform,
        &link.remote_address().to_string(),
        paired,
        hello.monitors.clone(),
    );

    if !paired {
        match shared.config.pair_mode {
            PairMode::Auto => match pair(&link, &shared.identity, true).await {
                Ok(outcome) => {
                    println!(
                        "配对 {} · SAS {} · 已自动接受（trust-on-first-use）",
                        hello.device_name, outcome.code
                    );
                    remember_peer(shared, &hello, outcome.peer_certificate)?;
                    paired = true;
                }
                Err(error) => return Err(format!("配对失败: {error}")),
            },
            PairMode::Confirm => {
                let preview = tokio::time::timeout(
                    PAIR_APPROVAL_TIMEOUT,
                    pair_begin_initiator(&link, &shared.identity),
                )
                .await
                .map_err(|_| "等待对端配对确认超时".to_owned())?
                .map_err(|error| format!("配对失败: {error}"))?;
                let accepted = approve_pair(shared, &link, &hello, &preview, "outgoing").await?;
                let outcome = pair_finish_initiator(&link, preview, accepted)
                    .await
                    .map_err(|error| format!("配对失败: {error}"))?;
                if accepted {
                    remember_peer(shared, &hello, outcome.peer_certificate)?;
                    paired = true;
                }
            }
            PairMode::Deny => {
                return Err(format!(
                    "{} 未配对；daemon 当前为 deny 模式",
                    hello.device_name
                ));
            }
        }
    }
    if !paired {
        return Err("配对未完成".to_owned());
    }
    run_authenticated(Arc::clone(shared), Arc::clone(&link), hello, status_token).await;
    Ok(())
}

fn local_hello(shared: &Shared) -> Hello {
    Hello {
        protocol: ProtocolVersion::CURRENT,
        device_id: shared.config.device_id.clone(),
        device_name: shared.config.device_name.clone(),
        platform: shared.config.platform.clone(),
        capabilities: Capabilities::INPUT
            | Capabilities::CLIPBOARD_TEXT
            | Capabilities::FILE_TRANSFER
            | Capabilities::DIRECTORY_TRANSFER
            | Capabilities::RESUME_TRANSFER
            | Capabilities::SAS_PAIRING,
        monitors: vec![shared.monitor.clone()],
    }
}

async fn recv_hello(link: &Link) -> Result<Hello, String> {
    match tokio::time::timeout(HELLO_TIMEOUT, link.recv_control()).await {
        Ok(Ok(ControlMessage::Hello(hello))) => Ok(hello),
        Ok(Ok(_)) => Err("对端首条消息不是 Hello".to_owned()),
        Ok(Err(error)) => Err(format!("读取 Hello 失败: {error}")),
        Err(_) => Err("等待 Hello 超时".to_owned()),
    }
}

async fn validate_hello(shared: &Shared, link: &Link, hello: &Hello) -> Result<Vec<u8>, String> {
    if hello.device_id == shared.config.device_id {
        return Err("对端设备 id 与本机相同，已拒绝连接".to_owned());
    }
    if hello.protocol.negotiate(ProtocolVersion::CURRENT).is_err() {
        let error = ProtocolError::new(ErrorCode::ProtocolVersion);
        let _ = link.send_control(&ControlMessage::Error(error)).await;
        return Err("协议主版本不兼容".to_owned());
    }
    let Some(peer_cert) = link.peer_certificate_der() else {
        return Err("对端未提供证书".to_owned());
    };
    Ok(sha256_bytes(&peer_cert))
}

fn lookup_device(shared: &Shared, id: &str) -> Option<DeviceRecord> {
    let store = shared
        .store
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    store.device(id).ok().flatten()
}

fn remember_peer(shared: &Shared, hello: &Hello, certificate: Vec<u8>) -> Result<(), String> {
    let now = now_ms();
    shared.store_device(DeviceRecord {
        id: hello.device_id.clone(),
        name: hello.device_name.clone(),
        cert_fingerprint: sha256_bytes(&certificate),
        public_key: certificate,
        capabilities: hello.capabilities.0,
        paired_at: now,
        last_seen: Some(now),
    })
}

async fn approve_pair(
    shared: &Shared,
    link: &Link,
    hello: &Hello,
    preview: &PairPreview,
    direction: &str,
) -> Result<bool, String> {
    let now = now_ms();
    let view = PendingPairView {
        id: String::new(),
        direction: direction.to_owned(),
        device_id: hello.device_id.clone(),
        device_name: hello.device_name.clone(),
        platform: hello.platform.clone(),
        address: link.remote_address().to_string(),
        code: preview.code.clone(),
        fingerprint: preview.peer_fingerprint.clone(),
        requested_at_ms: now,
        expires_at_ms: now + PAIR_APPROVAL_TIMEOUT.as_millis() as i64,
        monitors: hello.monitors.clone(),
    };
    let (view, receiver) = shared.status.request_pair(view);
    println!(
        "配对请求 {} · SAS {} · {direction} · 等待控制台批准",
        hello.device_name, preview.code
    );
    match tokio::time::timeout(PAIR_APPROVAL_TIMEOUT, receiver).await {
        Ok(Ok(accepted)) => {
            println!(
                "配对{}: {} · SAS {}",
                if accepted { "已批准" } else { "被拒绝" },
                hello.device_name,
                preview.code
            );
            Ok(accepted)
        }
        Ok(Err(_)) => Err("配对请求已取消".to_owned()),
        Err(_) => {
            shared.status.resolve_pair(&view.id, false);
            Err("配对请求超时，已自动拒绝".to_owned())
        }
    }
}

async fn connector_loop(
    shared: Arc<Shared>,
    mut requests: mpsc::UnboundedReceiver<SocketAddr>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut seen = BTreeSet::new();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    break;
                }
            }
            request = requests.recv() => {
                let Some(addr) = request else { break };
                if seen.insert(addr) {
                    tokio::spawn(peer_loop(
                        Arc::clone(&shared),
                        addr,
                        shutdown.clone(),
                    ));
                }
            }
        }
    }
}

async fn peer_loop(shared: Arc<Shared>, addr: SocketAddr, mut shutdown: watch::Receiver<bool>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    break;
                }
            }
            result = client_handshake(&shared, addr) => {
                match result {
                    Ok(()) => backoff = Duration::from_secs(1),
                    Err(error) => {
                        eprintln!("对端 {addr} 连接结束: {error}");
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }
        }
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    break;
                }
            }
            _ = tokio::time::sleep(backoff) => {}
        }
    }
}

async fn run_authenticated(shared: Arc<Shared>, link: Arc<Link>, hello: Hello, status_token: u64) {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let session_token = shared.registry.register(&hello.device_id, out_tx);
    shared.status.peer_online(
        &hello.device_id,
        status_token,
        link.rtt().as_secs_f64() * 1000.0,
    );
    shared.registry.send_to(
        &hello.device_id,
        Outbound::Control(ControlMessage::FeatureSet(shared.feature_set())),
    );
    shared.registry.send_to(
        &hello.device_id,
        Outbound::Control(ControlMessage::PairSettings(shared.pair_settings())),
    );

    let file_channel = Arc::new(FileChannel::new());
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let control_task = tokio::spawn(control_loop(Arc::clone(&link), control_tx));
    let (data_tx, data_rx) = mpsc::unbounded_channel();
    let mut data_task = tokio::spawn(data_loop(
        Arc::clone(&shared),
        Arc::clone(&link),
        hello.device_id.clone(),
        data_rx,
        Arc::clone(&file_channel),
    ));
    let input_task = tokio::spawn(datagram_loop(
        Arc::clone(&shared),
        Arc::clone(&link),
        hello.device_id.clone(),
    ));

    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut heartbeat_seq = 0_u64;
    let mut last_seen = Instant::now();
    let peer_id = hello.device_id.clone();

    loop {
        tokio::select! {
            event = control_rx.recv() => {
                match event {
                    Some(PeerControl::Message(message)) => {
                        last_seen = Instant::now();
                        handle_control(&shared, &file_channel, &peer_id, message);
                    }
                    Some(PeerControl::Closed(reason)) => {
                        shared.status.note_error(format!("控制通道关闭: {reason}"));
                        break;
                    }
                    None => break,
                }
            }
            outbound = out_rx.recv() => {
                match outbound {
                    Some(Outbound::SendFile { path, clipboard }) => {
                        let _ = data_tx.send(FileCommand { path, clipboard });
                    }
                    Some(Outbound::Clipboard(snapshot))
                        if matches!(&snapshot.content, Content::Files { .. }) =>
                    {
                        // A file-list clipboard is materialised through the
                        // file channel; paths themselves would be meaningless
                        // on the peer.
                        if let Content::Files { entries } = &snapshot.content {
                            for entry in entries {
                                let _ = data_tx.send(FileCommand {
                                    path: std::path::PathBuf::from(&entry.path),
                                    clipboard: true,
                                });
                            }
                        }
                    }
                    Some(Outbound::Disconnect) => {
                        println!("已撤销设备白名单，关闭当前会话");
                        break;
                    }
                    Some(outbound) => {
                        if let Err(error) = send_outbound(&shared, &link, outbound).await {
                            shared.status.note_error(error);
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = heartbeat.tick() => {
                if last_seen.elapsed() >= HEARTBEAT_TIMEOUT {
                    shared.status.note_error(format!("{} 心跳超时", hello.device_name));
                    break;
                }
                heartbeat_seq = heartbeat_seq.saturating_add(1);
                let beat = ControlMessage::Heartbeat(Heartbeat {
                    seq: heartbeat_seq,
                    ts_us: now_us(),
                });
                if let Err(error) = link.send_control(&beat).await {
                    shared.status.note_error(format!("发送心跳失败: {error}"));
                    break;
                }
            }
            _ = link.closed() => {
                break;
            }
        }
    }

    control_task.abort();
    input_task.abort();
    // Give an in-flight file transfer a moment to persist `Paused` and its
    // received-byte watermark before the link handle disappears.
    if tokio::time::timeout(Duration::from_secs(2), &mut data_task)
        .await
        .is_err()
    {
        data_task.abort();
    }
    shared.registry.unregister(&peer_id, session_token);
    shared.remote_input.store(false, Ordering::Relaxed);
    shared
        .status
        .peer_offline(&peer_id, status_token, "连接已断开");
    {
        let mut machine = lock_ownership(&shared.ownership);
        let actions = machine.handle(OwnershipEvent::PeerLost {
            at_ms: now_ms() as u64,
        });
        drop(machine);
        if !actions.is_empty() {
            shared.status.set_ownership("local_owned", None);
        }
    }
    if let Some(input) = shared
        .input
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .as_ref()
    {
        input.release();
    }
    if link.close_reason().is_none() {
        link.close(0, b"bye");
    }
}

enum PeerControl {
    Message(ControlMessage),
    Closed(String),
}

async fn control_loop(link: Arc<Link>, tx: mpsc::UnboundedSender<PeerControl>) {
    loop {
        match link.recv_control().await {
            Ok(message) => {
                if tx.send(PeerControl::Message(message)).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = tx.send(PeerControl::Closed(error.to_string()));
                break;
            }
        }
    }
}

async fn datagram_loop(shared: Arc<Shared>, link: Arc<Link>, _peer_id: String) {
    while let Ok(event) = link.recv_input().await {
        if !shared.remote_input.load(Ordering::Relaxed) || !shared.features.input() {
            continue;
        }
        if let Some(input) = shared
            .input
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
        {
            input.inject(event);
        }
    }
}

fn handle_control(
    shared: &Shared,
    file_channel: &FileChannel,
    peer_id: &str,
    message: ControlMessage,
) {
    match message {
        ControlMessage::Heartbeat(Heartbeat { .. }) => shared.status.peer_seen(peer_id, 1),
        ControlMessage::InputOwner(InputOwner { owner, phase, .. }) => {
            if owner == *shared.local_id() {
                shared.remote_input.store(false, Ordering::Relaxed);
                shared.status.set_ownership("local_owned", None);
            } else {
                shared.remote_input.store(true, Ordering::Relaxed);
                let label = match phase {
                    OwnerPhase::Remote => "remote_owned",
                    _ => "handoff_out",
                };
                shared.status.set_ownership(label, Some(peer_id));
            }
        }
        ControlMessage::ClipboardAnnounce(_) => {
            // The blob itself is handled on C2; the announce is advisory.
        }
        ControlMessage::TransferOffer(_) => {}
        ControlMessage::LayoutSet(layout) => {
            shared.status.peer_layout(peer_id, layout.revision);
        }
        ControlMessage::FileSendRequest(request) => {
            let granted = file_channel.granted(request.token, request.clipboard);
            let queued = shared.registry.send_to(
                peer_id,
                Outbound::Control(ControlMessage::FileSendGrant(FileSendGrant {
                    token: request.token,
                    granted,
                })),
            );
            if granted && !queued {
                file_channel.release(request.token);
            }
        }
        ControlMessage::FileSendGrant(grant) => {
            file_channel.deliver_grant(grant.token, grant.granted);
        }
        ControlMessage::FileSendRelease(release) => {
            file_channel.release(release.token);
        }
        ControlMessage::FeatureSet(features) => {
            shared.status.peer_features(
                peer_id,
                features.clipboard,
                features.input,
                features.revision,
            );
        }
        ControlMessage::PairSettings(settings) => {
            shared.status.peer_settings(peer_id, settings.normalized());
        }
        ControlMessage::TransferCancel(cancel) => {
            if shared.cancels.cancel(&cancel.task_id) {
                println!("对端取消传输 {}", cancel.task_id);
            }
        }
        ControlMessage::TransferPause(pause) => {
            if shared.set_transfer_paused(&pause.task_id, pause.paused) {
                println!(
                    "对端{}传输 {}",
                    if pause.paused { "暂停" } else { "继续" },
                    pause.task_id
                );
            }
        }
        ControlMessage::Error(error) => {
            shared
                .status
                .note_error(format!("对端错误: {}", error.message));
        }
        ControlMessage::Hello(_)
        | ControlMessage::PairBegin(_)
        | ControlMessage::PairConfirm(_) => {}
    }
}

async fn send_outbound(shared: &Shared, link: &Link, outbound: Outbound) -> Result<(), String> {
    match outbound {
        Outbound::Clipboard(snapshot) => {
            if !shared.features.clipboard() {
                return Ok(());
            }
            link.send_control(&ControlMessage::ClipboardAnnounce(snapshot.announce()))
                .await
                .map_err(|error| format!("发送剪贴板公告失败: {error}"))?;
            send_clipboard(link, &snapshot)
                .await
                .map_err(|error| format!("发送剪贴板失败: {error}"))?;
            shared.status.bytes(0, snapshot.size);
            shared.status.clipboard_local(&snapshot);
            Ok(())
        }
        Outbound::Input(event) => link
            .send_input(&event)
            .map_err(|error| format!("发送 C1 失败: {error}")),
        Outbound::Control(message) => link
            .send_control(&message)
            .await
            .map_err(|error| format!("发送 C0 失败: {error}")),
        Outbound::SendFile { .. } => Err("文件发送应由数据任务处理".to_owned()),
        Outbound::Disconnect => Ok(()),
    }
}

fn publish_local_clipboard(shared: &Shared, snapshot: Snapshot) {
    if !shared.features.clipboard() {
        return;
    }
    let now = now_ms() as u64;
    let mut guard = shared
        .guard
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if !guard.should_publish_local(&snapshot.hash, now) {
        return;
    }
    // Record our own origin/seq so an echo from the peer is classified as a
    // duplicate instead of being re-applied locally.
    guard.evaluate_remote(&snapshot.announce(), now);
    drop(guard);
    shared.status.clipboard_local(&snapshot);
    shared.registry.send_all(Outbound::Clipboard(snapshot));
}

struct FileCommand {
    path: std::path::PathBuf,
    clipboard: bool,
}

async fn data_loop(
    shared: Arc<Shared>,
    link: Arc<Link>,
    peer_id: String,
    mut commands: mpsc::UnboundedReceiver<FileCommand>,
    file_channel: Arc<FileChannel>,
) {
    loop {
        // Idle: service incoming C2 until the local console requests a send.
        let command = loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => break command,
                    None => return,
                },
                incoming = link.accept_data() => {
                    if let Err(error) =
                        handle_incoming(&shared, &link, &peer_id, &file_channel, incoming).await
                    {
                        if incoming_closed(&shared, &link, error) {
                            return;
                        }
                    }
                }
            }
        };

        let FileCommand { path, clipboard } = command;
        let target = match describe_target(&path) {
            Ok(target) => target,
            Err(error) => {
                shared.status.note_error(format!("文件发送失败: {error}"));
                continue;
            }
        };
        let backoff = retry_backoff(shared.local_id(), &peer_id);
        // Arbitration: only one side may own the file channel at a time.
        loop {
            if link.close_reason().is_some() {
                return;
            }
            let Some(token) = file_channel.try_reserve() else {
                // The peer owns the channel, most likely sending to us. Keep
                // servicing its C2 data instead of blocking the session.
                if let Service::Closed =
                    service_incoming(&shared, &link, &peer_id, &file_channel, backoff).await
                {
                    return;
                }
                continue;
            };
            match wait_for_grant(
                &shared,
                &link,
                &peer_id,
                &file_channel,
                &target,
                token,
                clipboard,
            )
            .await
            {
                GrantWait::Granted => {}
                GrantWait::Denied => {
                    release_outbound(&shared, &file_channel, &peer_id, token);
                    if let Service::Closed =
                        service_incoming(&shared, &link, &peer_id, &file_channel, backoff).await
                    {
                        return;
                    }
                    continue;
                }
                GrantWait::Closed => {
                    release_outbound(&shared, &file_channel, &peer_id, token);
                    return;
                }
            }

            let result = send_path(&shared, &link, &peer_id, &path, &target).await;
            release_outbound(&shared, &file_channel, &peer_id, token);
            if let Err(error) = result {
                shared.status.note_error(format!("文件发送失败: {error}"));
            }
            break;
        }
    }
}

enum IncomingError {
    /// The C2 connection is unusable; the session should stop.
    Link(String),
    /// A single blob or manifest failed but the session can continue.
    Recoverable(String),
}

/// Reports recoverable failures and returns true when the session must stop.
fn incoming_closed(shared: &Shared, link: &Link, error: IncomingError) -> bool {
    match error {
        IncomingError::Recoverable(message) => {
            shared.status.note_error(message);
            false
        }
        IncomingError::Link(message) => {
            if link.close_reason().is_none() {
                shared.status.note_error(message);
            }
            true
        }
    }
}

async fn handle_incoming(
    shared: &Shared,
    link: &Link,
    peer_id: &str,
    file_channel: &FileChannel,
    incoming: Result<cd_transport::DataReceiver, cd_transport::TransportError>,
) -> Result<(), IncomingError> {
    let mut data =
        incoming.map_err(|error| IncomingError::Link(format!("C2 接收失败: {error}")))?;
    let transfer = data
        .recv()
        .await
        .map_err(|error| IncomingError::Link(format!("C2 数据损坏: {error}")))?;
    match transfer.header.clone() {
        cd_proto::DataHeader::Clipboard(announce) => {
            apply_remote_clipboard(shared, announce, &transfer.payload);
        }
        cd_proto::DataHeader::FileManifest(header) => {
            let token = file_channel.inbound_token();
            let clipboard = file_channel.inbound_clipboard();
            let result = handle_file(shared, link, peer_id, header, transfer, clipboard).await;
            if token != 0 && file_channel.release(token) {
                let _ = shared.registry.send_to(
                    peer_id,
                    Outbound::Control(ControlMessage::FileSendRelease(FileSendRelease { token })),
                );
            }
            if let Err(error) = result {
                return Err(IncomingError::Recoverable(format!("文件接收失败: {error}")));
            }
        }
        _ => {
            return Err(IncomingError::Recoverable(
                "收到意外的 C2 数据流".to_owned(),
            ))
        }
    }
    Ok(())
}

enum Service {
    Finished,
    Closed,
}

/// Keeps accepting inbound C2 traffic for at most `wait`, so a peer that owns
/// the file channel can finish its transfer while we wait to retry.
async fn service_incoming(
    shared: &Shared,
    link: &Link,
    peer_id: &str,
    file_channel: &FileChannel,
    wait: Duration,
) -> Service {
    let deadline = tokio::time::sleep(wait);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return Service::Finished,
            incoming = link.accept_data() => {
                if let Err(error) =
                    handle_incoming(shared, link, peer_id, file_channel, incoming).await
                {
                    match error {
                        IncomingError::Recoverable(message) => shared.status.note_error(message),
                        IncomingError::Link(_) => return Service::Closed,
                    }
                }
            }
        }
    }
}

enum GrantWait {
    Granted,
    Denied,
    Closed,
}

async fn wait_for_grant(
    shared: &Shared,
    link: &Link,
    peer_id: &str,
    file_channel: &FileChannel,
    target: &SendTarget,
    token: u64,
    clipboard: bool,
) -> GrantWait {
    let mut rx = file_channel.register_waiter(token);
    let request = ControlMessage::FileSendRequest(FileSendRequest {
        token,
        root_name: target.root_name.clone(),
        total_bytes: target.total_bytes,
        clipboard,
    });
    if !shared.registry.send_to(peer_id, Outbound::Control(request)) {
        return GrantWait::Closed;
    }
    let deadline = tokio::time::sleep(FILE_GRANT_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            result = &mut rx => {
                return if result.unwrap_or(false) {
                    GrantWait::Granted
                } else {
                    GrantWait::Denied
                };
            }
            _ = &mut deadline => return GrantWait::Denied,
            incoming = link.accept_data() => {
                if let Err(error) =
                    handle_incoming(shared, link, peer_id, file_channel, incoming).await
                {
                    match error {
                        IncomingError::Recoverable(message) => shared.status.note_error(message),
                        IncomingError::Link(_) => return GrantWait::Closed,
                    }
                }
            }
        }
    }
}

fn release_outbound(shared: &Shared, file_channel: &FileChannel, peer_id: &str, token: u64) {
    if file_channel.release(token) {
        let _ = shared.registry.send_to(
            peer_id,
            Outbound::Control(ControlMessage::FileSendRelease(FileSendRelease { token })),
        );
    }
}

fn retry_backoff(local_id: &str, peer_id: &str) -> Duration {
    if local_id < peer_id {
        FILE_RETRY_FAST
    } else {
        FILE_RETRY_SLOW
    }
}

struct SendTarget {
    root_name: String,
    total_bytes: u64,
    is_dir: bool,
}

fn describe_target(path: &std::path::Path) -> Result<SendTarget, String> {
    if path.is_dir() {
        let manifest = cd_files::scan_dir(path).map_err(|error| error.to_string())?;
        Ok(SendTarget {
            root_name: manifest.root_name.clone(),
            total_bytes: manifest.total_bytes(),
            is_dir: true,
        })
    } else if path.is_file() {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("`{}` 文件名无效", path.display()))?;
        let root_name = cd_files::normalize_root_name(name)
            .map_err(|error| format!("非法文件名 `{name}`: {error}"))?;
        let total = std::fs::metadata(path)
            .map_err(|error| error.to_string())?
            .len();
        Ok(SendTarget {
            root_name,
            total_bytes: total,
            is_dir: false,
        })
    } else {
        Err(format!("`{}` 不存在或不是文件/目录", path.display()))
    }
}

async fn send_path(
    shared: &Shared,
    link: &Link,
    peer_id: &str,
    path: &std::path::Path,
    target: &SendTarget,
) -> Result<(), String> {
    let now = now_ms();
    let task_id = format!("send-{}-{}", now, now_us());
    let cancel = shared.cancels.register(&task_id);
    let mut record = TransferRecord {
        id: task_id.clone(),
        direction: TransferDirection::Outgoing,
        peer_id: peer_id.to_owned(),
        root_name: target.root_name.clone(),
        total_bytes: target.total_bytes,
        received_bytes: 0,
        state: cd_files::TransferState::Running,
        dest_path: path.display().to_string(),
        created_at: now,
        updated_at: now,
    };
    shared.save_transfer(&record);
    shared.status.transfer_started(&record);

    let side = |side_transfer: cd_transport::DataTransfer| {
        if let cd_proto::DataHeader::Clipboard(announce) = &side_transfer.header {
            apply_remote_clipboard(shared, announce.clone(), &side_transfer.payload);
        }
    };
    let result = if target.is_dir {
        cd_sync::send_tree_with_side_and_cancel(link, &task_id, path, &cancel, &side).await
    } else {
        cd_sync::send_file_with_side_and_cancel(link, &task_id, path, &cancel, &side).await
    };
    shared.cancels.finish(&task_id);

    record.updated_at = now_ms();
    if matches!(&result, Err(SyncError::Cancelled)) {
        record.state = cd_files::TransferState::Cancelled;
        shared.save_transfer(&record);
        shared.status.transfer_finished(&record);
        let _ = shared.registry.send_to(
            peer_id,
            Outbound::Control(ControlMessage::TransferCancel(TransferCancel {
                task_id: task_id.clone(),
            })),
        );
        println!("已取消发送 {}", record.root_name);
        return Ok(());
    }
    match result {
        Ok(report) => {
            record.total_bytes = report.manifest.total_bytes();
            if report.skipped {
                record.received_bytes = 0;
                record.state = cd_files::TransferState::Skipped;
                shared.save_transfer(&record);
                shared.status.transfer_finished(&record);
                println!("对端跳过 {} · 已存在同名目标", record.root_name);
                return Ok(());
            }
            record.received_bytes = report.manifest.total_bytes();
            record.state = cd_files::TransferState::Completed;
            shared.save_transfer(&record);
            shared.status.transfer_finished(&record);
            shared.status.bytes(0, report.bytes);
            Ok(())
        }
        Err(error) => {
            record.state = if matches!(
                &error,
                SyncError::Protocol(_)
                    | SyncError::InvalidManifest(_)
                    | SyncError::Path(_)
                    | SyncError::HashMismatch { .. }
                    | SyncError::Chunk(_)
            ) {
                cd_files::TransferState::Failed
            } else {
                cd_files::TransferState::Paused
            };
            shared.save_transfer(&record);
            shared.status.transfer_finished(&record);
            Err(error.to_string())
        }
    }
}

fn apply_remote_clipboard(shared: &Shared, announce: ClipboardAnnounce, payload: &[u8]) {
    if !shared.features.clipboard() {
        return;
    }
    let limit = if matches!(announce.kind, cd_proto::ClipboardKind::Image) {
        MAX_IMAGE_CLIPBOARD_BYTES
    } else {
        MAX_CLIPBOARD_BYTES
    };
    if announce.size > limit {
        shared
            .status
            .note_error(format!("剪贴板 {} B 超过自动接收上限", announce.size));
        return;
    }
    let content = match Content::decode(announce.kind, payload) {
        Ok(content) => content,
        Err(error) => {
            shared.status.note_error(format!("剪贴板解码失败: {error}"));
            return;
        }
    };
    if content.hash_hex() != announce.hash {
        shared.status.note_error("剪贴板哈希不匹配");
        return;
    }
    let snapshot = Snapshot {
        content,
        hash: announce.hash.clone(),
        size: announce.size,
        origin_id: announce.origin_id.clone(),
        seq: announce.seq,
    };
    let now = now_ms() as u64;
    let decision = {
        let mut guard = shared
            .guard
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard.evaluate_remote(&announce, now)
    };
    if decision != RemoteDecision::Apply {
        return;
    }
    match &snapshot.content {
        Content::Text { .. } => {
            if shared.clipboard_system {
                let clipboard = shared
                    .clipboard
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clone();
                if let Some(clipboard) = clipboard {
                    let _ = clipboard.send(ClipboardCommand::Apply(snapshot.clone()));
                } else {
                    shared.status.note_error("系统剪贴板不可用，内容未写入");
                }
            }
        }
        Content::Image { .. } => {
            if shared.clipboard_system {
                let clipboard = shared
                    .clipboard
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clone();
                if let Some(clipboard) = clipboard {
                    let _ = clipboard.send(ClipboardCommand::Apply(snapshot.clone()));
                } else {
                    shared.status.note_error("系统剪贴板不可用，图片未写入");
                }
            }
        }
        _ => {
            shared
                .status
                .note_error("当前仅支持文本/图片剪贴板，已忽略内容");
        }
    }
    shared
        .guard
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .note_applied_remote(snapshot.hash.clone(), now);
    shared.status.clipboard_remote(&snapshot);
}

async fn handle_file(
    shared: &Shared,
    link: &Link,
    peer_id: &str,
    header: cd_proto::FileManifestHeader,
    transfer: cd_transport::DataTransfer,
    clipboard: bool,
) -> Result<(), String> {
    let root_name = cd_files::normalize_root_name(&header.root_name)
        .map_err(|error| format!("非法目录名 `{}`: {error}", header.root_name))?;
    let cancel = shared.cancels.register(&header.task_id);
    let now = now_ms();
    let existing = {
        let store = shared
            .store
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        store
            .transfer(&header.task_id)
            .map_err(|error| error.to_string())?
    };
    let inbox = shared.config.inbox(&shared.paths);
    let base = inbox.join(&root_name);
    let mut policy = shared.conflict_policy();
    if policy == ConflictPolicy::Ask && existing.is_none() && base.exists() {
        match ask_conflict(shared, peer_id, &header, &base, &cancel).await {
            Some(chosen) => policy = chosen,
            None => {
                shared.cancels.finish(&header.task_id);
                let _ = shared.registry.send_to(
                    peer_id,
                    Outbound::Control(ControlMessage::TransferCancel(TransferCancel {
                        task_id: header.task_id.clone(),
                    })),
                );
                println!("已取消接收 {} · 等待同名冲突决策", header.root_name);
                return Ok(());
            }
        }
    }
    let (dest, swap_target, skip_existing) = if let Some(record) = &existing {
        let recorded = std::path::PathBuf::from(&record.dest_path);
        if policy == ConflictPolicy::Overwrite {
            (
                staging_destination(&inbox, &root_name, &header.task_id),
                Some(recorded),
                false,
            )
        } else {
            (recorded, None, false)
        }
    } else {
        match policy {
            ConflictPolicy::Overwrite => (
                staging_destination(&inbox, &root_name, &header.task_id),
                Some(base.clone()),
                false,
            ),
            ConflictPolicy::Skip if base.exists() => (base.clone(), None, true),
            _ => {
                let dest = if header.root_is_file {
                    unique_file_destination(&inbox, &root_name)
                } else {
                    unique_destination(&inbox, &root_name)
                };
                (dest, None, false)
            }
        }
    };
    let display_dest = swap_target.clone().unwrap_or_else(|| dest.clone());
    let mut record = existing.unwrap_or(TransferRecord {
        id: header.task_id.clone(),
        direction: TransferDirection::Incoming,
        peer_id: peer_id.to_owned(),
        root_name: root_name.clone(),
        total_bytes: header.total_bytes,
        received_bytes: 0,
        state: cd_files::TransferState::Running,
        dest_path: display_dest.display().to_string(),
        created_at: now,
        updated_at: now,
    });
    record.state = cd_files::TransferState::Running;
    record.total_bytes = header.total_bytes;
    record.updated_at = now;
    shared.save_transfer(&record);
    shared.status.transfer_started(&record);

    let received = Arc::new(std::sync::atomic::AtomicU64::new(record.received_bytes));
    let progress_received = Arc::clone(&received);
    let progress_status = Arc::clone(&shared.status);
    let progress_id = record.id.clone();
    let progress = move |done: u64, _total: u64| {
        progress_received.store(done, Ordering::Relaxed);
        progress_status.transfer_progress(&progress_id, done);
    };

    let result = if skip_existing {
        cd_sync::receive_skipped_from_manifest(link, &dest, transfer).await
    } else {
        cd_sync::receive_tree_from_manifest_with_side_and_cancel(
            link,
            &dest,
            &shared.cache,
            transfer,
            &progress,
            &cancel,
            &|side_transfer: cd_transport::DataTransfer| {
                if let cd_proto::DataHeader::Clipboard(announce) = &side_transfer.header {
                    apply_remote_clipboard(shared, announce.clone(), &side_transfer.payload);
                }
            },
        )
        .await
    };
    shared.cancels.finish(&header.task_id);

    let now = now_ms();
    if matches!(&result, Err(SyncError::Cancelled)) {
        record.received_bytes = received.load(Ordering::Relaxed);
        record.state = cd_files::TransferState::Cancelled;
        record.updated_at = now;
        shared.save_transfer(&record);
        shared.status.transfer_finished(&record);
        cleanup_cancelled(&dest, swap_target.is_some());
        let _ = shared.registry.send_to(
            peer_id,
            Outbound::Control(ControlMessage::TransferCancel(TransferCancel {
                task_id: header.task_id.clone(),
            })),
        );
        println!("已取消接收 {}", record.root_name);
        return Ok(());
    }
    match result {
        Ok(report) => {
            if let Some(target) = &swap_target {
                if let Err(error) = swap_into_place(&dest, target, &record.id) {
                    record.state = cd_files::TransferState::Failed;
                    record.updated_at = now;
                    shared.save_transfer(&record);
                    shared.status.transfer_finished(&record);
                    return Err(format!("替换目标失败: {error}"));
                }
            }
            if report.skipped {
                record.received_bytes = 0;
                record.state = cd_files::TransferState::Skipped;
                record.updated_at = now;
                shared.save_transfer(&record);
                shared.status.transfer_finished(&record);
                println!(
                    "跳过 {} · 目标 {} 已存在（同名策略 skip）",
                    record.root_name,
                    display_dest.display()
                );
                return Ok(());
            }
            record.received_bytes = report.manifest.total_bytes();
            record.state = cd_files::TransferState::Completed;
            record.updated_at = now;
            shared.save_transfer(&record);
            shared.status.transfer_finished(&record);
            shared.status.bytes(report.bytes, 0);
            println!(
                "接收完成 {} · {} 文件 / {} 目录 / {} B · 复用 {} 块 · 续传 {} B",
                record.root_name,
                report.manifest.file_count(),
                report.manifest.directory_count(),
                report.manifest.total_bytes(),
                report.reused_chunks,
                report.resumed_bytes
            );
            if clipboard {
                apply_received_file_clipboard(
                    shared,
                    &display_dest,
                    &report.manifest,
                    header.root_is_file,
                );
            }
            Ok(())
        }
        Err(error) => {
            record.received_bytes = received.load(Ordering::Relaxed);
            record.state = if matches!(
                &error,
                SyncError::Protocol(_)
                    | SyncError::InvalidManifest(_)
                    | SyncError::Path(_)
                    | SyncError::HashMismatch { .. }
                    | SyncError::Chunk(_)
            ) {
                cd_files::TransferState::Failed
            } else {
                cd_files::TransferState::Paused
            };
            record.updated_at = now;
            shared.save_transfer(&record);
            shared.status.transfer_finished(&record);
            Err(error.to_string())
        }
    }
}

/// Blocks the incoming transfer until the console chooses how to treat an
/// existing destination. A silent console defaults to rename so no data is
/// lost.
async fn ask_conflict(
    shared: &Shared,
    peer_id: &str,
    header: &cd_proto::FileManifestHeader,
    base: &std::path::Path,
    cancel: &CancelToken,
) -> Option<ConflictPolicy> {
    let view = PendingConflictView {
        id: String::new(),
        task_id: header.task_id.clone(),
        peer_id: peer_id.to_owned(),
        root_name: header.root_name.clone(),
        total_bytes: header.total_bytes,
        destination: base.display().to_string(),
        requested_at_ms: 0,
        expires_at_ms: 0,
    };
    let (view, rx) = shared.status.request_conflict(view);
    println!(
        "同名冲突等待控制台决策: {} · {}",
        view.root_name, view.destination
    );
    let outcome = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(60), rx) => result,
        _ = cancel.cancelled() => {
            shared.status.resolve_conflict(&view.id, ConflictPolicy::Rename);
            println!("同名冲突等待被取消: {}", view.root_name);
            return None;
        }
    };
    match outcome {
        Ok(Ok(policy)) => Some(policy),
        _ => {
            shared
                .status
                .resolve_conflict(&view.id, ConflictPolicy::Rename);
            println!("同名冲突决策超时，默认重命名: {}", view.root_name);
            Some(ConflictPolicy::Rename)
        }
    }
}

/// Removes partial `.cdpart` files (or the whole staging tree) after a transfer
/// is cancelled, so a cancelled task does not leave resume leftovers behind.
fn cleanup_cancelled(dest: &std::path::Path, staging: bool) {
    if staging {
        let _ = std::fs::remove_dir_all(dest);
        return;
    }
    if !dest.is_dir() {
        let _ = std::fs::remove_file(cd_files::partial_path(dest));
        return;
    }
    let mut stack = vec![dest.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("cdpart") {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Exposes the paths that just landed in the inbox as a real file clipboard
/// so the user can paste them into a file manager.
fn apply_received_file_clipboard(
    shared: &Shared,
    dest: &std::path::Path,
    manifest: &cd_files::Manifest,
    root_is_file: bool,
) {
    if !shared.features.clipboard() {
        return;
    }
    let mut paths = Vec::new();
    if root_is_file {
        paths.push(dest.to_path_buf());
    } else {
        for entry in manifest
            .entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .take(64)
        {
            paths.push(cd_files::join_rel(dest, &entry.rel_path));
        }
    }
    let mut entries = Vec::new();
    let mut total = 0_u64;
    for path in paths {
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        total = total.saturating_add(metadata.len());
        entries.push(cd_clipboard::FileRef {
            path: path.display().to_string(),
            size: metadata.len(),
            is_dir: false,
        });
    }
    if entries.is_empty() {
        return;
    }
    if total > cd_clipboard::DEFAULT_AUTO_FETCH_BYTES {
        shared
            .status
            .note_error("文件剪贴板超过自动同步上限，未写入系统剪贴板");
        return;
    }
    let snapshot = Snapshot::new(
        Content::Files { entries },
        &shared.config.device_id,
        now_us(),
    );
    shared.status.clipboard_remote(&snapshot);
    shared
        .guard
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .note_applied_remote(snapshot.hash.clone(), now_ms() as u64);
    if shared.clipboard_system {
        let sender = shared
            .clipboard
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        if let Some(sender) = sender {
            let _ = sender.send(ClipboardCommand::Apply(snapshot));
        }
    }
}

/// Hidden sibling used to stage an overwrite; the receiver writes here first
/// and only replaces the real destination after the whole task succeeded.
fn staging_destination(
    inbox: &std::path::Path,
    root_name: &str,
    task_id: &str,
) -> std::path::PathBuf {
    let tag = blake3::hash(task_id.as_bytes()).to_hex().to_string();
    inbox.join(format!(".{root_name}.cdnew-{}", &tag[..16]))
}

/// Replaces `target` with the completed staging tree. The old copy is moved
/// aside first so a failed rename can be rolled back.
fn swap_into_place(
    staging: &std::path::Path,
    target: &std::path::Path,
    task_id: &str,
) -> std::io::Result<()> {
    if !target.exists() {
        return std::fs::rename(staging, target);
    }
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("target");
    let tag = blake3::hash(task_id.as_bytes()).to_hex().to_string();
    let backup = target.with_file_name(format!(".{name}.cdold-{}", &tag[..16]));
    std::fs::rename(target, &backup)?;
    match std::fs::rename(staging, target) {
        Ok(()) => {
            let _ = if backup.is_dir() {
                std::fs::remove_dir_all(&backup)
            } else {
                std::fs::remove_file(&backup)
            };
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::rename(&backup, target);
            Err(error)
        }
    }
}

fn unique_destination(inbox: &std::path::Path, root_name: &str) -> std::path::PathBuf {
    let candidate = inbox.join(root_name);
    if !candidate.exists() || (candidate.is_dir() && is_empty_dir(&candidate)) {
        return candidate;
    }
    for index in 2..=9999 {
        let candidate = inbox.join(format!("{root_name}~{index}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    inbox.join(format!("{root_name}~{}", now_ms()))
}

fn unique_file_destination(inbox: &std::path::Path, root_name: &str) -> std::path::PathBuf {
    let candidate = inbox.join(root_name);
    if !candidate.exists() {
        return candidate;
    }
    let path = std::path::Path::new(root_name);
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| root_name.to_owned());
    let extension = path
        .extension()
        .map(|ext| ext.to_string_lossy().to_string());
    for index in 2..=9999 {
        let name = match &extension {
            Some(extension) => format!("{stem}~{index}.{extension}"),
            None => format!("{stem}~{index}"),
        };
        let candidate = inbox.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    inbox.join(format!("{stem}~{}.cdpart", now_ms()))
}

fn is_empty_dir(path: &std::path::Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
}

fn detect_monitor() -> MonitorInfo {
    cd_platform::detect()
        .ok()
        .and_then(|detection| {
            detection
                .monitors
                .into_iter()
                .max_by(|a, b| a.rect().right().total_cmp(&b.rect().right()))
        })
        .unwrap_or_else(presets::linux_4k)
}

fn paired_count(shared: &Shared) -> usize {
    shared
        .store
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .devices()
        .map(|devices| devices.len())
        .unwrap_or(0)
}

fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn cache_usage(root: &std::path::Path) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return (0, 0);
    };
    let mut blocks = 0_u64;
    let mut bytes = 0_u64;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name.len() != 64 {
            continue;
        }
        blocks = blocks.saturating_add(1);
        if let Ok(metadata) = entry.metadata() {
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    (blocks, bytes)
}

fn conflict_label(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::Rename => "rename",
        ConflictPolicy::Overwrite => "overwrite",
        ConflictPolicy::Skip => "skip",
        ConflictPolicy::Ask => "ask",
        ConflictPolicy::Merge => "merge",
    }
}

fn edge_label(edge: EdgeSide) -> &'static str {
    match edge {
        EdgeSide::Left => "left",
        EdgeSide::Right => "right",
    }
}

fn short(hash: &str) -> String {
    format!("{}…", hash.chars().take(12).collect::<String>())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros() as u64)
        .unwrap_or(0)
}

fn lock_ownership(
    ownership: &Arc<Mutex<OwnershipMachine>>,
) -> std::sync::MutexGuard<'_, OwnershipMachine> {
    ownership
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}
