//! UDP presence broadcast. This is deliberately simpler than mDNS: every
//! daemon sends a small JSON announcement every three seconds and listens on
//! the same port. Pairing still happens over QUIC with SAS approval, so
//! discovery only supplies a candidate address.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::watch;

use super::runtime::Shared;
use super::status::DiscoveredPeerView;

const DISCOVERY_PORT: u16 = 7825;
const APP_TAG: &str = "crossdev";
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Announce {
    app: String,
    device_id: String,
    device_name: String,
    platform: String,
    quic_port: u16,
    protocol: String,
}

pub async fn run(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    let socket = match bind_discovery_socket(DISCOVERY_PORT) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("发现服务不可用: {error}");
            return;
        }
    };

    let local = shared.status.local();
    let quic_port = local
        .quic_addr
        .parse::<SocketAddr>()
        .map(|addr| addr.port())
        .unwrap_or(0);
    if quic_port == 0 {
        return;
    }
    let announcement = Announce {
        app: APP_TAG.to_owned(),
        device_id: local.device_id,
        device_name: local.device_name,
        platform: local.platform,
        quic_port,
        protocol: local.protocol,
    };
    let payload = match serde_json::to_vec(&announcement) {
        Ok(payload) => payload,
        Err(error) => {
            eprintln!("发现服务编码失败: {error}");
            return;
        }
    };

    let mut interval = tokio::time::interval(ANNOUNCE_INTERVAL);
    let mut warned = false;
    let mut buffer = vec![0_u8; 2048];
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    break;
                }
            }
            _ = interval.tick() => {
                if let Err(error) = socket
                    .send_to(&payload, ("255.255.255.255", DISCOVERY_PORT))
                    .await
                {
                    if !warned {
                        eprintln!("发送发现广播失败: {error}");
                        warned = true;
                    }
                }
            }
            received = socket.recv_from(&mut buffer) => {
                let Ok((len, source)) = received else { continue };
                let Ok(announce) = serde_json::from_slice::<Announce>(&buffer[..len]) else {
                    continue;
                };
                if announce.app != APP_TAG
                    || announce.device_id == shared.config.device_id
                    || announce.quic_port == 0
                {
                    continue;
                }
                let address = SocketAddr::new(source.ip(), announce.quic_port);
                let paired = shared
                    .store
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .device(&announce.device_id)
                    .ok()
                    .flatten()
                    .is_some();
                shared.status.note_discovered(DiscoveredPeerView {
                    device_id: announce.device_id,
                    device_name: announce.device_name,
                    platform: announce.platform,
                    address: address.to_string(),
                    paired,
                    last_seen_ms: now_ms(),
                });
            }
        }
    }
}

/// Builds the shared discovery socket. `SO_REUSEPORT` lets several daemons
/// (for example two instances on one machine) listen on the same UDP port and
/// all receive broadcast announcements.
fn build_discovery_socket(port: u16) -> Result<std::net::UdpSocket, String> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|error| format!("创建发现 UDP socket 失败: {error}"))?;
    socket
        .set_reuse_address(true)
        .map_err(|error| format!("启用地址复用失败: {error}"))?;
    #[cfg(unix)]
    socket
        .set_reuse_port(true)
        .map_err(|error| format!("启用端口复用失败: {error}"))?;
    socket
        .set_broadcast(true)
        .map_err(|error| format!("启用广播失败: {error}"))?;
    socket
        .bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())
        .map_err(|error| format!("绑定 UDP {port} 失败: {error}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|error| format!("设置非阻塞失败: {error}"))?;
    Ok(socket.into())
}

fn bind_discovery_socket(port: u16) -> Result<UdpSocket, String> {
    let socket = build_discovery_socket(port)?;
    UdpSocket::from_std(socket).map_err(|error| format!("注册 tokio UDP socket 失败: {error}"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_daemons_can_listen_on_the_same_discovery_port() {
        let base = 20_000 + (std::process::id() % 1_000) as u16;
        for offset in 0..32 {
            let port = base + offset;
            let Ok(first) = build_discovery_socket(port) else {
                continue;
            };
            let second = build_discovery_socket(port)
                .unwrap_or_else(|error| panic!("同一端口二次绑定失败 ({port}): {error}"));
            drop(second);
            drop(first);
            return;
        }
        panic!("没有可用的 UDP 端口用于端口复用测试");
    }
}
