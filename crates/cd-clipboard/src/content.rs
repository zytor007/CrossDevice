use cd_proto::ClipboardKind;
use serde::{Deserialize, Serialize};

use crate::error::ClipboardError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    Html {
        html: String,
    },
    Image {
        png: Vec<u8>,
        width: u32,
        height: u32,
    },
    Files {
        entries: Vec<FileRef>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn kind(&self) -> ClipboardKind {
        match self {
            Self::Text { .. } => ClipboardKind::Text,
            Self::Html { .. } => ClipboardKind::Html,
            Self::Image { .. } => ClipboardKind::Image,
            Self::Files { .. } => ClipboardKind::FileList,
        }
    }

    /// Wire bytes after the C2 header. Directories and metadata are carried by
    /// the manifest, not by this blob.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Text { text } => text.as_bytes().to_vec(),
            Self::Html { html } => html.as_bytes().to_vec(),
            Self::Image { png, .. } => png.clone(),
            Self::Files { entries } => {
                serde_json::to_vec(entries).expect("FileRef is serialisable")
            }
        }
    }

    pub fn decode(kind: ClipboardKind, bytes: &[u8]) -> Result<Self, ClipboardError> {
        match kind {
            ClipboardKind::Text => Ok(Self::Text {
                text: String::from_utf8(bytes.to_vec())?,
            }),
            ClipboardKind::Html => Ok(Self::Html {
                html: String::from_utf8(bytes.to_vec())?,
            }),
            ClipboardKind::Image => Ok(Self::Image {
                png: bytes.to_vec(),
                width: 0,
                height: 0,
            }),
            ClipboardKind::FileList | ClipboardKind::DirectoryTree => Ok(Self::Files {
                entries: serde_json::from_slice(bytes)?,
            }),
        }
    }

    pub fn hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(kind_tag(self.kind()).as_bytes());
        hasher.update(&self.encode());
        let hash = hasher.finalize();
        *hash.as_bytes()
    }

    pub fn hash_hex(&self) -> String {
        hex(&self.hash())
    }

    pub fn size(&self) -> u64 {
        self.encode().len() as u64
    }

    pub fn preview(&self) -> String {
        match self {
            Self::Text { text } | Self::Html { html: text } => preview_text(text),
            Self::Image { png, width, height } => {
                if *width > 0 && *height > 0 {
                    format!("PNG 图片 {width}×{height} · {} B", png.len())
                } else {
                    format!("PNG 图片 · {} B", png.len())
                }
            }
            Self::Files { entries } => {
                let names = entries
                    .iter()
                    .take(3)
                    .map(|entry| basename(&entry.path))
                    .collect::<Vec<_>>()
                    .join("、");
                if entries.len() > 3 {
                    format!("{names} 等 {} 项", entries.len())
                } else {
                    names
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub content: Content,
    pub hash: String,
    pub size: u64,
    pub origin_id: String,
    pub seq: u64,
}

impl Snapshot {
    pub fn new(content: Content, origin_id: impl Into<String>, seq: u64) -> Self {
        let hash = content.hash_hex();
        let size = content.size();
        Self {
            content,
            hash,
            size,
            origin_id: origin_id.into(),
            seq,
        }
    }

    pub fn payload(&self) -> Vec<u8> {
        self.content.encode()
    }

    pub fn announce(&self) -> cd_proto::ClipboardAnnounce {
        cd_proto::ClipboardAnnounce {
            kind: self.content.kind(),
            hash: self.hash.clone(),
            size: self.size,
            origin_id: self.origin_id.clone(),
            seq: self.seq,
        }
    }
}

fn kind_tag(kind: ClipboardKind) -> &'static str {
    match kind {
        ClipboardKind::Text => "text",
        ClipboardKind::Html => "html",
        ClipboardKind::Image => "image",
        ClipboardKind::FileList => "files",
        ClipboardKind::DirectoryTree => "directory",
    }
}

fn preview_text(text: &str) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview = flattened.chars().take(200).collect::<String>();
    if flattened.chars().count() > 200 {
        preview.push('…');
    }
    preview
}

fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_hash_is_deterministic_and_kind_scoped() {
        let text = Content::text("密码是 hunter2");
        assert_eq!(text.hash(), text.hash());
        let html = Content::Html {
            html: "密码是 hunter2".to_owned(),
        };
        assert_ne!(text.hash(), html.hash());
        assert_eq!(text.hash_hex().len(), 64);
    }

    #[test]
    fn text_roundtrips_through_wire_bytes() {
        let content = Content::text("跨设备剪贴板");
        let bytes = content.encode();
        let decoded = Content::decode(ClipboardKind::Text, &bytes).unwrap();
        assert_eq!(decoded, content);
        assert_eq!(decoded.hash(), content.hash());
    }

    #[test]
    fn files_roundtrip_through_json() {
        let content = Content::Files {
            entries: vec![
                FileRef {
                    path: "/tmp/a.txt".to_owned(),
                    size: 10,
                    is_dir: false,
                },
                FileRef {
                    path: "/tmp/assets".to_owned(),
                    size: 0,
                    is_dir: true,
                },
            ],
        };
        let bytes = content.encode();
        assert_eq!(
            Content::decode(ClipboardKind::FileList, &bytes).unwrap(),
            content
        );
    }

    #[test]
    fn preview_is_capped_and_flattened() {
        let content = Content::text(format!("{}\n第二行", "x".repeat(300)));
        let preview = content.preview();
        assert_eq!(preview.chars().count(), 201);
        assert!(!preview.contains('\n'));
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn file_preview_uses_basenames() {
        let content = Content::Files {
            entries: vec![FileRef {
                path: "C:\\Users\\me\\report.pdf".to_owned(),
                size: 1,
                is_dir: false,
            }],
        };
        assert_eq!(content.preview(), "report.pdf");
    }

    #[test]
    fn snapshot_caches_hash_size_and_announce() {
        let content = Content::text("hello");
        let snapshot = Snapshot::new(content.clone(), "linux-4k", 9);
        assert_eq!(snapshot.hash, content.hash_hex());
        assert_eq!(snapshot.size, 5);
        assert_eq!(snapshot.payload(), b"hello");
        assert_eq!(snapshot.announce().seq, 9);
        assert_eq!(snapshot.announce().kind, ClipboardKind::Text);
    }
}
