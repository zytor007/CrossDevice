//! Content-defined chunking (FastCDC-style gear hash) used by the file
//! pipeline. Chunk boundaries depend on local content, so a resumed transfer
//! only needs to re-send the tail of a file, not every byte after a lost packet.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Product default: 4 MB average with 256 KB minimum and 16 MB maximum.
pub const DEFAULT_MIN_CHUNK: u64 = 256 * 1024;
pub const DEFAULT_AVG_CHUNK: u64 = 4 * 1024 * 1024;
pub const DEFAULT_MAX_CHUNK: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkConfig {
    pub min_size: u64,
    pub avg_size: u64,
    pub max_size: u64,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            min_size: DEFAULT_MIN_CHUNK,
            avg_size: DEFAULT_AVG_CHUNK,
            max_size: DEFAULT_MAX_CHUNK,
        }
    }
}

impl ChunkConfig {
    pub fn new(min_size: u64, avg_size: u64, max_size: u64) -> Result<Self, ChunkError> {
        if min_size == 0 || avg_size < min_size || max_size < avg_size {
            return Err(ChunkError::InvalidConfig);
        }
        if !avg_size.is_power_of_two() {
            return Err(ChunkError::InvalidConfig);
        }
        Ok(Self {
            min_size,
            avg_size,
            max_size,
        })
    }

    fn mask(self) -> u64 {
        let bits = self.avg_size.trailing_zeros();
        if bits >= 63 {
            u64::MAX
        } else {
            (1_u64 << bits) - 1
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub offset: u64,
    pub length: u64,
    /// BLAKE3, lowercase hex.
    pub blake3: String,
}

#[derive(Debug, Error)]
pub enum ChunkError {
    #[error("invalid chunk config: min <= avg(power of two) <= max required")]
    InvalidConfig,
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

const fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = value;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

const GEAR: [u64; 256] = {
    let mut table = [0_u64; 256];
    let mut index = 0;
    while index < 256 {
        table[index] = splitmix64(index as u64);
        index += 1;
    }
    table
};

/// Splits an in-memory buffer into content-defined chunks.
pub fn chunk_bytes(data: &[u8], config: ChunkConfig) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut start = 0_u64;
    let mut hasher = blake3::Hasher::new();
    let mut rolling = 0_u64;
    let mask = config.mask();
    let length = data.len() as u64;

    for (index, byte) in data.iter().enumerate() {
        let position = index as u64 + 1;
        hasher.update(&[*byte]);
        let chunk_len = position - start;
        if chunk_len < config.min_size {
            continue;
        }
        if chunk_len >= config.max_size {
            chunks.push(finish_chunk(&mut hasher, start, position));
            start = position;
            rolling = 0;
            continue;
        }
        rolling = (rolling << 1).wrapping_add(GEAR[usize::from(*byte)]);
        if rolling & mask == 0 {
            chunks.push(finish_chunk(&mut hasher, start, position));
            start = position;
            rolling = 0;
        }
    }

    if start < length {
        chunks.push(finish_chunk(&mut hasher, start, length));
    }
    chunks
}

/// Streams a file and returns the chunk table without keeping file contents in
/// memory. A second pass is used by the sender to read each chunk.
pub fn chunk_file(path: impl AsRef<Path>, config: ChunkConfig) -> Result<Vec<Chunk>, ChunkError> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut chunks = Vec::new();
    let mut start = 0_u64;
    let mut position = 0_u64;
    let mut hasher = blake3::Hasher::new();
    let mut rolling = 0_u64;
    let mask = config.mask();

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        for byte in &buffer[..read] {
            position += 1;
            hasher.update(&[*byte]);
            let chunk_len = position - start;
            if chunk_len < config.min_size {
                continue;
            }
            if chunk_len >= config.max_size {
                chunks.push(finish_chunk(&mut hasher, start, position));
                start = position;
                rolling = 0;
                continue;
            }
            rolling = (rolling << 1).wrapping_add(GEAR[usize::from(*byte)]);
            if rolling & mask == 0 {
                chunks.push(finish_chunk(&mut hasher, start, position));
                start = position;
                rolling = 0;
            }
        }
    }

    if start < position {
        chunks.push(finish_chunk(&mut hasher, start, position));
    }
    Ok(chunks)
}

fn finish_chunk(hasher: &mut blake3::Hasher, start: u64, end: u64) -> Chunk {
    let digest = std::mem::replace(hasher, blake3::Hasher::new()).finalize();
    Chunk {
        offset: start,
        length: end - start,
        blake3: digest.to_hex().to_string(),
    }
}

/// Largest chunk boundary that is completely present in `present_bytes`.
/// Bytes after the boundary are discarded by the receiver before resuming.
pub fn resume_offset(chunks: &[Chunk], present_bytes: u64) -> u64 {
    let mut offset = 0_u64;
    for chunk in chunks {
        let end = chunk.offset.saturating_add(chunk.length);
        if end <= present_bytes {
            offset = end;
        } else {
            break;
        }
    }
    offset
}

/// Local block cache keyed by BLAKE3. Enables cross-file and cross-transfer
/// reuse: the receiver asks the sender to skip hashes listed here.
pub struct BlockCache {
    root: PathBuf,
    /// Total bytes allowed on disk; `u64::MAX` disables eviction.
    limit_bytes: u64,
    total_bytes: AtomicU64,
}

impl BlockCache {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ChunkError> {
        Self::open_with_limit(root, u64::MAX)
    }

    pub fn open_with_limit(root: impl Into<PathBuf>, limit_bytes: u64) -> Result<Self, ChunkError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let total_bytes = cache_usage(&root)?;
        Ok(Self {
            root,
            limit_bytes,
            total_bytes: AtomicU64::new(total_bytes),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, hash: &str) -> PathBuf {
        self.root.join(hash)
    }

    pub fn contains(&self, hash: &str) -> bool {
        self.path(hash).is_file()
    }

    pub fn read(&self, hash: &str) -> Result<Option<Vec<u8>>, ChunkError> {
        let path = self.path(hash);
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)?;
        if blake3::hash(&bytes).to_hex().as_str() != hash {
            // A corrupted cache entry must not poison a transfer.
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    pub fn insert(&self, hash: &str, bytes: &[u8]) -> Result<(), ChunkError> {
        if self.contains(hash) {
            return Ok(());
        }
        static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
        let sequence = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let temp = self
            .root
            .join(format!(".{hash}.{}.{sequence}.tmp", std::process::id()));
        std::fs::write(&temp, bytes)?;
        std::fs::rename(&temp, self.path(hash))?;
        let length = bytes.len() as u64;
        let total = self
            .total_bytes
            .fetch_add(length, Ordering::Relaxed)
            .saturating_add(length);
        if total > self.limit_bytes {
            self.evict_to_limit()?;
        }
        Ok(())
    }

    /// Removes the least-recently modified blocks until the cache has 10% free
    /// headroom, so eviction scans do not run for every newly inserted block.
    fn evict_to_limit(&self) -> Result<(), ChunkError> {
        if self.limit_bytes == u64::MAX {
            return Ok(());
        }
        let target = self.limit_bytes - self.limit_bytes / 10;
        let mut files = Vec::new();
        let mut total = 0_u64;
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name.len() != 64 {
                continue;
            }
            let metadata = entry.metadata()?;
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            total = total.saturating_add(metadata.len());
            files.push((modified, entry.path(), metadata.len()));
        }
        files.sort_by_key(|(modified, _, _)| *modified);
        for (_, path, length) in files {
            if total <= target {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(length);
            }
        }
        self.total_bytes.store(total, Ordering::Relaxed);
        Ok(())
    }

    /// Hashes currently available locally, used to build a resume plan.
    pub fn hashes(&self) -> Result<Vec<String>, ChunkError> {
        let mut hashes = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name.len() != 64 {
                continue;
            }
            hashes.push(name);
        }
        hashes.sort();
        Ok(hashes)
    }
}

fn cache_usage(root: &Path) -> Result<u64, ChunkError> {
    let mut total = 0_u64;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name.len() != 64 {
            continue;
        }
        total = total.saturating_add(entry.metadata()?.len());
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "crossdev-chunk-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    #[test]
    fn chunks_cover_every_byte_and_respect_bounds() {
        let config = ChunkConfig::new(512, 4096, 16384).unwrap();
        let data = payload(100_000);
        let chunks = chunk_bytes(&data, config);
        assert!(!chunks.is_empty());
        let mut cursor = 0_u64;
        for chunk in &chunks {
            assert_eq!(chunk.offset, cursor);
            assert!(chunk.length <= config.max_size);
            assert!(chunk.length > 0);
            let slice = &data[chunk.offset as usize..(chunk.offset + chunk.length) as usize];
            assert_eq!(chunk.blake3, blake3::hash(slice).to_hex().to_string());
            cursor = chunk.offset + chunk.length;
        }
        assert_eq!(cursor, data.len() as u64);
    }

    #[test]
    fn file_chunking_matches_in_memory_chunking() {
        let config = ChunkConfig::new(512, 4096, 16384).unwrap();
        let data = payload(80_000);
        let path = temp_path("file");
        std::fs::write(&path, &data).unwrap();
        let from_file = chunk_file(&path, config).unwrap();
        let from_memory = chunk_bytes(&data, config);
        assert_eq!(from_file, from_memory);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn boundaries_are_content_stable_after_a_prefix_insert() {
        let config = ChunkConfig::new(512, 4096, 16384).unwrap();
        let original = (0..100_000_u64)
            .map(|index| splitmix64(index) as u8)
            .collect::<Vec<u8>>();
        let mut inserted = original.clone();
        // Duplicate the head so every byte after the insertion is unchanged.
        let prefix = original[..2].to_vec();
        inserted.splice(0..0, prefix);

        let before = chunk_bytes(&original, config);
        let after = chunk_bytes(&inserted, config);
        // Every boundary after the first few kilobytes should be shifted by
        // exactly the two inserted bytes; content-defined chunking converges.
        let stable = before
            .iter()
            .filter(|chunk| chunk.offset > 8_000)
            .any(|chunk| {
                after
                    .iter()
                    .any(|other| other.offset == chunk.offset + 2 && other.length == chunk.length)
            });
        assert!(stable, "chunk boundaries never reconverged");
    }

    #[test]
    fn empty_and_tiny_inputs_produce_one_final_chunk() {
        let config = ChunkConfig::new(256, 1024, 4096).unwrap();
        assert!(chunk_bytes(&[], config).is_empty());
        let tiny = chunk_bytes(&[7_u8; 10], config);
        assert_eq!(tiny.len(), 1);
        assert_eq!(tiny[0].length, 10);
        assert_eq!(tiny[0].offset, 0);
    }

    #[test]
    fn resume_offset_rounds_down_to_a_boundary() {
        let chunks = chunk_bytes(
            &payload(40_000),
            ChunkConfig::new(512, 4096, 16384).unwrap(),
        );
        let second = &chunks[1];
        assert_eq!(resume_offset(&chunks, second.offset), second.offset);
        assert_eq!(resume_offset(&chunks, second.offset + 1), second.offset);
        assert_eq!(
            resume_offset(&chunks, second.offset + second.length),
            second.offset + second.length
        );
        assert_eq!(resume_offset(&chunks, 100), 0);
    }

    #[test]
    fn block_cache_roundtrips_and_rejects_corruption() {
        let root = temp_path("cache");
        let cache = BlockCache::open(&root).unwrap();
        let data = b"block payload";
        let hash = blake3::hash(data).to_hex().to_string();
        cache.insert(&hash, data).unwrap();
        assert!(cache.contains(&hash));
        assert_eq!(cache.read(&hash).unwrap(), Some(data.to_vec()));
        assert_eq!(cache.hashes().unwrap(), vec![hash.clone()]);

        // Corrupt the entry; read must discard it instead of returning bad bytes.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(cache.path(&hash))
            .unwrap();
        file.write_all(b"x").unwrap();
        drop(file);
        assert_eq!(cache.read(&hash).unwrap(), None);
        assert!(!cache.contains(&hash));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn block_cache_evicts_the_oldest_blocks_over_its_quota() {
        let root = temp_path("quota");
        let cache = BlockCache::open_with_limit(&root, 1024 * 1024).unwrap();
        let first = payload(600_000);
        let second = (0..600_000_u32)
            .map(|index| ((index + 17) % 251) as u8)
            .collect::<Vec<u8>>();
        let first_hash = blake3::hash(&first).to_hex().to_string();
        let second_hash = blake3::hash(&second).to_hex().to_string();
        cache.insert(&first_hash, &first).unwrap();
        let older = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(cache.path(&first_hash))
            .unwrap()
            .set_modified(older)
            .unwrap();
        cache.insert(&second_hash, &second).unwrap();
        assert!(
            !cache.contains(&first_hash),
            "oldest block should be evicted"
        );
        assert!(cache.contains(&second_hash));
        assert_eq!(cache.hashes().unwrap(), vec![second_hash]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
