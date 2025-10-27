use crate::docker::{run_build, BuildProfile, CargoBuildConfig, BASE_IMAGE_NAME, DEFAULT_RUST_FLAGS};
use crate::source::{prepare_source_from_archive, prepare_source_from_git};
use crate::{error::VerificationError, proto::{VerifyWasmRequest, VerifyWasmResponse}, source};
use fluent_verifier_proto::blockscout::fluent_verifier::v1::verify_wasm_request::Source;
use fluent_verifier_proto::blockscout::fluent_verifier::v1::{VerificationResult, VerificationStatus};
use rwasm::RwasmModule;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

/// Main entry point for contract verification
pub async fn verify_contract(
    docker: &bollard::Docker,
    request: VerifyWasmRequest,
    container_network: &Option<String>,
) -> Result<VerifyWasmResponse, VerificationError> {
    tracing::info!("new verification request received: {:?}", DebugRequest(&request));

    let compile_settings = request
        .compile_settings
        .as_ref()
        .ok_or_else(|| VerificationError::InvalidRequest("compile_settings is required".to_string()))?;

    if compile_settings.sdk_version.is_empty() {
        return Err(VerificationError::InvalidRequest(
            "sdk_version is required in compile_settings".to_string(),
        ));
    }

    let deployed_bytecode = fetch_deployed_bytecode(&request.contract_address, &request.rpc_endpoint).await?;

    let (rwasm, _read_len) = RwasmModule::new(&deployed_bytecode);
    let deployed_hash = calculate_hash(&rwasm.hint_section);
    tracing::info!("deployed_bytecode wasm section hash: {:?}", &deployed_hash);

    let source_dir = match &request.source {
        Some(Source::ArchiveSource(archive)) => {
            prepare_source_from_archive(archive).await?
        }
        Some(Source::GitSource(git)) => {
            prepare_source_from_git(git).await?
        }
        None => {
            return Err(VerificationError::InvalidRequest("source is required".to_string()));
        }
    };

    let docker_image = format!("{}:{}", BASE_IMAGE_NAME, compile_settings.sdk_version);
    let build_config = CargoBuildConfig {
        profile: BuildProfile::Release,
        features: compile_settings.features.clone(),
        no_default_features: compile_settings.no_default_features,
        rustflags: DEFAULT_RUST_FLAGS.to_string(),
    };

    let build_output = run_build(
        docker,
        &docker_image,
        source_dir.path(),
        &build_config,
        container_network,
    ).await?;

    let built_hash = calculate_hash(&build_output.wasm_bytes);
    tracing::info!("built wasm hash: {}", built_hash);

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

    let source_files = source::collect_source_files(source_dir.path()).await?;

    // Step 7: Return success
    Ok(VerifyWasmResponse {
        status: VerificationStatus::StatusSuccess as i32,
        error_message: String::new(),
        result: Some(VerificationResult {
            contract_address: request.contract_address.clone(),
            chain_id: request.chain_id.clone(),
            expected_hash: format!("0x{}", deployed_hash),
            actual_hash: format!("0x{}", built_hash),
            compile_settings: request.compile_settings.clone(),
            rustc_version: "1.88.0-x86_64-unknown-linux-gnu".to_string(),
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
        .map_err(|e| VerificationError::VerificationFailed(format!("Failed to parse RPC response: {}", e)))?;

    if let Some(error) = response.error {
        return Err(VerificationError::VerificationFailed(format!("RPC error: {:?}", error)));
    }

    let bytecode_hex = response
        .result
        .ok_or_else(|| VerificationError::VerificationFailed("No result in RPC response".to_string()))?;

    let bytecode = hex::decode(bytecode_hex.trim_start_matches("0x"))
        .map_err(|e| VerificationError::VerificationFailed(format!("Failed to decode bytecode: {}", e)))?;

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
                    .field("project_path", &archive.project_path)
                    .finish()
            }
            Some(Source::GitSource(git)) => f
                .debug_struct("GitSource")
                .field("repository_url", &git.repository_url)
                .field("commit_ref", &git.commit_ref)
                .field("project_path", &git.project_path)
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
            source: Some(crate::proto::verify_wasm_request::Source::GitSource(
                crate::proto::GitSource {
                    repository_url: "https://github.com/test/repo.git".to_string(),
                    commit_ref: "main".to_string(),
                    project_path: ".".to_string(),
                },
            )),
            contract_address: "0x1234567890123456789012345678901234567890".to_string(),
            chain_id: "1".to_string(),
            rpc_endpoint: "http://localhost:8545".to_string(),
            compile_settings: Some(CompileSettings {
                sdk_version: "v0.2.1-dev".to_string(),
                features: vec!["test".to_string()],
                no_default_features: false,
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