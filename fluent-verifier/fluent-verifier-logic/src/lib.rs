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

mod docker;
mod error;
mod metadata;
mod source;
mod verify;

// Constants
pub const DOCKER_BASE_IMAGE_NAME: &str = "ghcr.io/fluentlabs-xyz/fluentbase-build";
pub const DOCKER_WORKDIR: &str = "/workspace";
pub const DOCKER_MAX_MEMORY_LIMIT: i64 = 4 * 1024 * 1024 * 1024; // 4GB
pub const DEFAULT_RUST_TOOLCHAIN: &str = "1.92.0";

// Import proto types
use fluent_verifier_proto::blockscout::fluent_verifier::v1 as proto;

// Re-export what's needed
pub use error::VerificationError;
pub use verify::verify_contract;

// Re-export proto types for convenience
pub use proto::{VerificationStatus, VerifyWasmRequest, VerifyWasmResponse};

// Helper function to connect to Docker
pub async fn connect_docker(url: &str) -> Result<bollard::Docker, VerificationError> {
    use bollard::Docker;

    let docker = if url.starts_with("unix://") {
        Docker::connect_with_local(url, 120, bollard::API_DEFAULT_VERSION)
    } else {
        Docker::connect_with_http(url, 120, bollard::API_DEFAULT_VERSION)
    }?;

    docker.ping().await?;
    Ok(docker)
}
