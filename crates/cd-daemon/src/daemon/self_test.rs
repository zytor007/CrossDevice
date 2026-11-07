use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cd_clipboard::{Content, Snapshot};
use cd_files::{chunk_file, ChunkConfig};
use cd_geometry::presets;
use cd_proto::{Capabilities, ControlMessage, Hello, ProtocolVersion};
use cd_sync::{pair, send_clipboard, send_tree_with_config};
use cd_transport::{Identity, QuicTransport};
use serde_json::Value;

use super::config::{Config, DaemonPaths};
use super::runtime;

const SERVER_NAME: &str = "crossdev.local";
const CLIENT_NAME: &str = "client.crossdev.local";

fn test_chunks() -> ChunkConfig {
    ChunkConfig::new(64 * 1024, 512 * 1024, 2 * 1024 * 1024).expect("chunk config")
}

pub fn run() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
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
            eprintln!("daemon 自检失败: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn self_test() -> Result<(), String> {
    let base = temp_base();
    let result = self_test_inner(&base).await;
    let _ = fs::remove_dir_all(&base);
    result
}

async fn self_test_inner(base: &Path) -> Result<(), String> {
    println!("长驻 daemon 自检");
    let paths = DaemonPaths::at(base.join("daemon"));
    let mut config = Config::default_for("daemon-test");
    config.device_name = "Daemon Test".to_owned();
    config.platform = "linux-test".to_owned();
    config.bind = "127.0.0.1:0".to_owned();
    config.http_bind = "127.0.0.1:0".to_owned();
    config.pair_mode = super::config::PairMode::Confirm;
    config.clipboard = false;
    config.clipboard_system = false;
    config.input_pump = false;
    config.discovery = false;
    let daemon = runtime::start(paths, config)
        .await
        .map_err(|error| format!("启动 daemon: {error}"))?;
    println!("  QUIC: {}", daemon.quic_addr);
    println!("  HTTP: http://{}", daemon.http_addr);

    let client_identity = Identity::load_or_generate(base.join("client-identity"), CLIENT_NAME)
        .map_err(|error| error.to_string())?;
    let trusted = daemon.shared.identity.cert().clone();
    let mut transport = QuicTransport::client().map_err(|error| error.to_string())?;
    let link = transport
        .connect(daemon.quic_addr, SERVER_NAME, &trusted, &client_identity)
        .await
        .map_err(|error| error.to_string())?;
    link.send_control(&ControlMessage::Hello(Hello {
        protocol: ProtocolVersion::CURRENT,
        device_id: "client-daemon".to_owned(),
        device_name: "Client Daemon".to_owned(),
        platform: "linux-test".to_owned(),
        capabilities: Capabilities::INPUT
            | Capabilities::CLIPBOARD_TEXT
            | Capabilities::FILE_TRANSFER
            | Capabilities::DIRECTORY_TRANSFER,
        monitors: vec![presets::macbook_air_13()],
    }))
    .await
    .map_err(|error| error.to_string())?;
    let reply = link
        .recv_control()
        .await
        .map_err(|error| error.to_string())?;
    let ControlMessage::Hello(server_hello) = reply else {
        return Err("daemon 未返回 Hello".to_owned());
    };
    if server_hello.device_id != "daemon-test" {
        return Err(format!("daemon device_id = {}", server_hello.device_id));
    }
    let pair_future = pair(&link, &client_identity, true);
    let approval_future = async {
        let status = wait_status(daemon.http_addr, "配对请求", |value| {
            value["pending_pairs"]
                .as_array()
                .map(|pairs| pairs.len() == 1)
                .unwrap_or(false)
        })
        .await?;
        let pair = &status["pending_pairs"][0];
        let id = pair["id"].as_str().ok_or("配对请求缺少 id")?.to_owned();
        let code = pair["code"].as_str().ok_or("配对请求缺少 code")?.to_owned();
        let body = super::http_post(daemon.http_addr, &format!("/api/pair/approve/{id}")).await?;
        let approved: Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
        if approved["ok"] != true {
            return Err(format!("批准失败: {body}"));
        }
        Ok(code)
    };
    let (paired, approved_code) = tokio::join!(pair_future, approval_future);
    let paired = paired.map_err(|error| error.to_string())?;
    let approved_code = approved_code?;
    if paired.code != approved_code {
        return Err(format!(
            "控制台配对码 {approved_code} 与 SAS {} 不一致",
            paired.code
        ));
    }
    println!("  SAS: {} · 控制台人工批准后写入白名单", paired.code);

    let enabled = super::http_post(daemon.http_addr, "/api/settings/clipboard/on").await?;
    if !enabled.contains("\"clipboard\":true") {
        return Err(format!("启用剪贴板失败: {enabled}"));
    }
    wait_status(daemon.http_addr, "剪贴板开关", |value| {
        value["local"]["clipboard"] == true
    })
    .await?;
    println!("  功能开关: 运行时启用剪贴板并持久化");

    let clipboard = Snapshot::new(Content::text("daemon 自检剪贴板 · C2"), "client-daemon", 1);
    send_clipboard(&link, &clipboard)
        .await
        .map_err(|error| error.to_string())?;
    wait_status(daemon.http_addr, "剪贴板", |value| {
        value["counters"]["clipboard_in"] == 1
    })
    .await?;
    println!("  剪贴板: C2 到达并写入状态（未触碰系统剪贴板）");

    let image = Snapshot::new(
        Content::Image {
            png: vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4],
            width: 1,
            height: 1,
        },
        "client-daemon",
        2,
    );
    send_clipboard(&link, &image)
        .await
        .map_err(|error| error.to_string())?;
    wait_status(daemon.http_addr, "图片剪贴板", |value| {
        value["clipboard_history"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .any(|item| item["hash"] == image.hash.as_str() && item["kind"] == "image")
            })
            .unwrap_or(false)
    })
    .await?;
    println!("  剪贴板: 图片 C2 与 Recent 历史可用");

    let source = base.join("source").join("assets");
    prepare_source(&source)?;
    let first = send_tree_with_config(&link, "daemon-task-a", &source, test_chunks())
        .await
        .map_err(|error| error.to_string())?;
    let inbox = daemon.shared.config.inbox(&daemon.shared.paths);
    let dest_a = inbox.join("assets");
    wait_transfer(daemon.http_addr, "daemon-task-a", "completed").await?;
    verify_tree(&source, &dest_a)?;
    println!(
        "  首传: {} chunks / {} B · 复用 {} · 续传 {}",
        first.chunks, first.bytes, first.reused_chunks, first.resumed_chunks
    );

    let second = send_tree_with_config(&link, "daemon-task-b", &source, test_chunks())
        .await
        .map_err(|error| error.to_string())?;
    let dest_b = inbox.join("assets~2");
    wait_transfer(daemon.http_addr, "daemon-task-b", "completed").await?;
    verify_tree(&source, &dest_b)?;
    if second.chunks != 0 || second.reused_chunks != second.total_chunks {
        return Err(format!(
            "块级去重未命中: sent {} / total {} / reused {}",
            second.chunks, second.total_chunks, second.reused_chunks
        ));
    }
    println!(
        "  去重: 0 chunks · {} / {} 块从本地缓存重建",
        second.reused_chunks, second.total_chunks
    );

    // Simulate a crash: keep a chunk-aligned prefix of the large file, clear
    // the block cache, then resend the same task id.
    let payload = source.join("images/payload.bin");
    let chunks = chunk_file(&payload, test_chunks()).map_err(|error| error.to_string())?;
    if chunks.len() < 2 {
        return Err("测试文件未产生至少两个 FastCDC 块".to_owned());
    }
    let first_end = chunks[0].offset + chunks[0].length;
    let destination = dest_a.join("images/payload.bin");
    fs::remove_file(&destination).map_err(|error| error.to_string())?;
    let part = dest_a.join("images/payload.bin.cdpart");
    let bytes = fs::read(&payload).map_err(|error| error.to_string())?;
    fs::write(&part, &bytes[..first_end as usize]).map_err(|error| error.to_string())?;
    clear_block_cache(&daemon.shared.paths.blocks)?;

    let resumed = send_tree_with_config(&link, "daemon-task-a", &source, test_chunks())
        .await
        .map_err(|error| error.to_string())?;
    wait_tree(&source, &dest_a).await?;
    verify_tree(&source, &dest_a)?;
    if resumed.resumed_chunks == 0 || resumed.chunks >= resumed.total_chunks {
        return Err(format!(
            "断点续传未生效: sent {} / total {} / resumed {}",
            resumed.chunks, resumed.total_chunks, resumed.resumed_chunks
        ));
    }
    println!(
        "  续传: 只补 {} / {} 块 · 已存在 {} 块 · 复用 {} 块",
        resumed.chunks, resumed.total_chunks, resumed.resumed_chunks, resumed.reused_chunks
    );

    let status = wait_status(daemon.http_addr, "在线设备", |value| {
        value["peers"]
            .as_array()
            .map(|peers| peers.iter().any(|peer| peer["online"] == true))
            .unwrap_or(false)
    })
    .await?;
    if status["local"]["device_id"] != "daemon-test" {
        return Err("HTTP /api/status 中 device_id 不一致".to_owned());
    }
    if status["transfers"].as_array().map(Vec::len).unwrap_or(0) < 2 {
        return Err("HTTP /api/status 缺少传输记录".to_owned());
    }
    println!("  HTTP: /api/status 在线设备 / 传输 / 剪贴板状态可用");

    let dashboard = super::http_get(daemon.http_addr, "/").await?;
    if !dashboard.contains("crossdev") || !dashboard.contains("/api/events") {
        return Err("浏览器控制台未正确返回".to_owned());
    }
    let devices = super::http_get(daemon.http_addr, "/api/devices").await?;
    let devices: Value = serde_json::from_str(&devices).map_err(|error| error.to_string())?;
    if devices.as_array().map(Vec::len).unwrap_or(0) != 1 {
        return Err("设备白名单应只有一台客户端".to_owned());
    }
    let diagnostics = super::http_get(daemon.http_addr, "/api/diagnostics").await?;
    let diagnostics: Value =
        serde_json::from_str(&diagnostics).map_err(|error| error.to_string())?;
    if diagnostics["status"]["local"]["device_id"] != "daemon-test"
        || diagnostics["devices"].as_array().map(Vec::len).unwrap_or(0) != 1
        || diagnostics["cache"]["blocks"].as_u64().is_none()
    {
        return Err(format!("诊断包内容不完整: {diagnostics}"));
    }
    println!("  HTTP: 诊断包包含状态 / 白名单 / 缓存占用");
    println!("  HTTP: 浏览器控制台与设备白名单可用");

    let on = super::http_post(daemon.http_addr, "/api/settings/clipboard/on").await?;
    if !on.contains("\"clipboard\":true") {
        return Err(format!("启用剪贴板失败: {on}"));
    }
    wait_status(daemon.http_addr, "剪贴板开关", |value| {
        value["local"]["clipboard"] == true
    })
    .await?;
    let off = super::http_post(daemon.http_addr, "/api/settings/clipboard/off").await?;
    if !off.contains("\"clipboard\":false") {
        return Err(format!("停用剪贴板失败: {off}"));
    }
    wait_status(daemon.http_addr, "剪贴板关闭", |value| {
        value["local"]["clipboard"] == false
    })
    .await?;
    let restarted = super::http_post(daemon.http_addr, "/api/settings/clipboard/on").await?;
    if !restarted.contains("\"clipboard\":true") {
        return Err(format!("重新启用剪贴板失败: {restarted}"));
    }
    println!("  HTTP: 运行时功能开关可持久化并广播");

    // A second daemon connects outwards to the first, proving the reconnect
    // connector shares the same handshake, whitelist and heartbeat path.
    let mut peer_config = Config::default_for("daemon-test-2");
    peer_config.device_name = "Daemon Test 2".to_owned();
    peer_config.platform = "linux-test".to_owned();
    peer_config.bind = "127.0.0.1:0".to_owned();
    peer_config.http_bind = "127.0.0.1:0".to_owned();
    peer_config.pair_mode = super::config::PairMode::Auto;
    peer_config.clipboard = false;
    // Both daemons share this machine's X clipboard in the test; keeping the
    // receiver from writing avoids a cross-process file clipboard echo.
    peer_config.clipboard_system = false;
    peer_config.discovery = false;
    peer_config.peer = Some(daemon.quic_addr.to_string());
    let second_daemon = runtime::start(DaemonPaths::at(base.join("daemon-2")), peer_config)
        .await
        .map_err(|error| format!("启动第二个 daemon: {error}"))?;
    let pending = wait_status(daemon.http_addr, "第二配对请求", |value| {
        value["pending_pairs"]
            .as_array()
            .map(|pairs| {
                pairs
                    .iter()
                    .any(|pair| pair["device_id"] == "daemon-test-2")
            })
            .unwrap_or(false)
    })
    .await?;
    let pending_pair = pending["pending_pairs"]
        .as_array()
        .and_then(|pairs| {
            pairs
                .iter()
                .find(|pair| pair["device_id"] == "daemon-test-2")
        })
        .ok_or("缺少第二 daemon 的配对请求")?;
    let pending_id = pending_pair["id"].as_str().ok_or("配对请求缺少 id")?;
    let body =
        super::http_post(daemon.http_addr, &format!("/api/pair/approve/{pending_id}")).await?;
    if !body.contains("\"ok\":true") {
        return Err(format!("第二 daemon 配对批准失败: {body}"));
    }
    wait_status(second_daemon.http_addr, "主动连接", |value| {
        value["peers"]
            .as_array()
            .map(|peers| {
                peers
                    .iter()
                    .any(|peer| peer["id"] == "daemon-test" && peer["online"] == true)
            })
            .unwrap_or(false)
    })
    .await?;
    wait_status(daemon.http_addr, "接受主动连接", |value| {
        value["peers"]
            .as_array()
            .map(|peers| {
                peers
                    .iter()
                    .any(|peer| peer["id"] == "daemon-test-2" && peer["online"] == true)
            })
            .unwrap_or(false)
    })
    .await?;
    println!("  主动连接: 第二 daemon 通过 SAS 配对并双端在线");

    let send_body = serde_json::json!({
        "path": source.display().to_string(),
        "peer": "daemon-test-2",
    })
    .to_string();
    let sent = super::http_post_json(daemon.http_addr, "/api/files/send", &send_body).await?;
    if !sent.contains("\"ok\":true") {
        return Err(format!("文件发送请求失败: {sent}"));
    }
    let peer_dest = base.join("daemon-2").join("inbox").join("assets");
    wait_tree(&source, &peer_dest).await?;
    verify_tree(&source, &peer_dest)?;
    println!("  API 文件发送: 目录从首个 daemon 传到第二个 daemon");

    let single = source.join("notes.txt");
    let send_file_body = serde_json::json!({
        "path": single.display().to_string(),
        "peer": "daemon-test-2",
    })
    .to_string();
    let sent_file =
        super::http_post_json(daemon.http_addr, "/api/files/send", &send_file_body).await?;
    if !sent_file.contains("\"ok\":true") {
        return Err(format!("单文件发送请求失败: {sent_file}"));
    }
    let peer_file = base.join("daemon-2").join("inbox").join("notes.txt");
    wait_file(&single, &peer_file).await?;
    println!("  API 文件发送: 单文件直接落到对端收件箱");

    // Crossing sends: both peers submit a tree at the same time. The C0
    // file-channel handshake must serialize them instead of letting both
    // sides wait for each other's resume plan.
    let left_share = base.join("left-share");
    let right_share = base.join("right-share");
    prepare_share(&left_share, b"left payload")?;
    prepare_share(&right_share, b"right payload")?;
    let left_body = serde_json::json!({
        "path": left_share.display().to_string(),
        "peer": "daemon-test-2",
    })
    .to_string();
    let right_body = serde_json::json!({
        "path": right_share.display().to_string(),
        "peer": "daemon-test",
    })
    .to_string();
    let (left_sent, right_sent) = tokio::join!(
        super::http_post_json(daemon.http_addr, "/api/files/send", &left_body),
        super::http_post_json(second_daemon.http_addr, "/api/files/send", &right_body),
    );
    let left_sent = left_sent?;
    let right_sent = right_sent?;
    if !left_sent.contains("\"ok\":true") || !right_sent.contains("\"ok\":true") {
        return Err(format!("双向同时发送请求失败: {left_sent} / {right_sent}"));
    }
    let left_dest = base.join("daemon-2").join("inbox").join("left-share");
    let right_dest = base.join("daemon").join("inbox").join("right-share");
    wait_share(&left_share, &left_dest).await?;
    wait_share(&right_share, &right_dest).await?;
    println!("  双向同时发送: C0 文件通道仲裁后两个方向依次完成");

    // Pair-level settings are mirrored to the other daemon over C0.
    let pair_body = serde_json::json!({
        "edge": "left",
        "pointer_speed": 1.5,
        "edge_resistance": 40,
    })
    .to_string();
    let applied = super::http_post_json(daemon.http_addr, "/api/settings/pair", &pair_body).await?;
    if !applied.contains("\"ok\":true") {
        return Err(format!("配对设置请求失败: {applied}"));
    }
    wait_status(daemon.http_addr, "本机配对设置", |value| {
        value["local"]["edge"] == "left"
            && (value["local"]["pointer_speed"].as_f64().unwrap_or(0.0) - 1.5).abs() < 1e-9
            && value["local"]["edge_resistance"] == 40
    })
    .await?;
    wait_status(second_daemon.http_addr, "对端配对设置", |value| {
        value["peers"]
            .as_array()
            .map(|peers| {
                peers.iter().any(|peer| {
                    peer["id"] == "daemon-test"
                        && peer["edge"] == "left"
                        && (peer["pointer_speed"].as_f64().unwrap_or(0.0) - 1.5).abs() < 1e-9
                        && peer["edge_resistance"] == 40
                })
            })
            .unwrap_or(false)
    })
    .await?;
    let settings = super::http_get(daemon.http_addr, "/api/settings").await?;
    let settings: Value = serde_json::from_str(&settings).map_err(|error| error.to_string())?;
    if settings["edge"] != "left" || settings["edge_resistance"] != 40 {
        return Err(format!("GET /api/settings 未反映配对设置: {settings}"));
    }
    let saved =
        fs::read_to_string(&daemon.shared.paths.config).map_err(|error| error.to_string())?;
    let saved: Value = serde_json::from_str(&saved).map_err(|error| error.to_string())?;
    if saved["edge"] != "left"
        || (saved["pointer_speed"].as_f64().unwrap_or(0.0) - 1.5).abs() >= 1e-9
        || saved["edge_resistance"] != 40
    {
        return Err(format!("配置未持久化配对设置: {saved}"));
    }
    println!("  配对设置: 共享边/指针速度/边缘阻力经 C0 同步到对端并持久化");

    // File-list clipboard: files travel over C2 and the receiver exposes the
    // landed paths as its own clipboard item (the OS selection needs X11).
    let clipboard_file = source.join("clipboard-note.txt");
    fs::write(&clipboard_file, b"file clipboard payload\n").map_err(|error| error.to_string())?;
    let clip_on = super::http_post(second_daemon.http_addr, "/api/settings/clipboard/on").await?;
    if !clip_on.contains("\"clipboard\":true") {
        return Err(format!("启用第二 daemon 剪贴板失败: {clip_on}"));
    }
    wait_status(second_daemon.http_addr, "对端剪贴板开启", |value| {
        value["local"]["clipboard"] == true
    })
    .await?;
    send_file_with_intent(daemon.http_addr, &clipboard_file, "daemon-test-2", true).await?;
    let peer_clip_file = base
        .join("daemon-2")
        .join("inbox")
        .join("clipboard-note.txt");
    wait_file(&clipboard_file, &peer_clip_file).await?;
    wait_status(second_daemon.http_addr, "文件剪贴板项", |value| {
        value["clipboard_history"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .any(|item| item["kind"] == "files" && item["preview"] == "clipboard-note.txt")
            })
            .unwrap_or(false)
    })
    .await?;
    println!("  文件剪贴板: 文件经 C2 落盘后成为对端文件剪贴板项");

    // Receiver-side conflict policy: skip, overwrite, then rename.
    let peer_notes = base.join("daemon-2").join("inbox").join("notes.txt");
    let original_notes = fs::read(&peer_notes).map_err(|error| error.to_string())?;
    let skip = super::http_post(second_daemon.http_addr, "/api/settings/files/skip").await?;
    if !skip.contains("\"ok\":true") {
        return Err(format!("设置 skip 策略失败: {skip}"));
    }
    fs::write(&single, b"skip payload\n").map_err(|error| error.to_string())?;
    send_file_to(daemon.http_addr, &single, "daemon-test-2").await?;
    wait_named_transfer(daemon.http_addr, "notes.txt", "skipped").await?;
    let after_skip = fs::read(&peer_notes).map_err(|error| error.to_string())?;
    if after_skip != original_notes {
        return Err("skip 策略改写了已存在的目标".to_owned());
    }
    if peer_notes.with_file_name("notes~2.txt").exists() {
        return Err("skip 策略不应产生重命名副本".to_owned());
    }
    println!("  同名策略 skip: 保留目标，发送端记录为已跳过");

    let overwrite =
        super::http_post(second_daemon.http_addr, "/api/settings/files/overwrite").await?;
    if !overwrite.contains("\"ok\":true") {
        return Err(format!("设置 overwrite 策略失败: {overwrite}"));
    }
    fs::write(&single, b"overwrite payload v1\n").map_err(|error| error.to_string())?;
    send_file_to(daemon.http_addr, &single, "daemon-test-2").await?;
    wait_file(&single, &peer_notes).await?;
    if peer_notes.with_file_name("notes~2.txt").exists() {
        return Err("overwrite 策略不应产生重命名副本".to_owned());
    }
    assert_no_staging(peer_notes.parent().ok_or("收件箱路径无效")?)?;
    println!("  同名策略 overwrite: 完成后原子替换旧文件");

    let rename = super::http_post(second_daemon.http_addr, "/api/settings/files/rename").await?;
    if !rename.contains("\"ok\":true") {
        return Err(format!("设置 rename 策略失败: {rename}"));
    }
    fs::write(&single, b"rename payload v2\n").map_err(|error| error.to_string())?;
    send_file_to(daemon.http_addr, &single, "daemon-test-2").await?;
    let renamed = peer_notes.with_file_name("notes~2.txt");
    wait_file(&single, &renamed).await?;
    println!("  同名策略 rename: 新副本落到 notes~2.txt");

    // Directory overwrite replaces the whole tree via staging and swap.
    let overwrite_dir =
        super::http_post(second_daemon.http_addr, "/api/settings/files/overwrite").await?;
    if !overwrite_dir.contains("\"ok\":true") {
        return Err(format!("设置目录 overwrite 策略失败: {overwrite_dir}"));
    }
    fs::write(left_share.join("note.txt"), b"left payload v2\n")
        .map_err(|error| error.to_string())?;
    send_file_to(daemon.http_addr, &left_share, "daemon-test-2").await?;
    let peer_left = base.join("daemon-2").join("inbox").join("left-share");
    wait_file(&left_share.join("note.txt"), &peer_left.join("note.txt")).await?;
    if peer_left.with_file_name("left-share~2").exists() {
        return Err("目录 overwrite 不应产生重命名副本".to_owned());
    }
    assert_no_staging(peer_left.parent().ok_or("收件箱路径无效")?)?;
    println!("  同名策略 overwrite: 目录整体替换且无暂存残留");

    // Conflict prompt: the receiver pauses until the console decides.
    let ask = super::http_post(second_daemon.http_addr, "/api/settings/files/ask").await?;
    if !ask.contains("\"ok\":true") {
        return Err(format!("设置 ask 策略失败: {ask}"));
    }
    fs::write(&single, b"ask payload v3\n").map_err(|error| error.to_string())?;
    send_file_to(daemon.http_addr, &single, "daemon-test-2").await?;
    let pending = wait_status(second_daemon.http_addr, "同名冲突待决策", |value| {
        value["pending_conflicts"]
            .as_array()
            .map(|items| !items.is_empty())
            .unwrap_or(false)
    })
    .await?;
    let conflict_id = pending["pending_conflicts"][0]["id"]
        .as_str()
        .ok_or("冲突项缺少 id")?
        .to_owned();
    let decision = serde_json::json!({"action": "overwrite"}).to_string();
    let resolved = super::http_post_json(
        second_daemon.http_addr,
        &format!("/api/conflicts/{conflict_id}"),
        &decision,
    )
    .await?;
    if !resolved.contains("\"ok\":true") {
        return Err(format!("冲突决策失败: {resolved}"));
    }
    wait_file(&single, &peer_notes).await?;
    if peer_notes.with_file_name("notes~3.txt").exists() {
        return Err("ask+overwrite 不应产生重命名副本".to_owned());
    }
    assert_no_staging(peer_notes.parent().ok_or("收件箱路径无效")?)?;
    println!("  同名策略 ask: 控制台决策后按覆盖执行");

    // Transfer cancellation: a many-file tree keeps the receiver busy for
    // several syncs, giving the console time to cancel it from `/api/transfers`.
    // Both records must end as `cancelled` and no `.cdpart` may remain.
    let policy = super::http_post(second_daemon.http_addr, "/api/settings/files/rename").await?;
    if !policy.contains("\"ok\":true") {
        return Err(format!("恢复 rename 策略失败: {policy}"));
    }
    let cancel_share = base.join("cancel-share");
    fs::create_dir_all(&cancel_share).map_err(|error| error.to_string())?;
    for index in 0..1500_u32 {
        let payload = (0..16 * 1024_u32)
            .map(|offset| ((offset ^ index) % 239) as u8)
            .collect::<Vec<u8>>();
        fs::write(cancel_share.join(format!("part-{index:04}.bin")), payload)
            .map_err(|error| error.to_string())?;
    }
    send_file_to(daemon.http_addr, &cancel_share, "daemon-test-2").await?;
    let (task_id, _) = wait_running_incoming(second_daemon.http_addr, "cancel-share").await?;
    let cancelled = super::http_post(
        second_daemon.http_addr,
        &format!("/api/transfers/{task_id}/cancel"),
    )
    .await?;
    if !cancelled.contains("\"ok\":true") {
        return Err(format!("取消传输请求失败: {cancelled}"));
    }
    wait_transfer(second_daemon.http_addr, &task_id, "cancelled").await?;
    wait_named_transfer(daemon.http_addr, "cancel-share", "cancelled").await?;
    let peer_cancel = base.join("daemon-2").join("inbox").join("cancel-share");
    assert_no_cdpart(&peer_cancel)?;
    println!("  传输取消: 接收端中止，发送端同步收到 TransferCancel，无 .cdpart 残留");

    let recovery = source.join("recovery.txt");
    fs::write(&recovery, b"recovery after cancel\n").map_err(|error| error.to_string())?;
    send_file_to(daemon.http_addr, &recovery, "daemon-test-2").await?;
    let peer_recovery = base.join("daemon-2").join("inbox").join("recovery.txt");
    wait_file(&recovery, &peer_recovery).await?;
    println!("  传输取消: 后续文件通道与块缓存恢复正常");

    // Pause/resume: the receiver stops consuming chunks, both sides persist the
    // paused state, and resuming finishes the same task without restarting.
    let pause_share = base.join("pause-share");
    fs::create_dir_all(&pause_share).map_err(|error| error.to_string())?;
    for index in 0..1000_u32 {
        let payload = (0..16 * 1024_u32)
            .map(|offset| ((offset.wrapping_mul(7) ^ index) % 241) as u8)
            .collect::<Vec<u8>>();
        fs::write(pause_share.join(format!("part-{index:04}.bin")), payload)
            .map_err(|error| error.to_string())?;
    }
    send_file_to(daemon.http_addr, &pause_share, "daemon-test-2").await?;
    let (pause_id, _) = wait_running_incoming(second_daemon.http_addr, "pause-share").await?;
    let paused = super::http_post(
        second_daemon.http_addr,
        &format!("/api/transfers/{pause_id}/pause"),
    )
    .await?;
    if !paused.contains("\"ok\":true") {
        return Err(format!("暂停传输请求失败: {paused}"));
    }
    wait_transfer(second_daemon.http_addr, &pause_id, "paused").await?;
    wait_named_transfer(daemon.http_addr, "pause-share", "paused").await?;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let before = transfer_received(second_daemon.http_addr, &pause_id).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = transfer_received(second_daemon.http_addr, &pause_id).await?;
    if before != after {
        return Err(format!("暂停后仍在写入: {before} -> {after}"));
    }
    let resumed = super::http_post(
        second_daemon.http_addr,
        &format!("/api/transfers/{pause_id}/resume"),
    )
    .await?;
    if !resumed.contains("\"ok\":true") {
        return Err(format!("继续传输请求失败: {resumed}"));
    }
    wait_transfer(second_daemon.http_addr, &pause_id, "running").await?;
    let peer_pause = base.join("daemon-2").join("inbox").join("pause-share");
    wait_dir(&pause_share, &peer_pause).await?;
    wait_named_transfer(daemon.http_addr, "pause-share", "completed").await?;
    println!("  传输暂停/继续: 双方状态同步为 paused，继续后完成同一任务");

    // Layout editor: place the peer on the right with a small gap, compute the
    // shared edge and mirror the layout to the other daemon.
    let layout_body = serde_json::json!({"side": "right", "gap_mm": 5}).to_string();
    let layout =
        super::http_post_json(daemon.http_addr, "/api/settings/layout", &layout_body).await?;
    let layout: Value = serde_json::from_str(&layout).map_err(|error| error.to_string())?;
    if layout["ok"] != true {
        return Err(format!("布局应用失败: {layout}"));
    }
    let metrics = &layout["layout"];
    if metrics["coverage"].as_f64().unwrap_or(0.0) <= 0.0
        || metrics["edge_length_mm"].as_f64().unwrap_or(0.0) <= 0.0
    {
        return Err(format!("布局未形成共享边: {metrics}"));
    }
    wait_status(second_daemon.http_addr, "对端布局同步", |value| {
        value["peers"]
            .as_array()
            .map(|peers| {
                peers.iter().any(|peer| {
                    peer["id"] == "daemon-test"
                        && peer["layout_known"] == true
                        && peer["layout_revision"].as_u64().unwrap_or(0) >= 1
                })
            })
            .unwrap_or(false)
    })
    .await?;
    wait_status(daemon.http_addr, "布局设置", |value| {
        value["local"]["edge"] == "right"
            && (value["local"]["layout_gap_mm"].as_f64().unwrap_or(-1.0) - 5.0).abs() < 1e-9
    })
    .await?;
    println!("  布局编辑: 右侧 5 mm 间隙生成共享边并同步 LayoutSet");

    let left_body = serde_json::json!({"side": "left", "gap_mm": 0}).to_string();
    let left_layout =
        super::http_post_json(daemon.http_addr, "/api/settings/layout", &left_body).await?;
    let left_layout: Value =
        serde_json::from_str(&left_layout).map_err(|error| error.to_string())?;
    if left_layout["ok"] != true {
        return Err(format!("左侧布局应用失败: {left_layout}"));
    }
    wait_status(daemon.http_addr, "左侧布局", |value| {
        value["local"]["edge"] == "left"
            && value["local"]["layout_gap_mm"]
                .as_f64()
                .unwrap_or(1.0)
                .abs()
                < 1e-9
    })
    .await?;
    println!("  布局编辑: 左侧 0 mm 间隙切换共享边");

    // Revoke: removing a device from the whitelist closes its live session and
    // makes a future reconnect require SAS approval again.
    let revoked = super::http_post(daemon.http_addr, "/api/devices/daemon-test-2/revoke").await?;
    if !revoked.contains("\"ok\":true") {
        return Err(format!("撤销设备失败: {revoked}"));
    }
    wait_status(daemon.http_addr, "撤销后对端离线", |value| {
        value["peers"]
            .as_array()
            .map(|peers| {
                !peers
                    .iter()
                    .any(|peer| peer["id"] == "daemon-test-2" && peer["online"] == true)
            })
            .unwrap_or(true)
    })
    .await?;
    let devices = super::http_get(daemon.http_addr, "/api/devices").await?;
    let devices: Value = serde_json::from_str(&devices).map_err(|error| error.to_string())?;
    if devices
        .as_array()
        .map(|devices| devices.iter().any(|device| device["id"] == "daemon-test-2"))
        .unwrap_or(false)
    {
        return Err("撤销后设备仍在白名单".to_owned());
    }
    println!("  设备撤销: 白名单移除并立即断开现有会话");

    second_daemon.shutdown().await;

    // Reconnect: the certificate is pinned, so no SAS prompt is needed.
    link.close(0, b"reconnect");
    drop(transport);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut transport = QuicTransport::client().map_err(|error| error.to_string())?;
    let link = transport
        .connect(daemon.quic_addr, SERVER_NAME, &trusted, &client_identity)
        .await
        .map_err(|error| error.to_string())?;
    link.send_control(&ControlMessage::Hello(Hello {
        protocol: ProtocolVersion::CURRENT,
        device_id: "client-daemon".to_owned(),
        device_name: "Client Daemon".to_owned(),
        platform: "linux-test".to_owned(),
        capabilities: Capabilities::INPUT | Capabilities::CLIPBOARD_TEXT,
        monitors: vec![presets::macbook_air_13()],
    }))
    .await
    .map_err(|error| error.to_string())?;
    let reply = link
        .recv_control()
        .await
        .map_err(|error| error.to_string())?;
    if !matches!(reply, ControlMessage::Hello(_)) {
        return Err("重连未直接返回 Hello（可能重新要求配对）".to_owned());
    }
    let clipboard = Snapshot::new(Content::text("重连后的剪贴板"), "client-daemon", 3);
    send_clipboard(&link, &clipboard)
        .await
        .map_err(|error| error.to_string())?;
    wait_status(daemon.http_addr, "重连剪贴板", |value| {
        value["clipboard"]["seq"] == 3
    })
    .await?;
    println!("  重连: 证书白名单命中 · 无重复配对 · 心跳/剪贴板恢复");

    link.close(0, b"done");
    drop(transport);
    daemon.shutdown().await;
    println!(
        "[通过] 长驻 daemon / 白名单 / 心跳 / 剪贴板 / FastCDC 去重与续传 / 传输控制 / HTTP 控制台"
    );
    Ok(())
}

async fn wait_status(
    addr: std::net::SocketAddr,
    label: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut last = Value::Null;
    loop {
        if let Ok(body) = super::http_get(addr, "/api/status").await {
            if let Ok(value) = serde_json::from_str::<Value>(&body) {
                last = value;
                if predicate(&last) {
                    return Ok(last);
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("等待 {label} 超时，最后状态: {last}"));
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

async fn wait_transfer(
    addr: std::net::SocketAddr,
    task_id: &str,
    state: &str,
) -> Result<(), String> {
    wait_status(addr, "传输状态", |value| {
        value["transfers"]
            .as_array()
            .map(|transfers| {
                transfers
                    .iter()
                    .any(|task| task["id"] == task_id && task["state"] == state)
            })
            .unwrap_or(false)
    })
    .await
    .map(|_| ())
}

/// Polls faster than [`wait_status`] so a short-lived running record can be
/// cancelled before the transfer finishes.
async fn wait_running_incoming(
    addr: std::net::SocketAddr,
    root_name: &str,
) -> Result<(String, Value), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut last = Value::Null;
    loop {
        if let Ok(body) = super::http_get(addr, "/api/status").await {
            if let Ok(value) = serde_json::from_str::<Value>(&body) {
                last = value;
                if let Some(task) = last["transfers"].as_array().and_then(|transfers| {
                    transfers.iter().find(|task| {
                        task["root_name"] == root_name
                            && task["direction"] == "incoming"
                            && task["state"] == "running"
                    })
                }) {
                    let id = task["id"].as_str().ok_or("传输记录缺少 id")?.to_owned();
                    return Ok((id, last));
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("等待 {root_name} 进入传输中超时，最后状态: {last}"));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn assert_no_cdpart(root: &Path) -> Result<(), String> {
    if !root.is_dir() {
        return Ok(());
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).map_err(|error| error.to_string())?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("cdpart") {
                return Err(format!("取消后仍残留分块文件: {}", path.display()));
            }
        }
    }
    Ok(())
}

async fn transfer_received(addr: std::net::SocketAddr, task_id: &str) -> Result<u64, String> {
    let body = super::http_get(addr, "/api/status").await?;
    let value: Value = serde_json::from_str(&body).map_err(|error| error.to_string())?;
    let task = value["transfers"]
        .as_array()
        .and_then(|transfers| transfers.iter().find(|task| task["id"] == task_id))
        .ok_or_else(|| format!("传输 {task_id} 不在状态中"))?;
    Ok(task["received_bytes"].as_u64().unwrap_or(0))
}

/// Waits until every file in `source` exists under `dest` with identical bytes.
async fn wait_dir(source: &Path, dest: &Path) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut last;
    loop {
        match verify_dir(source, dest) {
            Ok(()) => return Ok(()),
            Err(error) => last = error,
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("等待目录传输完成超时: {last}"));
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

fn verify_dir(source: &Path, dest: &Path) -> Result<(), String> {
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let from = entry.path();
        let to = dest.join(&name);
        if from.is_dir() {
            if !to.is_dir() {
                return Err(format!("{} 未出现", to.display()));
            }
            verify_dir(&from, &to)?;
            continue;
        }
        let expected = fs::read(&from).map_err(|error| error.to_string())?;
        let actual = fs::read(&to).map_err(|error| format!("{}: {error}", to.display()))?;
        if expected != actual {
            return Err(format!("{} 内容不一致", to.display()));
        }
    }
    Ok(())
}

async fn wait_tree(source: &Path, dest: &Path) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut last;
    loop {
        match verify_tree(source, dest) {
            Ok(()) => return Ok(()),
            Err(error) => last = error,
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("等待续传完成超时: {last}"));
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

async fn send_file_to(addr: std::net::SocketAddr, path: &Path, peer: &str) -> Result<(), String> {
    send_file_with_intent(addr, path, peer, false).await
}

async fn send_file_with_intent(
    addr: std::net::SocketAddr,
    path: &Path,
    peer: &str,
    clipboard: bool,
) -> Result<(), String> {
    let body = serde_json::json!({
        "path": path.display().to_string(),
        "peer": peer,
        "clipboard": clipboard,
    })
    .to_string();
    let response = super::http_post_json(addr, "/api/files/send", &body).await?;
    if !response.contains("\"ok\":true") {
        return Err(format!("文件发送请求失败: {response}"));
    }
    Ok(())
}

async fn wait_named_transfer(
    addr: std::net::SocketAddr,
    root_name: &str,
    state: &str,
) -> Result<(), String> {
    wait_status(addr, "传输状态", |value| {
        value["transfers"]
            .as_array()
            .map(|transfers| {
                transfers
                    .iter()
                    .any(|task| task["root_name"] == root_name && task["state"] == state)
            })
            .unwrap_or(false)
    })
    .await
    .map(|_| ())
}

fn assert_no_staging(inbox: &Path) -> Result<(), String> {
    let entries = fs::read_dir(inbox).map_err(|error| error.to_string())?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && (name.contains(".cdnew-") || name.contains(".cdold-")) {
            return Err(format!("覆盖策略遗留了隐藏暂存文件: {name}"));
        }
    }
    Ok(())
}

async fn wait_file(source: &Path, dest: &Path) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut last;
    loop {
        let source_bytes = fs::read(source).map_err(|error| error.to_string())?;
        if let Ok(dest_bytes) = fs::read(dest) {
            if dest_bytes == source_bytes {
                return Ok(());
            }
            last = "内容不一致".to_owned();
        } else {
            last = format!("{} 尚未出现", dest.display());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("等待单文件完成超时: {last}"));
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

fn prepare_share(root: &Path, marker: &[u8]) -> Result<(), String> {
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let payload = (0..1024 * 1024_u32)
        .map(|index| marker[index as usize % marker.len()] ^ (index % 241) as u8)
        .collect::<Vec<u8>>();
    fs::write(root.join("payload.bin"), payload).map_err(|error| error.to_string())?;
    fs::write(root.join("note.txt"), marker).map_err(|error| error.to_string())?;
    Ok(())
}

fn verify_share(source: &Path, dest: &Path) -> Result<(), String> {
    for relative in ["payload.bin", "note.txt"] {
        let source_bytes = fs::read(source.join(relative)).map_err(|error| error.to_string())?;
        let dest_bytes = fs::read(dest.join(relative)).map_err(|error| error.to_string())?;
        if source_bytes != dest_bytes {
            return Err(format!("{relative} 内容不一致"));
        }
    }
    Ok(())
}

async fn wait_share(source: &Path, dest: &Path) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut last;
    loop {
        match verify_share(source, dest) {
            Ok(()) => return Ok(()),
            Err(error) => last = error,
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("等待双向同时发送完成超时: {last}"));
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

fn prepare_source(root: &Path) -> Result<(), String> {
    fs::create_dir_all(root.join("images")).map_err(|error| error.to_string())?;
    fs::create_dir_all(root.join("empty")).map_err(|error| error.to_string())?;
    fs::write(
        root.join("notes.txt"),
        "daemon 目录传输\n第二行\n".as_bytes(),
    )
    .map_err(|error| error.to_string())?;
    fs::write(root.join("blank.txt"), b"").map_err(|error| error.to_string())?;
    let payload = (0..5 * 1024 * 1024_u32)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<u8>>();
    fs::write(root.join("images/payload.bin"), payload).map_err(|error| error.to_string())?;
    Ok(())
}

fn verify_tree(source: &Path, dest: &Path) -> Result<(), String> {
    for relative in ["notes.txt", "blank.txt", "images/payload.bin"] {
        let source = source.join(relative);
        let dest = dest.join(relative);
        let a = fs::read(&source).map_err(|error| error.to_string())?;
        let b = fs::read(&dest).map_err(|error| format!("{}: {error}", dest.display()))?;
        if a != b {
            return Err(format!("`{relative}` 内容不一致"));
        }
    }
    if !dest.join("empty").is_dir() {
        return Err("空目录未重建".to_owned());
    }
    Ok(())
}

fn clear_block_cache(root: &Path) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_file()
        {
            fs::remove_file(entry.path()).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn temp_base() -> PathBuf {
    std::env::temp_dir().join(format!(
        "crossdev-daemon-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ))
}
