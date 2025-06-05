use crate::types::{BuildProfile, CompileSettings, ResolvedVersions};
use anyhow::{Context, Result};
use bollard::{
    container::{Config, CreateContainerOptions, LogOutput, UploadToContainerOptions},
    exec::{CreateExecOptions, StartExecResults},
    image::{BuildImageOptions, BuilderVersion},
    models::HostConfig,
    Docker,
};
use bytes::Bytes;
use flate2::write::GzEncoder;
use flate2::Compression;
use futures_util::StreamExt;
use semver::Version;
use std::io::Write;
use std::path::Path;
use std::str;
use tar::Builder;
use uuid::Uuid;

const WORKDIR: &str = "/source";

pub struct CompilerOutput {
    pub wasm_bytecode: Bytes,
    pub rwasm_bytecode: Bytes,
    pub abi: serde_json::Value,
    pub metadata: serde_json::Value,
    pub contract_name: String,
    pub package_name: String,
    pub compile_logs: String,
}

/// Helper function to check if a Docker image exists locally.
async fn image_exists(docker: &Docker, image_name: &str) -> Result<bool> {
    match docker.inspect_image(image_name).await {
        Ok(_) => Ok(true),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(false),
        Err(e) => Err(e).context(format!("Failed to inspect docker image '{image_name}'")),
    }
}

/// Runs fluent-wasm-compiler-cli in Docker
pub async fn run_compiler(
    docker: &Docker,
    source_dir: &Path,
    compile_settings: &CompileSettings,
    resolved_versions: &ResolvedVersions,
    docker_image_prefix: &str,
    timeout_secs: u64,
) -> Result<CompilerOutput> {
    let image_name = format!(
        "{}/fluent-compiler:rust-{}",
        docker_image_prefix, resolved_versions.rustc_version
    );

    ensure_compiler_image(docker, &image_name, &resolved_versions.rustc_version).await?;

    let command = build_compiler_command(compile_settings);

    // Container will be auto-removed due to HostConfig.auto_remove = true
    let container_id = create_compiler_container(docker, &image_name, &command).await?;

    // The async block ensures that if any step fails, we proceed to the (now implicit) container removal.
    let result = async {
        copy_source_to_container(docker, &container_id, source_dir).await?;
        ensure_sdk_version(
            docker,
            &container_id,
            &compile_settings.fluentbase_sdk_version,
        )
        .await?;
        let output = run_container_with_timeout(docker, &container_id, timeout_secs).await?;
        extract_compiler_artifacts(docker, &container_id, &output).await
    }
    .await;

    result
}

async fn ensure_compiler_image(
    docker: &Docker,
    image_name: &str,
    rustc_version: &Version,
) -> Result<()> {
    if image_exists(docker, image_name).await? {
        tracing::debug!("Docker image {} already exists locally.", image_name);
        return Ok(());
    }

    tracing::info!("Building Docker image: {}", image_name);

    let dockerfile_content = format!(
        r#"
FROM rust:{rustc_version}-slim

RUN apt-get update && apt-get install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    git \
    && rm -rf /var/lib/apt/lists/*

RUN rustup target add wasm32-unknown-unknown

# Consider pinning to a specific commit/tag of fluent-wasm-compiler-cli for reproducibility
RUN cargo install --git https://github.com/fluentlabs-xyz/fluent-wasm-compiler-cli --branch main

WORKDIR {WORKDIR}
"#
    );

    let tar_body = create_dockerfile_tar(&dockerfile_content)?;

    let build_options = BuildImageOptions {
        t: image_name.to_string(),
        dockerfile: "Dockerfile".to_string(),
        pull: true,
        rm: true,
        forcerm: true,
        platform: "linux/amd64".to_string(),
        version: BuilderVersion::BuilderV1,
        ..Default::default()
    };

    let mut build_stream = docker.build_image(build_options, None, Some(tar_body.into()));

    let mut combined_output = String::new();

    while let Some(build_info_result) = build_stream.next().await {
        match build_info_result {
            Ok(build_info) => {
                if let Some(stream_msg) = &build_info.stream {
                    tracing::debug!(image_build_log = stream_msg.trim_end_matches('\n'));
                    combined_output.push_str(stream_msg);
                }
                if let Some(error_detail) = &build_info.error_detail {
                    tracing::error!(
                        "Error detail during Docker image build for {}: {}",
                        image_name,
                        error_detail.message.as_deref().unwrap_or("Unknown error")
                    );
                    return Err(anyhow::anyhow!(
                        "Build error for image {}: {} (Full log: {})",
                        image_name,
                        error_detail.message.as_deref().unwrap_or("Unknown error"),
                        combined_output
                    ));
                }
                if build_info.error.is_some() && build_info.error_detail.is_none() {
                    let err_msg = build_info
                        .error
                        .unwrap_or_else(|| "Unknown build error string".to_string());
                    tracing::error!(
                        "Error string during Docker image build for {}: {}",
                        image_name,
                        err_msg
                    );
                    return Err(anyhow::anyhow!(
                        "Build error string for image {}: {} (Full log: {})",
                        image_name,
                        err_msg,
                        combined_output
                    ));
                }
            }
            Err(e) => {
                tracing::error!(
                    "Docker stream error while building image {}: {} (Partial log: {})",
                    image_name,
                    e,
                    combined_output
                );
                return Err(e).context(format!(
                    "Docker stream error building image {image_name} (Partial log: {combined_output})",
                ));
            }
        }
    }

    if !image_exists(docker, image_name).await? {
        tracing::error!(
            "Image {} was not found after build process. Full log: {}",
            image_name,
            combined_output
        );
        return Err(anyhow::anyhow!(
            "Image {} not found after build. Full log: {}",
            image_name,
            combined_output
        ));
    }

    tracing::info!("Successfully built Docker image: {}", image_name);
    Ok(())
}

fn create_dockerfile_tar(dockerfile_content: &str) -> Result<Vec<u8>> {
    let mut tar_data = Vec::new();
    {
        let mut archive = Builder::new(&mut tar_data);
        let mut header = tar::Header::new_gnu();
        header.set_path("Dockerfile")?;
        header.set_size(dockerfile_content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append(&header, dockerfile_content.as_bytes())?;
        archive.finish()?;
    }

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&tar_data)?;
    Ok(encoder.finish()?)
}

fn build_compiler_command(settings: &CompileSettings) -> Vec<String> {
    let mut cmd = vec!["fluent-wasm-compiler".to_string()];

    match &settings.build_profile {
        BuildProfile::Release => cmd.push("--release".to_string()),
        BuildProfile::Debug => {}
        BuildProfile::Custom(profile) => {
            cmd.extend(["--profile".to_string(), profile.clone()]);
        }
    }

    if !settings.features.is_empty() {
        cmd.extend(["--features".to_string(), settings.features.join(",")]);
    }

    if let Some(contract) = &settings.contract_name {
        cmd.extend(["--contract".to_string(), contract.clone()]);
    }

    cmd.extend(["--output-format".to_string(), "json".to_string()]);

    for flag in &settings.cargo_flags {
        cmd.push(flag.clone());
    }

    cmd
}

async fn create_compiler_container(
    docker: &Docker,
    image_name: &str,
    command: &[String],
) -> Result<String> {
    let container_name = format!("fluent-compiler-{}", Uuid::new_v4().as_simple());

    let host_config = HostConfig {
        auto_remove: Some(true),
        ..Default::default()
    };

    let config = Config {
        image: Some(image_name.to_string()),
        working_dir: Some(WORKDIR.to_string()),
        cmd: Some(command.to_vec()),
        attach_stdout: Some(true),
        attach_stderr: Some(true),
        host_config: Some(host_config),
        ..Default::default()
    };

    let options = CreateContainerOptions {
        name: container_name,
        ..Default::default()
    };

    let container_response = docker.create_container(Some(options), config).await?;
    Ok(container_response.id)
}

async fn copy_source_to_container(
    docker: &Docker,
    container_id: &str,
    source_dir: &Path,
) -> Result<()> {
    let tar_data = create_source_tar(source_dir)?;

    let options = UploadToContainerOptions {
        path: WORKDIR,
        ..Default::default()
    };

    docker
        .upload_to_container(container_id, Some(options), tar_data.into())
        .await?;

    Ok(())
}

fn create_source_tar(source_dir: &Path) -> Result<Vec<u8>> {
    let mut tar_data = Vec::new();
    {
        let mut archive = Builder::new(&mut tar_data);
        archive.append_dir_all(".", source_dir)?;
        archive.finish()?;
    }

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&tar_data)?;
    Ok(encoder.finish()?)
}

async fn ensure_sdk_version(
    _docker: &Docker,
    _container_id: &str,
    sdk_version: &Version,
) -> Result<()> {
    tracing::debug!(
        "Ensuring fluentbase-sdk version (currently a log-only step): {}",
        sdk_version
    );
    Ok(())
}

async fn run_container_with_timeout(
    docker: &Docker,
    container_id: &str,
    timeout_secs: u64,
) -> Result<String> {
    docker
        .start_container::<String>(container_id, None)
        .await
        .with_context(|| format!("Failed to start container {container_id}"))?;

    let mut output_buffer = String::new();

    let attach_options = bollard::container::AttachContainerOptions::<String> {
        stream: Some(true),
        stdout: Some(true),
        stderr: Some(true),
        logs: Some(false),
        ..Default::default()
    };

    let mut attach_stream = docker
        .attach_container(container_id, Some(attach_options))
        .await
        .with_context(|| format!("Failed to attach to container {container_id}"))?;

    let processing_future = async {
        while let Some(result) = attach_stream.output.next().await {
            match result {
                Ok(log_output) => match log_output {
                    LogOutput::StdOut { message } => {
                        output_buffer.push_str(&String::from_utf8_lossy(&message));
                    }
                    LogOutput::StdErr { message } => {
                        output_buffer.push_str(&String::from_utf8_lossy(&message));
                    }
                    _ => {}
                },
                Err(e) => {
                    tracing::warn!(
                        "Error in attach_stream for container {}: {}",
                        container_id,
                        e
                    );
                    return Err(anyhow::anyhow!(
                        "Stream error while attaching to container {}: {}",
                        container_id,
                        e
                    ));
                }
            }
        }
        Ok(())
    };

    match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        processing_future,
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return Err(e);
        }
        Err(_) => {
            tracing::warn!(
                "Compilation in container {} timed out after {} seconds. Stopping container.",
                container_id,
                timeout_secs
            );
            let _ = docker.stop_container(container_id, None).await;
            return Err(anyhow::anyhow!(
                "Compilation timeout after {} seconds for container {}",
                timeout_secs,
                container_id
            ));
        }
    }

    let mut wait_stream = docker.wait_container::<String>(container_id, None);

    if let Some(wait_result) = wait_stream.next().await {
        match wait_result {
            Ok(exit_status) => {
                if exit_status.status_code != 0 {
                    tracing::error!(
                        "Compilation in container {} failed with exit code: {}.\nOutput:\n{}",
                        container_id,
                        exit_status.status_code,
                        output_buffer
                    );
                    return Err(anyhow::anyhow!(
                        "Compilation failed with exit code: {}.\nOutput:\n{}",
                        exit_status.status_code,
                        output_buffer
                    ));
                }
            }
            Err(e) => {
                tracing::error!(
                    "Failed to get exit status for container {}: {}",
                    container_id,
                    e
                );
                return Err(e).context(format!(
                    "Failed to get exit status for container {container_id}"
                ));
            }
        }
    } else {
        tracing::error!("No exit status received for container {}", container_id);
        return Err(anyhow::anyhow!(
            "No exit status received for container {}",
            container_id
        ));
    }

    Ok(output_buffer)
}

async fn extract_compiler_artifacts(
    docker: &Docker,
    container_id: &str,
    compile_output_str: &str,
) -> Result<CompilerOutput> {
    let output_json: serde_json::Value = serde_json::from_str(compile_output_str)
        .context("Failed to parse compiler JSON output from container logs")?;

    let contract_name = output_json["contract_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'contract_name' (string) in compiler JSON output"))?
        .to_string();

    let package_name = output_json["package_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'package_name' (string) in compiler JSON output"))?
        .to_string();

    let wasm_path_in_container = output_json["wasm_path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'wasm_path' (string) in compiler JSON output"))?;

    let rwasm_path_in_container = output_json["rwasm_path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing 'rwasm_path' (string) in compiler JSON output"))?;

    let wasm_bytecode = read_file_from_container(docker, container_id, wasm_path_in_container)
        .await
        .with_context(|| {
            format!(
                "Failed to read WASM file '{wasm_path_in_container}' from container {container_id}"
            )
        })?;
    let rwasm_bytecode = read_file_from_container(docker, container_id, rwasm_path_in_container)
        .await
        .with_context(|| {
            format!(
                "Failed to read rWASM file '{rwasm_path_in_container}' from container {container_id}"
            )
        })?;

    Ok(CompilerOutput {
        wasm_bytecode: Bytes::from(wasm_bytecode),
        rwasm_bytecode: Bytes::from(rwasm_bytecode),
        abi: output_json
            .get("abi")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        metadata: output_json
            .get("metadata")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        contract_name,
        package_name,
        compile_logs: compile_output_str.to_string(),
    })
}

async fn read_file_from_container(
    docker: &Docker,
    container_id: &str,
    file_path_in_container: &str,
) -> Result<Vec<u8>> {
    let exec_config = CreateExecOptions {
        cmd: Some(vec!["cat", file_path_in_container]),
        attach_stdout: Some(true),
        attach_stderr: Some(false),
        ..Default::default()
    };

    let exec_creation_response = docker
        .create_exec(container_id, exec_config)
        .await
        .with_context(|| {
            format!(
                "Failed to create exec 'cat {file_path_in_container}' in container {container_id}"
            )
        })?;

    let start_exec_result = docker
        .start_exec(&exec_creation_response.id, None)
        .await
        .with_context(|| {
            format!(
                "Failed to start exec 'cat {}' (id: {}) in container {}",
                file_path_in_container, exec_creation_response.id, container_id
            )
        })?;

    if let StartExecResults::Attached { mut output, .. } = start_exec_result {
        let mut file_data = Vec::new();
        while let Some(result) = output.next().await {
            match result {
                Ok(LogOutput::StdOut { message }) => {
                    file_data.extend_from_slice(&message);
                }
                Ok(LogOutput::StdErr { message }) => {
                    tracing::warn!(
                        "Stderr from 'cat {}' in container {}: {}",
                        file_path_in_container,
                        container_id,
                        String::from_utf8_lossy(&message)
                    );
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Stream error while reading file '{}' (exec id: {}) from container {}: {}",
                        file_path_in_container,
                        exec_creation_response.id,
                        container_id,
                        e
                    ));
                }
                _ => {}
            }
        }
        Ok(file_data)
    } else {
        Err(anyhow::anyhow!(
            "Failed to attach to exec 'cat {}' (id: {}) in container {}",
            file_path_in_container,
            exec_creation_response.id,
            container_id
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_compiler_command_basic() {
        let settings = CompileSettings {
            rustc_version: None,
            fluentbase_sdk_version: Version::new(0, 1, 0),
            build_profile: BuildProfile::Release,
            features: vec!["feature1".to_string(), "feature2".to_string()],
            contract_name: Some("my-contract".to_string()),
            cargo_flags: vec!["--verbose".to_string()],
        };

        let cmd = build_compiler_command(&settings);

        assert!(cmd.contains(&"fluent-wasm-compiler".to_string()));
        assert!(cmd.contains(&"--release".to_string()));
        assert!(cmd.contains(&"--features".to_string()));
        assert!(cmd.contains(&"feature1,feature2".to_string()));
        assert!(cmd.contains(&"--contract".to_string()));
        assert!(cmd.contains(&"my-contract".to_string()));
        assert!(cmd.contains(&"--output-format".to_string()));
        assert!(cmd.contains(&"json".to_string()));
        assert!(cmd.contains(&"--verbose".to_string()));
    }

    #[test]
    fn test_build_compiler_command_debug_no_contract_no_features() {
        let settings = CompileSettings {
            rustc_version: None,
            fluentbase_sdk_version: Version::new(0, 1, 0),
            build_profile: BuildProfile::Debug,
            features: vec![],
            contract_name: None,
            cargo_flags: vec![],
        };

        let cmd = build_compiler_command(&settings);
        assert_eq!(
            cmd,
            vec![
                "fluent-wasm-compiler".to_string(),
                "--output-format".to_string(),
                "json".to_string()
            ]
        );
    }

    #[test]
    fn test_create_dockerfile_tar_valid_content() {
        let dockerfile = "FROM rust:1.75\nRUN echo hello";
        let tar_data_result = create_dockerfile_tar(dockerfile);
        assert!(tar_data_result.is_ok());
        let tar_data = tar_data_result.unwrap();
        assert!(!tar_data.is_empty(), "Tar data should not be empty");
    }
}
