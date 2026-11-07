use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use cd_proto::{ControlMessage, DataHeader, InputEvent};
use quinn::crypto::rustls::QuicServerConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, RootCertStore, SignatureScheme};
use tokio::sync::Mutex;

use crate::error::TransportError;
use crate::identity::Identity;

const ALPN: &[u8] = b"crossdev/1";
const MAX_CONTROL_BYTES: usize = 64 * 1024;
const MAX_DATA_HEADER_BYTES: usize = 64 * 1024;
const MAX_DATA_PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;
const DATAGRAM_BUFFER_BYTES: usize = 1024 * 1024;

pub struct QuicTransport {
    endpoint: quinn::Endpoint,
}

impl QuicTransport {
    pub fn server(addr: SocketAddr, identity: &Identity) -> Result<Self, TransportError> {
        let provider = rustls::crypto::ring::default_provider();
        let verifier = Arc::new(AcceptAnyClientCert {
            algorithms: provider.signature_verification_algorithms,
        });
        let mut crypto = rustls::ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![identity.cert().clone()], identity.clone_key())?;
        crypto.alpn_protocols = vec![ALPN.to_vec()];

        let mut config = quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(crypto)
                .map_err(|error| TransportError::Config(error.to_string()))?,
        ));
        config.transport_config(datagram_transport());
        let endpoint = quinn::Endpoint::server(config, addr)?;
        Ok(Self { endpoint })
    }

    pub fn client() -> Result<Self, TransportError> {
        Ok(Self {
            endpoint: quinn::Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0)))?,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, TransportError> {
        Ok(self.endpoint.local_addr()?)
    }

    /// Accept one QUIC connection and its C0 control stream.
    pub async fn accept(&self) -> Result<Link, TransportError> {
        let Some(incoming) = self.endpoint.accept().await else {
            return Err(TransportError::Protocol("endpoint closed".to_owned()));
        };
        let connection = incoming.await?;
        Link::accept(connection).await
    }

    /// Connect to a peer, trusting exactly the pinned certificate for now.
    pub async fn connect(
        &mut self,
        addr: SocketAddr,
        server_name: &str,
        trusted: &CertificateDer<'static>,
        client_identity: &Identity,
    ) -> Result<Link, TransportError> {
        let mut roots = RootCertStore::empty();
        roots
            .add(trusted.clone())
            .map_err(|error| TransportError::Certificate(error.to_string()))?;
        let crypto = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_client_auth_cert(
                vec![client_identity.cert().clone()],
                client_identity.clone_key(),
            )?;
        self.finish_connect(crypto, addr, server_name).await
    }

    /// First-contact connect. The server certificate is accepted at the TLS
    /// layer and must be pinned immediately afterwards by comparing the
    /// SHA-256 fingerprint carried on the C0 `hello` exchange.
    pub async fn connect_unverified(
        &mut self,
        addr: SocketAddr,
        server_name: &str,
        client_identity: &Identity,
    ) -> Result<Link, TransportError> {
        let provider = rustls::crypto::ring::default_provider();
        let verifier = Arc::new(AcceptAnyServerCert {
            algorithms: provider.signature_verification_algorithms,
        });
        let crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(
                vec![client_identity.cert().clone()],
                client_identity.clone_key(),
            )?;
        self.finish_connect(crypto, addr, server_name).await
    }

    async fn finish_connect(
        &mut self,
        mut crypto: rustls::ClientConfig,
        addr: SocketAddr,
        server_name: &str,
    ) -> Result<Link, TransportError> {
        crypto.alpn_protocols = vec![ALPN.to_vec()];
        let client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
                .map_err(|error| TransportError::Config(error.to_string()))?,
        ));
        self.endpoint.set_default_client_config(client_config);

        let connection = self
            .endpoint
            .connect(addr, server_name)
            .map_err(|error| TransportError::Connect(error.to_string()))?
            .await?;
        Link::open(connection).await
    }

    pub fn close(&self) {
        self.endpoint.close(0u32.into(), b"bye");
    }
}

fn datagram_transport() -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_concurrent_bidi_streams(64_u32.into());
    transport.datagram_send_buffer_size(DATAGRAM_BUFFER_BYTES);
    transport.datagram_receive_buffer_size(Some(DATAGRAM_BUFFER_BYTES));
    Arc::new(transport)
}

#[derive(Debug)]
struct AcceptAnyClientCert {
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

#[derive(Debug)]
struct AcceptAnyServerCert {
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // The certificate fingerprint is pinned on the C0 hello right after
        // the handshake; TLS alone cannot know the peer on first contact.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for AcceptAnyClientCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        // First contact is authenticated by the pairwise SAS code, then the
        // certificate fingerprint is pinned in the device store.
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// One established peer: one reliable C0 stream plus the C1 datagram path.
/// The two C0 halves are mutexed so a daemon can share one link across a
/// heartbeat task, an input pump and a data task without interleaving frames.
pub struct Link {
    connection: quinn::Connection,
    control_send: Mutex<quinn::SendStream>,
    control_recv: Mutex<quinn::RecvStream>,
}

impl Link {
    pub async fn open(connection: quinn::Connection) -> Result<Self, TransportError> {
        let (control_send, control_recv) = connection.open_bi().await?;
        Ok(Self {
            connection,
            control_send: Mutex::new(control_send),
            control_recv: Mutex::new(control_recv),
        })
    }

    pub async fn accept(connection: quinn::Connection) -> Result<Self, TransportError> {
        let (control_send, control_recv) = connection.accept_bi().await?;
        Ok(Self {
            connection,
            control_send: Mutex::new(control_send),
            control_recv: Mutex::new(control_recv),
        })
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    /// DER certificate presented by the peer during mutual TLS.
    pub fn peer_certificate_der(&self) -> Option<Vec<u8>> {
        let identity = self.connection.peer_identity()?;
        let certs = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
        certs.first().map(|cert| cert.as_ref().to_vec())
    }

    pub fn max_datagram_size(&self) -> Option<usize> {
        self.connection.max_datagram_size()
    }

    pub fn rtt(&self) -> std::time::Duration {
        self.connection.stats().path.rtt
    }

    /// Resolves when the peer closes the connection or the path fails.
    pub async fn closed(&self) {
        self.connection.closed().await;
    }

    pub fn close_reason(&self) -> Option<quinn::ConnectionError> {
        self.connection.close_reason()
    }

    pub async fn send_control(&self, message: &ControlMessage) -> Result<(), TransportError> {
        let bytes = message.to_json()?;
        if bytes.len() > MAX_CONTROL_BYTES {
            return Err(TransportError::Protocol(format!(
                "control message too large: {} bytes",
                bytes.len()
            )));
        }
        let len = u32::try_from(bytes.len())
            .map_err(|_| TransportError::Protocol("control message length overflow".to_owned()))?;
        let mut send = self.control_send.lock().await;
        send.write_all(&len.to_be_bytes()).await?;
        send.write_all(bytes.as_bytes()).await?;
        Ok(())
    }

    pub async fn recv_control(&self) -> Result<ControlMessage, TransportError> {
        let mut recv = self.control_recv.lock().await;
        let mut len_bytes = [0_u8; 4];
        recv.read_exact(&mut len_bytes).await?;
        let len = u32::from_be_bytes(len_bytes) as usize;
        if len == 0 || len > MAX_CONTROL_BYTES {
            return Err(TransportError::Protocol(format!(
                "invalid control length {len}"
            )));
        }
        let mut bytes = vec![0_u8; len];
        recv.read_exact(&mut bytes).await?;
        ControlMessage::from_json(&String::from_utf8_lossy(&bytes)).map_err(TransportError::from)
    }

    pub fn send_input(&self, event: &InputEvent) -> Result<(), TransportError> {
        let bytes = event.to_bytes();
        if let Some(max) = self.max_datagram_size() {
            if bytes.len() > max {
                return Err(TransportError::Datagram(format!(
                    "C1 event {} exceeds datagram size {}",
                    bytes.len(),
                    max
                )));
            }
        } else {
            return Err(TransportError::Datagram(
                "peer did not negotiate QUIC datagrams".to_owned(),
            ));
        }
        self.connection
            .send_datagram(Bytes::copy_from_slice(&bytes))
            .map_err(|error| TransportError::Datagram(error.to_string()))
    }

    pub async fn recv_input(&self) -> Result<InputEvent, TransportError> {
        let bytes = self
            .connection
            .read_datagram()
            .await
            .map_err(|error| TransportError::Datagram(error.to_string()))?;
        InputEvent::from_bytes(&bytes).map_err(|error| TransportError::Protocol(error.to_string()))
    }

    /// Open a fresh C2 stream. One stream carries one framed data transfer.
    pub async fn open_data(&self) -> Result<DataSender, TransportError> {
        let (send, _recv) = self.connection.open_bi().await?;
        Ok(DataSender { send })
    }

    /// Accept a C2 stream opened by the peer.
    pub async fn accept_data(&self) -> Result<DataReceiver, TransportError> {
        let (_send, recv) = self.connection.accept_bi().await?;
        Ok(DataReceiver { recv })
    }

    pub fn close(&self, code: u32, reason: &[u8]) {
        self.connection.close(code.into(), reason);
    }
}

/// Sends one framed C2 transfer:
/// `u32 header_len | header JSON | u64 payload_len | payload`.
pub struct DataSender {
    send: quinn::SendStream,
}

impl DataSender {
    pub async fn send(
        &mut self,
        header: &DataHeader,
        payload: &[u8],
    ) -> Result<(), TransportError> {
        if header.declared_size() != payload.len() as u64 {
            return Err(TransportError::Protocol(format!(
                "header declares {} bytes but payload has {}",
                header.declared_size(),
                payload.len()
            )));
        }
        if payload.len() as u64 > MAX_DATA_PAYLOAD_BYTES {
            return Err(TransportError::Protocol(format!(
                "data payload {} exceeds limit",
                payload.len()
            )));
        }

        let header_json = serde_json::to_vec(header)?;
        if header_json.len() > MAX_DATA_HEADER_BYTES {
            return Err(TransportError::Protocol(format!(
                "data header {} bytes exceeds limit",
                header_json.len()
            )));
        }
        let header_len = u32::try_from(header_json.len())
            .map_err(|_| TransportError::Protocol("data header length overflow".to_owned()))?;
        self.send.write_all(&header_len.to_be_bytes()).await?;
        self.send.write_all(&header_json).await?;
        self.send
            .write_all(&(payload.len() as u64).to_be_bytes())
            .await?;
        self.send.write_all(payload).await?;
        let _ = self.send.finish();
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataTransfer {
    pub header: DataHeader,
    pub payload: Vec<u8>,
}

pub struct DataReceiver {
    recv: quinn::RecvStream,
}

impl DataReceiver {
    pub async fn recv(&mut self) -> Result<DataTransfer, TransportError> {
        let mut header_len_bytes = [0_u8; 4];
        self.recv.read_exact(&mut header_len_bytes).await?;
        let header_len = u32::from_be_bytes(header_len_bytes) as usize;
        if header_len == 0 || header_len > MAX_DATA_HEADER_BYTES {
            return Err(TransportError::Protocol(format!(
                "invalid data header length {header_len}"
            )));
        }
        let mut header_bytes = vec![0_u8; header_len];
        self.recv.read_exact(&mut header_bytes).await?;
        let header: DataHeader = serde_json::from_slice(&header_bytes)?;

        let declared = header.declared_size();
        if declared > MAX_DATA_PAYLOAD_BYTES {
            return Err(TransportError::Protocol(format!(
                "declared payload {declared} exceeds limit"
            )));
        }

        let mut payload_len_bytes = [0_u8; 8];
        self.recv.read_exact(&mut payload_len_bytes).await?;
        let payload_len = u64::from_be_bytes(payload_len_bytes);
        if payload_len != declared {
            return Err(TransportError::Protocol(format!(
                "payload length {payload_len} does not match header {declared}"
            )));
        }

        let mut payload = vec![0_u8; payload_len as usize];
        self.recv.read_exact(&mut payload).await?;
        Ok(DataTransfer { header, payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cd_proto::{ClipboardAnnounce, ClipboardKind, DataHeader, Heartbeat, InputEvent};
    use std::time::Duration;

    #[tokio::test]
    async fn loopback_c0_and_c1_roundtrip() {
        let identity = Identity::generate("crossdev.local").expect("identity");
        let server = QuicTransport::server(SocketAddr::from(([127, 0, 0, 1], 0)), &identity)
            .expect("server");
        let addr = server.local_addr().expect("addr");
        let trusted = identity.cert().clone();

        let server_task = tokio::spawn(async move {
            let link = server.accept().await.expect("accept");
            let hello = link.recv_control().await.expect("hello");
            assert!(matches!(hello, ControlMessage::Heartbeat(_)));

            link.send_control(&ControlMessage::Heartbeat(Heartbeat { seq: 7, ts_us: 8 }))
                .await
                .expect("reply");

            let input = link.recv_input().await.expect("input");
            assert_eq!(input, InputEvent::move_relative_um(12_000, -8_000, 42));
            link.send_input(&InputEvent::move_relative_um(-12_000, 8_000, 43))
                .expect("echo");
            tokio::time::sleep(Duration::from_millis(50)).await;
            link.close(0, b"done");
        });

        let mut client = QuicTransport::client().expect("client");
        let client_identity = Identity::generate("client.crossdev.local").expect("client identity");
        let link = client
            .connect(addr, "crossdev.local", &trusted, &client_identity)
            .await
            .expect("connect");
        assert!(link.max_datagram_size().is_some());

        link.send_control(&ControlMessage::Heartbeat(Heartbeat { seq: 6, ts_us: 7 }))
            .await
            .expect("send");
        let reply = link.recv_control().await.expect("reply");
        assert_eq!(
            reply,
            ControlMessage::Heartbeat(Heartbeat { seq: 7, ts_us: 8 })
        );

        link.send_input(&InputEvent::move_relative_um(12_000, -8_000, 42))
            .expect("input");
        let echo = tokio::time::timeout(Duration::from_secs(2), link.recv_input())
            .await
            .expect("timeout")
            .expect("echo");
        assert_eq!(echo, InputEvent::move_relative_um(-12_000, 8_000, 43));

        link.close(0, b"done");
        server_task.await.expect("server task");
        client.close();
    }

    #[tokio::test]
    async fn loopback_c2_clipboard_transfer() {
        let identity = Identity::generate("crossdev.local").expect("identity");
        let server = QuicTransport::server(SocketAddr::from(([127, 0, 0, 1], 0)), &identity)
            .expect("server");
        let addr = server.local_addr().expect("addr");
        let trusted = identity.cert().clone();

        let payload = "跨设备剪贴板 C2".as_bytes().to_vec();
        let header = DataHeader::Clipboard(ClipboardAnnounce {
            kind: ClipboardKind::Text,
            hash: "ab".repeat(32),
            size: payload.len() as u64,
            origin_id: "linux-4k".to_owned(),
            seq: 7,
        });
        let expected_header = header.clone();
        let expected_payload = payload.clone();

        let server_task = tokio::spawn(async move {
            let link = server.accept().await.expect("accept");
            let _hello = link.recv_control().await.expect("hello");
            let mut data = link.accept_data().await.expect("data stream");
            let transfer = data.recv().await.expect("transfer");
            assert_eq!(transfer.header, expected_header);
            assert_eq!(transfer.payload, expected_payload);
        });

        let mut client = QuicTransport::client().expect("client");
        let client_identity = Identity::generate("client.crossdev.local").expect("client identity");
        let link = client
            .connect(addr, "crossdev.local", &trusted, &client_identity)
            .await
            .expect("connect");
        link.send_control(&ControlMessage::Heartbeat(Heartbeat { seq: 1, ts_us: 1 }))
            .await
            .expect("hello");

        let mut data = link.open_data().await.expect("open data");
        data.send(&header, &payload).await.expect("send blob");

        tokio::time::timeout(Duration::from_secs(3), server_task)
            .await
            .expect("timeout")
            .expect("server task");

        link.close(0, b"done");
        client.close();
    }
}
