use thiserror::Error;

#[derive(Debug, Error)]
pub enum VerificationError {
    #[error("Source preparation failed: {0}")]
    Source(String),

    #[error("Docker operation failed: {0}")]
    Docker(String),

    #[error("Invalid request: {0}")]
    InvalidRequest(String),

    #[error("Verification failed: {0}")]
    VerificationFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parsing error: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<bollard::errors::Error> for VerificationError {
    fn from(err: bollard::errors::Error) -> Self {
        VerificationError::Docker(err.to_string())
    }
}

impl From<gix::head::peel::Error> for VerificationError {
    fn from(err: gix::head::peel::Error) -> Self {
        Self::VerificationFailed(err.to_string())
    }
}
impl From<gix::reference::find::existing::Error> for VerificationError {
    fn from(err: gix::reference::find::existing::Error) -> Self {
        Self::VerificationFailed(err.to_string())
    }
}
impl From<gix::clone::checkout::main_worktree::Error> for VerificationError {
    fn from(err: gix::clone::checkout::main_worktree::Error) -> Self {
        Self::VerificationFailed(err.to_string())
    }
}
impl From<gix::clone::fetch::Error> for VerificationError {
    fn from(err: gix::clone::fetch::Error) -> Self {
        Self::VerificationFailed(err.to_string())
    }
}
impl From<gix::clone::Error> for VerificationError {
    fn from(err: gix::clone::Error) -> Self {
        Self::VerificationFailed(err.to_string())
    }
}
impl From<gix::Error> for VerificationError {
    fn from(err: gix::Error) -> Self {
        Self::VerificationFailed(err.to_string())
    }
}

impl From<anyhow::Error> for VerificationError {
    fn from(err: anyhow::Error) -> Self {
        VerificationError::VerificationFailed(err.to_string())
    }
}
