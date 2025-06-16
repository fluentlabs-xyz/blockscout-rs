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

impl From<git2::Error> for VerificationError {
    fn from(err: git2::Error) -> Self {
        match err.code() {
            git2::ErrorCode::NotFound => {
                VerificationError::Source(format!("Repository or reference not found: {}", err))
            }
            git2::ErrorCode::Auth => {
                VerificationError::Source(format!("Authentication failed: {}", err))
            }
            _ => VerificationError::Source(format!("Git error: {}", err)),
        }
    }
}

impl From<anyhow::Error> for VerificationError {
    fn from(err: anyhow::Error) -> Self {
        VerificationError::VerificationFailed(err.to_string())
    }
}
