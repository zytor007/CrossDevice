pub const DEFAULT_AUTO_FETCH_BYTES: u64 = 64 * 1024 * 1024;
pub const MIN_AUTO_FETCH_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_AUTO_FETCH_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardPolicy {
    /// Files up to this size are pulled before the clipboard is published.
    pub auto_fetch_bytes: u64,
    pub history_limit: usize,
    /// Case-insensitive application name/bundle fragments.
    pub excluded_apps: Vec<String>,
}

impl Default for ClipboardPolicy {
    fn default() -> Self {
        Self {
            auto_fetch_bytes: DEFAULT_AUTO_FETCH_BYTES,
            history_limit: 200,
            excluded_apps: [
                "1password",
                "bitwarden",
                "lastpass",
                "keepass",
                "enpass",
                "dashlane",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }
    }
}

impl ClipboardPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if !(MIN_AUTO_FETCH_BYTES..=MAX_AUTO_FETCH_BYTES).contains(&self.auto_fetch_bytes) {
            return Err(format!(
                "auto_fetch_bytes must be between {} and {} bytes",
                MIN_AUTO_FETCH_BYTES, MAX_AUTO_FETCH_BYTES
            ));
        }
        if self.history_limit == 0 {
            return Err("history_limit must be at least 1".to_owned());
        }
        Ok(())
    }

    pub fn should_auto_fetch(&self, size: u64) -> bool {
        size <= self.auto_fetch_bytes
    }

    pub fn is_excluded(&self, app: &str) -> bool {
        let app = app.to_lowercase();
        self.excluded_apps
            .iter()
            .any(|pattern| app.contains(&pattern.to_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_auto_fetch_is_64_mb() {
        let policy = ClipboardPolicy::default();
        assert_eq!(policy.auto_fetch_bytes, 64 * 1024 * 1024);
        assert!(policy.should_auto_fetch(64 * 1024 * 1024));
        assert!(!policy.should_auto_fetch(64 * 1024 * 1024 + 1));
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn auto_fetch_range_is_enforced() {
        let too_small = ClipboardPolicy {
            auto_fetch_bytes: MIN_AUTO_FETCH_BYTES - 1,
            ..ClipboardPolicy::default()
        };
        assert!(too_small.validate().is_err());

        let too_large = ClipboardPolicy {
            auto_fetch_bytes: MAX_AUTO_FETCH_BYTES + 1,
            ..ClipboardPolicy::default()
        };
        assert!(too_large.validate().is_err());

        let policy = ClipboardPolicy {
            auto_fetch_bytes: 128 * 1024 * 1024,
            ..ClipboardPolicy::default()
        };
        assert!(policy.validate().is_ok());
        assert!(policy.should_auto_fetch(100 * 1024 * 1024));
        assert!(!policy.should_auto_fetch(200 * 1024 * 1024));
    }

    #[test]
    fn password_managers_are_excluded_case_insensitively() {
        let policy = ClipboardPolicy::default();
        assert!(policy.is_excluded("1Password 8"));
        assert!(policy.is_excluded("com.bitwarden.desktop"));
        assert!(!policy.is_excluded("Visual Studio Code"));
    }
}
