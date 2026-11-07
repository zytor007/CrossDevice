mod clipboard;
mod daemon;
mod demo;
mod detect;
mod files;
mod input;
mod link;
mod pair;
mod profile;
mod report;
mod sync;

use std::process::ExitCode;

use cd_geometry::presets;
use clap::{Args, Parser, Subcommand};

use profile::AirChoice;

#[derive(Debug, Parser)]
#[command(
    name = "crossdev",
    version,
    about = "跨设备协同套件 · 几何/会话诊断 CLI",
    long_about = "在真实设备联调前，用实测档案验证毫米布局、共享边配对、指针物理速度与输入所有权状态机。"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 输出屏幕布局、边配对与分辨率适配报告
    Geometry(GeometryArgs),
    /// 运行双节点穿越/所有权模拟与验收检查
    Demo(DemoArgs),
    /// 探测本机真实屏幕（X11 xrandr + Xft.dpi）
    Detect(DetectArgs),
    /// Linux X11 输入捕获与注入诊断
    Input(input::InputArgs),
    /// QUIC 链路诊断
    Link(link::LinkArgs),
    /// 剪贴板管道与 C2 数据通道诊断
    Clipboard(clipboard::ClipboardArgs),
    /// 文件/目录 manifest、路径与续传诊断
    Files(files::FilesArgs),
    /// SAS 配对与 SQLite 持久化诊断
    Pair(pair::PairArgs),
    /// 网络同步全链路（TLS / 配对 / 剪贴板 / 文件）
    Sync(sync::SyncArgs),
    /// 长驻协同守护进程、浏览器控制台与 HTTP API
    Daemon(daemon::DaemonArgs),
}

#[derive(Debug, Args)]
struct GeometryArgs {
    /// 目标 MacBook Air 尺寸
    #[arg(long, value_enum, default_value = "both")]
    air: AirChoice,
    /// 两屏之间的物理间隙（毫米）；超过 20 mm 不产生共享边
    #[arg(long, default_value_t = 0.0)]
    gap_mm: f64,
    /// 使用本机真实探测数据（X11 xrandr + Xft.dpi），而不是实测档案
    #[arg(long)]
    detect: bool,
    /// 输出 JSON，供 UI / 自动化消费
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DemoArgs {
    /// 目标 MacBook Air 尺寸
    #[arg(long, value_enum, default_value = "both")]
    air: AirChoice,
    /// 两屏之间的物理间隙（毫米）
    #[arg(long, default_value_t = 0.0)]
    gap_mm: f64,
}

#[derive(Debug, Args)]
struct DetectArgs {
    /// 输出 JSON，供 UI / 自动化消费
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Geometry(args) => {
            let local = if args.detect {
                match detected_local() {
                    Ok(local) => local,
                    Err(message) => {
                        eprintln!("{message}");
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                presets::linux_4k()
            };
            let failed = report::has_failure(args.air, args.gap_mm, &local);
            if args.json {
                println!("{}", report::geometry_json(args.air, args.gap_mm, &local));
            } else {
                print!("{}", report::render_geometry(args.air, args.gap_mm, &local));
            }
            if failed {
                eprintln!("验收检查存在失败项");
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Command::Demo(args) => {
            if demo::run_demo(args.air, args.gap_mm) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Command::Detect(args) => detect::run(args.json),
        Command::Input(args) => input::run(args),
        Command::Link(args) => link::run(args),
        Command::Clipboard(args) => clipboard::run(args),
        Command::Files(args) => files::run(args),
        Command::Pair(args) => pair::run(args),
        Command::Sync(args) => sync::run(args),
        Command::Daemon(args) => daemon::run(args),
    }
}

/// Rightmost detected monitor becomes the left anchor of the simulated Air.
fn detected_local() -> Result<cd_geometry::MonitorInfo, String> {
    let detection = cd_platform::detect().map_err(|error| format!("平台探测失败: {error}"))?;
    detection
        .monitors
        .into_iter()
        .max_by(|a, b| a.rect().right().total_cmp(&b.rect().right()))
        .ok_or_else(|| "平台探测失败: 没有可用屏幕".to_owned())
}
