use std::fs;
use std::io;
use std::path::{Component, Path};
use std::time::UNIX_EPOCH;

use thiserror::Error;
use walkdir::WalkDir;

use crate::manifest::Manifest;
use crate::path::PathError;

#[derive(Debug, Error)]
pub enum ScanError {
    #[error("root is not a directory: {0}")]
    NotDirectory(String),
    #[error("I/O error while scanning: {0}")]
    Io(#[from] io::Error),
    #[error("invalid manifest path: {0}")]
    Path(#[from] PathError),
}

/// Build a manifest from a real directory. Symlinks are skipped; empty directories
/// are preserved; relative paths use `/` separators.
pub fn scan_dir(root: &Path) -> Result<Manifest, ScanError> {
    let metadata = fs::metadata(root)?;
    if !metadata.is_dir() {
        return Err(ScanError::NotDirectory(root.display().to_string()));
    }

    let root_name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_owned());
    let mut manifest = Manifest::new(root_name)?;

    for entry in WalkDir::new(root).follow_links(false).min_depth(1) {
        let entry = entry.map_err(|error| io::Error::other(error.to_string()))?;
        if entry.file_type().is_symlink() {
            continue;
        }
        let rel_path = relative_path(root, entry.path());
        let metadata = entry
            .metadata()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let modified_ms = metadata.modified().ok().and_then(|time| {
            time.duration_since(UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_millis() as i64)
        });

        if metadata.is_dir() {
            manifest.push_dir(rel_path, modified_ms)?;
        } else if metadata.is_file() {
            manifest.push_file(rel_path, metadata.len(), modified_ms)?;
        }
    }

    Ok(manifest)
}

pub fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "crossdev-scan-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn scans_files_and_empty_directories() {
        let root = temp_root("tree");
        fs::create_dir_all(root.join("images")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::write(root.join("notes.txt"), b"hello").unwrap();
        fs::write(root.join("images/logo.bin"), b"12345678").unwrap();

        let manifest = scan_dir(&root).unwrap();
        assert_eq!(
            manifest.root_name,
            root.file_name().unwrap().to_string_lossy()
        );
        assert_eq!(manifest.file_count(), 2);
        assert_eq!(manifest.directory_count(), 2);
        assert_eq!(manifest.total_bytes(), 13);
        assert!(manifest
            .entries
            .iter()
            .any(|entry| entry.rel_path == "empty" && entry.is_dir));
        assert!(manifest
            .validate(crate::path::TargetPlatform::Linux)
            .is_empty());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn rejects_non_directory_roots() {
        let file = temp_root("file");
        fs::write(&file, b"x").unwrap();
        assert!(matches!(scan_dir(&file), Err(ScanError::NotDirectory(_))));
        fs::remove_file(&file).unwrap();
    }
}
