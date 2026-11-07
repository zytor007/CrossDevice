#[cfg(target_os = "linux")]
use std::env;
#[cfg(target_os = "linux")]
use std::process::Command;

use cd_geometry::MonitorInfo;
#[cfg(target_os = "linux")]
use cd_geometry::PointMm;
use thiserror::Error;

#[cfg(target_os = "linux")]
use crate::xrandr::{parse_outputs, XrandrOutput};

#[derive(Debug, Clone)]
pub struct Detection {
    pub monitors: Vec<MonitorInfo>,
    pub scale: f64,
    pub scale_source: String,
}

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("required command `{command}` is not available")]
    CommandUnavailable { command: String },
    #[error("command `{command}` failed: {message}")]
    CommandFailed { command: String, message: String },
    #[error("no connected monitor with a current mode was found")]
    NoMonitors,
    #[error("platform not supported by this build")]
    Unsupported,
}

#[cfg(target_os = "linux")]
pub fn detect() -> Result<Detection, PlatformError> {
    let output = Command::new("xrandr")
        .arg("--current")
        .output()
        .map_err(|error| PlatformError::CommandFailed {
            command: "xrandr".to_owned(),
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(PlatformError::CommandFailed {
            command: "xrandr".to_owned(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    let (scale, scale_source) = detect_scale();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let monitors = parse_outputs(&stdout)
        .into_iter()
        .filter(|output| output.connected)
        .filter_map(|output| to_monitor(&output, scale))
        .collect::<Vec<_>>();

    if monitors.is_empty() {
        return Err(PlatformError::NoMonitors);
    }
    Ok(Detection {
        monitors,
        scale,
        scale_source,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn detect() -> Result<Detection, PlatformError> {
    Err(PlatformError::Unsupported)
}

#[cfg(target_os = "linux")]
fn to_monitor(output: &XrandrOutput, scale: f64) -> Option<MonitorInfo> {
    let mode = output.mode?;
    let logical_w = ((f64::from(mode.width) / scale).round() as u32).max(1);
    let logical_h = ((f64::from(mode.height) / scale).round() as u32).max(1);
    let (physical_w_mm, physical_h_mm) = output.physical_mm.unwrap_or_else(|| {
        // No EDID size: assume a 96 PPI UI density for the fallback canvas.
        (
            f64::from(logical_w) / 96.0 * 25.4,
            f64::from(logical_h) / 96.0 * 25.4,
        )
    });

    let id = output.name.to_ascii_lowercase();
    let mut monitor = MonitorInfo::new(
        id,
        mode.width,
        mode.height,
        logical_w,
        logical_h,
        physical_w_mm,
        physical_h_mm,
    )
    .named(format!("{} (X11)", output.name))
    .rotated(output.rotation);

    let x_mm = f64::from(mode.x) / monitor.native_px_per_mm_x();
    let y_mm = f64::from(mode.y) / monitor.native_px_per_mm_y();
    monitor.origin_mm = PointMm::new(x_mm, y_mm);
    Some(monitor)
}

#[cfg(target_os = "linux")]
fn detect_scale() -> (f64, String) {
    if let Ok(raw) = env::var("GDK_SCALE") {
        if let Ok(value) = raw.parse::<f64>() {
            if value >= 1.0 && value.is_finite() {
                return (value.clamp(1.0, 4.0), format!("GDK_SCALE={raw}"));
            }
        }
    }
    if let Some(dpi) = query_xft_dpi() {
        if dpi > 0.0 {
            return ((dpi / 96.0).clamp(1.0, 4.0), format!("Xft.dpi={dpi}"));
        }
    }
    (1.0, "fallback 1.0（无 GDK_SCALE / Xft.dpi）".to_owned())
}

#[cfg(target_os = "linux")]
fn query_xft_dpi() -> Option<f64> {
    let output = Command::new("xrdb").arg("-query").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.trim() != "Xft.dpi" {
            return None;
        }
        value.trim().parse::<f64>().ok()
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::xrandr::{parse_outputs, XrandrMode, XrandrOutput};

    fn output(
        name: &str,
        width: u32,
        height: u32,
        physical_mm: Option<(f64, f64)>,
    ) -> XrandrOutput {
        XrandrOutput {
            name: name.to_owned(),
            connected: true,
            primary: false,
            mode: Some(XrandrMode {
                width,
                height,
                x: 0,
                y: 0,
            }),
            physical_mm,
            rotation: cd_geometry::Rotation::Deg0,
        }
    }

    #[test]
    fn converts_measured_4k_at_two_x() {
        let monitor =
            to_monitor(&output("HDMI-0", 3840, 2160, Some((631.0, 360.0))), 2.0).expect("monitor");
        assert_eq!(monitor.id, "hdmi-0");
        assert_eq!(monitor.logical_w, 1920);
        assert_eq!(monitor.logical_h, 1080);
        assert!((monitor.effective_scale_x() - 2.0).abs() < 1e-9);
        assert!((monitor.panel_ppi_x() - 154.6).abs() < 0.2);
    }

    #[test]
    fn missing_edid_size_falls_back_to_96_ppi() {
        let monitor = to_monitor(&output("eDP-1", 1920, 1080, None), 1.0).expect("monitor");
        assert!((monitor.display_w_mm() - 508.0).abs() < 0.2);
        assert!((monitor.display_h_mm() - 285.75).abs() < 0.2);
    }

    #[test]
    fn origin_is_converted_to_millimetres() {
        let mut source = output("HDMI-0", 3840, 2160, Some((631.0, 360.0)));
        source.mode = Some(XrandrMode {
            width: 3840,
            height: 2160,
            x: 3840,
            y: 0,
        });
        let monitor = to_monitor(&source, 2.0).expect("monitor");
        assert!((monitor.origin_mm.x - 631.0).abs() < 0.1);
        assert!((monitor.origin_mm.y - 0.0).abs() < 0.1);
    }

    #[test]
    fn disconnected_outputs_are_filtered_before_conversion() {
        let text = "DP-1 disconnected (normal left inverted right x axis y axis)";
        let monitors = parse_outputs(text)
            .into_iter()
            .filter(|output| output.connected)
            .filter_map(|output| to_monitor(&output, 1.0))
            .collect::<Vec<_>>();
        assert!(monitors.is_empty());
    }
}
