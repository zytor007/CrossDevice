use serde::{Deserialize, Serialize};

/// A point on the global millimetre canvas.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct PointMm {
    pub x: f64,
    pub y: f64,
}

impl PointMm {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn distance(self, other: Self) -> f64 {
        (self.x - other.x).hypot(self.y - other.y)
    }
}

/// A point in a monitor's logical pixel coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct PointLogical {
    pub x: f64,
    pub y: f64,
}

impl PointLogical {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn distance(self, other: Self) -> f64 {
        (self.x - other.x).hypot(self.y - other.y)
    }
}

/// A rectangle on the global millimetre canvas.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct RectMm {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl RectMm {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn left(self) -> f64 {
        self.x
    }

    pub fn right(self) -> f64 {
        self.x + self.w
    }

    pub fn top(self) -> f64 {
        self.y
    }

    pub fn bottom(self) -> f64 {
        self.y + self.h
    }

    pub fn center(self) -> PointMm {
        PointMm::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// Overlap of two rectangles, shrinking each side by `tolerance_mm` so that
    /// shared borders do not count as an overlap.
    pub fn intersection(self, other: Self, tolerance_mm: f64) -> Option<Self> {
        let left = self.left().max(other.left());
        let top = self.top().max(other.top());
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        let w = right - left;
        let h = bottom - top;
        if w > tolerance_mm && h > tolerance_mm {
            Some(Self::new(left, top, w, h))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Rotation {
    #[default]
    #[serde(rename = "0")]
    Deg0,
    #[serde(rename = "90")]
    Deg90,
    #[serde(rename = "180")]
    Deg180,
    #[serde(rename = "270")]
    Deg270,
}

impl Rotation {
    pub fn degrees(self) -> u16 {
        match self {
            Self::Deg0 => 0,
            Self::Deg90 => 90,
            Self::Deg180 => 180,
            Self::Deg270 => 270,
        }
    }

    /// Rotation of 90° or 270° swaps the display's width and height.
    pub fn swaps_axes(self) -> bool {
        matches!(self, Self::Deg90 | Self::Deg270)
    }
}

/// One physical monitor.
///
/// `native_*` and `logical_*` describe the **current, oriented** mode, which is
/// what XRandR and macOS report after rotation. `physical_*_mm` comes from EDID
/// and describes the panel in its unrotated orientation, hence the swap in
/// [`MonitorInfo::display_w_mm`] for portrait modes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub native_w: u32,
    pub native_h: u32,
    pub logical_w: u32,
    pub logical_h: u32,
    pub physical_w_mm: f64,
    pub physical_h_mm: f64,
    #[serde(default)]
    pub rotation: Rotation,
    /// Top-left corner on the global millimetre canvas.
    #[serde(default)]
    pub origin_mm: PointMm,
}

impl MonitorInfo {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        native_w: u32,
        native_h: u32,
        logical_w: u32,
        logical_h: u32,
        physical_w_mm: f64,
        physical_h_mm: f64,
    ) -> Self {
        Self {
            id: id.into(),
            name: String::new(),
            native_w,
            native_h,
            logical_w,
            logical_h,
            physical_w_mm,
            physical_h_mm,
            rotation: Rotation::Deg0,
            origin_mm: PointMm::default(),
        }
    }

    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn at(mut self, x_mm: f64, y_mm: f64) -> Self {
        self.origin_mm = PointMm::new(x_mm, y_mm);
        self
    }

    pub fn rotated(mut self, rotation: Rotation) -> Self {
        self.rotation = rotation;
        self
    }

    pub fn display_w_mm(&self) -> f64 {
        if self.rotation.swaps_axes() {
            self.physical_h_mm
        } else {
            self.physical_w_mm
        }
    }

    pub fn display_h_mm(&self) -> f64 {
        if self.rotation.swaps_axes() {
            self.physical_w_mm
        } else {
            self.physical_h_mm
        }
    }

    pub fn rect(&self) -> RectMm {
        RectMm::new(
            self.origin_mm.x,
            self.origin_mm.y,
            self.display_w_mm(),
            self.display_h_mm(),
        )
    }

    pub fn native_px_per_mm_x(&self) -> f64 {
        f64::from(self.native_w) / self.display_w_mm()
    }

    pub fn native_px_per_mm_y(&self) -> f64 {
        f64::from(self.native_h) / self.display_h_mm()
    }

    pub fn logical_px_per_mm_x(&self) -> f64 {
        f64::from(self.logical_w) / self.display_w_mm()
    }

    pub fn logical_px_per_mm_y(&self) -> f64 {
        f64::from(self.logical_h) / self.display_h_mm()
    }

    /// Effective Retina-like scale (`native / logical`). Do not assume 2.0.
    pub fn effective_scale_x(&self) -> f64 {
        f64::from(self.native_w) / f64::from(self.logical_w)
    }

    pub fn effective_scale_y(&self) -> f64 {
        f64::from(self.native_h) / f64::from(self.logical_h)
    }

    /// True panel density in physical pixels per inch.
    pub fn panel_ppi_x(&self) -> f64 {
        self.native_px_per_mm_x() * 25.4
    }

    pub fn panel_ppi_y(&self) -> f64 {
        self.native_px_per_mm_y() * 25.4
    }

    /// Density of the coordinate space that input injection actually uses.
    pub fn ui_ppi_x(&self) -> f64 {
        self.logical_px_per_mm_x() * 25.4
    }

    pub fn ui_ppi_y(&self) -> f64 {
        self.logical_px_per_mm_y() * 25.4
    }

    /// Convert a global millimetre point to this monitor's logical pixels,
    /// clamped to the drawable area.
    pub fn to_logical(&self, point: PointMm) -> PointLogical {
        let x = (point.x - self.origin_mm.x) * self.logical_px_per_mm_x();
        let y = (point.y - self.origin_mm.y) * self.logical_px_per_mm_y();
        PointLogical::new(clamp_axis(x, self.logical_w), clamp_axis(y, self.logical_h))
    }

    /// Convert a native-pixel relative delta into micrometres on the wire.
    ///
    /// The wire unit is µm so both ends can convert through millimetres without
    /// depending on each other's DPI.
    pub fn native_delta_to_um(&self, dx_native: f64, dy_native: f64) -> (i32, i32) {
        let dx_mm = dx_native / self.native_px_per_mm_x();
        let dy_mm = dy_native / self.native_px_per_mm_y();
        (
            round_micrometers(dx_mm * 1000.0),
            round_micrometers(dy_mm * 1000.0),
        )
    }

    pub fn um_to_logical_delta(&self, dx_um: i32, dy_um: i32) -> (f64, f64) {
        (
            f64::from(dx_um) / 1000.0 * self.logical_px_per_mm_x(),
            f64::from(dy_um) / 1000.0 * self.logical_px_per_mm_y(),
        )
    }

    /// X11 XTest moves the root pointer in native pixels, not in GDK logical
    /// points, so the injection path needs the native density.
    pub fn um_to_native_delta(&self, dx_um: i32, dy_um: i32) -> (f64, f64) {
        (
            f64::from(dx_um) / 1000.0 * self.native_px_per_mm_x(),
            f64::from(dy_um) / 1000.0 * self.native_px_per_mm_y(),
        )
    }

    /// Basic sanity check used before a layout can be saved.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("monitor id is empty".to_owned());
        }
        if self.native_w == 0 || self.native_h == 0 {
            return Err("native resolution is zero".to_owned());
        }
        if self.logical_w == 0 || self.logical_h == 0 {
            return Err("logical resolution is zero".to_owned());
        }
        if !self.physical_w_mm.is_finite()
            || !self.physical_h_mm.is_finite()
            || self.physical_w_mm <= 0.0
            || self.physical_h_mm <= 0.0
        {
            return Err("physical size must be positive and finite".to_owned());
        }
        if !self.origin_mm.x.is_finite() || !self.origin_mm.y.is_finite() {
            return Err("origin must be finite".to_owned());
        }
        Ok(())
    }
}

fn clamp_axis(value: f64, size: u32) -> f64 {
    let max = f64::from(size.max(1) - 1);
    value.clamp(0.0, max)
}

fn round_micrometers(value: f64) -> i32 {
    let rounded = value.round();
    if rounded > f64::from(i32::MAX) {
        i32::MAX
    } else if rounded < f64::from(i32::MIN) {
        i32::MIN
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let out = rounded as i32;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MonitorInfo {
        MonitorInfo::new("m1", 3840, 2160, 1920, 1080, 631.0, 360.0)
    }

    #[test]
    fn metrics_are_derived_from_millimetres() {
        let m = sample();
        assert!((m.native_px_per_mm_x() - 3840.0 / 631.0).abs() < 1e-9);
        assert!((m.logical_px_per_mm_x() - 1920.0 / 631.0).abs() < 1e-9);
        assert!((m.effective_scale_x() - 2.0).abs() < 1e-9);
        assert!((m.ui_ppi_x() - 77.3).abs() < 0.1);
    }

    #[test]
    fn logical_mapping_clamps_and_uses_origin() {
        let mut m = sample();
        m.origin_mm = PointMm::new(100.0, 0.0);
        assert_eq!(
            m.to_logical(PointMm::new(100.0, 0.0)),
            PointLogical::new(0.0, 0.0)
        );
        let end = m.to_logical(PointMm::new(731.0, 360.0));
        assert_eq!(end.x, 1919.0);
        assert_eq!(end.y, 1079.0);
    }

    #[test]
    fn rotation_swaps_physical_axes() {
        let mut m = sample();
        m.rotation = Rotation::Deg90;
        assert_eq!(m.display_w_mm(), 360.0);
        assert_eq!(m.display_h_mm(), 631.0);
        assert!((m.logical_px_per_mm_x() - 1920.0 / 360.0).abs() < 1e-9);
    }

    #[test]
    fn micrometre_conversion_preserves_physical_distance() {
        let source = sample();
        let target = MonitorInfo::new("air", 2560, 1664, 1470, 956, 293.0, 183.0);
        let (dx_um, _) = source.native_delta_to_um(100.0 * source.native_px_per_mm_x(), 0.0);
        assert_eq!(dx_um, 100_000);
        let (dx_logical, _) = target.um_to_logical_delta(dx_um, 0);
        let dx_mm = dx_logical / target.logical_px_per_mm_x();
        assert!((dx_mm - 100.0).abs() < 0.5);
    }

    #[test]
    fn native_density_roundtrip_for_x11_injection() {
        let monitor = sample();
        let (dx_um, _) = monitor.native_delta_to_um(100.0 * monitor.native_px_per_mm_x(), 0.0);
        let (dx_native, _) = monitor.um_to_native_delta(dx_um, 0);
        assert!((dx_native - 100.0 * monitor.native_px_per_mm_x()).abs() < 0.5);
    }

    #[test]
    fn validate_rejects_bad_geometry() {
        let mut m = sample();
        m.physical_w_mm = 0.0;
        assert!(m.validate().is_err());
        assert!(sample().validate().is_ok());
    }
}
