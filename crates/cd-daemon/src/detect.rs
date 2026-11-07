use std::process::ExitCode;

use cd_platform::detect;
use serde_json::json;

pub fn run(json_output: bool) -> ExitCode {
    let detection = match detect() {
        Ok(detection) => detection,
        Err(error) => {
            eprintln!("平台探测失败: {error}");
            return ExitCode::FAILURE;
        }
    };

    if json_output {
        println!(
            "{}",
            json!({
                "platform": "linux-x11",
                "scale": detection.scale,
                "scale_source": detection.scale_source,
                "monitors": detection.monitors,
            })
        );
        return ExitCode::SUCCESS;
    }

    println!("平台探测 · Linux X11");
    println!(
        "系统缩放: {:.2}×（来源 {}）",
        detection.scale, detection.scale_source
    );
    println!("检测到 {} 块屏幕\n", detection.monitors.len());
    for monitor in &detection.monitors {
        println!("  {}", monitor.name);
        println!(
            "    原生 {}×{} · 逻辑 {}×{} · 有效缩放 {:.3}×",
            monitor.native_w,
            monitor.native_h,
            monitor.logical_w,
            monitor.logical_h,
            monitor.effective_scale_x()
        );
        println!(
            "    物理 {}×{} mm · 面板 {:.1} PPI · 逻辑 {:.1} PPI",
            monitor.display_w_mm(),
            monitor.display_h_mm(),
            monitor.panel_ppi_x(),
            monitor.ui_ppi_x()
        );
        println!(
            "    原点 ({:.1}, {:.1}) mm · 逻辑密度 {:.3} px/mm",
            monitor.origin_mm.x,
            monitor.origin_mm.y,
            monitor.logical_px_per_mm_x()
        );
    }
    ExitCode::SUCCESS
}
