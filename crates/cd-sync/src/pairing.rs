use cd_proto::{sas::sas_digits, ControlMessage, PairBegin, PairConfirm};
use cd_transport::{Identity, Link};
use sha2::{Digest, Sha256};

use crate::error::SyncError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairOutcome {
    pub code: String,
    pub peer_certificate: Vec<u8>,
    pub peer_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairPreview {
    pub code: String,
    pub peer_certificate: Vec<u8>,
    pub peer_fingerprint: String,
}

impl PairPreview {
    fn into_outcome(self) -> PairOutcome {
        PairOutcome {
            code: self.code,
            peer_certificate: self.peer_certificate,
            peer_fingerprint: self.peer_fingerprint,
        }
    }
}

/// Simplified SAS exchange after the mutual-TLS handshake. The initiator sends
/// a nonce; both sides derive the six-digit code. This wrapper auto-confirms;
/// interactive callers use begin/finish to show the code and wait for approval.
pub async fn pair(
    link: &Link,
    local: &Identity,
    initiator: bool,
) -> Result<PairOutcome, SyncError> {
    if initiator {
        let preview = pair_begin_initiator(link, local).await?;
        pair_finish_initiator(link, preview, true).await
    } else {
        let preview = pair_begin_responder(link, local).await?;
        pair_finish_responder(link, preview, true).await
    }
}

/// Initiator step one: send the nonce and wait for the responder's code. The
/// returned preview must be confirmed with [`pair_finish_initiator`].
pub async fn pair_begin_initiator(link: &Link, local: &Identity) -> Result<PairPreview, SyncError> {
    let peer_cert = peer_certificate(link)?;
    let peer_fingerprint = sha256_hex(&peer_cert);
    let local_cert = local.cert().as_ref().to_vec();

    let nonce = random_hex(16)?;
    link.send_control(&ControlMessage::PairBegin(PairBegin {
        nonce: nonce.clone(),
        cert_fingerprint: local.fingerprint_hex(),
    }))
    .await?;

    let reply = link.recv_control().await?;
    let ControlMessage::PairConfirm(PairConfirm { code, confirm }) = reply else {
        return Err(SyncError::Pairing("expected PairConfirm".to_owned()));
    };
    if !confirm {
        return Err(SyncError::Pairing("peer rejected pairing".to_owned()));
    }
    let local_code = sas_digits(&local_cert, &peer_cert, &decode_hex(&nonce)?);
    if local_code != code {
        return Err(SyncError::Pairing(format!(
            "SAS mismatch: local {local_code}, peer {code}"
        )));
    }

    Ok(PairPreview {
        code: local_code,
        peer_certificate: peer_cert,
        peer_fingerprint,
    })
}

pub async fn pair_finish_initiator(
    link: &Link,
    preview: PairPreview,
    accepted: bool,
) -> Result<PairOutcome, SyncError> {
    link.send_control(&ControlMessage::PairConfirm(PairConfirm {
        code: preview.code.clone(),
        confirm: accepted,
    }))
    .await?;
    if !accepted {
        return Err(SyncError::Pairing("pairing rejected locally".to_owned()));
    }
    Ok(preview.into_outcome())
}

/// Responder step one: read PairBegin and derive the code without publishing a
/// confirmation yet.
pub async fn pair_begin_responder(link: &Link, local: &Identity) -> Result<PairPreview, SyncError> {
    let peer_cert = peer_certificate(link)?;
    let peer_fingerprint = sha256_hex(&peer_cert);
    let local_cert = local.cert().as_ref().to_vec();

    let begin = link.recv_control().await?;
    let ControlMessage::PairBegin(begin) = begin else {
        return Err(SyncError::Pairing("expected PairBegin".to_owned()));
    };
    if begin.cert_fingerprint != peer_fingerprint {
        return Err(SyncError::Pairing(
            "peer certificate fingerprint mismatch".to_owned(),
        ));
    }
    let nonce = decode_hex(&begin.nonce)?;
    let code = sas_digits(&local_cert, &peer_cert, &nonce);

    Ok(PairPreview {
        code,
        peer_certificate: peer_cert,
        peer_fingerprint,
    })
}

/// Responder step two: publish the decision and wait for the initiator's
/// confirmation when accepted.
pub async fn pair_finish_responder(
    link: &Link,
    preview: PairPreview,
    accepted: bool,
) -> Result<PairOutcome, SyncError> {
    link.send_control(&ControlMessage::PairConfirm(PairConfirm {
        code: preview.code.clone(),
        confirm: accepted,
    }))
    .await?;
    if !accepted {
        return Err(SyncError::Pairing("pairing rejected locally".to_owned()));
    }

    let reply = link.recv_control().await?;
    let ControlMessage::PairConfirm(PairConfirm {
        code: peer_code,
        confirm,
    }) = reply
    else {
        return Err(SyncError::Pairing("expected PairConfirm".to_owned()));
    };
    if !confirm || peer_code != preview.code {
        return Err(SyncError::Pairing(format!(
            "peer code {peer_code} != {}",
            preview.code
        )));
    }
    Ok(preview.into_outcome())
}

fn peer_certificate(link: &Link) -> Result<Vec<u8>, SyncError> {
    link.peer_certificate_der()
        .ok_or_else(|| SyncError::Certificate("peer did not present a certificate".to_owned()))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&sha256_bytes(bytes))
}

pub fn sha256_bytes(bytes: &[u8]) -> Vec<u8> {
    Sha256::digest(bytes).to_vec()
}

fn random_hex(length: usize) -> Result<String, SyncError> {
    let mut bytes = vec![0_u8; length];
    getrandom::getrandom(&mut bytes).map_err(|error| SyncError::Random(error.to_string()))?;
    Ok(hex(&bytes))
}

fn decode_hex(input: &str) -> Result<Vec<u8>, SyncError> {
    if input.len() % 2 != 0 {
        return Err(SyncError::Pairing("nonce has odd hex length".to_owned()));
    }
    let mut bytes = Vec::with_capacity(input.len() / 2);
    for chunk in input.as_bytes().chunks(2) {
        let byte = u8::from_str_radix(
            std::str::from_utf8(chunk)
                .map_err(|_| SyncError::Pairing("nonce is not UTF-8".to_owned()))?,
            16,
        )
        .map_err(|_| SyncError::Pairing("nonce is not hex".to_owned()))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrips() {
        let text = random_hex(16).unwrap();
        assert_eq!(decode_hex(&text).unwrap().len(), 16);
        assert!(decode_hex("xyz").is_err());
    }

    #[test]
    fn fingerprints_are_sha256() {
        let fingerprint = sha256_hex(b"certificate");
        assert_eq!(fingerprint.len(), 64);
    }
}
