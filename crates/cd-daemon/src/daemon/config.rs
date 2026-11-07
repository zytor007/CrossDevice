use std::fs;
use std::path::{Path, PathBuf};

use cd_files::ConflictPolicy;
use cd_proto::EdgeSide;
use serde::{Deserialize, Serialize};

/// Stable on-disk state. Everything the daemon needs survives a restart:
/// identity, device whitelist, block cache, inbox and this config file.
#[derive(Debug, Clone)]
pub struct DaemonPaths {
    pub root: PathBuf,
    pub identity: PathBuf,
    pub database: PathBuf,
    pub inbox: PathBuf,
    pub blocks: PathBuf,
    pub config: PathBuf,
}

impl DaemonPaths {
    pub fn resolve() -> Result<Self, String> {
        let root = if let Ok(explicit) = std::env::var("CROSSDEV_HOME") {
            PathBuf::from(explicit)
        } else if let Ok(data) = std::env::var("XDG_DATA_HOME") {
            PathBuf::from(data).join("crossdev")
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".local/share/crossdev")
        } else {
            std::env::temp_dir().join("crossdev")
        };
        Ok(Self::at(root))
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            identity: root.join("identity"),
            database: root.join("devices.sqlite"),
            inbox: root.join("inbox"),
            blocks: root.join("blocks"),
            config: root.join("config.json"),
            root,
        }
    }

    pub fn ensure(&self) -> Result<(), String> {
        fs::create_dir_all(&self.root).map_err(|error| error.to_string())?;
        fs::create_dir_all(&self.identity).map_err(|error| error.to_string())?;
        fs::create_dir_all(&self.inbox).map_err(|error| error.to_string())?;
        fs::create_dir_all(&self.blocks).map_err(|error| error.to_string())?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairMode {
    /// Accept any peer that completes the SAS exchange (development / trust-on-first-use).
    Auto,
    /// Only accept identities already pinned in the device store.
    Deny,
    /// Show the SAS code in the browser console and wait for a local decision.
    Confirm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub device_id: String,
    pub device_name: String,
    pub platform: String,
    pub bind: String,
    pub http_bind: String,
    pub pair_mode: PairMode,
    pub clipboard: bool,
    /// Write remote clipboard content into the OS clipboard. Integrations and
    /// the self-test can keep sync/status on without touching the user's clipboard.
    #[serde(default = "default_true")]
    pub clipboard_system: bool,
    /// Forward local pointer/keyboard to peers and inject remote C1 events.
    pub input_pump: bool,
    pub inbox: Option<PathBuf>,
    pub excluded_apps: Vec<String>,
    /// Optional peer QUIC address to connect outwards to; the mDNS discovery
    /// layer plugs into the same connector later.
    pub peer: Option<String>,
    /// Broadcast presence on UDP so nearby daemons can be paired with one click.
    pub discovery: bool,
    /// Screen edge that hands input to the peer; mirrored to the peer over C0.
    #[serde(default)]
    pub edge: EdgeSide,
    /// Pointer speed multiplier applied on top of the physical mm mapping.
    #[serde(default = "default_pointer_speed")]
    pub pointer_speed: f64,
    /// Milliseconds the pointer must stay on the edge before crossing.
    #[serde(default)]
    pub edge_resistance: u8,
    /// Block cache budget in bytes; 0 disables the cap.
    #[serde(default = "default_cache_limit")]
    pub cache_limit_bytes: u64,
    /// What to do when the incoming root name already exists in the inbox.
    #[serde(default = "default_conflict")]
    pub file_conflict: ConflictPolicy,
    /// Dead-zone gap in millimetres between the local and peer screen.
    #[serde(default)]
    pub layout_gap_mm: f64,
}

impl Config {
    pub fn default_for(device_id: impl Into<String>) -> Self {
        Self {
            device_id: device_id.into(),
            device_name: "Linux Desktop".to_owned(),
            platform: "linux-x11".to_owned(),
            bind: "0.0.0.0:7823".to_owned(),
            http_bind: "127.0.0.1:7824".to_owned(),
            pair_mode: PairMode::Confirm,
            clipboard: true,
            clipboard_system: true,
            input_pump: false,
            inbox: None,
            excluded_apps: cd_clipboard::ClipboardPolicy::default().excluded_apps,
            peer: None,
            discovery: true,
            edge: EdgeSide::Right,
            pointer_speed: 1.0,
            edge_resistance: 0,
            cache_limit_bytes: DEFAULT_CACHE_LIMIT_BYTES,
            file_conflict: ConflictPolicy::Rename,
            layout_gap_mm: 0.0,
        }
    }

    pub fn load(path: &Path, fallback_id: impl Into<String>) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default_for(fallback_id));
        }
        let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
        serde_json::from_str(&text).map_err(|error| format!("config {}: {error}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        fs::write(path, text).map_err(|error| error.to_string())
    }

    pub fn inbox(&self, paths: &DaemonPaths) -> PathBuf {
        self.inbox.clone().unwrap_or_else(|| paths.inbox.clone())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.device_id.trim().is_empty() {
            return Err("device_id must not be empty".to_owned());
        }
        self.bind
            .parse::<std::net::SocketAddr>()
            .map_err(|error| format!("invalid QUIC bind `{}`: {error}", self.bind))?;
        self.http_bind
            .parse::<std::net::SocketAddr>()
            .map_err(|error| format!("invalid HTTP bind `{}`: {error}", self.http_bind))?;
        if let Some(peer) = &self.peer {
            peer.parse::<std::net::SocketAddr>()
                .map_err(|error| format!("invalid peer address `{peer}`: {error}"))?;
        }
        if !self.pointer_speed.is_finite()
            || !(cd_proto::MIN_POINTER_SPEED..=cd_proto::MAX_POINTER_SPEED)
                .contains(&self.pointer_speed)
        {
            return Err(format!(
                "pointer_speed must be between {} and {}",
                cd_proto::MIN_POINTER_SPEED,
                cd_proto::MAX_POINTER_SPEED
            ));
        }
        if self.edge_resistance > cd_proto::MAX_EDGE_RESISTANCE {
            return Err(format!(
                "edge_resistance must not exceed {}",
                cd_proto::MAX_EDGE_RESISTANCE
            ));
        }
        if matches!(self.file_conflict, ConflictPolicy::Merge) {
            return Err(
                "file_conflict must be rename, overwrite, skip or ask; merge is not implemented"
                    .to_owned(),
            );
        }
        if !self.layout_gap_mm.is_finite()
            || !(0.0..=cd_geometry::MAX_GAP_MM).contains(&self.layout_gap_mm)
        {
            return Err(format!(
                "layout_gap_mm must be between 0 and {}",
                cd_geometry::MAX_GAP_MM
            ));
        }
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

fn default_pointer_speed() -> f64 {
    1.0
}

/// 8 GiB keeps block reuse useful without letting the cache grow unbounded.
pub const DEFAULT_CACHE_LIMIT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

fn default_cache_limit() -> u64 {
    DEFAULT_CACHE_LIMIT_BYTES
}

fn default_conflict() -> ConflictPolicy {
    ConflictPolicy::Rename
}

/// Short, filesystem-safe identity suffix so two machines with the same hostname
/// still get distinct device ids.
pub fn device_id_from(hostname: Option<&str>, fingerprint: &str) -> String {
    let host = hostname.unwrap_or("crossdev");
    let cleaned = host
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    let suffix = fingerprint.chars().take(8).collect::<String>();
    format!("{cleaned}-{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_are_safe_and_stable() {
        let id = device_id_from(Some("Studio Mac.local"), "abcdef0123456789");
        assert_eq!(id, "Studio-Mac-local-abcdef01");
        assert_eq!(device_id_from(None, "abc"), "crossdev-abc");
    }

    #[test]
    fn config_roundtrips_and_validates() {
        let root = std::env::temp_dir().join(format!(
            "crossdev-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("config.json");
        fs::create_dir_all(&root).unwrap();
        let config = Config::default_for("linux-test");
        assert!(config.validate().is_ok());
        config.save(&path).unwrap();
        assert_eq!(
            Config::load(&path, "ignored").unwrap().device_id,
            "linux-test"
        );

        let mut broken = config;
        broken.bind = "not-an-address".to_owned();
        assert!(broken.validate().is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
