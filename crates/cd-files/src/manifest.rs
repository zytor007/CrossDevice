use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::path::{normalize_rel_path, normalize_root_name, path_key, PathError, TargetPlatform};

pub const WINDOWS_MAX_PATH: usize = 260;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub rel_path: String,
    pub size: u64,
    pub is_dir: bool,
    pub modified_ms: Option<i64>,
}

impl FileEntry {
    pub fn file(rel_path: impl Into<String>, size: u64, modified_ms: Option<i64>) -> Self {
        Self {
            rel_path: rel_path.into(),
            size,
            is_dir: false,
            modified_ms,
        }
    }

    pub fn directory(rel_path: impl Into<String>, modified_ms: Option<i64>) -> Self {
        Self {
            rel_path: rel_path.into(),
            size: 0,
            is_dir: true,
            modified_ms,
        }
    }
}

/// One dropped directory tree. Paths are canonical (`/` separators, relative).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub root_name: String,
    pub entries: Vec<FileEntry>,
}

impl Manifest {
    pub fn new(root_name: impl AsRef<str>) -> Result<Self, PathError> {
        let root_name = normalize_root_name(root_name.as_ref())?;
        Ok(Self {
            root_name,
            entries: Vec::new(),
        })
    }

    pub fn from_entries(
        root_name: impl AsRef<str>,
        entries: Vec<FileEntry>,
    ) -> Result<Self, PathError> {
        let mut manifest = Self::new(root_name)?;
        manifest.entries = entries;
        Ok(manifest)
    }

    pub fn push_file(
        &mut self,
        rel_path: impl AsRef<str>,
        size: u64,
        modified_ms: Option<i64>,
    ) -> Result<(), PathError> {
        let rel_path = normalize_rel_path(rel_path.as_ref())?;
        self.entries
            .push(FileEntry::file(rel_path, size, modified_ms));
        Ok(())
    }

    pub fn push_dir(
        &mut self,
        rel_path: impl AsRef<str>,
        modified_ms: Option<i64>,
    ) -> Result<(), PathError> {
        let rel_path = normalize_rel_path(rel_path.as_ref())?;
        self.entries
            .push(FileEntry::directory(rel_path, modified_ms));
        Ok(())
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| entry.size)
            .sum()
    }

    pub fn file_count(&self) -> u64 {
        self.entries.iter().filter(|entry| !entry.is_dir).count() as u64
    }

    pub fn directory_count(&self) -> u64 {
        self.entries.iter().filter(|entry| entry.is_dir).count() as u64
    }

    pub fn validate(&self, target: TargetPlatform) -> Vec<ManifestIssue> {
        let mut issues = Vec::new();
        if let Err(error) = normalize_root_name(&self.root_name) {
            issues.push(ManifestIssue::InvalidRoot {
                reason: error.to_string(),
            });
        }

        let mut seen: BTreeMap<String, (String, bool)> = BTreeMap::new();
        let mut file_keys = BTreeSet::new();
        let mut valid_paths = Vec::new();

        for entry in &self.entries {
            let normalized = match normalize_rel_path(&entry.rel_path) {
                Ok(normalized) => normalized,
                Err(error) => {
                    issues.push(ManifestIssue::UnsafePath {
                        rel_path: entry.rel_path.clone(),
                        reason: error.to_string(),
                    });
                    continue;
                }
            };
            if normalized != entry.rel_path {
                issues.push(ManifestIssue::NonCanonicalPath {
                    rel_path: entry.rel_path.clone(),
                    normalized: normalized.clone(),
                });
            }

            if target == TargetPlatform::Windows {
                let length = self.root_name.chars().count() + 1 + entry.rel_path.chars().count();
                if length > WINDOWS_MAX_PATH {
                    issues.push(ManifestIssue::PathTooLong {
                        rel_path: entry.rel_path.clone(),
                        length,
                        limit: WINDOWS_MAX_PATH,
                    });
                }
            }

            let key = path_key(&normalized, target);
            if let Some((previous, _)) = seen.get(&key) {
                if previous == &normalized {
                    issues.push(ManifestIssue::DuplicatePath {
                        rel_path: normalized.clone(),
                    });
                } else {
                    issues.push(ManifestIssue::CaseCollision {
                        first: previous.clone(),
                        second: normalized.clone(),
                    });
                }
                continue;
            }
            seen.insert(key, (normalized.clone(), entry.is_dir));
            if !entry.is_dir {
                file_keys.insert(path_key(&normalized, target));
            }
            valid_paths.push((normalized, entry.is_dir));
        }

        for (rel_path, is_dir) in &valid_paths {
            if *is_dir {
                continue;
            }
            let mut parts: Vec<&str> = rel_path.split('/').collect();
            parts.pop();
            while !parts.is_empty() {
                let parent = parts.join("/");
                if file_keys.contains(&path_key(&parent, target)) {
                    issues.push(ManifestIssue::ParentIsFile {
                        rel_path: rel_path.clone(),
                        parent,
                    });
                    break;
                }
                parts.pop();
            }
        }

        issues
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManifestIssue {
    UnsafePath {
        rel_path: String,
        reason: String,
    },
    NonCanonicalPath {
        rel_path: String,
        normalized: String,
    },
    DuplicatePath {
        rel_path: String,
    },
    CaseCollision {
        first: String,
        second: String,
    },
    PathTooLong {
        rel_path: String,
        length: usize,
        limit: usize,
    },
    ParentIsFile {
        rel_path: String,
        parent: String,
    },
    InvalidRoot {
        reason: String,
    },
}

impl std::fmt::Display for ManifestIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsafePath { rel_path, reason } => {
                write!(f, "unsafe path `{rel_path}`: {reason}")
            }
            Self::NonCanonicalPath {
                rel_path,
                normalized,
            } => write!(
                f,
                "non-canonical path `{rel_path}` (expected `{normalized}`)"
            ),
            Self::DuplicatePath { rel_path } => write!(f, "duplicate path `{rel_path}`"),
            Self::CaseCollision { first, second } => {
                write!(f, "case collision `{first}` / `{second}`")
            }
            Self::PathTooLong {
                rel_path,
                length,
                limit,
            } => write!(f, "path `{rel_path}` is {length} chars (limit {limit})"),
            Self::ParentIsFile { rel_path, parent } => {
                write!(f, "`{parent}` is a file but is parent of `{rel_path}`")
            }
            Self::InvalidRoot { reason } => write!(f, "invalid root: {reason}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        let mut manifest = Manifest::new("assets").unwrap();
        manifest.push_dir("images", Some(1)).unwrap();
        manifest
            .push_file("images/logo.png", 2048, Some(2))
            .unwrap();
        manifest.push_file("README.txt", 12, None).unwrap();
        manifest
    }

    #[test]
    fn counts_and_sizes_only_include_files() {
        let manifest = sample();
        assert_eq!(manifest.total_bytes(), 2060);
        assert_eq!(manifest.file_count(), 2);
        assert_eq!(manifest.directory_count(), 1);
    }

    #[test]
    fn traversal_is_rejected_while_building() {
        let mut manifest = Manifest::new("assets").unwrap();
        assert!(manifest.push_file("../secret", 1, None).is_err());
        assert!(manifest.push_dir("/absolute", None).is_err());
        assert!(manifest.push_file("ok/file.txt", 1, None).is_ok());
        assert!(manifest.validate(TargetPlatform::Linux).is_empty());
    }

    #[test]
    fn valid_manifest_has_no_issues() {
        assert!(sample().validate(TargetPlatform::Linux).is_empty());
        assert!(sample().validate(TargetPlatform::Windows).is_empty());
    }

    #[test]
    fn windows_case_collisions_are_rejected() {
        let manifest = Manifest::from_entries(
            "assets",
            vec![
                FileEntry::file("Docs/Report.pdf", 1, None),
                FileEntry::file("docs/report.PDF", 2, None),
            ],
        )
        .unwrap();
        let issues = manifest.validate(TargetPlatform::Windows);
        assert!(issues
            .iter()
            .any(|issue| matches!(issue, ManifestIssue::CaseCollision { .. })));
        assert!(manifest.validate(TargetPlatform::Linux).is_empty());
    }

    #[test]
    fn duplicate_paths_are_rejected() {
        let manifest = Manifest::from_entries(
            "assets",
            vec![
                FileEntry::file("same.txt", 1, None),
                FileEntry::file("same.txt", 2, None),
            ],
        )
        .unwrap();
        assert!(manifest
            .validate(TargetPlatform::Linux)
            .iter()
            .any(|issue| matches!(issue, ManifestIssue::DuplicatePath { .. })));
    }

    #[test]
    fn a_file_cannot_be_the_parent_of_another_entry() {
        let manifest = Manifest::from_entries(
            "assets",
            vec![
                FileEntry::file("a", 1, None),
                FileEntry::file("a/b.txt", 2, None),
            ],
        )
        .unwrap();
        assert!(manifest
            .validate(TargetPlatform::Linux)
            .iter()
            .any(|issue| matches!(issue, ManifestIssue::ParentIsFile { .. })));
    }

    #[test]
    fn windows_path_length_is_enforced() {
        let long = format!("{}.txt", "x".repeat(300));
        let manifest =
            Manifest::from_entries("assets", vec![FileEntry::file(long, 1, None)]).unwrap();
        assert!(manifest
            .validate(TargetPlatform::Windows)
            .iter()
            .any(|issue| matches!(issue, ManifestIssue::PathTooLong { .. })));
        assert!(manifest.validate(TargetPlatform::Linux).is_empty());
    }

    #[test]
    fn deserialised_unsafe_paths_are_reported() {
        let manifest =
            Manifest::from_entries("assets", vec![FileEntry::file("../escape", 1, None)]).unwrap();
        assert!(manifest
            .validate(TargetPlatform::Linux)
            .iter()
            .any(|issue| matches!(issue, ManifestIssue::UnsafePath { .. })));
    }
}
