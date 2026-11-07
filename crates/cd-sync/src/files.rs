use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use cd_files::{
    chunk_file, join_rel, normalize_root_name, partial_path, resume_offset, scan_dir, BlockCache,
    Chunk, ChunkConfig, FileEntry, Manifest, ManifestIssue, TargetPlatform,
};
use cd_proto::{
    DataHeader, FileChunkHeader, FileChunkMapEntry, FileChunkMapHeader, FileChunkMeta,
    FileManifestEntry, FileManifestHeader, ResumeFile, ResumePlanBody, ResumePlanHeader,
};
use cd_transport::Link;

use crate::cancel::CancelToken;
use crate::error::SyncError;

/// Kept for callers that still refer to the historical fixed chunk size.
pub const CHUNK_BYTES: u64 = cd_files::DEFAULT_AVG_CHUNK;

#[derive(Debug, Clone)]
pub struct SendReport {
    pub task_id: String,
    pub manifest: Manifest,
    /// Chunks actually written to the network.
    pub chunks: u64,
    pub bytes: u64,
    /// Chunks skipped because the receiver's block cache already had them.
    pub reused_chunks: u64,
    /// Chunks skipped because the receiver already had that file prefix on disk.
    pub resumed_chunks: u64,
    pub total_chunks: u64,
    /// The receiver's conflict policy kept its existing copy.
    pub skipped: bool,
}

#[derive(Debug, Clone)]
pub struct ReceiveReport {
    pub task_id: String,
    pub manifest: Manifest,
    pub chunks: u64,
    pub bytes: u64,
    pub dest_root: PathBuf,
    /// Chunks materialised from the local block cache instead of the wire.
    pub reused_chunks: u64,
    /// Bytes that were already on disk when the transfer resumed.
    pub resumed_bytes: u64,
    /// The local conflict policy kept the existing destination.
    pub skipped: bool,
}

/// Sends a manifest, a content-defined chunk map, waits for the receiver's
/// resume plan, then sends only missing chunks.
pub async fn send_tree(
    link: &Link,
    task_id: &str,
    src_root: &Path,
) -> Result<SendReport, SyncError> {
    send_tree_with_config(link, task_id, src_root, ChunkConfig::default()).await
}

pub async fn send_tree_with_config(
    link: &Link,
    task_id: &str,
    src_root: &Path,
    chunk_config: ChunkConfig,
) -> Result<SendReport, SyncError> {
    send_tree_body(
        link,
        task_id,
        src_root,
        chunk_config,
        &CancelToken::new(),
        &|_| {},
    )
    .await
}

/// Sends a tree while routing clipboard blobs that arrive before the receiver's
/// resume plan to `side` instead of failing the transfer.
pub async fn send_tree_with_side(
    link: &Link,
    task_id: &str,
    src_root: &Path,
    side: &(dyn Fn(cd_transport::DataTransfer) + Send + Sync),
) -> Result<SendReport, SyncError> {
    send_tree_with_side_and_cancel(link, task_id, src_root, &CancelToken::new(), side).await
}

pub async fn send_tree_with_side_and_cancel(
    link: &Link,
    task_id: &str,
    src_root: &Path,
    cancel: &CancelToken,
    side: &(dyn Fn(cd_transport::DataTransfer) + Send + Sync),
) -> Result<SendReport, SyncError> {
    send_tree_body(
        link,
        task_id,
        src_root,
        ChunkConfig::default(),
        cancel,
        side,
    )
    .await
}

pub async fn send_file(link: &Link, task_id: &str, path: &Path) -> Result<SendReport, SyncError> {
    send_file_with_side(link, task_id, path, &|_| {}).await
}

pub async fn send_file_with_side(
    link: &Link,
    task_id: &str,
    path: &Path,
    side: &(dyn Fn(cd_transport::DataTransfer) + Send + Sync),
) -> Result<SendReport, SyncError> {
    send_file_with_side_and_cancel(link, task_id, path, &CancelToken::new(), side).await
}

pub async fn send_file_with_side_and_cancel(
    link: &Link,
    task_id: &str,
    path: &Path,
    cancel: &CancelToken,
    side: &(dyn Fn(cd_transport::DataTransfer) + Send + Sync),
) -> Result<SendReport, SyncError> {
    let manifest = single_file_manifest(path)?;
    send_manifest_body(
        link,
        task_id,
        manifest,
        SourceLayout::File(path),
        true,
        ChunkConfig::default(),
        cancel,
        side,
    )
    .await
}

#[derive(Clone, Copy)]
enum SourceLayout<'a> {
    Tree(&'a Path),
    File(&'a Path),
}

impl SourceLayout<'_> {
    fn path_for(&self, rel_path: &str) -> PathBuf {
        match self {
            Self::Tree(root) => join_rel(root, rel_path),
            Self::File(path) => path.to_path_buf(),
        }
    }
}

async fn send_tree_body(
    link: &Link,
    task_id: &str,
    src_root: &Path,
    chunk_config: ChunkConfig,
    cancel: &CancelToken,
    side: &(dyn Fn(cd_transport::DataTransfer) + Send + Sync),
) -> Result<SendReport, SyncError> {
    let manifest = scan_dir(src_root)?;
    send_manifest_body(
        link,
        task_id,
        manifest,
        SourceLayout::Tree(src_root),
        false,
        chunk_config,
        cancel,
        side,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn send_manifest_body(
    link: &Link,
    task_id: &str,
    manifest: Manifest,
    source: SourceLayout<'_>,
    root_is_file: bool,
    chunk_config: ChunkConfig,
    cancel: &CancelToken,
    side: &(dyn Fn(cd_transport::DataTransfer) + Send + Sync),
) -> Result<SendReport, SyncError> {
    cancel.checkpoint().await?;
    let issues = manifest.validate(TargetPlatform::Linux);
    if !issues.is_empty() {
        return Err(SyncError::InvalidManifest(summarize(&issues)));
    }
    if root_is_file {
        let files = manifest
            .entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .count();
        let dirs = manifest.entries.iter().filter(|entry| entry.is_dir).count();
        if files != 1 || dirs != 0 {
            return Err(SyncError::InvalidManifest(
                "single-file manifest must contain one file and no directories".to_owned(),
            ));
        }
    }

    let entries = manifest
        .entries
        .iter()
        .map(|entry| FileManifestEntry {
            rel_path: entry.rel_path.clone(),
            size: entry.size,
            is_dir: entry.is_dir,
            modified_ms: entry.modified_ms,
        })
        .collect::<Vec<_>>();
    let payload = serde_json::to_vec(&entries)?;
    let manifest_header = FileManifestHeader {
        task_id: task_id.to_owned(),
        root_name: manifest.root_name.clone(),
        root_is_file,
        entry_count: entries.len() as u64,
        total_bytes: manifest.total_bytes(),
        size: payload.len() as u64,
    };

    let mut chunk_maps = Vec::new();
    for entry in manifest.entries.iter().filter(|entry| !entry.is_dir) {
        let file = source.path_for(&entry.rel_path);
        let chunks = chunk_file(&file, chunk_config)?;
        if chunks.iter().map(|chunk| chunk.length).sum::<u64>() != entry.size {
            return Err(SyncError::Protocol(format!(
                "file `{}` changed while chunking",
                entry.rel_path
            )));
        }
        chunk_maps.push(FileChunkMapEntry {
            rel_path: entry.rel_path.clone(),
            chunks: chunks
                .into_iter()
                .map(|chunk| FileChunkMeta {
                    offset: chunk.offset,
                    length: chunk.length,
                    blake3: chunk.blake3,
                })
                .collect(),
        });
    }
    let map_payload = serde_json::to_vec(&chunk_maps)?;
    let map_header = FileChunkMapHeader {
        task_id: task_id.to_owned(),
        size: map_payload.len() as u64,
    };

    let mut data = link.open_data().await?;
    data.send(&DataHeader::FileManifest(manifest_header), &payload)
        .await?;
    let mut data = link.open_data().await?;
    data.send(&DataHeader::FileChunkMap(map_header), &map_payload)
        .await?;

    // Receiver decides what it already has. It sends its plan on a C2 stream
    // opened in the opposite direction; clipboard blobs can slip in between.
    let plan_transfer = loop {
        let mut incoming = tokio::select! {
            result = link.accept_data() => result?,
            _ = cancel.cancelled() => return Err(SyncError::Cancelled),
        };
        let incoming = incoming.recv().await?;
        match &incoming.header {
            DataHeader::ResumePlan(_) => break incoming,
            DataHeader::Clipboard(_) => side(incoming),
            _ => return Err(SyncError::Protocol("expected resume plan".to_owned())),
        }
    };
    let DataHeader::ResumePlan(plan_header) = plan_transfer.header else {
        return Err(SyncError::Protocol("expected resume plan".to_owned()));
    };
    if plan_header.task_id != task_id {
        return Err(SyncError::Protocol(format!(
            "resume plan task {} != {task_id}",
            plan_header.task_id
        )));
    }
    let plan: ResumePlanBody = serde_json::from_slice(&plan_transfer.payload)?;
    if plan.skipped {
        let total_chunks = chunk_maps.iter().map(|map| map.chunks.len() as u64).sum();
        return Ok(SendReport {
            task_id: task_id.to_owned(),
            manifest,
            chunks: 0,
            bytes: 0,
            reused_chunks: 0,
            resumed_chunks: 0,
            total_chunks,
            skipped: true,
        });
    }
    let mut resume_offsets = BTreeMap::new();
    for file in &plan.files {
        resume_offsets.insert(file.rel_path.clone(), file.received_bytes);
    }
    let known = plan
        .known_blocks
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();

    let mut chunks = 0_u64;
    let mut bytes = 0_u64;
    let mut reused_chunks = 0_u64;
    let mut resumed_chunks = 0_u64;
    let mut total_chunks = 0_u64;
    for (entry, chunk_map) in manifest
        .entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .zip(chunk_maps.iter())
    {
        if entry.rel_path != chunk_map.rel_path {
            return Err(SyncError::Protocol("chunk map order mismatch".to_owned()));
        }
        let resume = resume_offsets
            .get(&entry.rel_path)
            .copied()
            .unwrap_or(0)
            .min(entry.size);
        let source = source.path_for(&entry.rel_path);
        let mut file = File::open(&source)?;
        for meta in &chunk_map.chunks {
            total_chunks += 1;
            let end = meta.offset.saturating_add(meta.length);
            if meta.offset < resume {
                resumed_chunks += 1;
                continue;
            }
            if known.contains(&meta.blake3) {
                reused_chunks += 1;
                continue;
            }
            cancel.checkpoint().await?;

            let mut buffer = vec![0_u8; meta.length as usize];
            file.seek(SeekFrom::Start(meta.offset))?;
            file.read_exact(&mut buffer)?;
            let hash = blake3::hash(&buffer).to_hex().to_string();
            if hash != meta.blake3 {
                return Err(SyncError::Protocol(format!(
                    "file `{}` changed at chunk {}-{end}",
                    entry.rel_path, meta.offset
                )));
            }
            let header = FileChunkHeader {
                task_id: task_id.to_owned(),
                rel_path: entry.rel_path.clone(),
                offset: meta.offset,
                length: meta.length,
                blake3: meta.blake3.clone(),
            };
            let mut data = link.open_data().await?;
            data.send(&DataHeader::FileChunk(header), &buffer).await?;
            chunks += 1;
            bytes += meta.length;
        }
    }

    Ok(SendReport {
        task_id: task_id.to_owned(),
        manifest,
        chunks,
        bytes,
        reused_chunks,
        resumed_chunks,
        total_chunks,
        skipped: false,
    })
}

/// Receives a manifest and chunk map, publishes a resume plan, then materialises
/// files from network chunks and the local block cache. Empty directories from
/// the manifest are recreated.
pub async fn receive_tree(link: &Link, dest_root: &Path) -> Result<ReceiveReport, SyncError> {
    let cache_root = dest_root
        .parent()
        .map(|parent| parent.join(".crossdev-block-cache"))
        .unwrap_or_else(|| PathBuf::from(".crossdev-block-cache"));
    let cache = BlockCache::open(cache_root)?;
    receive_tree_with_cache(link, dest_root, &cache).await
}

pub async fn receive_tree_with_cache(
    link: &Link,
    dest_root: &Path,
    cache: &BlockCache,
) -> Result<ReceiveReport, SyncError> {
    receive_tree_inner(link, dest_root, cache, &|_, _| {}).await
}

/// Same as [`receive_tree_with_cache`], but reports `(processed_bytes, total_bytes)`
/// after every chunk so the daemon can persist progress.
pub async fn receive_tree_with_progress(
    link: &Link,
    dest_root: &Path,
    cache: &BlockCache,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<ReceiveReport, SyncError> {
    receive_tree_inner(link, dest_root, cache, progress).await
}

async fn receive_tree_inner(
    link: &Link,
    dest_root: &Path,
    cache: &BlockCache,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<ReceiveReport, SyncError> {
    let transfer = link.accept_data().await?.recv().await?;
    receive_tree_from_manifest(link, dest_root, cache, transfer, progress).await
}

/// Entry point for a daemon that already consumed the manifest stream in its
/// own data loop.
pub async fn receive_tree_from_manifest(
    link: &Link,
    dest_root: &Path,
    cache: &BlockCache,
    transfer: cd_transport::DataTransfer,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<ReceiveReport, SyncError> {
    receive_tree_from_manifest_with_side_and_cancel(
        link,
        dest_root,
        cache,
        transfer,
        progress,
        &CancelToken::new(),
        &|_| {},
    )
    .await
}

/// Same as [`receive_tree_from_manifest`], but hands clipboard blobs that
/// arrive between file chunks to `side` instead of failing the transfer.
pub async fn receive_tree_from_manifest_with_side(
    link: &Link,
    dest_root: &Path,
    cache: &BlockCache,
    transfer: cd_transport::DataTransfer,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    side: &(dyn Fn(cd_transport::DataTransfer) + Send + Sync),
) -> Result<ReceiveReport, SyncError> {
    receive_tree_from_manifest_with_side_and_cancel(
        link,
        dest_root,
        cache,
        transfer,
        progress,
        &CancelToken::new(),
        side,
    )
    .await
}

/// Full entry point: consumes the manifest, publishes the resume plan, then
/// materialises chunks while polling `cancel`. On cancellation the partially
/// written `.cdpart` files are left in place so a later retry can resume.
#[allow(clippy::too_many_arguments)]
pub async fn receive_tree_from_manifest_with_side_and_cancel(
    link: &Link,
    dest_root: &Path,
    cache: &BlockCache,
    transfer: cd_transport::DataTransfer,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    cancel: &CancelToken,
    side: &(dyn Fn(cd_transport::DataTransfer) + Send + Sync),
) -> Result<ReceiveReport, SyncError> {
    receive_tree_body(link, dest_root, cache, transfer, progress, cancel, side).await
}

/// Handles a task whose destination already exists under the receiver's
/// `skip` conflict policy: consume the manifest and chunk map, reply with a
/// plan that marks every file complete, and never write to disk.
pub async fn receive_skipped_from_manifest(
    link: &Link,
    dest_root: &Path,
    transfer: cd_transport::DataTransfer,
) -> Result<ReceiveReport, SyncError> {
    let DataHeader::FileManifest(header) = transfer.header else {
        return Err(SyncError::Protocol(
            "expected file manifest on first C2 stream".to_owned(),
        ));
    };
    let wire_entries: Vec<FileManifestEntry> = serde_json::from_slice(&transfer.payload)?;
    if wire_entries.len() as u64 != header.entry_count {
        return Err(SyncError::Protocol(format!(
            "manifest entry count {} != {}",
            wire_entries.len(),
            header.entry_count
        )));
    }
    let entries = wire_entries
        .iter()
        .map(|entry| cd_files::FileEntry {
            rel_path: entry.rel_path.clone(),
            size: entry.size,
            is_dir: entry.is_dir,
            modified_ms: entry.modified_ms,
        })
        .collect::<Vec<_>>();
    let manifest = Manifest::from_entries(header.root_name.clone(), entries)?;
    let issues = manifest.validate(TargetPlatform::Linux);
    if !issues.is_empty() {
        return Err(SyncError::InvalidManifest(summarize(&issues)));
    }
    if manifest.total_bytes() != header.total_bytes {
        return Err(SyncError::Protocol(format!(
            "manifest total {} != {}",
            manifest.total_bytes(),
            header.total_bytes
        )));
    }

    let map_transfer = link.accept_data().await?.recv().await?;
    let DataHeader::FileChunkMap(map_header) = map_transfer.header else {
        return Err(SyncError::Protocol("expected file chunk map".to_owned()));
    };
    if map_header.task_id != header.task_id {
        return Err(SyncError::Protocol(format!(
            "chunk map task {} != {}",
            map_header.task_id, header.task_id
        )));
    }
    let chunk_maps: Vec<FileChunkMapEntry> = serde_json::from_slice(&map_transfer.payload)?;
    validate_chunk_map(&manifest, &chunk_maps)?;

    let plan = ResumePlanBody {
        files: manifest
            .entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| ResumeFile {
                rel_path: entry.rel_path.clone(),
                received_bytes: entry.size,
            })
            .collect(),
        known_blocks: Vec::new(),
        skipped: true,
    };
    let plan_payload = serde_json::to_vec(&plan)?;
    let plan_header = ResumePlanHeader {
        task_id: header.task_id.clone(),
        size: plan_payload.len() as u64,
    };
    let mut data = link.open_data().await?;
    data.send(&DataHeader::ResumePlan(plan_header), &plan_payload)
        .await?;

    Ok(ReceiveReport {
        task_id: header.task_id,
        manifest,
        chunks: 0,
        bytes: 0,
        dest_root: dest_root.to_path_buf(),
        reused_chunks: 0,
        resumed_bytes: 0,
        skipped: true,
    })
}

async fn receive_tree_body(
    link: &Link,
    dest_root: &Path,
    cache: &BlockCache,
    transfer: cd_transport::DataTransfer,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
    cancel: &CancelToken,
    side: &(dyn Fn(cd_transport::DataTransfer) + Send + Sync),
) -> Result<ReceiveReport, SyncError> {
    cancel.checkpoint().await?;
    let DataHeader::FileManifest(header) = transfer.header else {
        return Err(SyncError::Protocol(
            "expected file manifest on first C2 stream".to_owned(),
        ));
    };
    let wire_entries: Vec<FileManifestEntry> = serde_json::from_slice(&transfer.payload)?;
    if wire_entries.len() as u64 != header.entry_count {
        return Err(SyncError::Protocol(format!(
            "manifest entry count {} != {}",
            wire_entries.len(),
            header.entry_count
        )));
    }
    let entries = wire_entries
        .iter()
        .map(|entry| cd_files::FileEntry {
            rel_path: entry.rel_path.clone(),
            size: entry.size,
            is_dir: entry.is_dir,
            modified_ms: entry.modified_ms,
        })
        .collect::<Vec<_>>();
    let manifest = Manifest::from_entries(header.root_name.clone(), entries)?;
    let issues = manifest.validate(TargetPlatform::Linux);
    if !issues.is_empty() {
        return Err(SyncError::InvalidManifest(summarize(&issues)));
    }
    if manifest.total_bytes() != header.total_bytes {
        return Err(SyncError::Protocol(format!(
            "manifest total {} != {}",
            manifest.total_bytes(),
            header.total_bytes
        )));
    }
    if header.root_is_file {
        let files = manifest
            .entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .count();
        let dirs = manifest.entries.iter().filter(|entry| entry.is_dir).count();
        if files != 1 || dirs != 0 {
            return Err(SyncError::InvalidManifest(
                "single-file manifest must contain one file and no directories".to_owned(),
            ));
        }
    }

    let map_transfer = link.accept_data().await?.recv().await?;
    let DataHeader::FileChunkMap(map_header) = map_transfer.header else {
        return Err(SyncError::Protocol("expected file chunk map".to_owned()));
    };
    if map_header.task_id != header.task_id {
        return Err(SyncError::Protocol(format!(
            "chunk map task {} != {}",
            map_header.task_id, header.task_id
        )));
    }
    let chunk_maps: Vec<FileChunkMapEntry> = serde_json::from_slice(&map_transfer.payload)?;
    let chunk_map = validate_chunk_map(&manifest, &chunk_maps)?;

    if header.root_is_file
        && dest_root.exists()
        && (!dest_root.is_file() || fs::metadata(dest_root)?.len() != header.total_bytes)
    {
        return Err(SyncError::DestinationExists(
            dest_root.display().to_string(),
        ));
    }
    if !header.root_is_file {
        ensure_tree_destination(dest_root, &manifest)?;
        fs::create_dir_all(dest_root)?;
        for entry in manifest.entries.iter().filter(|entry| entry.is_dir) {
            fs::create_dir_all(join_rel(dest_root, &entry.rel_path))?;
        }
    }

    let destination_for = |rel_path: &str| {
        if header.root_is_file {
            dest_root.to_path_buf()
        } else {
            join_rel(dest_root, rel_path)
        }
    };

    // Build the resume plan from `.cdpart` tails and the local block cache.
    let mut resume_files = Vec::new();
    let mut completed = std::collections::BTreeSet::new();
    let mut resumed_bytes = 0_u64;
    for entry in manifest.entries.iter().filter(|entry| !entry.is_dir) {
        let destination = destination_for(&entry.rel_path);
        if destination.is_file() && fs::metadata(&destination)?.len() == entry.size {
            completed.insert(entry.rel_path.clone());
            resume_files.push(ResumeFile {
                rel_path: entry.rel_path.clone(),
                received_bytes: entry.size,
            });
            continue;
        }
        let part = partial_path(&destination);
        let mut received = 0_u64;
        if part.is_file() {
            let part_len = fs::metadata(&part)?.len().min(entry.size);
            let chunks = chunk_map
                .get(&entry.rel_path)
                .map(|chunks| chunks.as_slice())
                .unwrap_or(&[]);
            received = resume_offset(chunks, part_len);
            if received < part_len {
                let file = OpenOptions::new().write(true).open(&part)?;
                file.set_len(received)?;
            }
        }
        resumed_bytes += received;
        resume_files.push(ResumeFile {
            rel_path: entry.rel_path.clone(),
            received_bytes: received,
        });
    }
    let plan = ResumePlanBody {
        files: resume_files,
        known_blocks: cache.hashes()?,
        skipped: false,
    };
    let plan_payload = serde_json::to_vec(&plan)?;
    let plan_header = ResumePlanHeader {
        task_id: header.task_id.clone(),
        size: plan_payload.len() as u64,
    };
    let mut data = link.open_data().await?;
    data.send(&DataHeader::ResumePlan(plan_header), &plan_payload)
        .await?;

    let mut resume_offsets = BTreeMap::new();
    for file in &plan.files {
        resume_offsets.insert(file.rel_path.clone(), file.received_bytes);
    }

    let mut chunks = 0_u64;
    let mut bytes = 0_u64;
    let mut reused_chunks = 0_u64;
    let total_bytes = manifest.total_bytes();
    let mut processed = resumed_bytes;

    for entry in manifest.entries.iter().filter(|entry| !entry.is_dir) {
        if completed.contains(&entry.rel_path) {
            continue;
        }
        let destination = destination_for(&entry.rel_path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let part = partial_path(&destination);
        let resume = resume_offsets
            .get(&entry.rel_path)
            .copied()
            .unwrap_or(0)
            .min(entry.size);
        let empty_map = Vec::new();
        let metas = chunk_map.get(&entry.rel_path).unwrap_or(&empty_map);

        let mut part_file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(resume > 0)
            .truncate(resume == 0)
            .open(&part)?;

        for meta in metas {
            let end = meta.offset.saturating_add(meta.length);
            if meta.offset < resume {
                continue;
            }
            let bytes_chunk: Vec<u8> = if let Some(cached) = cache.read(&meta.blake3)? {
                if cached.len() as u64 != meta.length
                    || blake3::hash(&cached).to_hex().as_str() != meta.blake3
                {
                    return Err(SyncError::HashMismatch {
                        rel_path: entry.rel_path.clone(),
                        offset: meta.offset,
                    });
                }
                reused_chunks += 1;
                cached
            } else {
                let transfer = loop {
                    let mut incoming = tokio::select! {
                        result = link.accept_data() => result?,
                        _ = cancel.cancelled() => return Err(SyncError::Cancelled),
                    };
                    let incoming = tokio::select! {
                        result = incoming.recv() => result?,
                        _ = cancel.cancelled() => return Err(SyncError::Cancelled),
                    };
                    match &incoming.header {
                        DataHeader::FileChunk(_) => break incoming,
                        DataHeader::Clipboard(_) => side(incoming),
                        _ => {
                            return Err(SyncError::Protocol(
                                "expected file chunk or clipboard".to_owned(),
                            ))
                        }
                    }
                };
                let DataHeader::FileChunk(chunk) = transfer.header else {
                    return Err(SyncError::Protocol("expected file chunk".to_owned()));
                };
                if chunk.task_id != header.task_id {
                    return Err(SyncError::Protocol(format!(
                        "chunk task {} != {}",
                        chunk.task_id, header.task_id
                    )));
                }
                if chunk.rel_path != entry.rel_path
                    || chunk.offset != meta.offset
                    || chunk.length != meta.length
                    || chunk.blake3 != meta.blake3
                {
                    return Err(SyncError::Protocol(format!(
                        "chunk mismatch for `{}`: got {}-{} expected {}-{end}",
                        chunk.rel_path, chunk.offset, chunk.length, meta.offset
                    )));
                }
                let hash = blake3::hash(&transfer.payload).to_hex().to_string();
                if hash != chunk.blake3 {
                    return Err(SyncError::HashMismatch {
                        rel_path: chunk.rel_path.clone(),
                        offset: chunk.offset,
                    });
                }
                chunks += 1;
                bytes += chunk.length;
                transfer.payload
            };

            cancel.checkpoint().await?;
            part_file.write_all(&bytes_chunk)?;
            cache.insert(&meta.blake3, &bytes_chunk)?;
            processed = processed.saturating_add(bytes_chunk.len() as u64);
            progress(processed.min(total_bytes), total_bytes);
        }
        part_file.sync_all()?;
        drop(part_file);

        cancel.checkpoint().await?;
        let part_len = fs::metadata(&part)?.len();
        if part_len != entry.size {
            return Err(SyncError::Protocol(format!(
                "part file `{}` has {part_len} bytes, expected {}",
                entry.rel_path, entry.size
            )));
        }
        if destination.exists() {
            fs::remove_file(&destination)?;
        }
        fs::rename(&part, &destination)?;
    }

    Ok(ReceiveReport {
        task_id: header.task_id,
        manifest,
        chunks,
        bytes,
        dest_root: dest_root.to_path_buf(),
        reused_chunks,
        resumed_bytes,
        skipped: false,
    })
}

/// Destination may contain a previous partial transfer, but complete files that
/// are not part of an explicit resume must not be overwritten silently.
fn ensure_tree_destination(dest_root: &Path, manifest: &Manifest) -> Result<(), SyncError> {
    if !dest_root.exists() {
        return Ok(());
    }
    if !dest_root.is_dir() {
        return Err(SyncError::DestinationExists(
            dest_root.display().to_string(),
        ));
    }
    for entry in fs::read_dir(dest_root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".cdpart") {
            continue;
        }
        let relative = name;
        let known = manifest
            .entries
            .iter()
            .any(|item| !item.is_dir && item.rel_path == relative);
        if !known {
            return Err(SyncError::DestinationExists(
                dest_root.display().to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_chunk_map(
    manifest: &Manifest,
    maps: &[FileChunkMapEntry],
) -> Result<BTreeMap<String, Vec<Chunk>>, SyncError> {
    let files = manifest
        .entries
        .iter()
        .filter(|entry| !entry.is_dir)
        .map(|entry| (entry.rel_path.as_str(), entry.size))
        .collect::<BTreeMap<_, _>>();
    if maps.len() != files.len() {
        return Err(SyncError::Protocol(format!(
            "chunk map covers {} files, manifest has {}",
            maps.len(),
            files.len()
        )));
    }

    let mut out = BTreeMap::new();
    for map in maps {
        let Some(size) = files.get(map.rel_path.as_str()) else {
            return Err(SyncError::Protocol(format!(
                "chunk map has unknown file `{}`",
                map.rel_path
            )));
        };
        let mut cursor = 0_u64;
        let mut chunks = Vec::with_capacity(map.chunks.len());
        for meta in &map.chunks {
            if meta.offset != cursor
                || meta.length == 0
                || meta.length > cd_files::DEFAULT_MAX_CHUNK
            {
                return Err(SyncError::Protocol(format!(
                    "invalid chunk {}-{} for `{}`",
                    meta.offset, meta.length, map.rel_path
                )));
            }
            if meta.blake3.len() != 64 || !meta.blake3.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(SyncError::Protocol(format!(
                    "invalid chunk hash for `{}`",
                    map.rel_path
                )));
            }
            cursor += meta.length;
            if cursor > *size {
                return Err(SyncError::Protocol(format!(
                    "chunk map for `{}` exceeds file size",
                    map.rel_path
                )));
            }
            chunks.push(Chunk {
                offset: meta.offset,
                length: meta.length,
                blake3: meta.blake3.clone(),
            });
        }
        if cursor != *size {
            return Err(SyncError::Protocol(format!(
                "chunk map for `{}` covers {cursor} of {size} bytes",
                map.rel_path
            )));
        }
        out.insert(map.rel_path.clone(), chunks);
    }
    Ok(out)
}

fn summarize(issues: &[ManifestIssue]) -> String {
    issues
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn single_file_manifest(path: &Path) -> Result<Manifest, SyncError> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(SyncError::Protocol(format!(
            "`{}` is not a regular file",
            path.display()
        )));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| SyncError::Protocol("file name is not valid UTF-8".to_owned()))?;
    let root_name = normalize_root_name(name)?;
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64);
    Manifest::from_entries(
        root_name.clone(),
        vec![FileEntry::file(root_name, metadata.len(), modified_ms)],
    )
    .map_err(SyncError::from)
}
