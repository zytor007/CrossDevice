use std::collections::BTreeSet;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cd_core::{OwnershipAction, OwnershipEvent, OwnershipMachine};
use cd_geometry::MonitorInfo;
use cd_input::{apply_input, CapturedEvent, PointerMapper, X11Capture, X11Injector};
use cd_proto::{EdgeSide, InputEvent, InputKind, InputOwner, Modifiers, OwnerPhase, PairSettings};

use super::registry::{Outbound, PeerRegistry};
use super::status::StatusBoard;

const EDGE_ARM_PX: i32 = 48;
const ESCAPE_KEY: u8 = 9;
const CTRL_KEYS: [u8; 2] = [37, 105];
const ALT_KEYS: [u8; 2] = [64, 108];

pub enum Command {
    Inject(InputEvent),
    Release,
    Stop,
}

pub struct InputHandle {
    commands: Sender<Command>,
    join: Option<JoinHandle<()>>,
}

impl InputHandle {
    pub fn inject(&self, event: InputEvent) {
        let _ = self.commands.send(Command::Inject(event));
    }

    pub fn release(&self) {
        let _ = self.commands.send(Command::Release);
    }
}

impl Drop for InputHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Starts the X11 capture → C1 → XTest pump. Returns `None` when there is no
/// display, so clipboard/file services still work headless.
pub fn spawn(
    local_id: String,
    monitor: MonitorInfo,
    registry: PeerRegistry,
    ownership: Arc<Mutex<OwnershipMachine>>,
    status: Arc<StatusBoard>,
    settings: Arc<Mutex<PairSettings>>,
) -> Option<InputHandle> {
    let (commands, receiver) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let join = thread::Builder::new()
        .name("crossdev-input".to_owned())
        .spawn(move || {
            run(
                local_id, monitor, registry, ownership, status, settings, receiver, ready_tx,
            )
        })
        .ok()?;
    match ready_rx.recv() {
        Ok(Ok(())) => Some(InputHandle {
            commands,
            join: Some(join),
        }),
        Ok(Err(error)) => {
            eprintln!("输入泵不可用: {error}");
            let _ = join.join();
            None
        }
        Err(_) => {
            let _ = join.join();
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    local_id: String,
    monitor: MonitorInfo,
    registry: PeerRegistry,
    ownership: Arc<Mutex<OwnershipMachine>>,
    status: Arc<StatusBoard>,
    settings: Arc<Mutex<PairSettings>>,
    receiver: Receiver<Command>,
    ready: Sender<Result<(), String>>,
) {
    let capture = match X11Capture::open() {
        Ok(capture) => capture,
        Err(error) => {
            let _ = ready.send(Err(format!("捕获不可用: {error}")));
            return;
        }
    };
    let injector = match X11Injector::open() {
        Ok(injector) => injector,
        Err(error) => {
            let _ = ready.send(Err(format!("注入不可用: {error}")));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    let mut mapper = PointerMapper::new(monitor.clone());
    let mut modifiers = BTreeSet::new();
    let mut edge_gate = EdgeGate {
        armed: true,
        since: None,
    };
    let mut primary_down = false;
    let mut tick = 0_u32;

    loop {
        let mut stop = false;
        while let Ok(command) = receiver.try_recv() {
            match command {
                Command::Inject(event) => {
                    status.input_in(1);
                    if let Err(error) = apply_input(&injector, &monitor, &event) {
                        status.note_error(format!("C1 注入失败: {error}"));
                    }
                }
                Command::Release => {
                    let actions = lock_ownership(&ownership)
                        .handle(OwnershipEvent::ReturnHome { at_ms: now_ms() });
                    perform_actions(actions, &local_id, &registry, &status, &ownership);
                    mapper.reset();
                }
                Command::Stop => stop = true,
            }
        }
        if stop {
            break;
        }

        let pair = pair_settings(&settings);
        let events = match capture.drain() {
            Ok(events) => events,
            Err(error) => {
                status.note_error(format!("捕获失败: {error}"));
                Vec::new()
            }
        };
        for event in events {
            track_modifiers(&mut modifiers, &event);
            if let CapturedEvent::Button {
                button, pressed, ..
            } = &event
            {
                if *button == 1 {
                    primary_down = *pressed;
                }
            }
            let remote = lock_ownership(&ownership).current_peer().is_some();
            if remote && is_return_shortcut(&modifiers, &event) {
                release_remote_modifiers(&registry, &ownership);
                let actions = lock_ownership(&ownership)
                    .handle(OwnershipEvent::ReturnHome { at_ms: now_ms() });
                perform_actions(actions, &local_id, &registry, &status, &ownership);
                mapper.reset();
                continue;
            }
            forward(
                &event,
                &mut mapper,
                &registry,
                &ownership,
                &status,
                pair.pointer_speed,
            );
        }

        if tick % 4 == 0 {
            let actions =
                lock_ownership(&ownership).handle(OwnershipEvent::Tick { at_ms: now_ms() });
            perform_actions(actions, &local_id, &registry, &status, &ownership);
            watch_edge(
                &local_id,
                &injector,
                &monitor,
                &registry,
                &ownership,
                &status,
                &mut edge_gate,
                &pair,
                primary_down,
            );
        }
        tick = tick.wrapping_add(1);
        thread::sleep(Duration::from_millis(4));
    }
}

fn forward(
    event: &CapturedEvent,
    mapper: &mut PointerMapper,
    registry: &PeerRegistry,
    ownership: &Arc<Mutex<OwnershipMachine>>,
    status: &StatusBoard,
    pointer_speed: f64,
) {
    let owner = lock_ownership(ownership).current_peer().map(str::to_owned);
    let Some(owner) = owner else {
        mapper.reset();
        return;
    };
    if let Some(mut forward) = mapper.map(event) {
        if forward.kind == InputKind::Move && !forward.is_absolute() {
            forward.x = scale_delta(forward.x, pointer_speed);
            forward.y = scale_delta(forward.y, pointer_speed);
        }
        status.input_out(1);
        registry.send_input(&owner, forward);
    }
}

#[allow(clippy::cast_possible_truncation)]
fn scale_delta(value: i32, speed: f64) -> i32 {
    (f64::from(value) * speed).round() as i32
}

struct EdgeGate {
    /// Set after the pointer leaves the edge zone; prevents repeated crossings.
    armed: bool,
    /// When the pointer first reached the edge, used for edge resistance.
    since: Option<Instant>,
}

fn pair_settings(settings: &Arc<Mutex<PairSettings>>) -> PairSettings {
    settings
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

#[allow(clippy::too_many_arguments)]
fn watch_edge(
    local_id: &str,
    injector: &X11Injector,
    monitor: &MonitorInfo,
    registry: &PeerRegistry,
    ownership: &Arc<Mutex<OwnershipMachine>>,
    status: &StatusBoard,
    gate: &mut EdgeGate,
    settings: &PairSettings,
    primary_down: bool,
) {
    let Ok((x, y)) = injector.pointer() else {
        return;
    };
    let max_x = i32::try_from(monitor.native_w)
        .unwrap_or(i32::MAX)
        .saturating_sub(1);
    let x = i32::from(x);
    let at_edge = match settings.edge {
        EdgeSide::Right => x >= max_x - 1,
        EdgeSide::Left => x <= 1,
    };
    if !at_edge {
        gate.since = None;
        let outside_zone = match settings.edge {
            EdgeSide::Right => x < max_x - EDGE_ARM_PX,
            EdgeSide::Left => x > EDGE_ARM_PX,
        };
        if outside_zone {
            gate.armed = true;
        }
        return;
    }
    // Dragging (primary button held) never crosses the edge; the clipboard
    // worker may turn a file drag into a transfer after a short hover.
    if primary_down {
        gate.since = None;
        return;
    }
    if !gate.armed || !lock_ownership(ownership).is_local() {
        return;
    }
    // Edge resistance: the pointer has to stay on the edge for the configured
    // time before ownership moves; 0 crosses on the first poll.
    let now = Instant::now();
    let since = *gate.since.get_or_insert(now);
    if now.duration_since(since) < Duration::from_millis(u64::from(settings.edge_resistance)) {
        return;
    }
    let Some(peer) = registry.first_peer() else {
        return;
    };
    gate.armed = false;
    gate.since = None;
    let actions = lock_ownership(ownership).handle(OwnershipEvent::EdgeCrossed {
        target: peer.clone(),
        modifiers: Modifiers::empty(),
        at_ms: now_ms(),
    });
    perform_actions(actions, local_id, registry, status, ownership);
    let accepted =
        lock_ownership(ownership).handle(OwnershipEvent::HandoffAccepted { at_ms: now_ms() });
    perform_actions(accepted, local_id, registry, status, ownership);
    let y_norm = ((f64::from(y) / f64::from(monitor.native_h.saturating_sub(1).max(1)) * 65_535.0)
        .clamp(0.0, 65_535.0)) as u16;
    // Enter the peer from the screen edge that faces us.
    let x_norm = match settings.edge {
        EdgeSide::Right => 0,
        EdgeSide::Left => 65_535,
    };
    registry.send_input(&peer, InputEvent::move_absolute(x_norm, y_norm, now_us()));
}

fn perform_actions(
    actions: Vec<OwnershipAction>,
    local_id: &str,
    registry: &PeerRegistry,
    status: &StatusBoard,
    ownership: &Arc<Mutex<OwnershipMachine>>,
) {
    for action in actions {
        match action {
            OwnershipAction::HideLocalCursor | OwnershipAction::ShowLocalCursor => {}
            OwnershipAction::InformPeerHandoff { target, modifiers } => {
                registry.send_to(
                    &target,
                    Outbound::Control(cd_proto::ControlMessage::InputOwner(InputOwner {
                        owner: target.clone(),
                        phase: OwnerPhase::HandoffOut,
                        modifiers,
                    })),
                );
            }
            OwnershipAction::InformPeerRelease { target } => {
                registry.send_to(
                    &target,
                    Outbound::Control(cd_proto::ControlMessage::InputOwner(InputOwner {
                        owner: local_id.to_owned(),
                        phase: OwnerPhase::HandoffBack,
                        modifiers: Modifiers::empty(),
                    })),
                );
            }
            OwnershipAction::NotifyHandoffSlow { target, elapsed_ms } => {
                eprintln!("交接慢: {target} 已 {elapsed_ms} ms 未确认");
            }
            OwnershipAction::NotifyHandoffTimeout { target, elapsed_ms } => {
                status.note_error(format!("交接超时: {target} 在 {elapsed_ms} ms 后回切"));
            }
            OwnershipAction::NotifyPeerLost { target } => {
                status.note_error(format!("对端 {target} 掉线，控制已回切"));
            }
            OwnershipAction::NotifySuspended { .. } | OwnershipAction::NotifyResumed => {}
        }
    }
    let machine = lock_ownership(ownership);
    let label = machine.state().label().to_lowercase();
    let peer = machine.current_peer().map(str::to_owned);
    drop(machine);
    status.set_ownership(&label, peer.as_deref());
}

fn release_remote_modifiers(registry: &PeerRegistry, ownership: &Arc<Mutex<OwnershipMachine>>) {
    let Some(peer) = lock_ownership(ownership).current_peer().map(str::to_owned) else {
        return;
    };
    for keycode in CTRL_KEYS.into_iter().chain(ALT_KEYS) {
        registry.send_input(&peer, InputEvent::key(u16::from(keycode), false, now_us()));
    }
}

fn track_modifiers(pressed: &mut BTreeSet<u8>, event: &CapturedEvent) {
    if let CapturedEvent::Key {
        keycode,
        pressed: down,
        ..
    } = event
    {
        if *down {
            pressed.insert(*keycode);
        } else {
            pressed.remove(keycode);
        }
    }
}

fn is_return_shortcut(pressed: &BTreeSet<u8>, event: &CapturedEvent) -> bool {
    let CapturedEvent::Key {
        keycode,
        pressed: true,
        ..
    } = event
    else {
        return false;
    };
    if *keycode != ESCAPE_KEY {
        return false;
    }
    let ctrl = CTRL_KEYS.iter().any(|key| pressed.contains(key));
    let alt = ALT_KEYS.iter().any(|key| pressed.contains(key));
    ctrl && alt
}

fn lock_ownership(
    ownership: &Arc<Mutex<OwnershipMachine>>,
) -> std::sync::MutexGuard<'_, OwnershipMachine> {
    ownership
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros() as u64)
        .unwrap_or(0)
}
