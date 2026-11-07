use serde::{Deserialize, Serialize};

use crate::monitor::MonitorInfo;

/// How the same physical hand movement translates between two devices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceComparison {
    pub source_id: String,
    pub target_id: String,
    /// Source display width divided by target display width.
    pub width_ratio: f64,
    pub height_ratio: f64,
    /// Logical pixels the target cursor must move per source logical pixel so
    /// that both cursors travel the same physical distance.
    pub speed_multiplier_x: f64,
    pub speed_multiplier_y: f64,
    pub source_logical_px_per_mm_x: f64,
    pub target_logical_px_per_mm_x: f64,
    pub source_ui_ppi: f64,
    pub target_ui_ppi: f64,
}

pub fn compare_devices(source: &MonitorInfo, target: &MonitorInfo) -> DeviceComparison {
    DeviceComparison {
        source_id: source.id.clone(),
        target_id: target.id.clone(),
        width_ratio: source.display_w_mm() / target.display_w_mm(),
        height_ratio: source.display_h_mm() / target.display_h_mm(),
        speed_multiplier_x: target.logical_px_per_mm_x() / source.logical_px_per_mm_x(),
        speed_multiplier_y: target.logical_px_per_mm_y() / source.logical_px_per_mm_y(),
        source_logical_px_per_mm_x: source.logical_px_per_mm_x(),
        target_logical_px_per_mm_x: target.logical_px_per_mm_x(),
        source_ui_ppi: source.ui_ppi_x(),
        target_ui_ppi: target.ui_ppi_x(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets;

    #[test]
    fn air_is_more_dense_than_the_4k_desktop() {
        let linux = presets::linux_4k();
        let air = presets::macbook_air_13();
        let comparison = compare_devices(&linux, &air);
        assert!((comparison.width_ratio - 631.0 / 293.0).abs() < 1e-9);
        assert!((comparison.speed_multiplier_x - 1.6488).abs() < 0.005);
        assert!(comparison.speed_multiplier_x > 1.0);
    }
}
