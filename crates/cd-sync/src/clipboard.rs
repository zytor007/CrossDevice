use cd_clipboard::{Content, Snapshot};
use cd_proto::DataHeader;
use cd_transport::Link;

use crate::error::SyncError;

pub async fn send_clipboard(link: &Link, snapshot: &Snapshot) -> Result<(), SyncError> {
    let payload = snapshot.payload();
    let mut data = link.open_data().await?;
    data.send(&DataHeader::Clipboard(snapshot.announce()), &payload)
        .await?;
    Ok(())
}

pub async fn receive_clipboard(link: &Link) -> Result<Snapshot, SyncError> {
    let transfer = link.accept_data().await?.recv().await?;
    let DataHeader::Clipboard(announce) = transfer.header else {
        return Err(SyncError::Protocol(
            "expected clipboard blob on C2 stream".to_owned(),
        ));
    };
    let content = Content::decode(announce.kind, &transfer.payload)
        .map_err(|error| SyncError::Protocol(error.to_string()))?;
    if content.hash_hex() != announce.hash {
        return Err(SyncError::HashMismatch {
            rel_path: "<clipboard>".to_owned(),
            offset: 0,
        });
    }
    Ok(Snapshot {
        content,
        hash: announce.hash,
        size: announce.size,
        origin_id: announce.origin_id,
        seq: announce.seq,
    })
}
