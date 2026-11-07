use serde::{Deserialize, Serialize};

use crate::control::{ClipboardAnnounce, ClipboardKind};

/// Header written at the start of a C2 QUIC stream, followed by the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataHeader {
    Clipboard(ClipboardAnnounce),
    FileManifest(FileManifestHeader),
    /// Content-defined chunk list for every file, sent by the sender after the
    /// manifest. The receiver uses it to plan block reuse and to know which
    /// chunk arrives next.
    FileChunkMap(FileChunkMapHeader),
    FileChunk(FileChunkHeader),
    /// Receiver → sender: which files already have bytes on disk and which
    /// blocks the local block cache can satisfy without a network read.
    ResumePlan(ResumePlanHeader),
}

impl DataHeader {
    /// Payload length that must follow the header on the stream.
    pub fn declared_size(&self) -> u64 {
        match self {
            Self::Clipboard(announce) => announce.size,
            Self::FileManifest(manifest) => manifest.size,
            Self::FileChunkMap(map) => map.size,
            Self::FileChunk(chunk) => chunk.length,
            Self::ResumePlan(plan) => plan.size,
        }
    }

    pub fn kind(&self) -> Option<ClipboardKind> {
        match self {
            Self::Clipboard(announce) => Some(announce.kind),
            Self::FileManifest(_)
            | Self::FileChunkMap(_)
            | Self::FileChunk(_)
            | Self::ResumePlan(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileManifestHeader {
    pub task_id: String,
    pub root_name: String,
    /// True when the sender selected one file instead of a directory tree.
    #[serde(default)]
    pub root_is_file: bool,
    pub entry_count: u64,
    pub total_bytes: u64,
    /// JSON list of `FileManifestEntry` carried as the payload.
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileManifestEntry {
    pub rel_path: String,
    pub size: u64,
    pub is_dir: bool,
    pub modified_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChunkMapHeader {
    pub task_id: String,
    /// JSON list of `FileChunkMapEntry` carried as the payload.
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChunkMapEntry {
    pub rel_path: String,
    pub chunks: Vec<FileChunkMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChunkMeta {
    pub offset: u64,
    pub length: u64,
    /// BLAKE3 hash of this chunk, lowercase hex.
    pub blake3: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumePlanHeader {
    pub task_id: String,
    /// JSON list of `ResumeFile` carried as the payload.
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeFile {
    pub rel_path: String,
    /// Bytes already on disk, aligned to a content-defined chunk boundary.
    pub received_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResumePlanBody {
    pub files: Vec<ResumeFile>,
    /// Lowercase hex BLAKE3 hashes available in the receiver's block cache.
    pub known_blocks: Vec<String>,
    /// True when the receiver's conflict policy kept the existing destination
    /// and wants the sender to finish without transmitting any chunk.
    #[serde(default)]
    pub skipped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChunkHeader {
    pub task_id: String,
    pub rel_path: String,
    pub offset: u64,
    pub length: u64,
    /// BLAKE3 hash of this chunk, lowercase hex.
    pub blake3: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_header_declares_its_payload() {
        let header = DataHeader::Clipboard(ClipboardAnnounce {
            kind: ClipboardKind::Text,
            hash: "ab".repeat(32),
            size: 42,
            origin_id: "linux-4k".to_owned(),
            seq: 7,
        });
        assert_eq!(header.declared_size(), 42);
        assert_eq!(header.kind(), Some(ClipboardKind::Text));
        let json = serde_json::to_string(&header).unwrap();
        assert!(json.starts_with(r#"{"type":"clipboard""#));
        assert_eq!(serde_json::from_str::<DataHeader>(&json).unwrap(), header);
    }

    #[test]
    fn file_chunk_header_roundtrips() {
        let header = DataHeader::FileChunk(FileChunkHeader {
            task_id: "task-1".to_owned(),
            rel_path: "assets/logo.png".to_owned(),
            offset: 4_194_304,
            length: 65_536,
            blake3: "cd".repeat(32),
        });
        assert_eq!(header.declared_size(), 65_536);
        assert_eq!(header.kind(), None);
        let json = serde_json::to_string(&header).unwrap();
        assert_eq!(serde_json::from_str::<DataHeader>(&json).unwrap(), header);
    }

    #[test]
    fn file_manifest_header_declares_payload() {
        let header = DataHeader::FileManifest(FileManifestHeader {
            task_id: "task-1".to_owned(),
            root_name: "assets".to_owned(),
            root_is_file: false,
            entry_count: 3,
            total_bytes: 100,
            size: 250,
        });
        assert_eq!(header.declared_size(), 250);
        assert_eq!(header.kind(), None);
        let json = serde_json::to_string(&header).unwrap();
        assert_eq!(serde_json::from_str::<DataHeader>(&json).unwrap(), header);
    }

    #[test]
    fn chunk_map_and_resume_plan_roundtrip() {
        let map = DataHeader::FileChunkMap(FileChunkMapHeader {
            task_id: "task-1".to_owned(),
            size: 42,
        });
        let plan = DataHeader::ResumePlan(ResumePlanHeader {
            task_id: "task-1".to_owned(),
            size: 9,
        });
        for header in [map, plan] {
            assert_eq!(
                serde_json::from_str::<DataHeader>(&serde_json::to_string(&header).unwrap())
                    .unwrap(),
                header
            );
        }

        let entry = FileChunkMapEntry {
            rel_path: "a.bin".to_owned(),
            chunks: vec![FileChunkMeta {
                offset: 0,
                length: 1024,
                blake3: "ab".repeat(32),
            }],
        };
        let body = ResumePlanBody {
            files: vec![ResumeFile {
                rel_path: "a.bin".to_owned(),
                received_bytes: 1024,
            }],
            known_blocks: vec!["ab".repeat(32)],
            skipped: false,
        };
        assert_eq!(
            serde_json::from_str::<FileChunkMapEntry>(&serde_json::to_string(&entry).unwrap())
                .unwrap(),
            entry
        );
        assert_eq!(
            serde_json::from_str::<ResumePlanBody>(&serde_json::to_string(&body).unwrap()).unwrap(),
            body
        );
    }
}
