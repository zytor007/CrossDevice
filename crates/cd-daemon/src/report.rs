use std::fmt::Write as _;

use cd_geometry::{compare_devices, presets, Layout, LayoutIssue, MonitorInfo, Side};
use serde::Serialize;
use serde_json::{json, Value};

use crate::profile::AirChoice;

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

impl Check {
    fn new(name: impl Into<String>, passed: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed,
            detail: detail.into(),
        }
    }
}

pub fn render_geometry(choice: AirChoice, gap_mm: f64, local: &MonitorInfo) -> String {
    let mut out = String::new();

    let _ = writeln!(out, "跨设备协同 · 几何适配报告");
    let _ = writeln!(
        out,
        "本机屏幕: {} · {}×{} @ {:.1}× · 物理 {}×{} mm",
        local.name,
        local.native_w,
        local.native_h,
        local.effective_scale_x(),
        fmt1(local.physical_w_mm),
        fmt1(local.physical_h_mm)
    );
    let _ = writeln!(
        out,
        "说明: 布局按物理毫米 1:1 绘制；macOS 有效缩放 = native/logical，不假设 2.0×。\n"
    );

    let _ = writeln!(out, "屏幕");
    for screen in all_screens(choice, local) {
        let _ = writeln!(out, "  {}", screen.name);
        let _ = writeln!(
            out,
            "    原生 {}×{} · 逻辑 {}×{} · 有效缩放 {:.3}×",
            screen.native_w,
            screen.native_h,
            screen.logical_w,
            screen.logical_h,
            screen.effective_scale_x()
        );
        let _ = writeln!(
            out,
            "    物理 {}×{} mm · 面板 {:.1} PPI · 逻辑 {:.1} PPI · 逻辑密度 {:.3} px/mm",
            fmt1(screen.display_w_mm()),
            fmt1(screen.display_h_mm()),
            screen.panel_ppi_x(),
            screen.ui_ppi_x(),
            screen.logical_px_per_mm_x()
        );
    }

    for air in choice.models() {
        let layout = compose(local, &air, gap_mm);
        let pairs = layout.pair_edges();
        let comparison = compare_devices(local, &air);
        let checks = checks_for(local, &air, &layout);

        let _ = writeln!(out, "\n━━━ {} ━━━", air.name);
        let _ = writeln!(
            out,
            "布局 revision {} · {} 屏 · {} 条共享边 · {}",
            layout.revision,
            layout.screens.len(),
            pairs.len(),
            if layout.validate().is_empty() {
                "校验通过"
            } else {
                "校验失败"
            }
        );
        for screen in &layout.screens {
            let _ = writeln!(
                out,
                "  {:<16} ({:>6.1}, {:>6.1}) mm · {:.1}×{:.1} mm",
                screen.id,
                screen.origin_mm.x,
                screen.origin_mm.y,
                screen.display_w_mm(),
                screen.display_h_mm()
            );
        }
        for issue in layout.validate() {
            let _ = writeln!(out, "  ! {issue}");
        }

        if let Some(pair) = pairs.first() {
            let local_edge = pair.segment_of(&local.id, Side::Right);
            let air_edge = pair.segment_of(&air.id, Side::Left);
            let _ = writeln!(out, "\n共享边");
            let _ = writeln!(
                out,
                "  {} 右 ↔ {} 左 · 间距 {:.1} mm · 重叠 {:.1} mm",
                local.id,
                air.id,
                pair.gap_mm,
                pair.overlap_mm()
            );
            if let (Some(local_edge), Some(air_edge)) = (local_edge, air_edge) {
                let _ = writeln!(
                    out,
                    "  覆盖率: {} {:.1}% | {} {:.1}%",
                    local.id,
                    pair.coverage_of(local_edge) * 100.0,
                    air.id,
                    pair.coverage_of(air_edge) * 100.0
                );
            }
            let _ = writeln!(
                out,
                "  速度倍率: {}→{} {:.2}× | {}→{} {:.2}×",
                local.id,
                air.id,
                comparison.speed_multiplier_x,
                air.id,
                local.id,
                1.0 / comparison.speed_multiplier_x
            );
            let _ = writeln!(
                out,
                "  物理宽度比 {:.2}× · 共享轴两端归一化 0 / 65535",
                comparison.width_ratio
            );
        } else {
            let _ = writeln!(out, "\n共享边: 无（间距超过 20 mm 或无投影重叠）");
        }

        let _ = writeln!(
            out,
            "\n指针物理一致性: 本机移动 100 mm → {} 指针 {:.1} 逻辑 px（{:.1} mm）",
            air.id,
            logical_px_for_100mm(local, &air),
            logical_mm_for_100mm(local, &air)
        );

        let _ = writeln!(out, "\n验收检查");
        for check in &checks {
            let _ = writeln!(
                out,
                "  [{}] {} · {}",
                if check.passed { "通过" } else { "失败" },
                check.name,
                check.detail
            );
        }
    }

    let _ = writeln!(
        out,
        "\n提示: 运行时以平台上报的原生/逻辑分辨率与 GDK scale factor 为准；此报告使用{}。",
        if local.name.contains("(X11)") {
            "本机真实探测数据"
        } else {
            "本机实测档案"
        }
    );
    out
}

pub fn checks_for(local: &MonitorInfo, air: &MonitorInfo, layout: &Layout) -> Vec<Check> {
    let mut checks = Vec::new();
    let issues = layout.validate();
    checks.push(Check::new(
        "布局校验",
        issues.is_empty(),
        if issues.is_empty() {
            "无重叠、无重复 id".to_owned()
        } else {
            summarize_issues(&issues)
        },
    ));

    let pairs = layout.pair_edges();
    checks.push(Check::new(
        "共享边配对",
        pairs.len() == 1,
        format!("{} 条", pairs.len()),
    ));

    let Some(pair) = pairs.first() else {
        return checks;
    };
    let Some(local_edge) = pair.segment_of(&local.id, Side::Right) else {
        checks.push(Check::new("共享边方向", false, "未找到本机右边缘"));
        return checks;
    };
    let coverage = pair.coverage_of(local_edge);
    let comparison = compare_devices(local, air);
    let expected = expected_metrics(air, local);

    if let Some(expected) = expected {
        checks.push(Check::new(
            "共享边覆盖率",
            (coverage - expected.coverage).abs() <= 0.005,
            format!(
                "{:.1}%（目标 {:.1}%）",
                coverage * 100.0,
                expected.coverage * 100.0
            ),
        ));
        checks.push(Check::new(
            "速度倍率",
            (comparison.speed_multiplier_x - expected.speed).abs() <= 0.02,
            format!(
                "{:.2}×（目标 {:.2}×）",
                comparison.speed_multiplier_x, expected.speed
            ),
        ));
        checks.push(Check::new(
            "物理宽度比",
            (comparison.width_ratio - expected.width_ratio).abs() <= 0.02,
            format!(
                "{:.2}×（目标 {:.2}×）",
                comparison.width_ratio, expected.width_ratio
            ),
        ));
    }

    let start = pair.overlap_start_mm;
    let overlap = pair.overlap_mm();
    let mut worst = 0.0_f64;
    let mut roundtrip_ok = true;
    for fraction in [0.1, 0.5, 0.9] {
        let y = start + overlap * fraction;
        match layout.check_roundtrip(&local.id, Side::Right, y) {
            Some(check) => {
                worst = worst.max(check.error_px);
                roundtrip_ok &= check.error_px <= 2.0;
            }
            None => roundtrip_ok = false,
        }
    }
    checks.push(Check::new(
        "穿越往返误差",
        roundtrip_ok,
        format!("最大 {worst:.2} 逻辑 px（上限 2.00 px）"),
    ));

    let mm_back = logical_mm_for_100mm(local, air);
    checks.push(Check::new(
        "100mm 物理一致性",
        (mm_back - 100.0).abs() <= 0.5,
        format!("{mm_back:.2} mm（允许偏差 0.5 mm）"),
    ));

    checks
}

pub fn has_failure(choice: AirChoice, gap_mm: f64, local: &MonitorInfo) -> bool {
    choice.models().iter().any(|air| {
        let layout = compose(local, air, gap_mm);
        checks_for(local, air, &layout)
            .iter()
            .any(|check| !check.passed)
    })
}

pub fn geometry_json(choice: AirChoice, gap_mm: f64, local: &MonitorInfo) -> Value {
    let models = choice.models();
    let profiles: Vec<Value> = models
        .iter()
        .map(|air| {
            let layout = compose(local, air, gap_mm);
            json!({
                "air": air.id,
                "screen": air,
                "layout": layout,
                "edge_pairs": layout.pair_edges(),
                "comparison": compare_devices(local, air),
                "checks": checks_for(local, air, &layout),
            })
        })
        .collect();

    json!({
        "source": local,
        "reference": "2026-11 measured profiles",
        "screens": all_screens(choice, local),
        "profiles": profiles,
        "protocol": {
            "c1_event_bytes": cd_proto::C1_EVENT_LEN,
            "relative_unit": "micrometer",
            "layout_unit": "millimeter"
        }
    })
}

fn all_screens(choice: AirChoice, local: &MonitorInfo) -> Vec<MonitorInfo> {
    let mut screens = vec![local.clone()];
    screens.extend(choice.models());
    screens
}

fn compose(local: &MonitorInfo, air: &MonitorInfo, gap_mm: f64) -> Layout {
    presets::compose_air_with(local.clone(), air.clone(), gap_mm)
}

fn expected_metrics(air: &MonitorInfo, local: &MonitorInfo) -> Option<ExpectedMetrics> {
    // Hard-coded acceptance numbers only make sense against the measured 4K
    // desktop; for any other host we still check physical consistency but do
    // not fail merely because its density differs.
    let is_reference_desktop = (local.display_w_mm() - 631.0).abs() <= 0.5
        && (local.display_h_mm() - 360.0).abs() <= 0.5
        && local.logical_w == 1920
        && local.logical_h == 1080;
    if !is_reference_desktop {
        return None;
    }
    if air.id == presets::AIR_13_ID {
        Some(ExpectedMetrics {
            coverage: 183.0 / 360.0,
            speed: 1.6488,
            width_ratio: 631.0 / 293.0,
        })
    } else if air.id == presets::AIR_15_ID {
        Some(ExpectedMetrics {
            coverage: 206.0 / 360.0,
            speed: 1.7030,
            width_ratio: 631.0 / 330.0,
        })
    } else {
        None
    }
}

fn logical_px_for_100mm(source: &MonitorInfo, target: &MonitorInfo) -> f64 {
    let dx_native = 100.0 * source.native_px_per_mm_x();
    let (dx_um, _) = source.native_delta_to_um(dx_native, 0.0);
    let (dx_logical, _) = target.um_to_logical_delta(dx_um, 0);
    dx_logical
}

fn logical_mm_for_100mm(source: &MonitorInfo, target: &MonitorInfo) -> f64 {
    logical_px_for_100mm(source, target) / target.logical_px_per_mm_x()
}

fn summarize_issues(issues: &[LayoutIssue]) -> String {
    issues
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn fmt1(value: f64) -> String {
    format!("{value:.1}")
}

struct ExpectedMetrics {
    coverage: f64,
    speed: f64,
    width_ratio: f64,
}
