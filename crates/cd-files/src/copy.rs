use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use thiserror::Error;
use walkdir::WalkDir;

use crate::conflict::{plan_conflicts, ConflictAction, ConflictPlan, ConflictPolicy};
use crate::manifest::Manifest;
use crate::path::TargetPlatform;
use crate::scan::{relative_path, scan_dir, ScanError};
use crate::task::{TransferError, TransferTask};

#[derive(Debug, Error)]
pub enum CopyError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("scan failed: {0}")]
    Scan(#[from] ScanError),
    #[error("transfer state error: {0}")]
    Transfer(#[from] TransferError),
    #[error("conflicts need a decision before copying")]
    NeedsDecision(ConflictPlan),
    #[error("destination root is not a directory: {0}")]
    DestinationNotDirectory(String),
}

#[derive(Debug, Clone)]
pub struct CopyReport {
    pub manifest: Manifest,
    pub plan: ConflictPlan,
    pub task: TransferTask,
    pub copied_files: u64,
    pub skipped_files: u64,
    pub created_dirs: u64,
    pub resumed_files: u64,
    pub bytes_written: u64,
}

/// Copy a real directory tree into a destination root. `.cdpart` files allow a
/// crashed copy to resume from the last byte already on disk.
pub fn copy_tree(
    src_root: &Path,
    dest_root: &Path,
    policy: ConflictPolicy,
    target: TargetPlatform,
) -> Result<CopyReport, CopyError> {
    let manifest = scan_dir(src_root)?;
    if dest_root.exists() && !dest_root.is_dir() {
        return Err(CopyError::DestinationNotDirectory(
            dest_root.display().to_string(),
        ));
    }

    let (existing_files, existing_dirs) = existing_at(dest_root)?;
    let plan = plan_conflicts(&manifest, &existing_files, &existing_dirs, policy, target);
    if plan.needs_attention() {
        return Err(CopyError::NeedsDecision(plan));
    }

    fs::create_dir_all(dest_root)?;
    let mut task = TransferTask::new(
        format!("copy-{}", manifest.root_name),
        manifest.root_name.clone(),
        manifest.total_bytes(),
    );
    task.start()?;

    let mut created_dirs = 0;
    for entry in &plan.entries {
        if !entry.is_dir {
            continue;
        }
        if matches!(
            entry.action,
            ConflictAction::Write | ConflictAction::MergeDirectory
        ) {
            fs::create_dir_all(join_rel(dest_root, &entry.destination))?;
            created_dirs += 1;
        }
    }

    let sizes = manifest
        .entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .map(|entry| (entry.rel_path.clone(), entry.size))
        .collect::<BTreeMap<_, _>>();

    let mut copied_files = 0;
    let mut skipped_files = 0;
    let mut resumed_files = 0;
    let mut bytes_written = 0;
    let mut processed = 0_u64;

    for entry in &plan.entries {
        if entry.is_dir {
            continue;
        }
        let size = sizes.get(&entry.rel_path).copied().unwrap_or(0);
        match entry.action {
            ConflictAction::Write | ConflictAction::Overwrite | ConflictAction::Rename => {
                let source = join_rel(src_root, &entry.rel_path);
                let destination = join_rel(dest_root, &entry.destination);
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent)?;
                }
                let outcome = copy_file_with_resume(&source, &destination)?;
                if outcome.resumed {
                    resumed_files += 1;
                }
                bytes_written += outcome.bytes_written;
                copied_files += 1;
            }
            _ => skipped_files += 1,
        }
        processed = processed.saturating_add(size);
        task.progress(processed)?;
    }

    task.complete()?;
    Ok(CopyReport {
        manifest,
        plan,
        task,
        copied_files,
        skipped_files,
        created_dirs,
        resumed_files,
        bytes_written,
    })
}

/// Temporary file used for resumable copies; it lives beside the destination so
/// the final rename stays on the same filesystem.
pub fn partial_path(destination: &Path) -> PathBuf {
    let mut file_name = destination.as_os_str().to_os_string();
    file_name.push(".cdpart");
    PathBuf::from(file_name)
}

pub fn join_rel(root: &Path, rel_path: &str) -> PathBuf {
    let mut path = root.to_path_buf();
    for component in rel_path.split('/') {
        if !component.is_empty() {
            path.push(component);
        }
    }
    path
}

fn existing_at(root: &Path) -> Result<(Vec<String>, Vec<String>), CopyError> {
    if !root.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    if !root.is_dir() {
        return Err(CopyError::DestinationNotDirectory(
            root.display().to_string(),
        ));
    }

    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).min_depth(1) {
        let entry = entry.map_err(|error| io::Error::other(error.to_string()))?;
        let rel_path = relative_path(root, entry.path());
        if entry.file_type().is_dir() {
            dirs.push(rel_path);
        } else {
            files.push(rel_path);
        }
    }
    Ok((files, dirs))
}

struct CopyOutcome {
    resumed: bool,
    bytes_written: u64,
}

fn copy_file_with_resume(source: &Path, destination: &Path) -> Result<CopyOutcome, io::Error> {
    let source_len = fs::metadata(source)?.len();
    let part = partial_path(destination);

    let mut start = 0_u64;
    if part.exists() {
        let part_len = fs::metadata(&part)?.len();
        if part_len > source_len {
            fs::remove_file(&part)?;
        } else {
            start = part_len;
        }
    }

    let mut source_file = File::open(source)?;
    if start > 0 {
        source_file.seek(SeekFrom::Start(start))?;
    }
    let mut part_file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(start > 0)
        .truncate(start == 0)
        .open(&part)?;
    let bytes_written = io::copy(&mut source_file, &mut part_file)?;
    part_file.sync_all()?;
    drop(part_file);
    let _ = source_file;

    let part_len = fs::metadata(&part)?.len();
    if part_len != source_len {
        return Err(io::Error::other(format!(
            "partial file has {part_len} bytes, expected {source_len}"
        )));
    }

    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(&part, destination)?;
    Ok(CopyOutcome {
        resumed: start > 0,
        bytes_written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "crossdev-copy-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn source_tree(root: &Path) {
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        fs::write(root.join("sub/b.bin"), b"12345678").unwrap();
    }

    #[test]
    fn copies_tree_with_empty_directories() {
        let root = temp_root("full");
        let src = root.join("src");
        let dst = root.join("dst");
        source_tree(&src);

        let report =
            copy_tree(&src, &dst, ConflictPolicy::Overwrite, TargetPlatform::Linux).unwrap();
        assert_eq!(report.copied_files, 2);
        assert_eq!(report.created_dirs, 2); // sub + empty
        assert_eq!(report.task.state, crate::task::TransferState::Completed);
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dst.join("sub/b.bin")).unwrap(), b"12345678");
        assert!(dst.join("empty").is_dir());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn partial_file_is_resumed_and_completed() {
        let root = temp_root("resume");
        let src = root.join("src");
        let dst = root.join("dst");
        source_tree(&src);
        fs::create_dir_all(&dst).unwrap();
        fs::write(partial_path(&dst.join("a.txt")), b"he").unwrap();

        let report =
            copy_tree(&src, &dst, ConflictPolicy::Overwrite, TargetPlatform::Linux).unwrap();
        assert!(report.resumed_files >= 1);
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert!(!partial_path(&dst.join("a.txt")).exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ask_policy_stops_before_writing() {
        let root = temp_root("ask");
        let src = root.join("src");
        let dst = root.join("dst");
        source_tree(&src);
        copy_tree(&src, &dst, ConflictPolicy::Overwrite, TargetPlatform::Linux).unwrap();

        let error = copy_tree(&src, &dst, ConflictPolicy::Ask, TargetPlatform::Linux).unwrap_err();
        assert!(matches!(error, CopyError::NeedsDecision(plan) if plan.needs_attention()));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rename_policy_moves_conflicting_tree() {
        let root = temp_root("rename");
        let src = root.join("src");
        let dst = root.join("dst");
        source_tree(&src);
        copy_tree(&src, &dst, ConflictPolicy::Overwrite, TargetPlatform::Linux).unwrap();

        let report = copy_tree(&src, &dst, ConflictPolicy::Rename, TargetPlatform::Linux).unwrap();
        assert_eq!(fs::read(dst.join("a~2.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dst.join("sub~2/b.bin")).unwrap(), b"12345678");
        assert!(report.copied_files >= 2);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn destination_file_is_rejected() {
        let root = temp_root("destfile");
        let src = root.join("src");
        let dst = root.join("dst");
        source_tree(&src);
        fs::write(&dst, b"x").unwrap();

        assert!(matches!(
            copy_tree(&src, &dst, ConflictPolicy::Overwrite, TargetPlatform::Linux),
            Err(CopyError::DestinationNotDirectory(_))
        ));

        fs::remove_dir_all(&root).unwrap();
    }
}
