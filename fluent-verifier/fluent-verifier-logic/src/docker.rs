//! Docker orchestration for smart contract compilation

use crate::error::VerificationError;
use bollard::{
    container::{
        self, AttachContainerOptions, CreateContainerOptions, DownloadFromContainerOptions,
        LogOutput, UploadToContainerOptions,
    },
    image::CreateImageOptions,
    models::HostConfig,
    Docker,
};
use futures_util::StreamExt;
use std::{io::Write, path::Path};
use tracing::{debug, info, trace};
use uuid::Uuid;

// Constants
pub const BASE_IMAGE_NAME: &str = "ghcr.io/fluentlabs-xyz/fluentbase-build";
const WORKDIR: &str = "/workspace";
const MEMORY_LIMIT: i64 = 4 * 1024 * 1024 * 1024; // 4GB

pub const DEFAULT_RUST_FLAGS: &str =
    "-Clink-arg=-zstack-size=131072 -Cpanic=abort -Ctarget-feature=+bulk-memory";

/// Cargo build configuration
#[derive(Debug, Clone)]
pub struct CargoBuildConfig {
    pub profile: BuildProfile,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub rustflags: String,
}

/// Build profile
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum BuildProfile {
    Release,
    Debug,
}

impl BuildProfile {
    pub fn as_str(&self) -> &str {
        match self {
            BuildProfile::Release => "release",
            BuildProfile::Debug => "debug",
        }
    }
}

impl CargoBuildConfig {
    pub fn to_command_args(&self) -> Vec<String> {
        let mut args = vec![
            "cargo".to_string(),
            "build".to_string(),
            "--target".to_string(),
            "wasm32-unknown-unknown".to_string(),
            "--locked".to_string(),
        ];

        match self.profile {
            BuildProfile::Release => args.push("--release".to_string()),
            BuildProfile::Debug => {}
        }

        if !self.features.is_empty() {
            args.push("--features".to_string());
            args.push(self.features.join(","));
        }

        if self.no_default_features {
            args.push("--no-default-features".to_string());
        }

        args
    }
    pub fn env_vars(&self) -> Vec<String> {
        let mut env = vec!["RUST_LOG=info".to_string()];
        env.push(format!("RUSTFLAGS={}", self.rustflags));

        env
    }

    pub fn target_path(&self) -> String {
        format!(
            "{}/target/wasm32-unknown-unknown/{}",
            WORKDIR,
            self.profile.as_str()
        )
    }
}

/// Build output
#[derive(Debug)]
pub struct BuildOutput {
    pub wasm_bytes: Vec<u8>,
}

/// Main entry point for Docker-based compilation
pub async fn run_build(
    docker: &Docker,
    docker_image: &str,
    source_dir: &Path,
    build_config: &CargoBuildConfig,
    container_network: &Option<String>,
) -> Result<BuildOutput, VerificationError> {
    info!("Starting build with image: {}", docker_image);

    // Pull image if needed
    pull_image_if_needed(docker, docker_image).await?;

    // Create container
    let container_id =
        create_container(docker, docker_image, build_config, container_network).await?;

    // Copy source
    copy_directory_to_container(docker, &container_id, source_dir).await?;

    // Run build
    run_container(docker, &container_id).await?;

    // Extract WASM
    let target_path = build_config.target_path();
    let wasm_bytes = copy_wasm_from_container(docker, &container_id, &target_path).await?;

    docker
        .remove_container(
            &container_id,
            Some(bollard::container::RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to remove container: {e}")))?;

    info!("Build completed, WASM size: {} bytes", wasm_bytes.len());

    Ok(BuildOutput { wasm_bytes })
}

async fn pull_image_if_needed(docker: &Docker, image_name: &str) -> Result<(), VerificationError> {
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
                    "Failed to pull image {image_name}: {e}"
                )));
            }
        }
    }

    info!("Successfully pulled Docker image: {}", image_name);
    Ok(())
}

async fn create_container(
    docker: &Docker,
    image_name: &str,
    build_config: &CargoBuildConfig,
    container_network: &Option<String>,
) -> Result<String, VerificationError> {
    let container_suffix = Uuid::new_v4();
    let container_name = format!("fluent-build-{container_suffix}");

    debug!("Creating container: {}", container_name);
    info!("Container will remain after completion for debugging");
    info!("Connect with: docker exec -it {} /bin/bash", container_name);

    let options = CreateContainerOptions {
        name: container_name,
        platform: Some("linux/amd64".to_string()),
    };

    let command = build_config.to_command_args();
    let cmd_refs: Vec<&str> = command.iter().map(|s| s.as_str()).collect();

    let env_vars = build_config.env_vars();
    let env_refs: Vec<&str> = env_vars.iter().map(|s| s.as_str()).collect();

    let network_mode = container_network.as_deref().unwrap_or("bridge").to_string();

    let config = container::Config {
        image: Some(image_name),
        working_dir: Some(WORKDIR),
        host_config: Some(HostConfig {
            network_mode: Some(network_mode),
            auto_remove: Some(false),
            memory: Some(MEMORY_LIMIT),
            ..Default::default()
        }),
        cmd: Some(cmd_refs),
        env: Some(env_refs),
        ..Default::default()
    };

    let container = docker
        .create_container(Some(options), config)
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to create container: {e}")))?;

    Ok(container.id)
}

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

async fn run_container(docker: &Docker, container_id: &str) -> Result<(), VerificationError> {
    info!("Starting container: {}", container_id);

    docker
        .start_container::<String>(container_id, None)
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to start container: {e}")))?;

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

    let mut stderr_output = vec![];

    while let Some(result) = attach_results.output.next().await {
        match result {
            Ok(LogOutput::StdErr { message }) => stderr_output.push(message),
            Err(err) => {
                return Err(VerificationError::Docker(format!(
                    "Error reading container output: {err}"
                )));
            }
            _ => {}
        }
    }

    if !stderr_output.is_empty() {
        let stderr = stderr_output
            .into_iter()
            .filter_map(|bytes| String::from_utf8(bytes.to_vec()).ok())
            .collect::<Vec<_>>()
            .join("");

        debug!("Container stderr: {}", stderr);

        if stderr.contains("error:") || stderr.contains("Error") {
            return Err(VerificationError::Docker(format!("Build failed: {stderr}")));
        }
    }

    Ok(())
}
async fn copy_wasm_from_container(
    docker: &Docker,
    container_id: &str,
    target_path: &str,
) -> Result<Vec<u8>, VerificationError> {
    info!("Copying WASM from container at {}", target_path);

    let options = DownloadFromContainerOptions {
        path: target_path.to_string(),
    };

    let mut stream = docker.download_from_container(container_id, Some(options));
    let mut tar_data = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            VerificationError::Docker(format!("Failed to download from container: {e}"))
        })?;
        tar_data.extend_from_slice(&chunk);
    }

    let mut archive = tar::Archive::new(std::io::Cursor::new(tar_data));

    for entry in archive
        .entries()
        .map_err(|e| VerificationError::Docker(format!("Failed to read tar entries: {e}")))?
    {
        let mut entry = entry
            .map_err(|e| VerificationError::Docker(format!("Failed to read tar entry: {e}")))?;

        let is_wasm = {
            let path = entry
                .path()
                .map_err(|e| VerificationError::Docker(format!("Failed to get entry path: {e}")))?;

            path.file_name()
                .and_then(|n| n.to_str())
                .map(|filename| filename.ends_with(".wasm") && !filename.contains(".metadata"))
                .unwrap_or(false)
        };

        if is_wasm {
            let mut wasm_bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut wasm_bytes)
                .map_err(|e| VerificationError::Docker(format!("Failed to read WASM file: {e}")))?;

            info!("Found WASM file ({} bytes)", wasm_bytes.len());
            return Ok(wasm_bytes);
        }
    }

    Err(VerificationError::Docker(
        "No WASM file found in build output".to_string(),
    ))
}

fn build_tar_from_directory(dir: &Path) -> Result<Vec<u8>, VerificationError> {
    let mut tar = tar::Builder::new(Vec::new());
    tar.append_dir_all("", dir)
        .map_err(|e| VerificationError::Docker(format!("Failed to append directory: {e}")))?;

    let uncompressed = tar
        .into_inner()
        .map_err(|e| VerificationError::Docker(format!("Failed to finalize tar: {e}")))?;

    compress_archive(&uncompressed)
}

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
    fn test_cargo_build_config() {
        let config = CargoBuildConfig {
            profile: BuildProfile::Release,
            features: vec!["mainnet".to_string()],
            no_default_features: false,
            rustflags: DEFAULT_RUST_FLAGS.to_string(),
        };

        let cmd = config.to_command_args();

        assert!(cmd.contains(&"cargo".to_string()));
        assert!(cmd.contains(&"--release".to_string()));
        assert!(cmd.contains(&"--features".to_string()));
        assert!(cmd.contains(&"mainnet".to_string()));
    }
}
