use semver::Version;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VerificationError {
    #[error("Bytecode mismatch: expected {expected}, got {actual}")]
    BytecodeMismatch { expected: String, actual: String },

    #[error("Source code error: {0}")]
    SourceError(#[from] SourceError),

    #[error("Compilation failed: {0}")]
    CompilationError(String),

    #[error("Version error: {0}")]
    VersionError(#[from] VersionError),

    #[error("Docker error: {0}")]
    DockerError(#[from] anyhow::Error),

    #[error("Timeout: compilation exceeded {0} seconds")]
    Timeout(u64),

    #[error("Invalid project structure: {0}")]
    InvalidProject(String),
}

// Implement From<bollard::errors::Error> for VerificationError
impl From<bollard::errors::Error> for VerificationError {
    fn from(err: bollard::errors::Error) -> Self {
        // Convert bollard error to anyhow error, which will then convert to VerificationError
        VerificationError::DockerError(anyhow::anyhow!("Docker error: {}", err))
    }
}

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("Git repository error: {0}")]
    GitError(String),

    #[error("Archive extraction failed: {0}")]
    ArchiveError(String),

    #[error("Repository not found: {0}")]
    RepositoryNotFound(String),

    #[error("Commit not found: {0}")]
    CommitNotFound(String),

    #[error("Invalid archive format")]
    InvalidArchiveFormat,

    #[error("Invalid project structure: {0}")]
    InvalidProject(String),
}

#[derive(Debug, Error)]
pub enum VersionError {
    #[error("Invalid rustc version in rust-toolchain.toml: {0}")]
    InvalidToolchainVersion(String),

    #[error("Unsupported rustc version: {version} (reason: {reason})")]
    UnsupportedRustcVersion { version: Version, reason: String },

    #[error("rust-toolchain.toml parsing error: {0}")]
    ToolchainParseError(String),
}
