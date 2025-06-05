//! Fluent contract verification logic
//!
//! This crate provides functionality to verify Fluent WASM smart contracts
//! by compiling source code and comparing against deployed bytecode.
//!
//! # Features
//! - Dual source input: Git repositories or archive files
//! - Flexible version management with priority-based resolution
//! - Docker-based reproducible compilation
//! - Comprehensive artifact collection

mod docker_runner;
mod error;
pub mod source;
pub mod types;
mod verifier;
mod version;

// Re-export public types
pub use error::{SourceError, VerificationError, VersionError};
pub use types::{
    ArchiveFormat, BuildProfile, CompileSettings, ResolvedVersions, RustcVersionSource, SourceCode,
    VerificationSettings, VerificationSuccess, VerifyRequest,
};

// Main public API
pub use verifier::verify_contract;

// Helper function to connect to Docker
pub async fn connect_docker(docker_url: &str) -> Result<bollard::Docker, VerificationError> {
    use url::Url;

    let url = Url::parse(docker_url).map_err(|e| {
        VerificationError::DockerError(anyhow::anyhow!("Invalid Docker URL: {}", e))
    })?;

    let docker = match url.scheme() {
        "unix" => bollard::Docker::connect_with_local(
            docker_url,
            120, // timeout
            bollard::API_DEFAULT_VERSION,
        ),
        "http" | "tcp" => {
            bollard::Docker::connect_with_http(docker_url, 120, bollard::API_DEFAULT_VERSION)
        }
        scheme => {
            return Err(VerificationError::DockerError(anyhow::anyhow!(
                "Unsupported Docker URL scheme: {}",
                scheme
            )))
        }
    }?;

    // Test connection
    docker.ping().await.map_err(|e| {
        VerificationError::DockerError(anyhow::anyhow!("Docker connection test failed: {}", e))
    })?;

    Ok(docker)
}
