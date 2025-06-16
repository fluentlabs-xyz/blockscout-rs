use crate::{docker, error::VerificationError, source};
use crate::proto::{
    VerifyWasmRequest, VerifyWasmResponse, VerificationResult, 
    VerificationMetadata, VerificationStatus
};
use std::time::{SystemTime, UNIX_EPOCH};

/// Main entry point for contract verification
pub async fn verify_contract(
    docker: &bollard::Docker,
    request: VerifyWasmRequest,
) -> Result<VerifyWasmResponse, VerificationError> {
    // Step 1: Prepare source code
    tracing::info!("Preparing source code");
    let source_dir = match &request.source {
        Some(crate::proto::verify_wasm_request::Source::ArchiveSource(archive)) => {
            source::prepare_source_from_archive(archive).await?
        }
        Some(crate::proto::verify_wasm_request::Source::GitSource(git)) => {
            source::prepare_source_from_git(git).await?
        }
        None => {
            return Err(VerificationError::InvalidRequest(
                "No source provided".to_string()
            ));
        }
    };

    // Step 2: Validate compile settings
    let compile_settings = request.compile_settings.as_ref()
        .ok_or_else(|| VerificationError::InvalidRequest(
            "compile_settings is required".to_string()
        ))?;
    
    if compile_settings.rustc_version.is_empty() {
        return Err(VerificationError::InvalidRequest(
            "rustc_version is required in compile_settings".to_string()
        ));
    }
    
    if compile_settings.sdk_version.is_empty() {
        return Err(VerificationError::InvalidRequest(
            "sdk_version is required in compile_settings".to_string()
        ));
    }
    
    tracing::info!("Using rustc version: {}", compile_settings.rustc_version);
    tracing::info!("Using SDK version: {}", compile_settings.sdk_version);

    // Step 3: Run verification through CLI
    tracing::info!("Running verification");
    let cli_output = docker::run_verification(
        docker,
        source_dir.path(),
        &request.contract_address,
        &request.chain_id,
        &request.rpc_endpoint,
        &compile_settings.rustc_version,
        &compile_settings.sdk_version,
        &compile_settings.profile,
        &compile_settings.features,
        compile_settings.no_default_features,
    ).await?;

    // Step 4: Process result
    match cli_output {
        docker::CliOutput::Success { 
            verified, 
            contract_name,
            expected_hash,
            actual_hash,
            abi,
            compiler_version,
            sdk_version,
            ..
        } => {
            if !verified {
                // Verification failed - bytecode mismatch
                return Ok(VerifyWasmResponse {
                    status: VerificationStatus::StatusBytecodeMismatch as i32,
                    contract_name,
                    error_message: format!(
                        "Bytecode mismatch. Expected: {}, Actual: {}", 
                        expected_hash, actual_hash
                    ),
                    result: None,
                });
            }

            // Step 5: Collect source files for successful verification
            tracing::info!("Collecting source files");
            let source_files = source::collect_source_files(source_dir.path()).await?;

            // Step 6: Build successful response
            let metadata = VerificationMetadata {
                deployed_bytecode_hash: expected_hash.clone(),
                compiled_bytecode_hash: actual_hash.clone(),
                compiler_version_full: compiler_version,
                sdk_version_used: sdk_version,
                verified_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            };

            let result = VerificationResult {
                contract_address: request.contract_address.clone(),
                chain_id: request.chain_id.clone(),
                abi_json: abi.map(|v| v.to_string()).unwrap_or_else(|| "{}".to_string()),
                compile_settings_used: request.compile_settings.clone(),
                source_files,
                metadata: Some(metadata),
            };

            Ok(VerifyWasmResponse {
                status: VerificationStatus::StatusSuccess as i32,
                contract_name,
                error_message: String::new(),
                result: Some(result),
            })
        }
        
        docker::CliOutput::Error { error_type, message } => {
            // Map error types to status codes
            let status = match error_type.as_str() {
                "compilation_failed" => VerificationStatus::StatusCompilationFailed,
                "network_error" => VerificationStatus::StatusNetworkError,
                "no_git_repository" | "git_dirty_state" => VerificationStatus::StatusInvalidSource,
                _ => VerificationStatus::StatusError,
            };
            
            Ok(VerifyWasmResponse {
                status: status as i32,
                contract_name: String::new(),
                error_message: message,
                result: None,
            })
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
        
        let docker = bollard::Docker::connect_with_local_defaults().unwrap();
        let result = tokio_test::block_on(verify_contract(&docker, request));
        
        assert!(result.is_err());
    }

}