use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Queued,
    Running,
    /// Network loss pauses instead of cancelling; resume picks up missing blocks.
    Paused,
    Completed,
    /// The receiver kept the existing destination and consciously ignored the
    /// incoming copy because of its file-conflict policy.
    Skipped,
    Cancelled,
    Failed,
}

impl TransferState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Skipped | Self::Cancelled | Self::Failed
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferTask {
    pub id: String,
    pub root_name: String,
    pub total_bytes: u64,
    pub received_bytes: u64,
    pub state: TransferState,
}

impl TransferTask {
    pub fn new(id: impl Into<String>, root_name: impl Into<String>, total_bytes: u64) -> Self {
        Self {
            id: id.into(),
            root_name: root_name.into(),
            total_bytes,
            received_bytes: 0,
            state: TransferState::Queued,
        }
    }

    pub fn start(&mut self) -> Result<(), TransferError> {
        self.require(TransferState::Queued, "start")?;
        self.state = TransferState::Running;
        Ok(())
    }

    pub fn pause(&mut self) -> Result<(), TransferError> {
        self.require(TransferState::Running, "pause")?;
        self.state = TransferState::Paused;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<(), TransferError> {
        self.require(TransferState::Paused, "resume")?;
        self.state = TransferState::Running;
        Ok(())
    }

    pub fn progress(&mut self, received_bytes: u64) -> Result<(), TransferError> {
        self.require(TransferState::Running, "progress")?;
        if received_bytes > self.total_bytes {
            return Err(TransferError::ProgressExceedsTotal {
                received: received_bytes,
                total: self.total_bytes,
            });
        }
        self.received_bytes = received_bytes;
        Ok(())
    }

    pub fn complete(&mut self) -> Result<(), TransferError> {
        self.require(TransferState::Running, "complete")?;
        if self.received_bytes < self.total_bytes {
            return Err(TransferError::Incomplete {
                received: self.received_bytes,
                total: self.total_bytes,
            });
        }
        self.state = TransferState::Completed;
        Ok(())
    }

    pub fn cancel(&mut self) -> Result<(), TransferError> {
        if self.state.is_terminal() {
            return Err(TransferError::InvalidState {
                state: self.state,
                action: "cancel",
            });
        }
        self.state = TransferState::Cancelled;
        Ok(())
    }

    pub fn fail(&mut self) -> Result<(), TransferError> {
        if self.state.is_terminal() {
            return Err(TransferError::InvalidState {
                state: self.state,
                action: "fail",
            });
        }
        self.state = TransferState::Failed;
        Ok(())
    }

    pub fn progress_percent(&self) -> f64 {
        if self.total_bytes == 0 {
            return 100.0;
        }
        (self.received_bytes as f64 / self.total_bytes as f64 * 100.0).min(100.0)
    }

    fn require(&self, expected: TransferState, action: &'static str) -> Result<(), TransferError> {
        if self.state != expected {
            return Err(TransferError::InvalidState {
                state: self.state,
                action,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransferError {
    #[error("action `{action}` is not allowed in state {state:?}")]
    InvalidState {
        state: TransferState,
        action: &'static str,
    },
    #[error("received {received} bytes exceeds total {total}")]
    ProgressExceedsTotal { received: u64, total: u64 },
    #[error("cannot complete: received {received} of {total} bytes")]
    Incomplete { received: u64, total: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_pauses_instead_of_cancelling() {
        let mut task = TransferTask::new("t1", "assets", 1000);
        task.start().unwrap();
        task.progress(400).unwrap();
        task.pause().unwrap();
        assert_eq!(task.state, TransferState::Paused);
        assert_eq!(task.received_bytes, 400);
        task.resume().unwrap();
        task.progress(1000).unwrap();
        task.complete().unwrap();
        assert_eq!(task.state, TransferState::Completed);
    }

    #[test]
    fn progress_cannot_exceed_the_manifest_total() {
        let mut task = TransferTask::new("t1", "assets", 1000);
        task.start().unwrap();
        assert!(matches!(
            task.progress(1001),
            Err(TransferError::ProgressExceedsTotal { .. })
        ));
        assert_eq!(task.received_bytes, 0);
    }

    #[test]
    fn completion_requires_every_byte() {
        let mut task = TransferTask::new("t1", "assets", 1000);
        task.start().unwrap();
        task.progress(999).unwrap();
        assert!(matches!(
            task.complete(),
            Err(TransferError::Incomplete { .. })
        ));
        task.progress(1000).unwrap();
        task.complete().unwrap();
    }

    #[test]
    fn empty_tree_completes_immediately() {
        let mut task = TransferTask::new("t1", "empty-dir", 0);
        task.start().unwrap();
        task.complete().unwrap();
        assert_eq!(task.progress_percent(), 100.0);
    }

    #[test]
    fn invalid_transitions_are_rejected() {
        let mut task = TransferTask::new("t1", "assets", 100);
        assert!(task.pause().is_err());
        assert!(task.resume().is_err());
        task.start().unwrap();
        assert!(task.start().is_err());
        task.pause().unwrap();
        assert!(task.pause().is_err());
        task.cancel().unwrap();
        assert!(task.resume().is_err());
        assert!(task.cancel().is_err());
    }

    #[test]
    fn failed_tasks_cannot_resume() {
        let mut task = TransferTask::new("t1", "assets", 100);
        task.start().unwrap();
        task.fail().unwrap();
        assert!(task.resume().is_err());
        assert_eq!(task.state, TransferState::Failed);
    }
}
