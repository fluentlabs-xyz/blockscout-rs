//! Docker orchestration for reproducible smart contract verification
//! 
//! This module provides functionality to:
//! - Build versioned Docker images with specific Rust and SDK versions
//! - Run contract verification in isolated containers
//! - Parse and return verification results

use crate::error::VerificationError;
use bollard::{
    container::{self, AttachContainerOptions, CreateContainerOptions, LogOutput, UploadToContainerOptions},
    image::{BuildImageOptions, BuilderVersion},
    models::HostConfig,
    Docker,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{io::Write, path::Path, str};
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

// Constants
const WORKDIR: &str = "/workspace";
const FLUENT_BUILDER_REPO: &str = "https://github.com/fluentlabs-xyz/fluent-builder.git";
const BASE_IMAGE: &str = "rust:latest";
const MEMORY_LIMIT: i64 = 4 * 1024 * 1024 * 1024; // 4GB
const DEFAULT_TIMEOUT: u64 = 120; // 2 minutes

/// CLI output structure matching fluent-builder's JSON format
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "status")]
pub enum CliOutput {
    #[serde(rename = "success")]
    Success {
        command: String,
        verified: bool,
        contract_name: String,
        expected_hash: String,
        actual_hash: String,
        #[serde(default)]
        abi: Option<serde_json::Value>,
        compiler_version: String,
        sdk_version: String,
    },
    #[serde(rename = "error")]
    Error {
        error_type: String,
        message: String,
    },
}

/// Main entry point for Docker-based verification
pub async fn run_verification(
    docker: &Docker,
    source_dir: &Path,
    contract_address: &str,
    chain_id: &str,
    rpc_endpoint: &str,
    rustc_version: &str,
    sdk_version: &str,
    profile: &str,
    features: &[String],
    no_default_features: bool,
) -> Result<CliOutput, VerificationError> {
    info!(
        "Starting verification for contract {} on chain {}",
        contract_address, chain_id
    );

    // Ensure Docker image exists
    let image_name = format_image_name(sdk_version, rustc_version);
    
    create_image(docker, &image_name, rustc_version, sdk_version)
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to create image: {}", e)))?;

    // Build verification command
    let command = build_verify_command(
        contract_address,
        chain_id,
        rpc_endpoint,
        profile,
        features,
        no_default_features,
    );

    // Create container
    let container_id = create_container(docker, &image_name, &command)
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to create container: {}", e)))?;

    // Copy source directory to container
    copy_directory_to_container(docker, &container_id, source_dir)
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to copy source: {}", e)))?;

    // Run container and get output
    let output = run_container(docker, &container_id)
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to run container: {}", e)))?;

    // Parse and return results
    parse_cli_output(&output)
}

/// Format Docker image name based on SDK and Rust versions
fn format_image_name(sdk_version: &str, rust_version: &str) -> String {
    // Clean version strings for Docker tag compatibility
    let sdk_tag = sdk_version
        .trim_start_matches('v')
        .replace(['/', ':', '\\'], "-");
    
    let rust_tag = rust_version
        .trim_start_matches("rustc ")
        .split_whitespace()
        .next()
        .unwrap_or("unknown")
        .replace('.', "-");

    format!("fluent-builder:{}-rust-{}", sdk_tag, rust_tag)
}

/// Check if Docker image exists locally
async fn image_exists(docker: &Docker, name: &str) -> Result<bool, VerificationError> {
    match docker.inspect_image(name).await {
        Ok(_) => Ok(true),
        Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => {
            Ok(false)
        }
        Err(e) => Err(VerificationError::Docker(format!(
            "Failed to inspect image: {}",
            e
        ))),
    }
}

/// Create Docker image if it doesn't exist
async fn create_image(
    docker: &Docker,
    image_name: &str,
    rust_version: &str,
    sdk_version: &str,
) -> Result<(), VerificationError> {
    if image_exists(docker, image_name).await? {
        info!("Using existing Docker image: {}", image_name);
        return Ok(());
    }

    info!(
        "Building Docker image {} (Rust: {}, SDK: {})",
        image_name, rust_version, sdk_version
    );

    // Format Rust toolchain version
    let toolchain = format_rust_toolchain(rust_version);
    
    // Determine SDK checkout command
    let checkout_cmd = format_sdk_checkout(sdk_version);

    let dockerfile = format!(
        r#"FROM {BASE_IMAGE}

# Install specific Rust toolchain
RUN rustup toolchain install {toolchain} && \
    rustup default {toolchain} && \
    rustup target add wasm32-unknown-unknown --toolchain {toolchain} && \
    rustup component add rust-src --toolchain {toolchain}

# Install build dependencies
RUN apt-get update && apt-get install -y git && rm -rf /var/lib/apt/lists/*

# Clone and build fluent-builder at specific version
RUN git clone {FLUENT_BUILDER_REPO} /tmp/fluent-builder && \
    cd /tmp/fluent-builder && \
    {checkout_cmd} && \
    cargo build --release --manifest-path crates/cli/Cargo.toml && \
    mv target/release/fluent-builder /usr/local/bin/fluent-builder && \
    rm -rf /tmp/fluent-builder

# Set working directory
WORKDIR {WORKDIR}

# Mark as fluent-builder Docker image
ENV FLUENT_BUILDER_DOCKER=1

# Verify installation
RUN fluent-builder --version
"#
    );

    // Create tar archive with Dockerfile
    let content = build_tar_with_dockerfile(&dockerfile)?;

    // Build image options
    let build_options = BuildImageOptions {
        t: image_name.to_string(),
        dockerfile: "Dockerfile".to_string(),
        version: BuilderVersion::BuilderV1,
        networkmode: "host".to_string(),
        pull: true,
        rm: true,
        forcerm: true,
        platform: "linux/amd64".to_string(),
        ..Default::default()
    };

    let mut stream = docker.build_image(build_options, None, Some(content.into()));

    let mut output = vec![];
    while let Some(result) = stream.next().await {
        match result {
            Ok(info) => {
                if let Some(value) = info.stream {
                    trace!(image_name = image_name, value = value, "building an image");
                    output.push(value);
                }
            }
            Err(bollard::errors::Error::DockerStreamError { error }) => {
                output.push(error);
                let output = output.join("");
                return Err(VerificationError::Docker(format!(
                    "Build error for image {}: {}",
                    image_name, output
                )));
            }
            Err(err) => {
                let output = output.join("");
                return Err(VerificationError::Docker(format!(
                    "Unknown error building image {}: {} (output: {})",
                    image_name, err, output
                )));
            }
        }
    }

    info!("Successfully built Docker image: {}", image_name);
    Ok(())
}

/// Format Rust toolchain version for rustup
fn format_rust_toolchain(rust_version: &str) -> String {
    let version = rust_version
        .trim_start_matches("rustc ")
        .split_whitespace()
        .next()
        .unwrap_or(rust_version);

    if version == "nightly" || version.starts_with("nightly-") {
        format!("{}-x86_64-unknown-linux-gnu", version)
    } else {
        format!("{}-x86_64-unknown-linux-gnu", version)
    }
}

/// Format SDK checkout command based on version format
fn format_sdk_checkout(sdk_version: &str) -> String {
    if sdk_version.len() == 40 {
        // Full commit hash
        format!("git checkout {}", sdk_version)
    } else if sdk_version.starts_with('v') {
        // Version tag with 'v' prefix
        format!("git checkout tags/{}", sdk_version)
    } else {
        // Try multiple tag formats
        format!(
            "git checkout tags/v{} || git checkout tags/{} || git checkout {}",
            sdk_version, sdk_version, sdk_version
        )
    }
}

/// Build fluent-builder verify command arguments
fn build_verify_command(
    contract_address: &str,
    chain_id: &str,
    rpc_endpoint: &str,
    profile: &str,
    features: &[String],
    no_default_features: bool,
) -> Vec<String> {
    let mut cmd = vec![
        "fluent-builder".to_string(),
        "verify".to_string(),
        ".".to_string(), // Project directory (mounted at WORKDIR)
        "--address".to_string(),
        contract_address.to_string(),
        "--chain-id".to_string(),
        chain_id.to_string(),
        "--rpc".to_string(),
        rpc_endpoint.to_string(),
        "--json".to_string(), // Always use JSON output for parsing
    ];

    // Add profile if not default
    if !profile.is_empty() && profile != "release" {
        cmd.push("--profile".to_string());
        cmd.push(profile.to_string());
    }

    // Add features if specified
    if !features.is_empty() {
        cmd.push("--features".to_string());
        // Join features with spaces as expected by the CLI
        cmd.push(features.join(" "));
    }

    // Add no-default-features flag if set
    if no_default_features {
        cmd.push("--no-default-features".to_string());
    }

    cmd
}

/// Create container
async fn create_container(
    docker: &Docker,
    image_name: &str,
    command: &Vec<std::string::String>,
) -> Result<String, VerificationError> {
    let container_suffix = Uuid::new_v4();
    let container_name = format!(
        "fluent-verify-{}",
        container_suffix
    );
    
    debug!("Creating container: {}", container_name);

    let options = CreateContainerOptions {
        name: container_name.clone(),
        ..Default::default()
    };

    let cmd_refs: Vec<&str> = command.iter().map(|s| s.as_str()).collect();

    let config = container::Config {
        image: Some(image_name),
        working_dir: Some(WORKDIR),
        host_config: Some(HostConfig {
            network_mode: Some("host".to_string()),
            auto_remove: Some(true),
            memory: Some(MEMORY_LIMIT),
            ..Default::default()
        }),
        cmd: Some(cmd_refs),
        env: Some(vec!["RUST_LOG=info"]),
        ..Default::default()
    };

    let container = docker
        .create_container(Some(options), config)
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to create container: {}", e)))?;

    Ok(container.id)
}

/// Copy source directory to container
async fn copy_directory_to_container(
    docker: &Docker,
    container_id: &str,
    dir: &Path,
) -> Result<(), VerificationError> {
    debug!("Copying source from: {:?}", dir);

    let tar = build_tar_from_directory(dir)
        .map_err(|e| VerificationError::Docker(format!("Failed to build tar: {}", e)))?;

    let options = UploadToContainerOptions {
        path: WORKDIR,
        no_overwrite_dir_non_dir: "",
    };

    docker
        .upload_to_container(container_id, Some(options), tar.into())
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to upload to container: {}", e)))?;

    Ok(())
}

/// Run container and collect output
async fn run_container(docker: &Docker, container_id: &str) -> Result<String, VerificationError> {
    // Start container
    docker
        .start_container::<String>(container_id, None)
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to start container: {}", e)))?;

    // Attach to container to get output
    let mut attach_results = docker
        .attach_container::<String>(
            container_id,
            Some(AttachContainerOptions {
                stdout: Some(true),
                stderr: Some(true),
                stream: Some(true),
                logs: Some(true),
                ..Default::default()
            }),
        )
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to attach to container: {}", e)))?;

    let mut stdout_output = vec![];
    let mut stderr_output = vec![];

    while let Some(result) = attach_results.output.next().await {
        match result {
            Ok(output) => match output {
                LogOutput::StdOut { message } => stdout_output.push(message),
                LogOutput::StdErr { message } => stderr_output.push(message),
                _ => (),
            },
            Err(err) => {
                return Err(VerificationError::Docker(format!(
                    "Error reading container output: {}",
                    err
                )));
            }
        }
    }

    // Convert output to string
    let output = stdout_output
        .into_iter()
        .filter_map(|bytes| {
            match str::from_utf8(&bytes) {
                Ok(s) => Some(s.to_string()),
                Err(err) => {
                    warn!("Failed to convert output to UTF-8: {}", err);
                    None
                }
            }
        })
        .collect::<Vec<_>>()
        .join("");

    // Log stderr for debugging
   if !stderr_output.is_empty() {
    let stderr = stderr_output
        .into_iter()
        .filter_map(|bytes| String::from_utf8(bytes.to_vec()).ok())
        .collect::<Vec<_>>()
        .join("");
    debug!("Container stderr: {}", stderr);
}

    Ok(output)
}

/// Parse CLI JSON output
fn parse_cli_output(output: &str) -> Result<CliOutput, VerificationError> {
    // Find JSON in output (in case there's other text)
    let json_start = output.find('{');
    let json_end = output.rfind('}');
    
    if let (Some(start), Some(end)) = (json_start, json_end) {
        let json_str = &output[start..=end];
        serde_json::from_str(json_str)
            .map_err(|e| VerificationError::Json(e))
    } else {
        Err(VerificationError::Docker(format!(
            "No JSON output found in: {}",
            output
        )))
    }
}

/// Build tar archive with Dockerfile
fn build_tar_with_dockerfile(content: &str) -> Result<Vec<u8>, VerificationError> {
    let mut header = tar::Header::new_gnu();
    header
        .set_path("Dockerfile")
        .map_err(|e| VerificationError::Docker(format!("Failed to set path: {}", e)))?;
    header.set_size(content.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    
    let mut tar = tar::Builder::new(Vec::new());
    tar.append(&header, content.as_bytes())
        .map_err(|e| VerificationError::Docker(format!("Failed to append: {}", e)))?;

    let uncompressed = tar
        .into_inner()
        .map_err(|e| VerificationError::Docker(format!("Failed to finalize tar: {}", e)))?;
    
    compress_archive(&uncompressed)
}

/// Build tar archive from directory
fn build_tar_from_directory(dir: &Path) -> Result<Vec<u8>, VerificationError> {
    let mut tar = tar::Builder::new(Vec::new());
    tar.append_dir_all("", dir)
        .map_err(|e| VerificationError::Docker(format!("Failed to append directory: {}", e)))?;
    
    let uncompressed = tar
        .into_inner()
        .map_err(|e| VerificationError::Docker(format!("Failed to finalize tar: {}", e)))?;
    
    compress_archive(&uncompressed)
}

/// Compress archive data
fn compress_archive(uncompressed: &[u8]) -> Result<Vec<u8>, VerificationError> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(uncompressed)
        .map_err(|e| VerificationError::Docker(format!("Failed to compress: {}", e)))?;
    encoder
        .finish()
        .map_err(|e| VerificationError::Docker(format!("Failed to finish compression: {}", e)))
}

/// Clean up old Docker images keeping only the most recent ones
pub async fn cleanup_old_images(
    docker: &Docker,
    keep_recent: usize,
) -> Result<(), VerificationError> {
    use bollard::image::ListImagesOptions;
    use std::collections::HashMap;
    
    let mut filters = HashMap::new();
    filters.insert("reference", vec!["fluent-builder:*"]);
    
    let options = ListImagesOptions {
        filters,
        ..Default::default()
    };

    let images = docker
        .list_images(Some(options))
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to list images: {}", e)))?;

    if images.len() <= keep_recent {
        return Ok(());
    }

   // Sort by creation date (newest first)
    let mut image_list: Vec<_> = images
        .into_iter()
        .filter_map(|img| {
            img.repo_tags
                .first()
                .map(|tag| (tag.clone(), img.created))
        })
        .collect();

    image_list.sort_by(|a, b| b.1.cmp(&a.1));

    // Remove oldest images
    for (tag, _) in image_list.into_iter().skip(keep_recent) {
        info!("Removing old Docker image: {}", tag);
        
        if let Err(e) = docker.remove_image(&tag, None, None).await {
            warn!("Failed to remove image {}: {}", tag, e);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_image_name() {
        assert_eq!(
            format_image_name("v0.1.0", "1.75.0"),
            "fluent-builder:0-1-0-rust-1-75-0"
        );
        
        assert_eq!(
            format_image_name("v0.2.0-beta", "nightly-2024-01-01"),
            "fluent-builder:0-2-0-beta-rust-nightly-2024-01-01"
        );
        
        assert_eq!(
            format_image_name("abc123def456", "rustc 1.80.0 (051478957 2024-07-21)"),
            "fluent-builder:abc123def456-rust-1-80-0"
        );
    }

    #[test]
    fn test_format_rust_toolchain() {
        assert_eq!(
            format_rust_toolchain("1.75.0"),
            "1.75.0-x86_64-unknown-linux-gnu"
        );
        
        assert_eq!(
            format_rust_toolchain("rustc 1.80.0 (051478957 2024-07-21)"),
            "1.80.0-x86_64-unknown-linux-gnu"
        );
        
        assert_eq!(
            format_rust_toolchain("nightly-2024-01-01"),
            "nightly-2024-01-01-x86_64-unknown-linux-gnu"
        );
    }

    #[test]
    fn test_format_sdk_checkout() {
        // Full commit hash
        assert_eq!(
            format_sdk_checkout("abc123def456789012345678901234567890abcd"),
            "git checkout abc123def456789012345678901234567890abcd"
        );
        
        // Version with 'v' prefix
        assert_eq!(
            format_sdk_checkout("v0.1.0"),
            "git checkout tags/v0.1.0"
        );
        
        // Version without prefix
        assert_eq!(
            format_sdk_checkout("0.1.0"),
            "git checkout tags/v0.1.0 || git checkout tags/0.1.0 || git checkout 0.1.0"
        );
    }
}