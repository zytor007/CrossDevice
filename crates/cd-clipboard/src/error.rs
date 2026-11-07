use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("invalid UTF-8 payload: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("invalid clipboard JSON: {0}")]
    Json(#[from] serde_json::Error),
}
