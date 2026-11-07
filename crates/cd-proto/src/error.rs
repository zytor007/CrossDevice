use serde::{Deserialize, Serialize};

/// Stable error codes shared by C0 and the UI. The UI shows `message()` plus a
/// concrete next action; the code itself is never shown to end users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    Transport,
    Timeout,
    ProtocolVersion,
    Unauthorized,
    PairRequired,
    CertificateMismatch,
    PermissionDenied,
    ClipboardTooLarge,
    DiskFull,
    FileConflict,
    TransferCancelled,
    PeerUnavailable,
    InvalidArgument,
    Busy,
    RateLimited,
    Unsupported,
    Internal,
}

impl ErrorCode {
    /// Human-readable, actionable text (Chinese product copy).
    pub fn message(self) -> &'static str {
        match self {
            Self::Transport => "网络连接中断，请检查两台设备是否仍在同一局域网",
            Self::Timeout => "操作超时，请重试；若反复超时请检查防火墙",
            Self::ProtocolVersion => "两台设备版本不兼容，请升级到同一版本",
            Self::Unauthorized => "设备未获授权，请在已配对设备列表中确认",
            Self::PairRequired => "请先完成设备配对",
            Self::CertificateMismatch => "设备身份校验失败，配对可能被篡改，请重新配对",
            Self::PermissionDenied => "系统权限不足，请按引导授予辅助功能/输入监控权限",
            Self::ClipboardTooLarge => "剪贴板内容过大，已改为手动传输",
            Self::DiskFull => "目标磁盘空间不足，请清理后重试",
            Self::FileConflict => "存在同名文件，请选择覆盖、跳过、重命名或合并",
            Self::TransferCancelled => "传输已取消",
            Self::PeerUnavailable => "对端设备当前离线",
            Self::InvalidArgument => "请求参数无效",
            Self::Busy => "设备正忙，请稍后重试",
            Self::RateLimited => "操作过于频繁，请稍后再试",
            Self::Unsupported => "当前平台不支持该功能",
            Self::Internal => "内部错误，请导出诊断包并反馈",
        }
    }

    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::Transport
                | Self::Timeout
                | Self::PeerUnavailable
                | Self::Busy
                | Self::RateLimited
                | Self::Internal
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_copy_and_sane_retry_semantics() {
        assert!(!ErrorCode::Transport.message().is_empty());
        assert!(ErrorCode::Transport.retryable());
        assert!(!ErrorCode::CertificateMismatch.retryable());
        assert!(!ErrorCode::DiskFull.retryable());
    }

    #[test]
    fn codes_serialise_to_stable_names() {
        let json = serde_json::to_string(&ErrorCode::PairRequired).unwrap();
        assert_eq!(json, "\"PAIR_REQUIRED\"");
    }
}
