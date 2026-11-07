use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::manifest::Manifest;
use crate::path::{path_key, TargetPlatform};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Ask once, apply the same answer to the whole task.
    Ask,
    Overwrite,
    Skip,
    Rename,
    /// Merge directories; files still need a decision.
    Merge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictAction {
    Write,
    Ask,
    Overwrite,
    Skip,
    Rename,
    MergeDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictPlanEntry {
    pub rel_path: String,
    pub destination: String,
    pub action: ConflictAction,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictPlan {
    pub entries: Vec<ConflictPlanEntry>,
}

impl ConflictPlan {
    pub fn count(&self, action: ConflictAction) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.action == action)
            .count()
    }

    pub fn needs_attention(&self) -> bool {
        self.count(ConflictAction::Ask) > 0
    }
}

/// Decide what to do with every manifest entry against the existing destination.
///
/// Directory decisions are made first (parents before children), and children
/// follow a renamed parent. Skipping a directory skips its whole subtree.
pub fn plan_conflicts(
    manifest: &Manifest,
    existing_files: &[String],
    existing_dirs: &[String],
    policy: ConflictPolicy,
    target: TargetPlatform,
) -> ConflictPlan {
    let existing_file_keys = existing_files
        .iter()
        .map(|path| path_key(path, target))
        .collect::<BTreeSet<_>>();
    let existing_dir_keys = existing_dirs
        .iter()
        .map(|path| path_key(path, target))
        .collect::<BTreeSet<_>>();
    let mut occupied = existing_file_keys
        .union(&existing_dir_keys)
        .cloned()
        .collect::<BTreeSet<_>>();

    let mut dir_map: BTreeMap<String, String> = BTreeMap::new();
    let mut skipped_dirs: BTreeSet<String> = BTreeSet::new();
    let mut planned: BTreeMap<String, ConflictPlanEntry> = BTreeMap::new();

    let mut directories = manifest
        .entries
        .iter()
        .filter(|entry| entry.is_dir)
        .collect::<Vec<_>>();
    directories.sort_by_key(|entry| entry.rel_path.matches('/').count());

    for entry in directories {
        let base = remap_path(&entry.rel_path, &dir_map);
        let parent_skipped = ancestor_skipped(&entry.rel_path, &skipped_dirs);
        let exists_dir = existing_dir_keys.contains(&path_key(&base, target));
        let exists_file = existing_file_keys.contains(&path_key(&base, target));

        let action = if parent_skipped {
            ConflictAction::Skip
        } else if exists_dir {
            match policy {
                ConflictPolicy::Ask => ConflictAction::Ask,
                ConflictPolicy::Skip => ConflictAction::Skip,
                ConflictPolicy::Rename => ConflictAction::Rename,
                ConflictPolicy::Overwrite | ConflictPolicy::Merge => ConflictAction::MergeDirectory,
            }
        } else if exists_file {
            // A file where a directory should go cannot be merged safely.
            match policy {
                ConflictPolicy::Skip => ConflictAction::Skip,
                ConflictPolicy::Rename => ConflictAction::Rename,
                ConflictPolicy::Ask | ConflictPolicy::Overwrite | ConflictPolicy::Merge => {
                    ConflictAction::Ask
                }
            }
        } else {
            ConflictAction::Write
        };

        let destination = if action == ConflictAction::Rename {
            unique_rename(&base, &occupied, target)
        } else {
            base
        };

        if matches!(
            action,
            ConflictAction::Write
                | ConflictAction::Overwrite
                | ConflictAction::Rename
                | ConflictAction::MergeDirectory
        ) {
            occupied.insert(path_key(&destination, target));
        }
        dir_map.insert(entry.rel_path.clone(), destination.clone());
        if action == ConflictAction::Skip {
            skipped_dirs.insert(entry.rel_path.clone());
        }

        planned.insert(
            entry.rel_path.clone(),
            ConflictPlanEntry {
                rel_path: entry.rel_path.clone(),
                destination,
                action,
                is_dir: true,
            },
        );
    }

    for entry in manifest.entries.iter().filter(|entry| !entry.is_dir) {
        let base = remap_path(&entry.rel_path, &dir_map);
        let parent_skipped = ancestor_skipped(&entry.rel_path, &skipped_dirs);
        let exists_dir = existing_dir_keys.contains(&path_key(&base, target));
        let exists_file = existing_file_keys.contains(&path_key(&base, target));

        let action = if parent_skipped {
            ConflictAction::Skip
        } else if exists_file {
            match policy {
                ConflictPolicy::Ask => ConflictAction::Ask,
                ConflictPolicy::Overwrite => ConflictAction::Overwrite,
                ConflictPolicy::Skip => ConflictAction::Skip,
                ConflictPolicy::Rename => ConflictAction::Rename,
                ConflictPolicy::Merge => ConflictAction::Ask,
            }
        } else if exists_dir {
            // A directory cannot be replaced by a file without an explicit ask.
            match policy {
                ConflictPolicy::Skip => ConflictAction::Skip,
                ConflictPolicy::Rename => ConflictAction::Rename,
                ConflictPolicy::Ask | ConflictPolicy::Overwrite | ConflictPolicy::Merge => {
                    ConflictAction::Ask
                }
            }
        } else {
            ConflictAction::Write
        };

        let destination = if action == ConflictAction::Rename {
            unique_rename(&base, &occupied, target)
        } else {
            base
        };

        if matches!(
            action,
            ConflictAction::Write | ConflictAction::Overwrite | ConflictAction::Rename
        ) {
            occupied.insert(path_key(&destination, target));
        }

        planned.insert(
            entry.rel_path.clone(),
            ConflictPlanEntry {
                rel_path: entry.rel_path.clone(),
                destination,
                action,
                is_dir: false,
            },
        );
    }

    let entries = manifest
        .entries
        .iter()
        .filter_map(|entry| planned.remove(&entry.rel_path))
        .collect();

    ConflictPlan { entries }
}

/// Replace the longest mapped ancestor directory, keeping the rest of the path.
fn remap_path(path: &str, dir_map: &BTreeMap<String, String>) -> String {
    let components = path.split('/').collect::<Vec<_>>();
    let limit = components.len().saturating_sub(1);
    for take in (1..=limit).rev() {
        let prefix = components[..take].join("/");
        if let Some(destination) = dir_map.get(&prefix) {
            let rest = components[take..].join("/");
            if rest.is_empty() {
                return destination.clone();
            }
            return format!("{destination}/{rest}");
        }
    }
    path.to_owned()
}

fn ancestor_skipped(path: &str, skipped: &BTreeSet<String>) -> bool {
    let components = path.split('/').collect::<Vec<_>>();
    for take in 1..components.len() {
        if skipped.contains(&components[..take].join("/")) {
            return true;
        }
    }
    false
}

fn unique_rename(path: &str, occupied: &BTreeSet<String>, target: TargetPlatform) -> String {
    let (directory, name) = match path.rsplit_once('/') {
        Some((directory, name)) => (Some(directory), name),
        None => (None, path),
    };
    let (stem, extension) = split_extension(name);

    let mut counter = 2_u64;
    loop {
        let candidate_name = format!("{stem}~{counter}{extension}");
        let candidate = match directory {
            Some(directory) => format!("{directory}/{candidate_name}"),
            None => candidate_name,
        };
        if !occupied.contains(&path_key(&candidate, target)) {
            return candidate;
        }
        counter += 1;
    }
}

fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(position) if position > 0 => (&name[..position], &name[position..]),
        _ => (name, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::FileEntry;

    fn manifest(entries: Vec<FileEntry>) -> Manifest {
        Manifest::from_entries("assets", entries).unwrap()
    }

    #[test]
    fn no_conflict_writes_everything() {
        let manifest = manifest(vec![
            FileEntry::directory("images", None),
            FileEntry::file("images/logo.png", 1, None),
        ]);
        let plan = plan_conflicts(
            &manifest,
            &[],
            &[],
            ConflictPolicy::Ask,
            TargetPlatform::Linux,
        );
        assert_eq!(plan.count(ConflictAction::Write), 2);
        assert!(!plan.needs_attention());
    }

    #[test]
    fn rename_keeps_the_extension_and_finds_a_free_name() {
        let manifest = manifest(vec![FileEntry::file("docs/report.pdf", 1, None)]);
        let existing = vec!["docs/report.pdf".to_owned(), "docs/report~2.pdf".to_owned()];
        let plan = plan_conflicts(
            &manifest,
            &existing,
            &[],
            ConflictPolicy::Rename,
            TargetPlatform::Linux,
        );
        assert_eq!(plan.entries[0].action, ConflictAction::Rename);
        assert_eq!(plan.entries[0].destination, "docs/report~3.pdf");
    }

    #[test]
    fn renamed_directory_carries_its_children() {
        let manifest = manifest(vec![
            FileEntry::directory("images", None),
            FileEntry::file("images/logo.png", 1, None),
        ]);
        let plan = plan_conflicts(
            &manifest,
            &[],
            &["images".to_owned()],
            ConflictPolicy::Rename,
            TargetPlatform::Linux,
        );
        assert_eq!(plan.entries[0].destination, "images~2");
        assert_eq!(plan.entries[1].destination, "images~2/logo.png");
        assert_eq!(plan.entries[1].action, ConflictAction::Write);
    }

    #[test]
    fn skipped_directory_skips_its_children() {
        let manifest = manifest(vec![
            FileEntry::directory("images", None),
            FileEntry::file("images/logo.png", 1, None),
        ]);
        let plan = plan_conflicts(
            &manifest,
            &[],
            &["images".to_owned()],
            ConflictPolicy::Skip,
            TargetPlatform::Linux,
        );
        assert_eq!(plan.count(ConflictAction::Skip), 2);
    }

    #[test]
    fn directories_merge_and_files_ask_under_merge_policy() {
        let manifest = manifest(vec![
            FileEntry::directory("images", None),
            FileEntry::file("images/logo.png", 1, None),
        ]);
        let plan = plan_conflicts(
            &manifest,
            &["images/logo.png".to_owned()],
            &["images".to_owned()],
            ConflictPolicy::Merge,
            TargetPlatform::Linux,
        );
        assert_eq!(plan.entries[0].action, ConflictAction::MergeDirectory);
        assert_eq!(plan.entries[1].action, ConflictAction::Ask);
        assert!(plan.needs_attention());
    }

    #[test]
    fn ask_overwrite_and_skip_are_honoured() {
        let manifest = manifest(vec![FileEntry::file("a.txt", 1, None)]);
        let existing = vec!["a.txt".to_owned()];
        assert_eq!(
            plan_conflicts(
                &manifest,
                &existing,
                &[],
                ConflictPolicy::Ask,
                TargetPlatform::Linux
            )
            .entries[0]
                .action,
            ConflictAction::Ask
        );
        assert_eq!(
            plan_conflicts(
                &manifest,
                &existing,
                &[],
                ConflictPolicy::Overwrite,
                TargetPlatform::Linux
            )
            .entries[0]
                .action,
            ConflictAction::Overwrite
        );
        assert_eq!(
            plan_conflicts(
                &manifest,
                &existing,
                &[],
                ConflictPolicy::Skip,
                TargetPlatform::Linux
            )
            .entries[0]
                .action,
            ConflictAction::Skip
        );
    }

    #[test]
    fn windows_case_collision_counts_as_a_conflict() {
        let manifest = manifest(vec![FileEntry::file("Docs/Report.pdf", 1, None)]);
        let plan = plan_conflicts(
            &manifest,
            &["docs/report.pdf".to_owned()],
            &[],
            ConflictPolicy::Rename,
            TargetPlatform::Windows,
        );
        assert_eq!(plan.entries[0].action, ConflictAction::Rename);
        assert_eq!(plan.entries[0].destination, "Docs/Report~2.pdf");
    }

    #[test]
    fn file_where_a_directory_belongs_requires_a_decision() {
        let manifest = manifest(vec![FileEntry::directory("assets", None)]);
        let plan = plan_conflicts(
            &manifest,
            &["assets".to_owned()],
            &[],
            ConflictPolicy::Merge,
            TargetPlatform::Linux,
        );
        assert_eq!(plan.entries[0].action, ConflictAction::Ask);
    }

    #[test]
    fn directory_where_a_file_belongs_requires_a_decision() {
        let manifest = manifest(vec![FileEntry::file("assets", 1, None)]);
        let plan = plan_conflicts(
            &manifest,
            &[],
            &["assets".to_owned()],
            ConflictPolicy::Overwrite,
            TargetPlatform::Linux,
        );
        assert_eq!(plan.entries[0].action, ConflictAction::Ask);
    }

    #[test]
    fn directory_rename_has_no_extension() {
        let manifest = manifest(vec![FileEntry::directory("assets", None)]);
        let plan = plan_conflicts(
            &manifest,
            &[],
            &["assets".to_owned()],
            ConflictPolicy::Rename,
            TargetPlatform::Linux,
        );
        assert_eq!(plan.entries[0].destination, "assets~2");
    }
}
