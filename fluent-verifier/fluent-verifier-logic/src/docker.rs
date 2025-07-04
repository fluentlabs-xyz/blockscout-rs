//! Docker orchestration for smart contract verification using pre-built images
//!
//! This module provides functionality to:
//! - Pull versioned Docker images from ghcr.io
//! - Run contract verification in isolated containers
//! - Parse and return verification results

use crate::error::VerificationError;
use bollard::{
    container::{
        self, AttachContainerOptions, CreateContainerOptions, LogOutput, UploadToContainerOptions,
    },
    image::CreateImageOptions,
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
const BASE_IMAGE_NAME: &str = "ghcr.io/fluentlabs-xyz/fluentbase-build";
const MEMORY_LIMIT: i64 = 4 * 1024 * 1024 * 1024; // 4GB
                                                  // const DEFAULT_TIMEOUT: u64 = 120; // 2 minutes - Reserved for future use

/// CLI output structure matching fluentbase's JSON format
#[derive(Debug, Deserialize, Serialize)]
pub struct CliOutput {
    pub verified: bool,
    pub expected_hash: String,
    pub actual_hash: String,
    pub rustc_version: String,
    pub sdk_version: String,
    pub build_platform: String,
}

/// Main entry point for Docker-based verification
#[allow(clippy::too_many_arguments)]
pub async fn run_verification(
    docker: &Docker,
    source_dir: &Path,
    contract_address: &str,
    chain_id: &str,
    rpc_endpoint: &str,
    sdk_version: &str,
    features: &[String],
    no_default_features: bool,
) -> Result<CliOutput, VerificationError> {
    info!(
        "Starting verification for contract {} on chain {} with SDK version {}",
        contract_address, chain_id, sdk_version
    );

    // Format image name with SDK version tag
    let image_name = format!("{BASE_IMAGE_NAME}:{sdk_version}");

    // Pull image if needed
    pull_image_if_needed(docker, &image_name).await?;

    // Build verification command
    let command = build_verify_command(
        contract_address,
        chain_id,
        rpc_endpoint,
        features,
        no_default_features,
    );

    // Create container
    let container_id = create_container(docker, &image_name, &command).await?;

    // Copy source directory to container
    copy_directory_to_container(docker, &container_id, source_dir).await?;

    // Run container and get output
    let output = run_container(docker, &container_id).await?;

    // Parse and return results
    parse_cli_output(&output)
}

/// Pull Docker image if it doesn't exist locally
async fn pull_image_if_needed(docker: &Docker, image_name: &str) -> Result<(), VerificationError> {
    // Check if image exists locally
    match docker.inspect_image(image_name).await {
        Ok(_) => {
            info!("Using existing Docker image: {}", image_name);
            return Ok(());
        }
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => {
            info!(
                "Image {} not found locally, pulling from registry",
                image_name
            );
        }
        Err(e) => {
            return Err(VerificationError::Docker(format!(
                "Failed to inspect image: {e}"
            )));
        }
    }

    // Pull the image
    let options = CreateImageOptions {
        from_image: image_name,
        ..Default::default()
    };

    let mut stream = docker.create_image(Some(options), None, None);

    while let Some(result) = stream.next().await {
        match result {
            Ok(info) => {
                if let Some(status) = info.status {
                    trace!("Pull status: {}", status);
                }
                if let Some(error) = info.error {
                    return Err(VerificationError::Docker(format!(
                        "Error pulling image {image_name}: {error}"
                    )));
                }
            }
            Err(e) => {
                return Err(VerificationError::Docker(format!(
                    "Failed to pull image {image_name}: {e}. Make sure the SDK version exists in the registry."
                )));
            }
        }
    }

    info!("Successfully pulled Docker image: {}", image_name);
    Ok(())
}

/// Build fluentbase verify command arguments
fn build_verify_command(
    contract_address: &str,
    chain_id: &str,
    rpc_endpoint: &str,
    features: &[String],
    no_default_features: bool,
) -> Vec<String> {
    let mut cmd = vec![
        "fluentbase".to_string(),
        "verify".to_string(),
        ".".to_string(), // Project directory (mounted at WORKDIR)
        "--address".to_string(),
        contract_address.to_string(),
        "--rpc".to_string(),
        rpc_endpoint.to_string(),
        "--chain-id".to_string(),
        chain_id.to_string(),
        "--no-docker".to_string(), // IMPORTANT: Disable Docker since we're already in a container
    ];

    // Add features if specified
    if !features.is_empty() {
        cmd.push("--features".to_string());
        cmd.push(features.join(","));
    }

    // Add no-default-features flag if set
    if no_default_features {
        cmd.push("--no-default-features".to_string());
    }

    // Log the command for debugging
    info!("Verification command: {:?}", cmd);

    cmd
}

/// Create container
async fn create_container(
    docker: &Docker,
    image_name: &str,
    command: &[String],
) -> Result<String, VerificationError> {
    let container_suffix = Uuid::new_v4();
    let container_name = format!("fluent-verify-{container_suffix}");

    debug!("Creating container: {}", container_name);

    let options = CreateContainerOptions {
        name: container_name.clone(),
        platform: Some("linux/amd64".to_string()),
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
        .map_err(|e| VerificationError::Docker(format!("Failed to create container: {e}")))?;

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
        .map_err(|e| VerificationError::Docker(format!("Failed to build tar: {e}")))?;

    let options = UploadToContainerOptions {
        path: WORKDIR,
        no_overwrite_dir_non_dir: "",
    };

    docker
        .upload_to_container(container_id, Some(options), tar.into())
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to upload to container: {e}")))?;

    Ok(())
}

/// Run container and collect output
async fn run_container(docker: &Docker, container_id: &str) -> Result<String, VerificationError> {
    info!("Starting container: {}", container_id);

    // Start container
    docker
        .start_container::<String>(container_id, None)
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to start container: {e}")))?;

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
        .map_err(|e| VerificationError::Docker(format!("Failed to attach to container: {e}")))?;

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
                    "Error reading container output: {err}"
                )));
            }
        }
    }

    // Convert output to string
    let output = stdout_output
        .into_iter()
        .filter_map(|bytes| match str::from_utf8(&bytes) {
            Ok(s) => Some(s.to_string()),
            Err(err) => {
                warn!("Failed to convert output to UTF-8: {}", err);
                None
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
        info!("Container stderr output: {}", stderr);
    }

    info!("Container stdout length: {} bytes", output.len());
    if output.len() < 1000 {
        debug!("Container stdout: {}", output);
    } else {
        debug!("Container stdout (first 1000 chars): {}", &output[..1000]);
    }

    Ok(output)
}

/// Parse CLI JSON output from the verify command
fn parse_cli_output(output: &str) -> Result<CliOutput, VerificationError> {
    // Log the raw output for debugging
    debug!("Raw container output: {}", output);

    // The CLI outputs JSON when verification completes
    // Try to parse the entire output as JSON first
    match serde_json::from_str::<CliOutput>(output) {
        Ok(result) => Ok(result),
        Err(e) => {
            debug!("Failed to parse output as JSON directly: {}", e);

            // If that fails, try to find JSON in the output
            let json_start = output.find('{');
            let json_end = output.rfind('}');

            if let (Some(start), Some(end)) = (json_start, json_end) {
                let json_str = &output[start..=end];
                debug!("Attempting to parse JSON substring: {}", json_str);

                serde_json::from_str(json_str).map_err(|e| {
                    warn!("Failed to parse JSON substring: {}", e);
                    VerificationError::Json(e)
                })
            } else {
                // If no JSON found, the command might have failed
                // Check if output contains error indicators
                if output.contains("error") || output.contains("Error") {
                    Err(VerificationError::Docker(format!(
                        "Command failed with output: {output}"
                    )))
                } else if output.is_empty() {
                    Err(VerificationError::Docker(
                        "No output from verification command".to_string(),
                    ))
                } else {
                    Err(VerificationError::Docker(format!(
                        "No JSON output found. Raw output: {output}"
                    )))
                }
            }
        }
    }
}

/// Build tar archive from directory
fn build_tar_from_directory(dir: &Path) -> Result<Vec<u8>, VerificationError> {
    let mut tar = tar::Builder::new(Vec::new());
    tar.append_dir_all("", dir)
        .map_err(|e| VerificationError::Docker(format!("Failed to append directory: {e}")))?;

    let uncompressed = tar
        .into_inner()
        .map_err(|e| VerificationError::Docker(format!("Failed to finalize tar: {e}")))?;

    compress_archive(&uncompressed)
}

/// Compress archive data
fn compress_archive(uncompressed: &[u8]) -> Result<Vec<u8>, VerificationError> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(uncompressed)
        .map_err(|e| VerificationError::Docker(format!("Failed to compress: {e}")))?;
    encoder
        .finish()
        .map_err(|e| VerificationError::Docker(format!("Failed to finish compression: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_verify_command() {
        let cmd = build_verify_command(
            "0x1234567890123456789012345678901234567890",
            "9999",
            "https://mainnet.fluent.xyz",
            &vec!["mainnet".to_string()],
            false,
        );

        assert_eq!(cmd[0], "fluentbase");
        assert_eq!(cmd[1], "verify");
        assert_eq!(cmd[2], ".");
        assert!(cmd.contains(&"--address".to_string()));
        assert!(cmd.contains(&"--no-docker".to_string()));
        assert!(cmd.contains(&"--features".to_string()));
        assert!(cmd.contains(&"mainnet".to_string()));
    }

    #[test]
    fn test_build_verify_command_no_features() {
        let cmd = build_verify_command(
            "0x1234567890123456789012345678901234567890",
            "9999",
            "https://mainnet.fluent.xyz",
            &vec![],
            false,
        );

        assert!(!cmd.contains(&"--features".to_string()));
    }

    #[test]
    fn test_build_verify_command_no_default_features() {
        let cmd = build_verify_command(
            "0x1234567890123456789012345678901234567890",
            "9999",
            "https://mainnet.fluent.xyz",
            &vec![],
            true,
        );

        assert!(cmd.contains(&"--no-default-features".to_string()));
    }
}
