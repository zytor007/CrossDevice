use std::sync::{Arc, Mutex};
use std::time::Duration;

use cd_store::Store;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::input::InputHandle;
use super::runtime::Shared;
use super::status::{StatusBoard, TransferView};

#[derive(Clone)]
pub struct WebState {
    pub status: Arc<StatusBoard>,
    pub store: Arc<Mutex<Store>>,
    pub input: Arc<Mutex<Option<InputHandle>>>,
    pub shared: Arc<Shared>,
    pub connector: tokio::sync::mpsc::UnboundedSender<std::net::SocketAddr>,
}

const DASHBOARD: &str = include_str!("dashboard.html");

pub async fn serve(listener: TcpListener, state: WebState, mut shutdown: watch::Receiver<bool>) {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let state = state.clone();
                        let shutdown = shutdown.clone();
                        connections.spawn(async move {
                            let _ = handle(stream, state, shutdown).await;
                        });
                    }
                    Err(error) => {
                        eprintln!("HTTP accept 失败: {error}");
                    }
                }
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    eprintln!("HTTP 连接任务失败: {error}");
                }
            }
        }
    }
    connections.abort_all();
}

async fn handle(
    mut stream: tokio::net::TcpStream,
    state: WebState,
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let Some((method, path, body)) = read_request(&mut stream).await? else {
        return Ok(());
    };

    if method == "POST" {
        if path == "/api/files/send" {
            let request: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let Some(path) = request["path"].as_str() else {
                let body = json!({"ok": false, "error": "missing path"}).to_string();
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
                return Ok(());
            };
            let candidate = std::path::Path::new(path);
            if !candidate.is_dir() && !candidate.is_file() {
                let body =
                    json!({"ok": false, "error": "path must be a file or directory"}).to_string();
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
                return Ok(());
            }
            let snapshot = state.status.snapshot();
            let peer = request["peer"].as_str().map(str::to_owned).or_else(|| {
                snapshot
                    .peers
                    .iter()
                    .find(|peer| peer.online)
                    .map(|peer| peer.id.clone())
            });
            let Some(peer) = peer else {
                let body = json!({"ok": false, "error": "no online peer"}).to_string();
                respond(
                    &mut stream,
                    "409 Conflict",
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
                return Ok(());
            };
            let sent = state.shared.registry.send_to(
                &peer,
                super::registry::Outbound::SendFile {
                    path: std::path::PathBuf::from(path),
                    clipboard: request["clipboard"].as_bool().unwrap_or(false),
                },
            );
            let body = json!({"ok": sent, "peer": peer, "path": path}).to_string();
            let status = if sent { "200 OK" } else { "409 Conflict" };
            respond(&mut stream, status, "application/json", body.as_bytes()).await?;
            return Ok(());
        }
        if let Some(action) = path.strip_prefix("/api/settings/clipboard/") {
            return apply_feature(&mut stream, &state, action, true).await;
        }
        if let Some(action) = path.strip_prefix("/api/settings/input/") {
            return apply_feature(&mut stream, &state, action, false).await;
        }
        if let Some(action) = path.strip_prefix("/api/settings/files/") {
            let policy = match action {
                "rename" => Some(cd_files::ConflictPolicy::Rename),
                "overwrite" => Some(cd_files::ConflictPolicy::Overwrite),
                "skip" => Some(cd_files::ConflictPolicy::Skip),
                "ask" => Some(cd_files::ConflictPolicy::Ask),
                _ => None,
            };
            let Some(policy) = policy else {
                let body =
                    json!({"ok": false, "error": format!("unknown policy `{action}`")}).to_string();
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
                return Ok(());
            };
            match state.shared.set_conflict_policy(policy) {
                Ok(()) => {
                    let body = json!({
                        "ok": true,
                        "file_conflict": state.shared.conflict_policy(),
                    })
                    .to_string();
                    respond(&mut stream, "200 OK", "application/json", body.as_bytes()).await?;
                }
                Err(error) => {
                    let body = json!({"ok": false, "error": error}).to_string();
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        body.as_bytes(),
                    )
                    .await?;
                }
            }
            return Ok(());
        }
        if path == "/api/settings/pair" {
            let request: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let current = state.shared.pair_settings();
            let edge = match request["edge"].as_str() {
                Some("left") => cd_proto::EdgeSide::Left,
                Some("right") => cd_proto::EdgeSide::Right,
                Some(other) => {
                    let body = json!({"ok": false, "error": format!("unknown edge `{other}`")})
                        .to_string();
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        body.as_bytes(),
                    )
                    .await?;
                    return Ok(());
                }
                None => current.edge,
            };
            let pointer_speed = request["pointer_speed"]
                .as_f64()
                .unwrap_or(current.pointer_speed);
            let edge_resistance = request["edge_resistance"]
                .as_u64()
                .map_or(current.edge_resistance, |value| value.min(255) as u8);
            match state
                .shared
                .set_pair_settings(edge, pointer_speed, edge_resistance)
            {
                Ok(settings) => {
                    let body = json!({"ok": true, "settings": settings}).to_string();
                    respond(&mut stream, "200 OK", "application/json", body.as_bytes()).await?;
                }
                Err(error) => {
                    let body = json!({"ok": false, "error": error}).to_string();
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        body.as_bytes(),
                    )
                    .await?;
                }
            }
            return Ok(());
        }
        if path == "/api/settings/layout" {
            let request: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let edge = match request["side"].as_str() {
                Some("left") => Some(cd_proto::EdgeSide::Left),
                Some("right") => Some(cd_proto::EdgeSide::Right),
                _ => None,
            };
            let Some(edge) = edge else {
                let body = json!({"ok": false, "error": "side must be left or right"}).to_string();
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
                return Ok(());
            };
            let gap_mm = request["gap_mm"].as_f64().unwrap_or(0.0);
            match state.shared.apply_layout(edge, gap_mm) {
                Ok(layout) => {
                    let body = json!({"ok": true, "layout": layout}).to_string();
                    respond(&mut stream, "200 OK", "application/json", body.as_bytes()).await?;
                }
                Err(error) => {
                    let body = json!({"ok": false, "error": error}).to_string();
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        body.as_bytes(),
                    )
                    .await?;
                }
            }
            return Ok(());
        }
        if let Some(rest) = path.strip_prefix("/api/devices/") {
            let Some(id) = rest.strip_suffix("/revoke") else {
                let body = json!({"ok": false, "error": "unknown device action"}).to_string();
                respond(
                    &mut stream,
                    "404 Not Found",
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
                return Ok(());
            };
            let removed = state
                .store
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove_device(id);
            match removed {
                Ok(removed) => {
                    let online = state
                        .shared
                        .registry
                        .send_to(id, super::registry::Outbound::Disconnect);
                    let body = json!({"ok": removed, "id": id, "online": online}).to_string();
                    let status = if removed { "200 OK" } else { "404 Not Found" };
                    respond(&mut stream, status, "application/json", body.as_bytes()).await?;
                }
                Err(error) => {
                    let body = json!({"ok": false, "error": error.to_string()}).to_string();
                    respond(
                        &mut stream,
                        "500 Internal Server Error",
                        "application/json",
                        body.as_bytes(),
                    )
                    .await?;
                }
            }
            return Ok(());
        }
        if let Some(rest) = path.strip_prefix("/api/transfers/") {
            let action = if rest.ends_with("/cancel") {
                "cancel"
            } else if rest.ends_with("/pause") {
                "pause"
            } else if rest.ends_with("/resume") {
                "resume"
            } else {
                let body = json!({"ok": false, "error": "unknown transfer action"}).to_string();
                respond(
                    &mut stream,
                    "404 Not Found",
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
                return Ok(());
            };
            let id = rest
                .strip_suffix("/cancel")
                .or_else(|| rest.strip_suffix("/pause"))
                .or_else(|| rest.strip_suffix("/resume"))
                .unwrap_or(rest);
            let (active, paused) = match action {
                "cancel" => (state.shared.cancels.cancel(id), false),
                "pause" => (state.shared.set_transfer_paused(id, true), true),
                _ => (state.shared.set_transfer_paused(id, false), false),
            };
            if active && action != "cancel" {
                state
                    .shared
                    .registry
                    .send_all(super::registry::Outbound::Control(
                        cd_proto::ControlMessage::TransferPause(cd_proto::TransferPause {
                            task_id: id.to_owned(),
                            paused,
                        }),
                    ));
            }
            let body = json!({"ok": active, "id": id, "paused": paused}).to_string();
            let status = if active { "200 OK" } else { "404 Not Found" };
            respond(&mut stream, status, "application/json", body.as_bytes()).await?;
            return Ok(());
        }
        if let Some(id) = path.strip_prefix("/api/conflicts/") {
            let request: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let policy = match request["action"].as_str() {
                Some("rename") => Some(cd_files::ConflictPolicy::Rename),
                Some("overwrite") => Some(cd_files::ConflictPolicy::Overwrite),
                Some("skip") => Some(cd_files::ConflictPolicy::Skip),
                _ => None,
            };
            let Some(policy) = policy else {
                let body =
                    json!({"ok": false, "error": "action must be rename, overwrite or skip"})
                        .to_string();
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
                return Ok(());
            };
            let ok = state.status.resolve_conflict(id, policy);
            let body = json!({"ok": ok, "id": id, "action": policy}).to_string();
            let status = if ok { "200 OK" } else { "404 Not Found" };
            respond(&mut stream, status, "application/json", body.as_bytes()).await?;
            return Ok(());
        }
        if let Some(id) = path.strip_prefix("/api/pair/approve/") {
            let ok = state.status.resolve_pair(id, true);
            let body = json!({"ok": ok, "id": id}).to_string();
            let status = if ok { "200 OK" } else { "404 Not Found" };
            respond(&mut stream, status, "application/json", body.as_bytes()).await?;
            return Ok(());
        }
        if let Some(id) = path.strip_prefix("/api/pair/reject/") {
            let ok = state.status.resolve_pair(id, false);
            let body = json!({"ok": ok, "id": id}).to_string();
            let status = if ok { "200 OK" } else { "404 Not Found" };
            respond(&mut stream, status, "application/json", body.as_bytes()).await?;
            return Ok(());
        }
        if let Some(hash) = path.strip_prefix("/api/clipboard/apply/") {
            let Some(snapshot) = state.status.history_snapshot(hash) else {
                let body = json!({"ok": false, "error": "item not found"}).to_string();
                respond(
                    &mut stream,
                    "404 Not Found",
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
                return Ok(());
            };
            let sender = state
                .shared
                .clipboard
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();
            if let Some(sender) = sender {
                let sent = sender
                    .send(super::clipboard::Command::Apply(snapshot))
                    .is_ok();
                let body = json!({"ok": sent}).to_string();
                respond(
                    &mut stream,
                    if sent { "200 OK" } else { "409 Conflict" },
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
            } else {
                let body = json!({"ok": false, "error": "system clipboard disabled"}).to_string();
                respond(
                    &mut stream,
                    "409 Conflict",
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
            }
            return Ok(());
        }
        if let Some(id) = path.strip_prefix("/api/peers/connect/") {
            let snapshot = state.status.snapshot();
            if snapshot
                .peers
                .iter()
                .any(|peer| peer.id == id && peer.online)
            {
                let body = json!({"ok": true, "already_connected": true}).to_string();
                respond(&mut stream, "200 OK", "application/json", body.as_bytes()).await?;
                return Ok(());
            }
            let Some(peer) = snapshot.discovered.iter().find(|peer| peer.device_id == id) else {
                let body = json!({"ok": false, "error": "device not discovered"}).to_string();
                respond(
                    &mut stream,
                    "404 Not Found",
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
                return Ok(());
            };
            match peer.address.parse::<std::net::SocketAddr>() {
                Ok(addr) => {
                    let sent = state.connector.send(addr).is_ok();
                    let body = json!({"ok": sent, "address": peer.address}).to_string();
                    respond(&mut stream, "200 OK", "application/json", body.as_bytes()).await?;
                }
                Err(error) => {
                    let body = json!({"ok": false, "error": error.to_string()}).to_string();
                    respond(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        body.as_bytes(),
                    )
                    .await?;
                }
            }
            return Ok(());
        }
    }

    match (method.as_str(), path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                DASHBOARD.as_bytes(),
            )
            .await?;
        }
        ("GET", "/api/status") => {
            let body = serde_json::to_vec(&state.status.snapshot()).unwrap_or_default();
            respond(&mut stream, "200 OK", "application/json", &body).await?;
        }
        ("GET", "/api/devices") => {
            let devices = state
                .store
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .devices()
                .unwrap_or_default();
            let list = devices
                .iter()
                .map(|device| {
                    json!({
                        "id": device.id,
                        "name": device.name,
                        "fingerprint": hex(&device.cert_fingerprint),
                        "capabilities": device.capabilities,
                        "paired_at": device.paired_at,
                        "last_seen": device.last_seen,
                    })
                })
                .collect::<Vec<_>>();
            let body = serde_json::to_vec(&list).unwrap_or_default();
            respond(&mut stream, "200 OK", "application/json", &body).await?;
        }
        ("GET", "/api/transfers") => {
            let transfers = state
                .store
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .transfers()
                .unwrap_or_default();
            let list = transfers
                .iter()
                .map(|record| {
                    serde_json::to_value(TransferView::from_record(record)).unwrap_or_default()
                })
                .collect::<Vec<_>>();
            let body = serde_json::to_vec(&list).unwrap_or_default();
            respond(&mut stream, "200 OK", "application/json", &body).await?;
        }
        ("GET", "/api/pair/pending") => {
            let body = serde_json::to_vec(&state.status.pending_pairs()).unwrap_or_default();
            respond(&mut stream, "200 OK", "application/json", &body).await?;
        }
        ("GET", "/api/conflicts") => {
            let body = serde_json::to_vec(&state.status.pending_conflicts()).unwrap_or_default();
            respond(&mut stream, "200 OK", "application/json", &body).await?;
        }
        ("GET", "/api/diagnostics") => {
            let body = state.shared.diagnostics().to_string();
            respond(&mut stream, "200 OK", "application/json", body.as_bytes()).await?;
        }
        ("GET", "/api/settings") => {
            let pair = state.shared.pair_settings();
            let body = json!({
                "clipboard": state.shared.features.clipboard(),
                "input_pump": state.shared.features.input(),
                "revision": state.shared.features.revision(),
                "pair_mode": state.status.local().pair_mode,
                "edge": pair.edge,
                "pointer_speed": pair.pointer_speed,
                "edge_resistance": pair.edge_resistance,
                "pair_settings_revision": pair.revision,
                "file_conflict": state.shared.conflict_policy(),
                "layout_gap_mm": state.shared.layout_gap(),
            })
            .to_string();
            respond(&mut stream, "200 OK", "application/json", body.as_bytes()).await?;
        }
        ("POST", "/api/input/release") => {
            let released = {
                let input = state
                    .input
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                input.as_ref().map(|input| {
                    input.release();
                    true
                })
            };
            if released == Some(true) {
                let body = json!({"ok": true}).to_string();
                respond(&mut stream, "200 OK", "application/json", body.as_bytes()).await?;
            } else {
                let body = json!({"ok": false, "error": "input pump disabled"}).to_string();
                respond(
                    &mut stream,
                    "409 Conflict",
                    "application/json",
                    body.as_bytes(),
                )
                .await?;
            }
        }
        ("GET", "/api/events") => {
            sse(stream, state, shutdown).await?;
        }
        _ => {
            respond(&mut stream, "404 Not Found", "text/plain", b"not found").await?;
        }
    }
    Ok(())
}

async fn apply_feature(
    stream: &mut tokio::net::TcpStream,
    state: &WebState,
    action: &str,
    clipboard: bool,
) -> std::io::Result<()> {
    let Some(enabled) = parse_switch(action) else {
        let body = json!({"ok": false, "error": "expected on or off"}).to_string();
        return respond(
            stream,
            "400 Bad Request",
            "application/json",
            body.as_bytes(),
        )
        .await;
    };
    let current_clipboard = state.shared.features.clipboard();
    let current_input = state.shared.features.input();
    let result = if clipboard {
        state.shared.set_features(enabled, current_input)
    } else {
        state.shared.set_features(current_clipboard, enabled)
    };
    match result {
        Ok(()) => {
            let body = json!({
                "ok": true,
                "clipboard": state.shared.features.clipboard(),
                "input_pump": state.shared.features.input(),
            })
            .to_string();
            respond(stream, "200 OK", "application/json", body.as_bytes()).await
        }
        Err(error) => {
            let body = json!({"ok": false, "error": error}).to_string();
            respond(
                stream,
                "500 Internal Server Error",
                "application/json",
                body.as_bytes(),
            )
            .await
        }
    }
}

fn parse_switch(action: &str) -> Option<bool> {
    match action {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

async fn sse(
    mut stream: tokio::net::TcpStream,
    state: WebState,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
              Connection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        )
        .await?;
    let initial = json!({"type": "snapshot", "status": state.status.snapshot()}).to_string();
    stream
        .write_all(format!("data: {initial}\n\n").as_bytes())
        .await?;
    let mut events = state.status.subscribe();
    let mut keepalive = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    let _ = stream.write_all(b"event: bye\ndata: {}\n\n").await;
                    break;
                }
            }
            event = events.recv() => {
                match event {
                    Ok(text) => {
                        stream.write_all(format!("data: {text}\n\n").as_bytes()).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = keepalive.tick() => {
                stream.write_all(b": ping\n\n").await?;
            }
        }
    }
    Ok(())
}

async fn read_request(
    stream: &mut tokio::net::TcpStream,
) -> std::io::Result<Option<(String, String, String)>> {
    let mut buffer = Vec::with_capacity(8192);
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if buffer.len() > 16 * 1024 {
            return Ok(None);
        }
    };
    let header_text = String::from_utf8_lossy(&buffer[..header_end]);
    let Some(line) = header_text.lines().next() else {
        return Ok(None);
    };
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts
        .next()
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/")
        .to_owned();
    let content_length = header_text
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0)
        .min(1024 * 1024);
    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(Some((
        method,
        path,
        String::from_utf8_lossy(&body).to_string(),
    )))
}

async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
