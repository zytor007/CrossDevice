use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::edges::{edge_segment, pair_all, EdgePair, Side};
use crate::monitor::{MonitorInfo, PointLogical, PointMm};

const OVERLAP_TOLERANCE_MM: f64 = 0.1;
const CROSSING_TOLERANCE_MM: f64 = 0.5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub revision: u64,
    pub screens: Vec<MonitorInfo>,
}

impl Layout {
    pub fn new(screens: Vec<MonitorInfo>) -> Self {
        Self {
            revision: 1,
            screens,
        }
    }

    pub fn screen(&self, id: &str) -> Option<&MonitorInfo> {
        self.screens.iter().find(|screen| screen.id == id)
    }

    pub fn screen_mut(&mut self, id: &str) -> Option<&mut MonitorInfo> {
        self.screens.iter_mut().find(|screen| screen.id == id)
    }

    /// Reject layouts that the rest of the system cannot reason about.
    /// Gaps are allowed (they are dead zones), overlaps are not.
    pub fn validate(&self) -> Vec<LayoutIssue> {
        let mut issues = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for screen in &self.screens {
            if !seen.insert(screen.id.as_str()) {
                issues.push(LayoutIssue::DuplicateId {
                    id: screen.id.clone(),
                });
            }
            if let Err(reason) = screen.validate() {
                issues.push(LayoutIssue::InvalidScreen {
                    id: screen.id.clone(),
                    reason,
                });
            }
        }

        for i in 0..self.screens.len() {
            for j in (i + 1)..self.screens.len() {
                let a = &self.screens[i];
                let b = &self.screens[j];
                if a.rect()
                    .intersection(b.rect(), OVERLAP_TOLERANCE_MM)
                    .is_some()
                {
                    issues.push(LayoutIssue::OverlappingScreens {
                        a: a.id.clone(),
                        b: b.id.clone(),
                    });
                }
            }
        }
        issues
    }

    pub fn pair_edges(&self) -> Vec<EdgePair> {
        pair_all(&self.screens)
    }

    /// The only legal way to leave a screen: outside an overlap segment the
    /// pointer stops at the edge (dead zone).
    pub fn find_crossing(&self, source_id: &str, side: Side, along_mm: f64) -> Option<Crossing> {
        let source = self.screen(source_id)?;
        let source_edge = edge_segment(source, side);
        if !source_edge.contains(along_mm, CROSSING_TOLERANCE_MM) {
            return None;
        }

        let pairs = self.pair_edges();
        for (pair_index, pair) in pairs.iter().enumerate() {
            let Some(_) = pair.segment_of(source_id, side) else {
                continue;
            };
            if !pair.contains_along(along_mm) {
                continue;
            }
            let target_edge = pair.other_segment(source_id, side)?;
            let target = self.screen(&target_edge.monitor_id)?;

            let (source_mm, target_mm) = if side.is_vertical() {
                (
                    PointMm::new(source_edge.fixed_mm, along_mm),
                    PointMm::new(target_edge.fixed_mm, along_mm),
                )
            } else {
                (
                    PointMm::new(along_mm, source_edge.fixed_mm),
                    PointMm::new(along_mm, target_edge.fixed_mm),
                )
            };

            let overlap_start = pair.overlap_start_mm;
            let overlap = pair.overlap_mm();
            let t = if overlap <= 0.0 {
                0.0
            } else {
                ((along_mm - overlap_start) / overlap).clamp(0.0, 1.0)
            };

            return Some(Crossing {
                source_id: source_id.to_owned(),
                target_id: target.id.clone(),
                source_side: side,
                target_side: side.opposite(),
                source_mm,
                target_mm,
                target_logical: target.to_logical(target_mm),
                overlap_t: t,
                normalized_along: pair.normalized_along(along_mm),
                pair_index,
            });
        }
        None
    }

    /// Cross out and immediately back; a healthy layout lands within 2 logical
    /// pixels of the original edge point.
    pub fn check_roundtrip(
        &self,
        source_id: &str,
        side: Side,
        along_mm: f64,
    ) -> Option<RoundtripCheck> {
        let outbound = self.find_crossing(source_id, side, along_mm)?;
        let along_back = if side.is_vertical() {
            outbound.target_mm.y
        } else {
            outbound.target_mm.x
        };
        let inbound = self.find_crossing(&outbound.target_id, outbound.target_side, along_back)?;
        let expected = self.screen(source_id)?.to_logical(outbound.source_mm);
        let dx_px = inbound.target_logical.x - expected.x;
        let dy_px = inbound.target_logical.y - expected.y;
        Some(RoundtripCheck {
            outbound,
            inbound,
            dx_px,
            dy_px,
            error_px: dx_px.hypot(dy_px),
        })
    }

    /// Vertically centre `screen_id` against `anchor_id` without touching X.
    pub fn auto_center_vertical(&mut self, screen_id: &str, anchor_id: &str) -> bool {
        let Some(anchor_center_y) = self.screen(anchor_id).map(|a| a.rect().center().y) else {
            return false;
        };
        let Some(screen) = self.screen_mut(screen_id) else {
            return false;
        };
        screen.origin_mm.y = anchor_center_y - screen.display_h_mm() / 2.0;
        true
    }

    /// Place `screen_id` immediately to the right of `anchor_id`, with an
    /// explicit dead-zone gap.
    pub fn place_right_of(&mut self, screen_id: &str, anchor_id: &str, gap_mm: f64) -> bool {
        let Some(anchor_right) = self.screen(anchor_id).map(|a| a.rect().right()) else {
            return false;
        };
        let Some(screen) = self.screen_mut(screen_id) else {
            return false;
        };
        screen.origin_mm.x = anchor_right + gap_mm;
        true
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Crossing {
    pub source_id: String,
    pub target_id: String,
    pub source_side: Side,
    pub target_side: Side,
    pub source_mm: PointMm,
    pub target_mm: PointMm,
    pub target_logical: PointLogical,
    /// Position within the overlap segment, 0..1.
    pub overlap_t: f64,
    /// Same position quantised for a C1 absolute entry packet.
    pub normalized_along: u16,
    pub pair_index: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoundtripCheck {
    pub outbound: Crossing,
    pub inbound: Crossing,
    pub dx_px: f64,
    pub dy_px: f64,
    pub error_px: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LayoutIssue {
    DuplicateId { id: String },
    InvalidScreen { id: String, reason: String },
    OverlappingScreens { a: String, b: String },
}

impl fmt::Display for LayoutIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId { id } => write!(f, "duplicate screen id: {id}"),
            Self::InvalidScreen { id, reason } => write!(f, "invalid screen {id}: {reason}"),
            Self::OverlappingScreens { a, b } => {
                write!(f, "screens overlap: {a} / {b}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets;

    #[test]
    fn valid_side_by_side_layout_has_no_issues() {
        let layout = presets::compose_air(presets::macbook_air_13(), 0.0);
        assert!(layout.validate().is_empty());
    }

    #[test]
    fn overlapping_layout_is_rejected() {
        let mut layout = presets::compose_air(presets::macbook_air_13(), 0.0);
        layout.screens[1].origin_mm.x = 600.0;
        let issues = layout.validate();
        assert!(issues
            .iter()
            .any(|issue| matches!(issue, LayoutIssue::OverlappingScreens { .. })));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let mut layout = presets::compose_air(presets::macbook_air_13(), 0.0);
        layout.screens[1].id = "linux-4k".to_owned();
        assert!(layout
            .validate()
            .iter()
            .any(|issue| matches!(issue, LayoutIssue::DuplicateId { .. })));
    }

    #[test]
    fn crossing_is_identity_on_the_shared_physical_axis() {
        let layout = presets::compose_air(presets::macbook_air_13(), 0.0);
        let crossing = layout
            .find_crossing("linux-4k", Side::Right, 180.0)
            .expect("centre should cross");
        assert_eq!(crossing.target_id, "macbook-air-13");
        assert!((crossing.target_mm.y - 180.0).abs() < 1e-9);
        assert!((crossing.target_logical.y - 478.0).abs() < 1.0);
    }

    #[test]
    fn dead_zone_outside_overlap_does_not_cross() {
        let layout = presets::compose_air(presets::macbook_air_13(), 0.0);
        assert!(layout
            .find_crossing("linux-4k", Side::Right, 20.0)
            .is_none());
        assert!(layout
            .find_crossing("linux-4k", Side::Right, 340.0)
            .is_none());
    }

    #[test]
    fn roundtrip_stays_within_two_logical_pixels() {
        let layout = presets::compose_air(presets::macbook_air_13(), 0.0);
        for y in [90.0, 180.0, 270.0] {
            let check = layout
                .check_roundtrip("linux-4k", Side::Right, y)
                .expect("roundtrip");
            assert!(check.error_px <= 2.0, "error was {}", check.error_px);
        }
    }
}
