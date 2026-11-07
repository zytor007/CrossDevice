use crate::content::Snapshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryItem {
    snapshot: Snapshot,
    pinned: bool,
    created_at_ms: u64,
}

impl HistoryItem {
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn pinned(&self) -> bool {
        self.pinned
    }

    pub fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Added,
    Duplicate,
}

/// Newest-first clipboard history. Duplicates move to the front; pinned items
/// are exempt from eviction.
#[derive(Debug, Clone)]
pub struct History {
    limit: usize,
    items: Vec<HistoryItem>,
}

impl History {
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            items: Vec::new(),
        }
    }

    pub fn push(&mut self, snapshot: Snapshot, now_ms: u64) -> PushOutcome {
        if let Some(index) = self
            .items
            .iter()
            .position(|item| item.snapshot.hash == snapshot.hash)
        {
            let mut existing = self.items.remove(index);
            existing.created_at_ms = now_ms;
            self.items.insert(0, existing);
            return PushOutcome::Duplicate;
        }

        self.items.insert(
            0,
            HistoryItem {
                snapshot,
                pinned: false,
                created_at_ms: now_ms,
            },
        );
        self.evict();
        PushOutcome::Added
    }

    pub fn items(&self) -> &[HistoryItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn pin(&mut self, hash: &str) -> bool {
        if let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.snapshot.hash == hash)
        {
            item.pinned = true;
            true
        } else {
            false
        }
    }

    pub fn unpin(&mut self, hash: &str) -> bool {
        if let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.snapshot.hash == hash)
        {
            item.pinned = false;
            true
        } else {
            false
        }
    }

    fn evict(&mut self) {
        while self.items.len() > self.limit {
            let Some(index) = self.items.iter().rposition(|item| !item.pinned) else {
                break;
            };
            self.items.remove(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::Content;

    fn snapshot(text: &str) -> Snapshot {
        Snapshot::new(Content::text(text), "linux-4k", 1)
    }

    #[test]
    fn oldest_unpinned_item_is_evicted() {
        let mut history = History::new(3);
        for (index, text) in ["a", "b", "c", "d"].iter().enumerate() {
            history.push(snapshot(text), index as u64);
        }
        assert_eq!(history.len(), 3);
        assert_eq!(history.items()[0].snapshot().content, Content::text("d"));
        assert!(!history
            .items()
            .iter()
            .any(|item| item.snapshot().content == Content::text("a")));
    }

    #[test]
    fn duplicate_moves_to_front_without_growing() {
        let mut history = History::new(3);
        history.push(snapshot("a"), 0);
        history.push(snapshot("b"), 1);
        assert_eq!(history.push(snapshot("a"), 2), PushOutcome::Duplicate);
        assert_eq!(history.len(), 2);
        assert_eq!(history.items()[0].snapshot().content, Content::text("a"));
        assert_eq!(history.items()[0].created_at_ms(), 2);
    }

    #[test]
    fn pinned_items_survive_eviction() {
        let mut history = History::new(2);
        history.push(snapshot("a"), 0);
        assert!(history.pin(&snapshot("a").hash));
        history.push(snapshot("b"), 1);
        history.push(snapshot("c"), 2);
        history.push(snapshot("d"), 3);
        assert!(history
            .items()
            .iter()
            .any(|item| item.snapshot().content == Content::text("a")));
        assert!(!history
            .items()
            .iter()
            .any(|item| item.snapshot().content == Content::text("b")));
    }

    #[test]
    fn pin_and_unpin_report_missing_hashes() {
        let mut history = History::new(3);
        assert!(!history.pin("missing"));
        history.push(snapshot("a"), 0);
        let hash = history.items()[0].snapshot().hash.clone();
        assert!(history.pin(&hash));
        assert!(history.unpin(&hash));
    }
}
