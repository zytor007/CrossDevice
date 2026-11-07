use std::net::SocketAddr;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cd_clipboard::{ClipboardPolicy, Content, LoopGuard, RemoteDecision, Snapshot};
use cd_platform::SystemClipboard;
use cd_proto::{ClipboardKind, ControlMessage, DataHeader, Heartbeat};
use cd_transport::{Identity, QuicTransport, TransportError};
use clap::{Args, Subcommand};

const TEST_NAME: &str = "crossdev.local";

#[derive(Debug, Args)]
pub struct ClipboardArgs {
    #[command(subcommand)]
    action: ClipboardAction,
}

#[derive(Debug, Subcommand)]
enum ClipboardAction {
    /// 读取系统剪贴板文本
    Get,
    /// 写入系统剪贴板文本
    Set {
        #[arg(long)]
        text: String,
        /// 保持选区 owner 的秒数（X11 无剪贴板管理器时需要）
        #[arg(long, default_value_t = 0)]
        hold_seconds: u64,
    },
    /// 真实系统剪贴板写入/读取闭环（会临时改写并恢复文本）
    SystemTest,
    /// 剪贴板内容/防回环/策略 + QUIC C2 往返
    SelfTest,
}

pub fn run(args: ClipboardArgs) -> ExitCode {
    match args.action {
        ClipboardAction::Get => system_get(),
        ClipboardAction::Set { text, hold_seconds } => system_set(&text, hold_seconds),
        ClipboardAction::SystemTest => system_test(),
        ClipboardAction::SelfTest => {
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
                    eprintln!("剪贴板自检失败: {error}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}

fn system_get() -> ExitCode {
    let mut clipboard = match SystemClipboard::new() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            eprintln!("打开系统剪贴板失败: {error}");
            return ExitCode::FAILURE;
        }
    };
    match clipboard.read_text() {
        Ok(Some(text)) => println!("{text}"),
        Ok(None) => println!("（剪贴板为空或不是文本）"),
        Err(error) => {
            eprintln!("读取系统剪贴板失败: {error}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn system_set(text: &str, hold_seconds: u64) -> ExitCode {
    let mut clipboard = match SystemClipboard::new() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            eprintln!("打开系统剪贴板失败: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = clipboard.write_text(text) {
        eprintln!("写入系统剪贴板失败: {error}");
        return ExitCode::FAILURE;
    }
    println!("已写入系统剪贴板（{} 字符）", text.chars().count());
    if hold_seconds > 0 {
        println!("保持选区 {hold_seconds} 秒，请在此时段内粘贴…");
        thread::sleep(Duration::from_secs(hold_seconds));
    } else {
        println!(
            "提示: 独立命令退出后，X11 无剪贴板管理器时内容可能被清除；常驻 daemon 会长期持有。"
        );
    }
    ExitCode::SUCCESS
}

fn system_test() -> ExitCode {
    let mut clipboard = match SystemClipboard::new() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            eprintln!("打开系统剪贴板失败: {error}");
            return ExitCode::FAILURE;
        }
    };
    let previous = match clipboard.read_text() {
        Ok(previous) => previous,
        Err(error) => {
            eprintln!("读取原剪贴板失败: {error}");
            return ExitCode::FAILURE;
        }
    };
    let marker = format!(
        "crossdev clipboard probe {}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0)
    );

    let result = (|| -> Result<(), String> {
        clipboard
            .write_text(&marker)
            .map_err(|error| error.to_string())?;
        let actual = clipboard.read_text().map_err(|error| error.to_string())?;
        if actual.as_deref() != Some(marker.as_str()) {
            return Err(format!("读回内容不一致: {actual:?}"));
        }
        if let Some(previous) = previous {
            clipboard
                .write_text(&previous)
                .map_err(|error| error.to_string())?;
        } else {
            clipboard.clear().map_err(|error| error.to_string())?;
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            println!("系统剪贴板闭环: 写入 → 读回一致 → 已恢复原内容");
            println!("[通过] X11 系统剪贴板文本读写可用");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("系统剪贴板自检失败: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn self_test() -> Result<(), String> {
    println!("剪贴板管道自检");

    let content = Content::text("跨设备剪贴板 C2 · 2026-11-06");
    let snapshot = Snapshot::new(content.clone(), "linux-4k", 1);
    println!(
        "  内容 {} · {} B · blake3 {}",
        kind_label(snapshot.content.kind()),
        snapshot.size,
        short_hash(&snapshot.hash)
    );
    println!("  预览: {}", snapshot.content.preview());

    let mut guard = LoopGuard::new();
    let apply = guard.evaluate_remote(&snapshot.announce(), 10);
    let duplicate = guard.evaluate_remote(&snapshot.announce(), 20);
    let stale = guard.evaluate_remote(
        &cd_proto::ClipboardAnnounce {
            hash: "bb".repeat(32),
            ..snapshot.announce()
        },
        30,
    );
    guard.note_applied_remote(snapshot.hash.clone(), 100);
    let suppressed = !guard.should_publish_local(&snapshot.hash, 300);
    let expired = guard.should_publish_local(&snapshot.hash, 10_300);
    if apply != RemoteDecision::Apply
        || duplicate != RemoteDecision::Duplicate
        || stale != RemoteDecision::Stale
        || !suppressed
        || !expired
    {
        return Err("防回环状态机结果不符合预期".to_owned());
    }
    println!("  防回环: apply / duplicate / stale / 回声抑制 / 10s 窗口 均通过");

    let policy = ClipboardPolicy::default();
    policy.validate().map_err(|error| error.to_string())?;
    if !policy.should_auto_fetch(64 * 1024 * 1024)
        || policy.should_auto_fetch(64 * 1024 * 1024 + 1)
        || !policy.is_excluded("1Password 8")
    {
        return Err("剪贴板策略校验失败".to_owned());
    }
    println!(
        "  策略: 自动拉取 ≤ {} MB · 历史 {} 条 · 密码管理器排除",
        policy.auto_fetch_bytes / (1024 * 1024),
        policy.history_limit
    );

    let identity = Identity::generate(TEST_NAME).map_err(|e| e.to_string())?;
    let server = QuicTransport::server(SocketAddr::from(([127, 0, 0, 1], 0)), &identity)
        .map_err(|e| e.to_string())?;
    let addr = server.local_addr().map_err(|e| e.to_string())?;
    let trusted = identity.cert().clone();

    let expected_content = content.clone();
    let expected_hash = snapshot.hash.clone();
    let header = DataHeader::Clipboard(snapshot.announce());
    let payload = snapshot.payload();

    let server_task = tokio::spawn(async move {
        let link = server.accept().await?;
        let hello = link.recv_control().await?;
        let ControlMessage::Heartbeat(_) = hello else {
            return Err(TransportError::Protocol(
                "expected Heartbeat on C0".to_owned(),
            ));
        };
        let mut data = link.accept_data().await?;
        let transfer = data.recv().await?;
        let DataHeader::Clipboard(announce) = &transfer.header else {
            return Err(TransportError::Protocol(
                "expected clipboard header".to_owned(),
            ));
        };
        let received = Content::decode(announce.kind, &transfer.payload)
            .map_err(|error| TransportError::Protocol(error.to_string()))?;
        if received != expected_content || received.hash_hex() != expected_hash {
            return Err(TransportError::Protocol(
                "clipboard content mismatch".to_owned(),
            ));
        }
        Ok::<_, TransportError>(transfer.payload.len())
    });

    let mut client = QuicTransport::client().map_err(|e| e.to_string())?;
    let client_identity = Identity::generate("client.crossdev.local").map_err(|e| e.to_string())?;
    let link = client
        .connect(addr, TEST_NAME, &trusted, &client_identity)
        .await
        .map_err(|e| e.to_string())?;
    link.send_control(&ControlMessage::Heartbeat(Heartbeat { seq: 1, ts_us: 1 }))
        .await
        .map_err(|e| e.to_string())?;

    let mut data = link.open_data().await.map_err(|e| e.to_string())?;
    data.send(&header, &payload)
        .await
        .map_err(|e| e.to_string())?;

    let received_len = tokio::time::timeout(Duration::from_secs(3), server_task)
        .await
        .map_err(|_| "等待 C2 接收方超时".to_owned())?
        .map_err(|error| format!("C2 接收任务失败: {error}"))?
        .map_err(|error| error.to_string())?;
    if received_len != payload.len() {
        return Err("C2 载荷长度不一致".to_owned());
    }

    println!(
        "  C2: {} 头 + {} B 载荷 · QUIC 往返内容一致",
        kind_label(snapshot.content.kind()),
        payload.len()
    );
    println!("[通过] 剪贴板管道、防回环与 C2 数据通道均可用");

    link.close(0, b"done");
    client.close();
    Ok(())
}

fn kind_label(kind: ClipboardKind) -> &'static str {
    match kind {
        ClipboardKind::Text => "text",
        ClipboardKind::Html => "html",
        ClipboardKind::Image => "image",
        ClipboardKind::FileList => "files",
        ClipboardKind::DirectoryTree => "directory",
    }
}

fn short_hash(hash: &str) -> String {
    let prefix = hash.chars().take(16).collect::<String>();
    format!("{prefix}…")
}
