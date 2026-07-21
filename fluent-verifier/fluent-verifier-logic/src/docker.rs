//! Docker orchestration for smart contract compilation

use crate::{
    error::VerificationError, metadata::cdylib_target_name, DOCKER_MAX_MEMORY_LIMIT, DOCKER_WORKDIR,
};
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
use std::{collections::HashMap, io::Write, path::PathBuf};
use tracing::{debug, info, trace};
use uuid::Uuid;

const RUST_TOOLCHAIN_LABEL: &str = "xyz.fluent.rust.toolchain";
const RUST_VERSION_ENV: &str = "RUST_VERSION";
const INSTALL_TOOLCHAIN_ENV: &str = "FLUENT_VERIFIER_INSTALL_TOOLCHAIN";
const RUSTC_VERSION_PREFIX: &str = "__FLUENT_VERIFIER_RUSTC_VERSION__=";
const BUILD_COMMAND: &str = r#"set -eu
if [ "${FLUENT_VERIFIER_INSTALL_TOOLCHAIN:-0}" = "1" ]; then
    rustup toolchain install \
        --profile minimal \
        --target wasm32-unknown-unknown \
        --no-self-update \
        -- "$RUSTUP_TOOLCHAIN"
fi
rustc_version="$(rustc --version)"
rustc_sysroot="$(rustc --print sysroot)"
printf '__FLUENT_VERIFIER_RUSTC_VERSION__=%s\n' "$rustc_version"
if [ ! -d "$rustc_sysroot/lib/rustlib/wasm32-unknown-unknown" ]; then
    echo "error: SDK image toolchain does not include wasm32-unknown-unknown: $rustc_sysroot" >&2
    exit 1
fi
exec "$@"
"#;

/// Cargo build configuration
#[derive(Debug, Clone)]
pub struct CargoBuildConfig {
    pub profile: BuildProfile,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub rust_flags: Vec<String>,
    pub manifest_path: PathBuf,
    pub target_dir: PathBuf,
    pub requested_rust_toolchain: String,
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

        if self.no_default_features {
            args.push("--no-default-features".to_string());
        }

        if !self.features.is_empty() {
            args.push("--features".to_string());
            args.push(self.features.join(","));
        }

        let manifest_path = self.manifest_path.to_str().unwrap_or_default().to_string();
        args.push("--manifest-path".to_string());
        args.push(manifest_path.clone());

        args
    }

    fn env_vars(&self, toolchain: &ToolchainSelection) -> Vec<String> {
        let mut env = vec!["RUST_LOG=info".to_string()];
        env.push(format!("RUSTFLAGS={}", self.rust_flags.join(" ")));
        env.push(format!("CARGO_TARGET_DIR={}", self.target_dir.display()));
        if let Some(name) = &toolchain.override_name {
            env.push(format!("RUSTUP_TOOLCHAIN={name}"));
        }
        if toolchain.install {
            env.push(format!("{INSTALL_TOOLCHAIN_ENV}=1"));
        }
        env
    }

    pub fn container_command(&self) -> Vec<String> {
        let mut command = vec![
            "sh".to_string(),
            "-c".to_string(),
            BUILD_COMMAND.to_string(),
            "fluent-verifier-build".to_string(),
        ];
        command.extend(self.to_command_args());
        command
    }

    pub fn wasm_output_path(&self, file_name: String) -> anyhow::Result<String> {
        Ok(format!(
            "{}/wasm32-unknown-unknown/{}/{}.wasm",
            self.target_dir.display(),
            self.profile.as_str(),
            file_name.as_str()
        ))
    }
}

/// Build output
#[derive(Debug)]
pub struct BuildOutput {
    pub wasm_bytes: Vec<u8>,
    pub rustc_version: String,
    pub image_reference: String,
}

#[derive(Debug)]
struct ResolvedImage {
    id: String,
    reference: String,
    rust_toolchain: String,
}

#[derive(Debug, Default, PartialEq)]
struct ToolchainSelection {
    override_name: Option<String>,
    install: bool,
}

/// Main entry point for Docker-based compilation
pub async fn run_build(
    docker: &Docker,
    docker_image: &str,
    source_dir: PathBuf,
    build_config: &CargoBuildConfig,
    container_network: &Option<String>,
    source_manifest_path: &PathBuf,
) -> Result<BuildOutput, VerificationError> {
    debug!("Starting build with image: {}", docker_image);
    let output_file_name = cdylib_target_name(source_manifest_path)?;

    let resolved_image = pull_and_resolve_image(docker, docker_image).await?;
    let toolchain = select_toolchain(
        &build_config.requested_rust_toolchain,
        &resolved_image.rust_toolchain,
    )?;

    // Create container
    let container_id = create_container(
        docker,
        &resolved_image.id,
        build_config,
        &toolchain,
        container_network,
    )
    .await?;

    // Copy source
    copy_directory_to_container(docker, &container_id, source_dir).await?;

    // Run build
    let stdout = run_container(docker, &container_id).await?;
    let rustc_version = parse_rustc_version(&stdout)?;

    // Extract WASM
    let target_path = build_config.wasm_output_path(output_file_name)?;
    let wasm_bytes = copy_wasm_from_container(docker, &container_id, &target_path).await?;

    docker
        .remove_container(
            &container_id,
            Some(container::RemoveContainerOptions {
                force: true,
                ..Default::default()
            }),
        )
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to remove container: {e}")))?;

    info!("Build completed, WASM size: {} bytes", wasm_bytes.len());

    Ok(BuildOutput {
        wasm_bytes,
        rustc_version,
        image_reference: resolved_image.reference,
    })
}

async fn pull_and_resolve_image(
    docker: &Docker,
    image_name: &str,
) -> Result<ResolvedImage, VerificationError> {
    info!("Refreshing Docker image: {}", image_name);
    let options = CreateImageOptions {
        from_image: image_name,
        platform: "linux/amd64",
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

    let image = docker.inspect_image(image_name).await.map_err(|e| {
        VerificationError::Docker(format!("Failed to inspect image {image_name}: {e}"))
    })?;
    let id = image.id.ok_or_else(|| {
        VerificationError::Docker(format!("Docker image {image_name} has no immutable ID"))
    })?;
    let config = image.config.ok_or_else(|| {
        VerificationError::Docker(format!("Docker image {image_name} has no configuration"))
    })?;
    let labels = config.labels.unwrap_or_default();
    let env = config.env.unwrap_or_default();
    let rust_toolchain = image_rust_toolchain(&labels, &env).ok_or_else(|| {
        VerificationError::Docker(format!(
            "Docker image {image_name} does not declare {RUST_TOOLCHAIN_LABEL} or {RUST_VERSION_ENV}"
        ))
    })?;
    let reference = image
        .repo_digests
        .and_then(|digests| digests.into_iter().next())
        .unwrap_or_else(|| id.clone());

    info!(
        "Resolved Docker image: tag={} reference={} rust_toolchain={}",
        image_name, reference, rust_toolchain
    );

    Ok(ResolvedImage {
        id,
        reference,
        rust_toolchain,
    })
}

fn image_rust_toolchain(labels: &HashMap<String, String>, env: &[String]) -> Option<String> {
    labels
        .get(RUST_TOOLCHAIN_LABEL)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .or_else(|| {
            env.iter().find_map(|entry| {
                entry
                    .strip_prefix(&format!("{RUST_VERSION_ENV}="))
                    .filter(|value| !value.trim().is_empty())
                    .map(ToOwned::to_owned)
            })
        })
}

fn select_toolchain(
    requested_toolchain: &str,
    image_toolchain: &str,
) -> Result<ToolchainSelection, VerificationError> {
    let requested_toolchain = requested_toolchain.trim();
    let canonical_image_toolchain = format!("{image_toolchain}-x86_64-unknown-linux-gnu");
    if requested_toolchain.is_empty()
        || requested_toolchain == image_toolchain
        || requested_toolchain == canonical_image_toolchain
    {
        return Ok(ToolchainSelection::default());
    }

    if !requested_toolchain
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_'))
    {
        return Err(VerificationError::InvalidRequest(format!(
            "invalid rust_toolchain: {requested_toolchain}"
        )));
    }

    Ok(ToolchainSelection {
        override_name: Some(requested_toolchain.to_string()),
        install: true,
    })
}

fn parse_rustc_version(stdout: &str) -> Result<String, VerificationError> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(RUSTC_VERSION_PREFIX))
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            VerificationError::Docker(
                "Build container did not report the active rustc version".to_string(),
            )
        })
}

async fn create_container(
    docker: &Docker,
    image_name: &str,
    build_config: &CargoBuildConfig,
    toolchain: &ToolchainSelection,
    container_network: &Option<String>,
) -> Result<String, VerificationError> {
    let container_suffix = Uuid::new_v4();
    let container_name = format!("fluent-build-{container_suffix}");

    debug!("Creating container: {}", container_name);
    debug!("Container will remain after completion for debugging");
    debug!("Connect with: docker exec -it {} /bin/bash", container_name);

    let options = CreateContainerOptions {
        name: container_name,
        platform: Some("linux/amd64".to_string()),
    };

    let command = build_config.container_command();
    let cmd_refs: Vec<&str> = command.iter().map(|s| s.as_str()).collect();

    let env_vars = build_config.env_vars(toolchain);
    let env_refs: Vec<&str> = env_vars.iter().map(|s| s.as_str()).collect();

    let network_mode = container_network.as_deref().unwrap_or("bridge").to_string();

    debug!(
        "Creating Docker container: image={} workdir={} cmd={:?}, env={:?}",
        image_name, DOCKER_WORKDIR, cmd_refs, env_refs
    );

    let config = container::Config {
        image: Some(image_name),
        working_dir: Some(DOCKER_WORKDIR),
        host_config: Some(HostConfig {
            network_mode: Some(network_mode),
            auto_remove: Some(false),
            memory: Some(DOCKER_MAX_MEMORY_LIMIT),
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
    dir: PathBuf,
) -> Result<(), VerificationError> {
    debug!("Copying source from: {:?}", dir);

    let tar = build_tar_from_directory(dir)
        .map_err(|e| VerificationError::Docker(format!("Failed to build tar: {e}")))?;

    let options = UploadToContainerOptions {
        path: DOCKER_WORKDIR,
        no_overwrite_dir_non_dir: "",
    };

    docker
        .upload_to_container(container_id, Some(options), tar.into())
        .await
        .map_err(|e| VerificationError::Docker(format!("Failed to upload to container: {e}")))?;

    Ok(())
}

async fn run_container(docker: &Docker, container_id: &str) -> Result<String, VerificationError> {
    debug!("Starting container: {}", container_id);

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

    let mut stdout_output = vec![];
    let mut stderr_output = vec![];

    while let Some(result) = attach_results.output.next().await {
        match result {
            Ok(LogOutput::StdOut { message }) => stdout_output.push(message),
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

    let stdout = stdout_output
        .into_iter()
        .filter_map(|bytes| String::from_utf8(bytes.to_vec()).ok())
        .collect::<Vec<_>>()
        .join("");

    Ok(stdout)
}

async fn copy_wasm_from_container(
    docker: &Docker,
    container_id: &str,
    target_path: &str,
) -> Result<Vec<u8>, VerificationError> {
    debug!("Copying WASM from container at {}", target_path);

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

fn build_tar_from_directory(dir: PathBuf) -> Result<Vec<u8>, VerificationError> {
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
            rust_flags: vec![],
            manifest_path: Default::default(),
            target_dir: Default::default(),
            requested_rust_toolchain: "".to_string(),
        };

        let cmd = config.to_command_args();

        assert!(cmd.contains(&"cargo".to_string()));
        assert!(cmd.contains(&"--release".to_string()));
        assert!(cmd.contains(&"--features".to_string()));
        assert!(cmd.contains(&"mainnet".to_string()));
    }

    #[test]
    fn build_environment_does_not_override_image_toolchain() {
        let config = CargoBuildConfig {
            profile: BuildProfile::Release,
            features: vec![],
            no_default_features: false,
            rust_flags: vec![],
            manifest_path: Default::default(),
            target_dir: Default::default(),
            requested_rust_toolchain: "1.93.1".to_string(),
        };

        let image_toolchain = select_toolchain(&config.requested_rust_toolchain, "1.93.1").unwrap();
        assert!(config
            .env_vars(&image_toolchain)
            .iter()
            .all(|entry| !entry.starts_with("RUSTUP_TOOLCHAIN=")));
        assert_eq!(config.container_command()[0..2], ["sh", "-c"]);
        assert!(config
            .container_command()
            .windows(2)
            .any(|args| args == ["--target", "wasm32-unknown-unknown"]));
    }

    #[test]
    fn resolves_toolchain_from_label_then_environment() {
        let labels = HashMap::from([(RUST_TOOLCHAIN_LABEL.to_string(), "1.93.1".to_string())]);
        let env = vec!["RUST_VERSION=1.92.0".to_string()];

        assert_eq!(
            image_rust_toolchain(&labels, &env).as_deref(),
            Some("1.93.1")
        );
        assert_eq!(
            image_rust_toolchain(&HashMap::new(), &env).as_deref(),
            Some("1.92.0")
        );
    }

    #[test]
    fn selects_or_installs_requested_toolchain() {
        assert_eq!(
            select_toolchain("", "1.93.1").unwrap(),
            ToolchainSelection::default()
        );
        assert_eq!(
            select_toolchain("1.93.1", "1.93.1").unwrap(),
            ToolchainSelection::default()
        );
        assert_eq!(
            select_toolchain("1.93.1-x86_64-unknown-linux-gnu", "1.93.1").unwrap(),
            ToolchainSelection::default()
        );

        let selection = select_toolchain("1.92.0", "1.93.1").unwrap();
        assert_eq!(selection.override_name.as_deref(), Some("1.92.0"));
        assert!(selection.install);
        assert!(matches!(
            select_toolchain("1.92.0; echo unsafe", "1.93.1"),
            Err(VerificationError::InvalidRequest(_))
        ));
    }

    #[test]
    fn configures_explicit_toolchain_installation() {
        let config = CargoBuildConfig {
            profile: BuildProfile::Release,
            features: vec![],
            no_default_features: false,
            rust_flags: vec![],
            manifest_path: Default::default(),
            target_dir: Default::default(),
            requested_rust_toolchain: "1.92.0".to_string(),
        };
        let selection = select_toolchain(&config.requested_rust_toolchain, "1.93.1").unwrap();
        let env = config.env_vars(&selection);

        assert!(env.contains(&"RUSTUP_TOOLCHAIN=1.92.0".to_string()));
        assert!(env.contains(&format!("{INSTALL_TOOLCHAIN_ENV}=1")));
        assert!(BUILD_COMMAND.contains("rustup toolchain install"));
        assert!(BUILD_COMMAND.contains("--target wasm32-unknown-unknown"));
    }

    #[test]
    fn parses_actual_rustc_version() {
        let stdout = "__FLUENT_VERIFIER_RUSTC_VERSION__=rustc 1.93.1 (01f6ddf75 2026-02-11)\n";

        assert_eq!(
            parse_rustc_version(stdout).unwrap(),
            "rustc 1.93.1 (01f6ddf75 2026-02-11)"
        );
    }
}
