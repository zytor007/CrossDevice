use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use cd_geometry::presets;
use cd_proto::{Capabilities, ControlMessage, Heartbeat, Hello, InputEvent, ProtocolVersion};
use cd_transport::{Identity, QuicTransport, TransportError};
use clap::{Args, Subcommand};

const TEST_NAME: &str = "crossdev.local";

#[derive(Debug, Args)]
pub struct LinkArgs {
    #[command(subcommand)]
    action: LinkAction,
}

#[derive(Debug, Subcommand)]
enum LinkAction {
    /// 本机 QUIC + TLS 回环：Hello 往返与 C1 数据报往返
    SelfTest,
}

pub fn run(args: LinkArgs) -> ExitCode {
    match args.action {
        LinkAction::SelfTest => {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("创建运行时失败: {error}");
                    return ExitCode::FAILURE;
                }
            };
            match runtime.block_on(self_test()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("QUIC 链路自检失败: {error}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

async fn self_test() -> Result<(), String> {
    let identity = Identity::generate(TEST_NAME).map_err(|e| e.to_string())?;
    let server = QuicTransport::server(SocketAddr::from(([127, 0, 0, 1], 0)), &identity)
        .map_err(|e| e.to_string())?;
    let addr = server.local_addr().map_err(|e| e.to_string())?;
    let trusted = identity.cert().clone();
    let fingerprint = identity.fingerprint_hex();

    println!("QUIC 链路自检");
    println!("  身份 {TEST_NAME} · SHA-256 {fingerprint}");
    println!("  服务端监听 {addr}");

    let event = InputEvent::move_relative_um(100_000, -80_000, 42);
    let server_task = tokio::spawn(async move {
        let link = server.accept().await?;
        let hello = link.recv_control().await?;
        let ControlMessage::Hello(hello) = hello else {
            return Err(TransportError::Protocol("expected Hello on C0".to_owned()));
        };
        println!("  收到 Hello: {} ({})", hello.device_name, hello.device_id);

        link.send_control(&ControlMessage::Hello(Hello {
            protocol: ProtocolVersion::CURRENT,
            device_id: "macbook-air-13".to_owned(),
            device_name: "MacBook Air 13.6\"".to_owned(),
            platform: "macos".to_owned(),
            capabilities: Capabilities::INPUT
                | Capabilities::CLIPBOARD_TEXT
                | Capabilities::CLIPBOARD_IMAGE
                | Capabilities::FILE_TRANSFER
                | Capabilities::DIRECTORY_TRANSFER,
            monitors: vec![presets::macbook_air_13()],
        }))
        .await?;

        link.send_input(&event)?;
        let echo = link.recv_input().await?;
        if echo != event {
            return Err(TransportError::Protocol("C1 echo mismatch".to_owned()));
        }

        link.send_control(&ControlMessage::Heartbeat(Heartbeat { seq: 1, ts_us: 1 }))
            .await?;
        let remote = link.remote_address();
        let rtt = link.rtt();
        tokio::time::sleep(Duration::from_millis(20)).await;
        link.close(0, b"done");
        Ok::<_, TransportError>((remote, rtt))
    });

    let mut client = QuicTransport::client().map_err(|e| e.to_string())?;
    let client_identity = Identity::generate("client.crossdev.local").map_err(|e| e.to_string())?;
    let link = client
        .connect(addr, TEST_NAME, &trusted, &client_identity)
        .await
        .map_err(|e| e.to_string())?;
    let max_datagram = link
        .max_datagram_size()
        .ok_or_else(|| "对端未协商 QUIC DATAGRAM".to_owned())?;

    link.send_control(&ControlMessage::Hello(Hello {
        protocol: ProtocolVersion::CURRENT,
        device_id: "linux-4k".to_owned(),
        device_name: "HDMI-0 (Linux 4K)".to_owned(),
        platform: "linux-x11".to_owned(),
        capabilities: Capabilities::INPUT
            | Capabilities::CLIPBOARD_TEXT
            | Capabilities::CLIPBOARD_FILES
            | Capabilities::FILE_TRANSFER
            | Capabilities::DIRECTORY_TRANSFER,
        monitors: vec![presets::linux_4k()],
    }))
    .await
    .map_err(|e| e.to_string())?;

    let reply = tokio::time::timeout(Duration::from_secs(3), link.recv_control())
        .await
        .map_err(|_| "等待对端 Hello 超时".to_owned())?
        .map_err(|e| e.to_string())?;
    let ControlMessage::Hello(reply) = reply else {
        return Err("对端返回的不是 Hello".to_owned());
    };
    println!("  收到 Hello: {} ({})", reply.device_name, reply.device_id);

    let echo = tokio::time::timeout(Duration::from_secs(3), link.recv_input())
        .await
        .map_err(|_| "等待 C1 数据报超时".to_owned())?
        .map_err(|e| e.to_string())?;
    if echo != event {
        return Err("C1 数据报内容不一致".to_owned());
    }
    link.send_input(&echo).map_err(|e| e.to_string())?;

    let (remote, rtt) = tokio::time::timeout(Duration::from_secs(3), server_task)
        .await
        .map_err(|_| "等待服务端结束超时".to_owned())?
        .map_err(|error| format!("服务端任务失败: {error}"))?
        .map_err(|error| error.to_string())?;

    println!(
        "  C1: dx=+{:.0} mm, dy={:.0} mm · 数据报上限 {max_datagram} B · 往返一致",
        f64::from(event.x) / 1000.0,
        f64::from(event.y) / 1000.0
    );
    println!("  RTT {:.2} ms · 对端 {remote}", rtt.as_secs_f64() * 1000.0);
    println!("[通过] TLS 1.3 握手、C0 控制流与 C1 DATAGRAM 均可用");

    link.close(0, b"done");
    client.close();
    Ok(())
}
