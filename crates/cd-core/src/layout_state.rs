use cd_geometry::{EdgePair, Layout, LayoutIssue};

/// Holds the latest accepted layout, its derived edge pairs and validation
/// issues. Invalid layouts are rejected instead of entering the session.
#[derive(Debug, Clone)]
pub struct LayoutState {
    layout: Layout,
    edge_pairs: Vec<EdgePair>,
    issues: Vec<LayoutIssue>,
}

impl LayoutState {
    pub fn new(layout: Layout) -> Self {
        let edge_pairs = layout.pair_edges();
        let issues = layout.validate();
        Self {
            layout,
            edge_pairs,
            issues,
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn edge_pairs(&self) -> &[EdgePair] {
        &self.edge_pairs
    }

    pub fn issues(&self) -> &[LayoutIssue] {
        &self.issues
    }

    /// Accept a new layout if it is valid; returns the new revision.
    pub fn try_apply(&mut self, mut layout: Layout) -> Result<u64, Vec<LayoutIssue>> {
        let issues = layout.validate();
        if !issues.is_empty() {
            return Err(issues);
        }
        let revision = self.layout.revision.saturating_add(1);
        layout.revision = revision;
        self.edge_pairs = layout.pair_edges();
        self.issues.clear();
        self.layout = layout;
        Ok(revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cd_geometry::presets;

    #[test]
    fn initial_layout_derives_pairs() {
        let state = LayoutState::new(presets::compose_air(presets::macbook_air_13(), 0.0));
        assert_eq!(state.edge_pairs().len(), 1);
        assert!(state.issues().is_empty());
        assert_eq!(state.layout().revision, 1);
    }

    #[test]
    fn valid_layout_bumps_revision_and_repairs_pairs() {
        let mut state = LayoutState::new(presets::compose_air(presets::macbook_air_13(), 0.0));
        let revision = state
            .try_apply(presets::compose_air(presets::macbook_air_15(), 0.0))
            .unwrap();
        assert_eq!(revision, 2);
        assert_eq!(state.edge_pairs().len(), 1);
    }

    #[test]
    fn invalid_layout_is_rejected_without_mutating_state() {
        let mut state = LayoutState::new(presets::compose_air(presets::macbook_air_13(), 0.0));
        let mut overlapping = presets::compose_air(presets::macbook_air_13(), 0.0);
        overlapping.screens[1].origin_mm.x = 600.0;
        let result = state.try_apply(overlapping);
        assert!(result.is_err());
        assert_eq!(state.layout().revision, 1);
        assert_eq!(state.layout().screens[1].origin_mm.x, 631.0);
    }
}
