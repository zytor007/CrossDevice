use std::process::ExitCode;
#[cfg(target_os = "linux")]
use std::time::Duration;

use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct InputArgs {
    #[command(subcommand)]
    action: InputAction,
}

#[derive(Debug, Subcommand)]
enum InputAction {
    /// 监听本机真实输入事件
    Probe {
        /// 监听时长（秒）
        #[arg(long, default_value_t = 3)]
        seconds: u64,
        /// 最多打印多少条事件
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// 包含自身 XTest 注入事件（默认过滤，防回环）
        #[arg(long)]
        include_injected: bool,
    },
    /// XTest 注入 + XI2 捕获闭环自检（会移动指针 8px 并复位）
    SelfTest,
    /// C1 DATAGRAM → XTest 注入的网络闭环（会移动指针 10mm 并复位）
    NetTest,
}

#[cfg(target_os = "linux")]
pub fn run(args: InputArgs) -> ExitCode {
    use cd_input::{self_test, CapturedEvent, X11Capture};

    match args.action {
        InputAction::Probe {
            seconds,
            limit,
            include_injected,
        } => {
            let capture = match if include_injected {
                X11Capture::open_including_injected()
            } else {
                X11Capture::open()
            } {
                Ok(capture) => capture,
                Err(error) => {
                    eprintln!("打开 X11 捕获失败: {error}");
                    return ExitCode::FAILURE;
                }
            };
            println!(
                "监听输入 {seconds} 秒（root window 0x{:x}，忽略 XTest 源 {:?}）…",
                capture.root(),
                capture.ignored_sources()
            );
            let events = match capture.probe(Duration::from_secs(seconds)) {
                Ok(events) => events,
                Err(error) => {
                    eprintln!("捕获失败: {error}");
                    return ExitCode::FAILURE;
                }
            };
            let motion = events
                .iter()
                .filter(|event| matches!(event, CapturedEvent::Motion { .. }))
                .count();
            let buttons = events
                .iter()
                .filter(|event| matches!(event, CapturedEvent::Button { .. }))
                .count();
            let keys = events
                .iter()
                .filter(|event| matches!(event, CapturedEvent::Key { .. }))
                .count();
            println!(
                "捕获 {} 条：移动 {motion} · 按钮 {buttons} · 键盘 {keys}",
                events.len()
            );
            for event in events.iter().take(limit) {
                match event {
                    CapturedEvent::Motion { x, y, time_ms, .. } => {
                        println!("  move   ({x:>7.1}, {y:>7.1}) px · t={time_ms} ms");
                    }
                    CapturedEvent::Button {
                        button,
                        pressed,
                        time_ms,
                        ..
                    } => {
                        println!(
                            "  button {button} {} · t={time_ms} ms",
                            if *pressed { "按下" } else { "抬起" }
                        );
                    }
                    CapturedEvent::Key {
                        keycode,
                        pressed,
                        time_ms,
                        ..
                    } => {
                        println!(
                            "  key    {keycode} {} · t={time_ms} ms",
                            if *pressed { "按下" } else { "抬起" }
                        );
                    }
                }
            }
            ExitCode::SUCCESS
        }
        InputAction::NetTest => {
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
            match runtime.block_on(net_test()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("C1 网络注入自检失败: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        InputAction::SelfTest => {
            let report = match self_test() {
                Ok(report) => report,
                Err(error) => {
                    eprintln!("X11 输入自检失败: {error}");
                    return ExitCode::FAILURE;
                }
            };
            println!("X11 输入自检");
            println!(
                "  指针 ({}, {}) → ({}, {})，偏移 ({}, {})",
                report.original.0,
                report.original.1,
                report.target.0,
                report.target.1,
                report.delta.0,
                report.delta.1
            );
            println!(
                "  捕获：移动 {} · 按钮 {} · 键盘 {}",
                report.motion_events, report.button_events, report.key_events
            );
            if let Some(first) = report.first_motion {
                println!("  首个移动事件落点 ({}, {}) px", first.0, first.1);
            }
            println!(
                "  指针已复位: {}",
                if report.restored { "是" } else { "否" }
            );
            if report.passed() {
                println!("[通过] XI2 捕获与 XTest 注入闭环可用");
                ExitCode::SUCCESS
            } else {
                eprintln!("[失败] 未捕获到移动事件或指针未复位");
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn run(_args: InputArgs) -> ExitCode {
    eprintln!("当前平台暂未实现输入后端");
    ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
async fn net_test() -> Result<(), String> {
    use cd_geometry::presets;
    use cd_input::{apply_input, X11Injector};
    use cd_proto::{ControlMessage, Heartbeat, InputEvent};
    use cd_transport::{Identity, QuicTransport};
    use std::net::SocketAddr;

    let monitor = cd_platform::detect()
        .ok()
        .and_then(|detection| detection.monitors.into_iter().next())
        .unwrap_or_else(presets::linux_4k);
    let identity = Identity::generate("crossdev.local").map_err(|e| e.to_string())?;
    let server = QuicTransport::server(SocketAddr::from(([127, 0, 0, 1], 0)), &identity)
        .map_err(|e| e.to_string())?;
    let addr = server.local_addr().map_err(|e| e.to_string())?;
    let trusted = identity.cert().clone();
    let server_monitor = monitor.clone();

    let server_task = tokio::spawn(async move {
        let link = server.accept().await.map_err(|e| e.to_string())?;
        let injector = X11Injector::open().map_err(|e| e.to_string())?;
        let before = injector.pointer().map_err(|e| e.to_string())?;

        let ping = link.recv_control().await.map_err(|e| e.to_string())?;
        if !matches!(ping, ControlMessage::Heartbeat(_)) {
            return Err("客户端未发送 Heartbeat".to_owned());
        }
        link.send_control(&ControlMessage::Heartbeat(Heartbeat { seq: 1, ts_us: 1 }))
            .await
            .map_err(|e| e.to_string())?;

        let first = link.recv_input().await.map_err(|e| e.to_string())?;
        apply_input(&injector, &server_monitor, &first).map_err(|e| e.to_string())?;
        let middle = injector.pointer().map_err(|e| e.to_string())?;

        let second = link.recv_input().await.map_err(|e| e.to_string())?;
        apply_input(&injector, &server_monitor, &second).map_err(|e| e.to_string())?;
        let after = injector.pointer().map_err(|e| e.to_string())?;

        link.close(0, b"done");
        Ok::<_, String>((before, middle, after))
    });

    let mut client = QuicTransport::client().map_err(|e| e.to_string())?;
    let client_identity = Identity::generate("client.crossdev.local").map_err(|e| e.to_string())?;
    let link = client
        .connect(addr, "crossdev.local", &trusted, &client_identity)
        .await
        .map_err(|e| e.to_string())?;
    link.send_control(&ControlMessage::Heartbeat(Heartbeat { seq: 0, ts_us: 0 }))
        .await
        .map_err(|e| e.to_string())?;
    let ready = link.recv_control().await.map_err(|e| e.to_string())?;
    if !matches!(ready, ControlMessage::Heartbeat(_)) {
        return Err("服务端未就绪".to_owned());
    }

    link.send_input(&InputEvent::move_relative_um(10_000, 0, 1))
        .map_err(|e| e.to_string())?;
    link.send_input(&InputEvent::move_relative_um(-10_000, 0, 2))
        .map_err(|e| e.to_string())?;

    let (before, middle, after) =
        tokio::time::timeout(std::time::Duration::from_secs(3), server_task)
            .await
            .map_err(|_| "等待注入完成超时".to_owned())?
            .map_err(|error| format!("注入任务失败: {error}"))??;

    if middle == before {
        return Err("C1 移动未改变指针位置".to_owned());
    }
    if after != before {
        return Err(format!("指针未复位: {before:?} → {after:?}"));
    }

    println!("C1 网络注入自检");
    println!("  指针 {before:?} → {middle:?} → {after:?}");
    println!("  位移 10mm / -10mm · 经 QUIC DATAGRAM 解码后 XTest 注入");
    println!("[通过] C1 数据报 → XTest 注入闭环可用");

    link.close(0, b"done");
    client.close();
    Ok(())
}
