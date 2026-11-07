use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("certificate generation failed: {0}")]
    Certificate(String),
    #[error("TLS configuration failed: {0}")]
    Tls(#[from] rustls::Error),
    #[error("QUIC configuration failed: {0}")]
    Config(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("QUIC connection error: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("write failed: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("read failed: {0}")]
    Read(#[from] quinn::ReadError),
    #[error("read exact failed: {0}")]
    ReadExact(#[from] quinn::ReadExactError),
    #[error("datagram failed: {0}")]
    Datagram(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
