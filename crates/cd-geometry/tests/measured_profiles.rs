//! Acceptance numbers from the measured Linux 4K + MacBook Air profiles.
//! These mirror section 3.3 of IMPLEMENTATION_PLAN.md.

use cd_geometry::presets::{self, AIR_13_ID, LINUX_4K_ID};
use cd_geometry::{compare_devices, Side};

fn assert_close(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "expected {expected} ± {tolerance}, got {actual}"
    );
}

#[test]
fn linux_4k_profile_matches_measurements() {
    let screen = presets::linux_4k();
    assert_close(screen.native_px_per_mm_x(), 6.0857, 0.001);
    assert_close(screen.logical_px_per_mm_x(), 3.0428, 0.001);
    assert_close(screen.panel_ppi_x(), 154.6, 0.2);
    assert_close(screen.ui_ppi_x(), 77.3, 0.1);
    assert_close(screen.effective_scale_x(), 2.0, 1e-9);
}

#[test]
fn air_13_profile_matches_measurements() {
    let screen = presets::macbook_air_13();
    assert_close(screen.logical_px_per_mm_x(), 5.0171, 0.001);
    assert_close(screen.effective_scale_x(), 1.7415, 0.001);
    assert_close(screen.ui_ppi_x(), 127.4, 0.2);
}

#[test]
fn air_13_speed_and_size_ratios() {
    let linux = presets::linux_4k();
    let air = presets::macbook_air_13();
    let comparison = compare_devices(&linux, &air);
    assert_close(comparison.speed_multiplier_x, 1.6488, 0.005);
    assert_close(comparison.width_ratio, 2.1536, 0.005);
    assert_close(comparison.height_ratio, 1.9672, 0.005);

    let reversed = compare_devices(&air, &linux);
    assert_close(reversed.speed_multiplier_x, 0.6065, 0.005);
}

#[test]
fn air_15_speed_and_size_ratios() {
    let linux = presets::linux_4k();
    let air = presets::macbook_air_15();
    let comparison = compare_devices(&linux, &air);
    assert_close(comparison.speed_multiplier_x, 1.7030, 0.005);
    assert_close(comparison.width_ratio, 1.9121, 0.005);
}

#[test]
fn shared_edge_coverage_matches_spec() {
    let expected = [(AIR_13_ID, 0.5083), ("macbook-air-15", 0.5722)];
    for (air_id, coverage) in expected {
        let air = if air_id == AIR_13_ID {
            presets::macbook_air_13()
        } else {
            presets::macbook_air_15()
        };
        let layout = presets::compose_air(air, 0.0);
        let pairs = layout.pair_edges();
        assert_eq!(pairs.len(), 1, "expected exactly one shared edge");
        let linux_edge = pairs[0]
            .segment_of(LINUX_4K_ID, Side::Right)
            .expect("linux right edge");
        assert_close(pairs[0].coverage_of(linux_edge), coverage, 0.001);
    }
}

#[test]
fn entrance_mapping_is_physical_and_clamped() {
    let layout = presets::compose_air(presets::macbook_air_13(), 0.0);
    let top = layout
        .find_crossing(LINUX_4K_ID, Side::Right, 88.5)
        .expect("top of overlap");
    assert_close(top.target_logical.y, 0.0, 0.5);
    assert_close(top.target_logical.x, 0.0, 0.5);

    let middle = layout
        .find_crossing(LINUX_4K_ID, Side::Right, 180.0)
        .expect("middle of overlap");
    assert_close(middle.target_logical.y, 478.0, 1.0);

    let bottom = layout
        .find_crossing(LINUX_4K_ID, Side::Right, 271.5)
        .expect("bottom of overlap");
    assert_close(bottom.target_logical.y, 955.0, 0.5);
}

#[test]
fn roundtrip_at_three_points_stays_within_two_pixels() {
    for air in [presets::macbook_air_13(), presets::macbook_air_15()] {
        let layout = presets::compose_air(air, 0.0);
        let overlap = layout.pair_edges().first().expect("pair").overlap_mm();
        let start = (360.0 - overlap) / 2.0;
        for fraction in [0.1, 0.5, 0.9] {
            let y = start + overlap * fraction;
            let check = layout
                .check_roundtrip(LINUX_4K_ID, Side::Right, y)
                .expect("roundtrip");
            assert_close(check.error_px, 0.0, 0.01);
        }
    }
}

#[test]
fn one_hundred_millimetres_stays_one_hundred_millimetres() {
    let linux = presets::linux_4k();
    let air = presets::macbook_air_13();
    let dx_native = 100.0 * linux.native_px_per_mm_x();
    let (dx_um, _) = linux.native_delta_to_um(dx_native, 0.0);
    assert_eq!(dx_um, 100_000);
    let (dx_logical, _) = air.um_to_logical_delta(dx_um, 0);
    assert_close(dx_logical / air.logical_px_per_mm_x(), 100.0, 0.5);
}
