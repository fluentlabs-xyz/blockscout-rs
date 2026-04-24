use crate::{
    docker::{run_build, BuildProfile, CargoBuildConfig},
    error::VerificationError,
    metadata::workspace_manifest_path,
    proto::{VerifyWasmRequest, VerifyWasmResponse},
    source,
    source::{prepare_source_from_archive, prepare_source_from_git},
    DEFAULT_RUST_TOOLCHAIN, DOCKER_BASE_IMAGE_NAME,
};
use fluent_verifier_proto::blockscout::fluent_verifier::v1::{
    verify_wasm_request::Source, VerificationResult, VerificationStatus,
};
use rwasm::RwasmModule;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fmt, path::PathBuf};
use tracing::{info, log::debug};

/// Main entry point for contract verification
pub async fn verify_contract(
    docker: &bollard::Docker,
    request: VerifyWasmRequest,
    container_network: &Option<String>,
) -> Result<VerifyWasmResponse, VerificationError> {
    info!(
        "New verification request received: {:?}",
        DebugRequest(&request)
    );

    let compile_settings = request.compile_settings.as_ref().ok_or_else(|| {
        VerificationError::InvalidRequest("compile_settings is required".to_string())
    })?;

    if compile_settings.sdk_version.is_empty() {
        return Err(VerificationError::InvalidRequest(
            "sdk_version is required in compile_settings".to_string(),
        ));
    }

    let deployed_bytecode =
        fetch_deployed_bytecode(&request.contract_address, &request.rpc_endpoint).await?;

    let (rwasm, _read_len) = RwasmModule::new(&deployed_bytecode);
    let deployed_hash = calculate_hash(&rwasm.hint_section);
    debug!("Deployed bytecode WASM section hash: {:?}", &deployed_hash);

    // Clone repository
    let temp_dir = tempfile::tempdir()?;

    let source_dir = match &request.source {
        Some(Source::ArchiveSource(archive)) => {
            prepare_source_from_archive(&temp_dir, archive).await?
        }
        Some(Source::GitSource(git)) => prepare_source_from_git(&temp_dir, git).await?,
        None => {
            return Err(VerificationError::InvalidRequest(
                "source is required".to_string(),
            ));
        }
    }
    .canonicalize()?;

    // We don't specify default rust flags
    let rust_flags = compile_settings.rust_flags.clone();

    let mut rust_toolchain = compile_settings.rust_toolchain.clone();
    if rust_toolchain.is_empty() {
        rust_toolchain = DEFAULT_RUST_TOOLCHAIN.to_string();
    }

    let relative_manifest_path = if compile_settings.manifest_path.is_empty() {
        PathBuf::from("Cargo.toml")
    } else {
        PathBuf::from(compile_settings.manifest_path.clone())
    };
    let mut source_manifest_path = source_dir.join(&relative_manifest_path).canonicalize()?;
    if source_manifest_path.is_dir() {
        source_manifest_path = source_manifest_path.join("Cargo.toml");
    } else if source_manifest_path.is_file() {
        source_manifest_path
            .file_name()
            .filter(|manifest_file_name| manifest_file_name == &"Cargo.toml")
            .ok_or(VerificationError::InvalidRequest(
                "incorrect manifest path".to_string(),
            ))?;
    }

    let workspace_source_manifest_path = workspace_manifest_path(&source_manifest_path)?;
    let Ok(workspace_relative_manifest_path) =
        workspace_source_manifest_path.strip_prefix(&source_dir)
    else {
        return Err(VerificationError::InvalidRequest(
            "incorrect workspace manifest path".to_string(),
        ));
    };

    let docker_image = format!(
        "{}:{}",
        DOCKER_BASE_IMAGE_NAME, compile_settings.sdk_version
    );
    let build_config = CargoBuildConfig {
        profile: BuildProfile::Release,
        features: compile_settings.features.clone(),
        no_default_features: compile_settings.no_default_features,
        rust_flags,
        manifest_path: PathBuf::from("/workspace").join(workspace_relative_manifest_path),
        target_dir: PathBuf::from("/workspace").join("target"),
        rust_toolchain: rust_toolchain.clone(),
    };

    let build_output = run_build(
        docker,
        &docker_image,
        source_dir.clone(),
        &build_config,
        container_network,
        &source_manifest_path,
    )
    .await?;

    let built_hash = calculate_hash(&build_output.wasm_bytes);
    debug!(
        "Output WASM hash: {}, expected_hash={}",
        built_hash, deployed_hash
    );

    if deployed_hash != built_hash {
        return Ok(VerifyWasmResponse {
            status: VerificationStatus::StatusBytecodeMismatch as i32,
            error_message: format!(
                "Bytecode mismatch. Expected: 0x{}, Actual: 0x{}",
                deployed_hash, built_hash
            ),
            result: None,
        });
    }

    let source_files = source::collect_source_files(source_dir.clone()).await?;

    Ok(VerifyWasmResponse {
        status: VerificationStatus::StatusSuccess as i32,
        error_message: String::new(),
        result: Some(VerificationResult {
            contract_address: request.contract_address.clone(),
            chain_id: request.chain_id.clone(),
            expected_hash: format!("0x{}", deployed_hash),
            actual_hash: format!("0x{}", built_hash),
            compile_settings: request.compile_settings.clone(),
            rustc_version: format!("{}-x86_64-unknown-linux-gnu", rust_toolchain),
            sdk_version: compile_settings.sdk_version.clone(),
            build_platform: docker_image,
            source_files,
        }),
    })
}

fn calculate_hash(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// RPC response structure
#[derive(Deserialize)]
struct RpcResponse {
    result: Option<String>,
    error: Option<serde_json::Value>,
}

async fn fetch_deployed_bytecode(address: &str, rpc: &str) -> Result<Vec<u8>, VerificationError> {
    let client = reqwest::Client::new();

    let response: RpcResponse = client
        .post(rpc)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getCode",
            "params": [address, "latest"],
            "id": 1
        }))
        .send()
        .await
        .map_err(|e| VerificationError::VerificationFailed(format!("RPC request failed: {}", e)))?
        .json()
        .await
        .map_err(|e| {
            VerificationError::VerificationFailed(format!("Failed to parse RPC response: {}", e))
        })?;

    if let Some(error) = response.error {
        return Err(VerificationError::VerificationFailed(format!(
            "RPC error: {:?}",
            error
        )));
    }

    let bytecode_hex = response.result.ok_or_else(|| {
        VerificationError::VerificationFailed("No result in RPC response".to_string())
    })?;

    let bytecode = hex::decode(bytecode_hex.trim_start_matches("0x")).map_err(|e| {
        VerificationError::VerificationFailed(format!("Failed to decode bytecode: {}", e))
    })?;

    Ok(bytecode)
}

pub struct DebugRequest<'a>(pub &'a VerifyWasmRequest);

impl fmt::Debug for DebugRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifyWasmRequest")
            .field("contract_address", &self.0.contract_address)
            .field("chain_id", &self.0.chain_id)
            .field("rpc_endpoint", &self.0.rpc_endpoint)
            .field("compile_settings", &self.0.compile_settings)
            .field("source", &DebugSource(&self.0.source))
            .finish()
    }
}

struct DebugSource<'a>(&'a Option<Source>);

impl fmt::Debug for DebugSource<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => write!(f, "None"),
            Some(Source::ArchiveSource(archive)) => {
                let len = archive.content.len();
                let preview = if len > 16 {
                    format!(
                        "[{:02x}{:02x}...{:02x}{:02x}] ({} bytes)",
                        archive.content[0],
                        archive.content[1],
                        archive.content[len - 2],
                        archive.content[len - 1],
                        len
                    )
                } else if len > 0 {
                    format!("[{} bytes]", len)
                } else {
                    "[empty]".to_string()
                };
                f.debug_struct("ArchiveSource")
                    .field("content", &preview)
                    .finish()
            }
            Some(Source::GitSource(git)) => f
                .debug_struct("GitSource")
                .field("repository_url", &git.repository_url)
                .field("commit_ref", &git.commit_ref)
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::CompileSettings;

    #[test]
    fn test_missing_compile_settings() {
        let request = VerifyWasmRequest {
            source: None,
            contract_address: "0x123".to_string(),
            chain_id: "1".to_string(),
            rpc_endpoint: "http://localhost:8545".to_string(),
            compile_settings: None,
        };

        let compile_settings = request.compile_settings.as_ref();
        assert!(compile_settings.is_none());

        let error = compile_settings.ok_or_else(|| {
            VerificationError::InvalidRequest("compile_settings is required".to_string())
        });

        assert!(error.is_err());
        if let Err(VerificationError::InvalidRequest(msg)) = error {
            assert!(msg.contains("compile_settings is required"));
        }
    }

    #[test]
    fn test_missing_sdk_version() {
        let request = VerifyWasmRequest {
            source: None,
            contract_address: "0x123".to_string(),
            chain_id: "1".to_string(),
            rpc_endpoint: "http://localhost:8545".to_string(),
            compile_settings: Some(CompileSettings {
                sdk_version: String::new(),
                features: vec![],
                no_default_features: false,
                rust_flags: vec![],
                rust_toolchain: "".to_string(),
                manifest_path: "".to_string(),
            }),
        };

        let compile_settings = request.compile_settings.as_ref().unwrap();
        assert!(compile_settings.sdk_version.is_empty());

        let error = if compile_settings.sdk_version.is_empty() {
            Err(VerificationError::InvalidRequest(
                "sdk_version is required in compile_settings".to_string(),
            ))
        } else {
            Ok(())
        };

        assert!(error.is_err());
        if let Err(VerificationError::InvalidRequest(msg)) = error {
            assert!(msg.contains("sdk_version is required"));
        }
    }

    #[test]
    fn test_valid_request() {
        let request = VerifyWasmRequest {
            source: Some(Source::GitSource(crate::proto::GitSource {
                repository_url: "https://github.com/test/repo.git".to_string(),
                commit_ref: "main".to_string(),
            })),
            contract_address: "0x1234567890123456789012345678901234567890".to_string(),
            chain_id: "1".to_string(),
            rpc_endpoint: "http://localhost:8545".to_string(),
            compile_settings: Some(CompileSettings {
                sdk_version: "v0.2.1-dev".to_string(),
                features: vec!["test".to_string()],
                no_default_features: false,
                rust_flags: vec![],
                rust_toolchain: "".to_string(),
                manifest_path: "".to_string(),
            }),
        };

        assert!(request.source.is_some());
        assert!(!request.contract_address.is_empty());
        assert!(!request.chain_id.is_empty());
        assert!(!request.rpc_endpoint.is_empty());

        let compile_settings = request.compile_settings.as_ref().unwrap();
        assert!(!compile_settings.sdk_version.is_empty());
    }
}
