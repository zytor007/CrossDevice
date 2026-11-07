//! Cooperative pause/cancel control for file transfers.

use std::sync::Arc;

use tokio::sync::watch;

use crate::error::SyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferSignal {
    Running,
    Paused,
    Cancelled,
}

/// A cheap, cloneable control flag shared by the daemon and the send/receive
/// loops. The control channel can pause, resume or cancel a transfer while the
/// data loop is blocked on QUIC I/O.
#[derive(Clone, Debug)]
pub struct CancelToken {
    tx: Arc<watch::Sender<TransferSignal>>,
    rx: watch::Receiver<TransferSignal>,
}

impl CancelToken {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(TransferSignal::Running);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    pub fn cancel(&self) {
        let _ = self.tx.send_if_modified(|state| {
            if *state == TransferSignal::Cancelled {
                false
            } else {
                *state = TransferSignal::Cancelled;
                true
            }
        });
    }

    pub fn pause(&self) {
        let _ = self.tx.send_if_modified(|state| {
            if *state == TransferSignal::Cancelled || *state == TransferSignal::Paused {
                false
            } else {
                *state = TransferSignal::Paused;
                true
            }
        });
    }

    /// Resumes a paused transfer. A cancelled transfer stays cancelled.
    pub fn resume(&self) {
        let _ = self.tx.send_if_modified(|state| {
            if *state == TransferSignal::Paused {
                *state = TransferSignal::Running;
                true
            } else {
                false
            }
        });
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(*self.rx.borrow(), TransferSignal::Cancelled)
    }

    pub fn is_paused(&self) -> bool {
        matches!(*self.rx.borrow(), TransferSignal::Paused)
    }

    /// Resolves once the token is cancelled. Safe to poll from `select!`.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let mut rx = self.rx.clone();
        loop {
            if matches!(*rx.borrow(), TransferSignal::Cancelled) {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Blocks while the transfer is paused and returns `Cancelled` when it was
    /// aborted. Data loops call this before every chunk and before committing
    /// files to disk.
    pub async fn checkpoint(&self) -> Result<(), SyncError> {
        loop {
            match *self.rx.borrow() {
                TransferSignal::Running => return Ok(()),
                TransferSignal::Cancelled => return Err(SyncError::Cancelled),
                TransferSignal::Paused => {}
            }
            let mut rx = self.rx.clone();
            if rx.changed().await.is_err() {
                return Ok(());
            }
        }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}
