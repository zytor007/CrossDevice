//! File and directory transfer primitives. Path handling and conflict policy
//! are pure so they can be tested on every platform without touching a disk.

mod chunk;
mod conflict;
mod copy;
mod manifest;
mod path;
mod scan;
mod task;

pub use chunk::{
    chunk_bytes, chunk_file, resume_offset, BlockCache, Chunk, ChunkConfig, ChunkError,
    DEFAULT_AVG_CHUNK, DEFAULT_MAX_CHUNK, DEFAULT_MIN_CHUNK,
};
pub use conflict::{
    plan_conflicts, ConflictAction, ConflictPlan, ConflictPlanEntry, ConflictPolicy,
};
pub use copy::{copy_tree, join_rel, partial_path, CopyError, CopyReport};
pub use manifest::{FileEntry, Manifest, ManifestIssue};
pub use path::{
    normalize_rel_path, normalize_root_name, path_key, sanitize_component, sanitize_rel_path,
    PathError, TargetPlatform,
};
pub use scan::{scan_dir, ScanError};
pub use task::{TransferError, TransferState, TransferTask};
