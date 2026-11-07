use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Protocol negotiation is `major.minor`. A different major is rejected; a
/// different minor is accepted at the lower of the two versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const CURRENT: Self = Self { major: 1, minor: 0 };

    pub fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    pub fn parse(input: &str) -> Result<Self, VersionError> {
        let (major, minor) = input
            .split_once('.')
            .ok_or_else(|| VersionError::InvalidFormat {
                input: input.to_owned(),
            })?;
        let major = major
            .parse::<u16>()
            .map_err(|_| VersionError::InvalidFormat {
                input: input.to_owned(),
            })?;
        let minor = minor
            .parse::<u16>()
            .map_err(|_| VersionError::InvalidFormat {
                input: input.to_owned(),
            })?;
        Ok(Self { major, minor })
    }

    pub fn negotiate(self, remote: Self) -> Result<Self, VersionError> {
        if self.major != remote.major {
            return Err(VersionError::IncompatibleMajor {
                local_major: self.major,
                remote_major: remote.major,
            });
        }
        Ok(Self {
            major: self.major,
            minor: self.minor.min(remote.minor),
        })
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VersionError {
    #[error("invalid protocol version `{input}`; expected MAJOR.MINOR")]
    InvalidFormat { input: String },
    #[error("incompatible protocol major {remote_major} (local {local_major})")]
    IncompatibleMajor { local_major: u16, remote_major: u16 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats() {
        assert_eq!(
            ProtocolVersion::parse("1.2").unwrap(),
            ProtocolVersion::new(1, 2)
        );
        assert_eq!(ProtocolVersion::new(1, 2).to_string(), "1.2");
    }

    #[test]
    fn rejects_malformed_versions() {
        for input in ["", "1", "1.", ".0", "1.0.0", "x.1", "1.-1"] {
            assert!(ProtocolVersion::parse(input).is_err(), "{input}");
        }
    }

    #[test]
    fn major_must_match_and_minor_takes_the_minimum() {
        let local = ProtocolVersion::new(1, 5);
        assert_eq!(
            local.negotiate(ProtocolVersion::new(1, 2)).unwrap(),
            ProtocolVersion::new(1, 2)
        );
        assert_eq!(
            local.negotiate(ProtocolVersion::new(1, 9)).unwrap(),
            ProtocolVersion::new(1, 5)
        );
        assert!(local.negotiate(ProtocolVersion::new(2, 0)).is_err());
    }
}
