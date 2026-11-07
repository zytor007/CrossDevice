use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cd_clipboard::{Content, Snapshot};
use cd_geometry::presets;
use cd_proto::{Capabilities, ControlMessage, Hello, ProtocolVersion};
use cd_store::{DeviceRecord, Store};
use cd_sync::{
    pair, receive_clipboard, receive_tree, send_clipboard, send_tree, sha256_bytes, PairOutcome,
    ReceiveReport, SyncError,
};
use cd_transport::{Identity, QuicTransport};
use clap::{Args, Subcommand};

const SERVER_NAME: &str = "crossdev.local";
const CLIENT_NAME: &str = "client.crossdev.local";

#[derive(Debug, Args)]
pub struct SyncArgs {
    #[command(subcommand)]
    action: SyncAction,
}

#[derive(Debug, Subcommand)]
enum SyncAction {
    /// 双向 TLS → SAS 配对 → 剪贴板 C2 → 目录分块传输 → SQLite
    SelfTest,
}

pub fn run(args: SyncArgs) -> ExitCode {
    match args.action {
        SyncAction::SelfTest => {
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
                    eprintln!("网络同步自检失败: {error}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

struct ServerResult {
    client_hello: Hello,
    pair: PairOutcome,
    clipboard: Snapshot,
    files: ReceiveReport,
}

async fn self_test() -> Result<(), String> {
    let base = temp_base();
    let result = self_test_inner(&base).await;
    let _ = fs::remove_dir_all(&base);
    result
}

async fn self_test_inner(base: &Path) -> Result<(), String> {
    println!("网络同步自检");

    let server_identity = Identity::load_or_generate(base.join("server-identity"), SERVER_NAME)
        .map_err(|e| e.to_string())?;
    let client_identity = Identity::load_or_generate(base.join("client-identity"), CLIENT_NAME)
        .map_err(|e| e.to_string())?;
    let server = QuicTransport::server(SocketAddr::from(([127, 0, 0, 1], 0)), &server_identity)
        .map_err(|e| e.to_string())?;
    let addr = server.local_addr().map_err(|e| e.to_string())?;
    let trusted = server_identity.cert().clone();
    println!(
        "  TLS: 服务端 {} · 客户端 {}（双向证书）",
        short(&server_identity.fingerprint_hex()),
        short(&client_identity.fingerprint_hex())
    );

    let src_root = base.join("source").join("素材");
    let dest_root = base.join("dest").join("素材");
    prepare_source(&src_root)?;

    let server_dest = dest_root.clone();
    let server_task =
        tokio::spawn(async move { server_session(server, server_identity, server_dest).await });

    let mut client = QuicTransport::client().map_err(|e| e.to_string())?;
    let link = client
        .connect(addr, SERVER_NAME, &trusted, &client_identity)
        .await
        .map_err(|e| e.to_string())?;
    link.send_control(&ControlMessage::Hello(Hello {
        protocol: ProtocolVersion::CURRENT,
        device_id: "linux-4k".to_owned(),
        device_name: "HDMI-0 (Linux 4K)".to_owned(),
        platform: "linux-x11".to_owned(),
        capabilities: Capabilities::INPUT
            | Capabilities::CLIPBOARD_TEXT
            | Capabilities::FILE_TRANSFER
            | Capabilities::DIRECTORY_TRANSFER,
        monitors: vec![presets::linux_4k()],
    }))
    .await
    .map_err(|e| e.to_string())?;
    let reply = link.recv_control().await.map_err(|e| e.to_string())?;
    let ControlMessage::Hello(server_hello) = reply else {
        return Err("对端未返回 Hello".to_owned());
    };

    let client_pair = pair(&link, &client_identity, true)
        .await
        .map_err(|e| e.to_string())?;
    let snapshot = Snapshot::new(Content::text("跨设备网络剪贴板 · C2 全链路"), "linux-4k", 1);
    send_clipboard(&link, &snapshot)
        .await
        .map_err(|e| e.to_string())?;
    let sent = send_tree(&link, "sync-task", &src_root)
        .await
        .map_err(|e| e.to_string())?;

    let server_result = tokio::time::timeout(Duration::from_secs(30), server_task)
        .await
        .map_err(|_| "等待服务端超时".to_owned())?
        .map_err(|error| format!("服务端任务失败: {error}"))?
        .map_err(|error| error.to_string())?;
    let ServerResult {
        client_hello: _client_hello,
        pair: server_pair,
        clipboard,
        files,
    } = server_result;

    if client_pair.code != server_pair.code {
        return Err(format!(
            "SAS 不一致: {} / {}",
            client_pair.code, server_pair.code
        ));
    }
    println!("  SAS: {}（双端一致，双向证书校验）", client_pair.code);

    if clipboard.content != snapshot.content || clipboard.hash != snapshot.hash {
        return Err("剪贴板 C2 内容不一致".to_owned());
    }
    println!(
        "  剪贴板: text {} B · C2 往返一致 · blake3 {}",
        clipboard.size,
        short(&clipboard.hash)
    );

    if files.manifest.file_count() != 3 || files.manifest.directory_count() != 2 {
        return Err(format!(
            "接收目录结构异常: {} 文件 / {} 目录",
            files.manifest.file_count(),
            files.manifest.directory_count()
        ));
    }
    verify_dest(&src_root, &dest_root)?;
    println!(
        "  文件: {} 文件 / {} 目录 / {} B · {} chunks · {} B 传输",
        files.manifest.file_count(),
        files.manifest.directory_count(),
        files.manifest.total_bytes(),
        files.chunks,
        files.bytes
    );
    if sent.chunks == 0 {
        return Err("未发送任何文件块".to_owned());
    }

    let now = unix_ms();
    let record = DeviceRecord {
        id: server_hello.device_id.clone(),
        name: server_hello.device_name.clone(),
        cert_fingerprint: sha256_bytes(&client_pair.peer_certificate),
        public_key: client_pair.peer_certificate.clone(),
        capabilities: server_hello.capabilities.0,
        paired_at: now,
        last_seen: Some(now),
    };
    let db = base.join("devices.sqlite");
    persist_device(&db, &record)?;
    println!("  持久化: {} 已写入设备白名单", record.name);

    println!("[通过] TLS / SAS / 剪贴板 / 目录传输 / SQLite 全链路可用");

    link.close(0, b"done");
    client.close();
    Ok(())
}

async fn server_session(
    server: QuicTransport,
    identity: Identity,
    dest_root: PathBuf,
) -> Result<ServerResult, SyncError> {
    let link = server.accept().await?;
    let hello = link.recv_control().await?;
    let ControlMessage::Hello(client_hello) = hello else {
        return Err(SyncError::Protocol("expected Hello".to_owned()));
    };
    link.send_control(&ControlMessage::Hello(Hello {
        protocol: ProtocolVersion::CURRENT,
        device_id: "macbook-air-13".to_owned(),
        device_name: "MacBook Air 13.6\"".to_owned(),
        platform: "macos".to_owned(),
        capabilities: Capabilities::INPUT
            | Capabilities::CLIPBOARD_TEXT
            | Capabilities::FILE_TRANSFER
            | Capabilities::DIRECTORY_TRANSFER,
        monitors: vec![presets::macbook_air_13()],
    }))
    .await?;

    let pair = pair(&link, &identity, false).await?;
    let clipboard = receive_clipboard(&link).await?;
    let files = receive_tree(&link, &dest_root).await?;
    link.close(0, b"done");
    Ok(ServerResult {
        client_hello,
        pair,
        clipboard,
        files,
    })
}

fn prepare_source(src_root: &Path) -> Result<(), String> {
    fs::create_dir_all(src_root.join("images")).map_err(|error| error.to_string())?;
    fs::create_dir_all(src_root.join("empty")).map_err(|error| error.to_string())?;
    fs::write(
        src_root.join("notes.txt"),
        "网络目录传输\n第二行\n".as_bytes(),
    )
    .map_err(|error| error.to_string())?;
    fs::write(src_root.join("blank.txt"), b"").map_err(|error| error.to_string())?;
    let payload = (0..5 * 1024 * 1024_u32)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<u8>>();
    fs::write(src_root.join("images/payload.bin"), payload).map_err(|error| error.to_string())?;
    Ok(())
}

fn verify_dest(src_root: &Path, dest_root: &Path) -> Result<(), String> {
    for relative in ["notes.txt", "blank.txt", "images/payload.bin"] {
        let source = src_root.join(relative);
        let dest = dest_root.join(relative);
        if fs::read(&source).map_err(|error| error.to_string())?
            != fs::read(&dest).map_err(|error| error.to_string())?
        {
            return Err(format!("`{relative}` 内容不一致"));
        }
    }
    if !dest_root.join("empty").is_dir() {
        return Err("空目录未重建".to_owned());
    }
    Ok(())
}

fn persist_device(path: &Path, record: &DeviceRecord) -> Result<(), String> {
    let store = Store::open(path).map_err(|error| error.to_string())?;
    store
        .upsert_device(record)
        .map_err(|error| error.to_string())?;
    let read_back = store
        .device(&record.id)
        .map_err(|error| error.to_string())?;
    if read_back.as_ref() != Some(record) {
        return Err("设备记录读回不一致".to_owned());
    }
    Ok(())
}

fn short(hash: &str) -> String {
    let prefix = hash.chars().take(12).collect::<String>();
    format!("{prefix}…")
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn temp_base() -> PathBuf {
    std::env::temp_dir().join(format!(
        "crossdev-sync-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ))
}
