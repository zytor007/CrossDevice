//! Pure physical-millimetre geometry for cross-device screen layouts.
//!
//! This crate must stay free of platform APIs, async runtimes and UI concerns so
//! that every rule in the product spec can be unit-tested as a pure function.

mod adaptation;
mod edges;
mod layout;
mod monitor;
pub mod presets;

pub use adaptation::{compare_devices, DeviceComparison};
pub use edges::{edge_segment, EdgePair, EdgeSegment, Side, MAX_GAP_MM, MIN_OVERLAP_RATIO};
pub use layout::{Crossing, Layout, LayoutIssue, RoundtripCheck};
pub use monitor::{MonitorInfo, PointLogical, PointMm, RectMm, Rotation};
