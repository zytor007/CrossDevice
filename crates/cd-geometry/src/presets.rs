//! Measured reference profiles from the delivery environment (2026-11).
//!
//! These are inputs for diagnostics, demos and tests. At runtime the daemon
//! must read the real modes from the platform instead of hard-coding them.

use crate::layout::Layout;
use crate::monitor::MonitorInfo;

pub const LINUX_4K_ID: &str = "linux-4k";
pub const AIR_13_ID: &str = "macbook-air-13";
pub const AIR_15_ID: &str = "macbook-air-15";

/// 28.6" HDMI panel: 3840×2160 at 2.0× UI scale, 631×360 mm.
pub fn linux_4k() -> MonitorInfo {
    MonitorInfo::new(LINUX_4K_ID, 3840, 2160, 1920, 1080, 631.0, 360.0).named("HDMI-0 (Linux 4K)")
}

/// MacBook Air 13.6": native 2560×1664, default "looks like" 1470×956.
pub fn macbook_air_13() -> MonitorInfo {
    MonitorInfo::new(AIR_13_ID, 2560, 1664, 1470, 956, 293.0, 183.0).named("MacBook Air 13.6\"")
}

/// MacBook Air 15.3": native 2880×1864, default "looks like" 1710×1107.
pub fn macbook_air_15() -> MonitorInfo {
    MonitorInfo::new(AIR_15_ID, 2880, 1864, 1710, 1107, 330.0, 206.0).named("MacBook Air 15.3\"")
}

/// The Linux desktop on the left, the Air vertically centred on the right.
pub fn compose_air(air: MonitorInfo, gap_mm: f64) -> Layout {
    compose_air_with(linux_4k(), air, gap_mm)
}

/// Same as [`compose_air`] but with a caller-supplied anchor, e.g. a monitor
/// detected from the live platform instead of the measured profile.
pub fn compose_air_with(anchor: MonitorInfo, air: MonitorInfo, gap_mm: f64) -> Layout {
    let anchor_id = anchor.id.clone();
    let air_id = air.id.clone();
    let mut layout = Layout::new(vec![anchor, air]);
    layout.place_right_of(&air_id, &anchor_id, gap_mm);
    layout.auto_center_vertical(&air_id, &anchor_id);
    layout
}
