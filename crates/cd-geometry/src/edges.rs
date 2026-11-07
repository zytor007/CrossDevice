use serde::{Deserialize, Serialize};

use crate::monitor::{MonitorInfo, RectMm};

/// Two edges farther apart than this cannot be paired; the gap becomes dead space.
pub const MAX_GAP_MM: f64 = 20.0;

/// Minimum projection overlap, relative to the shorter edge, before two edges pair.
pub const MIN_OVERLAP_RATIO: f64 = 0.25;

const EPS_MM: f64 = 0.1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }

    /// Left/Right edges are vertical lines; Top/Bottom are horizontal lines.
    pub fn is_vertical(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Top => "top",
            Self::Bottom => "bottom",
        };
        f.write_str(name)
    }
}

/// One edge of one monitor, expressed in global millimetre coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeSegment {
    pub monitor_id: String,
    pub side: Side,
    /// X for vertical edges, Y for horizontal edges.
    pub fixed_mm: f64,
    /// Start on the shared axis (Y for vertical, X for horizontal).
    pub start_mm: f64,
    pub end_mm: f64,
}

impl EdgeSegment {
    pub fn len(&self) -> f64 {
        (self.end_mm - self.start_mm).max(0.0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() <= EPS_MM
    }

    pub fn contains(&self, along_mm: f64, tolerance_mm: f64) -> bool {
        along_mm >= self.start_mm - tolerance_mm && along_mm <= self.end_mm + tolerance_mm
    }

    pub fn midpoint_mm(&self) -> f64 {
        (self.start_mm + self.end_mm) / 2.0
    }
}

/// A pair of facing edges with a usable overlap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgePair {
    pub a: EdgeSegment,
    pub b: EdgeSegment,
    pub gap_mm: f64,
    pub overlap_start_mm: f64,
    pub overlap_end_mm: f64,
}

impl EdgePair {
    pub fn overlap_mm(&self) -> f64 {
        (self.overlap_end_mm - self.overlap_start_mm).max(0.0)
    }

    /// Overlap relative to the shorter edge. This is the pairing gate.
    pub fn overlap_ratio(&self) -> f64 {
        let shorter = self.a.len().min(self.b.len());
        if shorter <= 0.0 {
            0.0
        } else {
            self.overlap_mm() / shorter
        }
    }

    pub fn covers(&self, segment: &EdgeSegment) -> bool {
        segment.monitor_id == self.a.monitor_id && segment.side == self.a.side
            || segment.monitor_id == self.b.monitor_id && segment.side == self.b.side
    }

    /// Coverage of this pair relative to one device's full edge length, which is
    /// what the UI shows, e.g. `183/360 mm (50.8%)`.
    pub fn coverage_of(&self, segment: &EdgeSegment) -> f64 {
        if segment.len() <= 0.0 {
            0.0
        } else {
            self.overlap_mm() / segment.len()
        }
    }

    pub fn segment_of(&self, monitor_id: &str, side: Side) -> Option<&EdgeSegment> {
        if self.a.monitor_id == monitor_id && self.a.side == side {
            Some(&self.a)
        } else if self.b.monitor_id == monitor_id && self.b.side == side {
            Some(&self.b)
        } else {
            None
        }
    }

    pub fn other_segment(&self, monitor_id: &str, side: Side) -> Option<&EdgeSegment> {
        if self.a.monitor_id == monitor_id && self.a.side == side {
            Some(&self.b)
        } else if self.b.monitor_id == monitor_id && self.b.side == side {
            Some(&self.a)
        } else {
            None
        }
    }

    pub fn contains_along(&self, along_mm: f64) -> bool {
        along_mm >= self.overlap_start_mm - EPS_MM && along_mm <= self.overlap_end_mm + EPS_MM
    }

    /// Position inside the overlap, normalised to 0..65535 for C1 absolute entry.
    pub fn normalized_along(&self, along_mm: f64) -> u16 {
        let overlap = self.overlap_mm();
        if overlap <= 0.0 {
            return 0;
        }
        let t = ((along_mm - self.overlap_start_mm) / overlap).clamp(0.0, 1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let value = (t * 65535.0).round() as u16;
        value
    }
}

pub fn edge_segment(monitor: &MonitorInfo, side: Side) -> EdgeSegment {
    let rect = monitor.rect();
    match side {
        Side::Left => EdgeSegment {
            monitor_id: monitor.id.clone(),
            side,
            fixed_mm: rect.left(),
            start_mm: rect.top(),
            end_mm: rect.bottom(),
        },
        Side::Right => EdgeSegment {
            monitor_id: monitor.id.clone(),
            side,
            fixed_mm: rect.right(),
            start_mm: rect.top(),
            end_mm: rect.bottom(),
        },
        Side::Top => EdgeSegment {
            monitor_id: monitor.id.clone(),
            side,
            fixed_mm: rect.top(),
            start_mm: rect.left(),
            end_mm: rect.right(),
        },
        Side::Bottom => EdgeSegment {
            monitor_id: monitor.id.clone(),
            side,
            fixed_mm: rect.bottom(),
            start_mm: rect.left(),
            end_mm: rect.right(),
        },
    }
}

/// Pair every facing edge in the layout. Screen order only affects output order;
/// each physical adjacency appears once.
pub(crate) fn pair_all(screens: &[MonitorInfo]) -> Vec<EdgePair> {
    let mut pairs = Vec::new();
    for i in 0..screens.len() {
        for j in (i + 1)..screens.len() {
            let first = &screens[i];
            let second = &screens[j];
            for side in [Side::Right, Side::Left, Side::Bottom, Side::Top] {
                if let Some(pair) = try_pair(first, second, side) {
                    pairs.push(pair);
                }
            }
        }
    }
    pairs.sort_by(|left, right| {
        let key = |pair: &EdgePair| {
            (
                pair.a.monitor_id.clone(),
                pair.a.side,
                pair.b.monitor_id.clone(),
                pair.b.side,
            )
        };
        key(left).cmp(&key(right))
    });
    pairs
}

fn try_pair(first: &MonitorInfo, second: &MonitorInfo, first_side: Side) -> Option<EdgePair> {
    let first_rect: RectMm = first.rect();
    let second_rect: RectMm = second.rect();

    let gap_mm = match first_side {
        Side::Right => second_rect.left() - first_rect.right(),
        Side::Left => first_rect.left() - second_rect.right(),
        Side::Bottom => second_rect.top() - first_rect.bottom(),
        Side::Top => first_rect.top() - second_rect.bottom(),
    };
    if gap_mm < -EPS_MM || gap_mm > MAX_GAP_MM {
        return None;
    }

    let a = edge_segment(first, first_side);
    let b = edge_segment(second, first_side.opposite());
    if a.is_empty() || b.is_empty() {
        return None;
    }

    let overlap_start_mm = a.start_mm.max(b.start_mm);
    let overlap_end_mm = a.end_mm.min(b.end_mm);
    let overlap = overlap_end_mm - overlap_start_mm;
    if overlap <= EPS_MM {
        return None;
    }
    let shorter = a.len().min(b.len());
    if overlap / shorter < MIN_OVERLAP_RATIO {
        return None;
    }

    Some(EdgePair {
        a,
        b,
        gap_mm,
        overlap_start_mm,
        overlap_end_mm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets;

    #[test]
    fn adjacent_faces_pair_with_full_coverage_on_the_small_screen() {
        let layout = presets::compose_air(presets::macbook_air_13(), 0.0);
        let pairs = pair_all(&layout.screens);
        assert_eq!(pairs.len(), 1);
        let pair = &pairs[0];
        let linux = pair.segment_of("linux-4k", Side::Right).unwrap();
        let air = pair.segment_of("macbook-air-13", Side::Left).unwrap();
        assert!((pair.overlap_mm() - 183.0).abs() < 0.1);
        assert!((pair.coverage_of(linux) - 183.0 / 360.0).abs() < 1e-6);
        assert!((pair.coverage_of(air) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn distant_edges_do_not_pair() {
        let mut layout = presets::compose_air(presets::macbook_air_13(), 25.0);
        let pairs = pair_all(&layout.screens);
        assert!(pairs.is_empty());
        layout.screens[1].origin_mm.x += 1.0;
        assert!(pair_all(&layout.screens).is_empty());
    }

    #[test]
    fn small_projection_overlap_does_not_pair() {
        let mut layout = presets::compose_air(presets::macbook_air_13(), 0.0);
        // Push the Air almost off the bottom: only ~20 mm of 183 mm remains.
        layout.screens[1].origin_mm.y = 340.0;
        let pairs = pair_all(&layout.screens);
        assert!(pairs.is_empty());
    }
}
