use cd_files::{ChunkError, PathError, ScanError};
use cd_transport::TransportError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("chunk error: {0}")]
    Chunk(#[from] ChunkError),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("scan error: {0}")]
    Scan(#[from] ScanError),
    #[error("path error: {0}")]
    Path(#[from] PathError),
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("hash mismatch for `{rel_path}` at offset {offset}")]
    HashMismatch { rel_path: String, offset: u64 },
    #[error("destination already exists: {0}")]
    DestinationExists(String),
    #[error("certificate error: {0}")]
    Certificate(String),
    #[error("pairing failed: {0}")]
    Pairing(String),
    #[error("random generation failed: {0}")]
    Random(String),
    #[error("transfer cancelled")]
    Cancelled,
}
