mod clipboard;
mod config;
mod discovery;
#[cfg(target_os = "linux")]
mod input;
#[cfg(not(target_os = "linux"))]
#[path = "input_stub.rs"]
mod input;
mod registry;
mod runtime;
pub mod self_test;
mod status;
mod web;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use cd_transport::Identity;
use clap::{Args, Subcommand, ValueEnum};

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    action: DaemonAction,
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// 启动长驻会话守护进程（QUIC + HTTP 控制台）
    Run(RunArgs),
    /// 在临时目录内验证配对、白名单、心跳、剪贴板、FastCDC 续传与 HTTP API
    SelfTest,
    /// 读取本机 daemon 的 /api/status
    Status(StatusArgs),
    /// 导出诊断包（状态、白名单元数据、传输记录、缓存占用与设置）
    Diagnostics(DiagnosticsArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PairArg {
    Auto,
    Deny,
    Confirm,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// 数据目录；默认 $CROSSDEV_HOME 或 ~/.local/share/crossdev
    #[arg(long)]
    home: Option<PathBuf>,
    /// QUIC 监听地址
    #[arg(long)]
    bind: Option<String>,
    /// HTTP 控制台监听地址
    #[arg(long)]
    http_bind: Option<String>,
    /// 设备显示名
    #[arg(long)]
    device_name: Option<String>,
    /// 首次配对策略；deny 只接受白名单设备
    #[arg(long, value_enum)]
    pair: Option<PairArg>,
    /// 关闭系统剪贴板 watcher
    #[arg(long)]
    no_clipboard: bool,
    /// 启用 X11 输入捕获与注入
    #[arg(long)]
    input_pump: bool,
    /// 主动连接的对端 QUIC 地址，断线自动退避重连
    #[arg(long)]
    peer: Option<String>,
    /// 关闭 UDP 局域网发现
    #[arg(long)]
    no_discovery: bool,
}

#[derive(Debug, Args)]
struct StatusArgs {
    /// daemon HTTP 地址
    #[arg(long, default_value = "127.0.0.1:7824")]
    http: String,
}

#[derive(Debug, Args)]
struct DiagnosticsArgs {
    /// daemon HTTP 地址
    #[arg(long, default_value = "127.0.0.1:7824")]
    http: String,
    /// 输出文件；默认写入当前目录的 crossdev-diagnostics.json
    #[arg(long)]
    out: Option<PathBuf>,
}

pub fn run(args: DaemonArgs) -> ExitCode {
    match args.action {
        DaemonAction::Run(args) => run_daemon(args),
        DaemonAction::SelfTest => self_test::run(),
        DaemonAction::Status(args) => status(args),
        DaemonAction::Diagnostics(args) => diagnostics(args),
    }
}

fn run_daemon(args: RunArgs) -> ExitCode {
    let paths = match args.home {
        Some(home) => Ok(config::DaemonPaths::at(home)),
        None => config::DaemonPaths::resolve(),
    };
    let paths = match paths {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("无法确定数据目录: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = paths.ensure() {
        eprintln!("创建数据目录失败: {error}");
        return ExitCode::FAILURE;
    }
    let identity = match Identity::load_or_generate(&paths.identity, "crossdev.local") {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("加载设备身份失败: {error}");
            return ExitCode::FAILURE;
        }
    };
    let hostname = std::env::var("HOSTNAME").ok();
    let fallback_id = config::device_id_from(hostname.as_deref(), &identity.fingerprint_hex());
    let mut cfg = match config::Config::load(&paths.config, fallback_id) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("读取配置失败: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(device_name) = args.device_name {
        cfg.device_name = device_name;
    }
    if let Some(bind) = args.bind {
        cfg.bind = bind;
    }
    if let Some(http_bind) = args.http_bind {
        cfg.http_bind = http_bind;
    }
    if let Some(pair) = args.pair {
        cfg.pair_mode = match pair {
            PairArg::Auto => config::PairMode::Auto,
            PairArg::Deny => config::PairMode::Deny,
            PairArg::Confirm => config::PairMode::Confirm,
        };
    }
    if args.no_clipboard {
        cfg.clipboard = false;
    }
    if args.input_pump {
        cfg.input_pump = true;
    }
    if let Some(peer) = args.peer {
        cfg.peer = Some(peer);
    }
    if args.no_discovery {
        cfg.discovery = false;
    }

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
    runtime.block_on(async move {
        let daemon = match runtime::start(paths, cfg).await {
            Ok(daemon) => daemon,
            Err(error) => {
                eprintln!("daemon 启动失败: {error}");
                return ExitCode::FAILURE;
            }
        };
        println!("按 Ctrl+C 停止 daemon");
        let _ = tokio::signal::ctrl_c().await;
        println!("正在关闭…");
        daemon.shutdown().await;
        ExitCode::SUCCESS
    })
}

fn status(args: StatusArgs) -> ExitCode {
    let addr = match args.http.parse::<SocketAddr>() {
        Ok(addr) => addr,
        Err(error) => {
            eprintln!("无效 HTTP 地址: {error}");
            return ExitCode::FAILURE;
        }
    };
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
    match runtime.block_on(http_get(addr, "/api/status")) {
        Ok(body) => {
            println!("{body}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("/api/status 请求失败: {error}");
            ExitCode::FAILURE
        }
    }
}

fn diagnostics(args: DiagnosticsArgs) -> ExitCode {
    let addr = match args.http.parse::<SocketAddr>() {
        Ok(addr) => addr,
        Err(error) => {
            eprintln!("无效 HTTP 地址: {error}");
            return ExitCode::FAILURE;
        }
    };
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
    let body = match runtime.block_on(http_get(addr, "/api/diagnostics")) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("/api/diagnostics 请求失败: {error}");
            return ExitCode::FAILURE;
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("诊断响应不是合法 JSON: {error}");
            return ExitCode::FAILURE;
        }
    };
    let text = match serde_json::to_string_pretty(&value) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("格式化诊断数据失败: {error}");
            return ExitCode::FAILURE;
        }
    };
    let out = args
        .out
        .unwrap_or_else(|| PathBuf::from("crossdev-diagnostics.json"));
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                eprintln!("创建诊断目录失败: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    match std::fs::write(&out, text) {
        Ok(()) => {
            println!("诊断已导出: {}", out.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("写入诊断文件失败: {error}");
            ExitCode::FAILURE
        }
    }
}

pub(crate) async fn http_get(addr: SocketAddr, path: &str) -> Result<String, String> {
    http_request(addr, "GET", path, "").await
}

pub(crate) async fn http_post(addr: SocketAddr, path: &str) -> Result<String, String> {
    http_request(addr, "POST", path, "").await
}

pub(crate) async fn http_post_json(
    addr: SocketAddr,
    path: &str,
    body: &str,
) -> Result<String, String> {
    http_request(addr, "POST", path, body).await
}

async fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: &str,
) -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|error| error.to_string())?;
    let content_type = if body.is_empty() {
        ""
    } else {
        "Content-Type: application/json\r\n"
    };
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    let mut buffer = String::new();
    stream
        .read_to_string(&mut buffer)
        .await
        .map_err(|error| error.to_string())?;
    let body = buffer
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or(buffer);
    Ok(body)
}
