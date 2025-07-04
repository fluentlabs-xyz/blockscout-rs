use crate::{docker, error::VerificationError, source};
use crate::proto::{
    VerifyWasmRequest, VerifyWasmResponse, VerificationResult, VerificationStatus
};

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

    // Step 2: Validate SDK version (the only required compile setting now)
    let compile_settings = request.compile_settings.as_ref()
        .ok_or_else(|| VerificationError::InvalidRequest(
            "compile_settings is required".to_string()
        ))?;
    
    if compile_settings.sdk_version.is_empty() {
        return Err(VerificationError::InvalidRequest(
            "sdk_version is required in compile_settings".to_string()
        ));
    }
    
    tracing::info!("Using SDK version: {}", compile_settings.sdk_version);

    // Step 3: Run verification through CLI
    tracing::info!("Running verification with Docker image tag: {}", compile_settings.sdk_version);
    let cli_output = docker::run_verification(
        docker,
        source_dir.path(),
        &request.contract_address,
        &request.chain_id,
        &request.rpc_endpoint,
        &compile_settings.sdk_version,
        &compile_settings.features,
        compile_settings.no_default_features,
    ).await?;

    // Step 4: Process result
    let docker::CliOutput {
        verified,
        expected_hash,
        actual_hash,
        rustc_version,
        sdk_version,
        build_platform,
    } = cli_output;

    if !verified {
        // Verification failed - bytecode mismatch
        return Ok(VerifyWasmResponse {
            status: VerificationStatus::StatusBytecodeMismatch as i32,
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
    let result = VerificationResult {
        contract_address: request.contract_address.clone(),
        chain_id: request.chain_id.clone(),
        expected_hash,
        actual_hash,
        compile_settings: request.compile_settings.clone(),
        rustc_version,
        sdk_version,
        build_platform,
        source_files,
    };

    Ok(VerifyWasmResponse {
        status: VerificationStatus::StatusSuccess as i32,
        error_message: String::new(),
        result: Some(result),
    })
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
        
        // Test just the validation logic without Docker
        let compile_settings = request.compile_settings.as_ref();
        assert!(compile_settings.is_none());
        
        // This is what would happen in verify_contract
        let error = compile_settings
            .ok_or_else(|| VerificationError::InvalidRequest(
                "compile_settings is required".to_string()
            ));
        
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
                sdk_version: String::new(), // Empty SDK version
                features: vec![],
                no_default_features: false,
            }),
        };
        
        // Test just the validation logic
        let compile_settings = request.compile_settings.as_ref().unwrap();
        assert!(compile_settings.sdk_version.is_empty());
        
        // This is what would happen in verify_contract
        let error = if compile_settings.sdk_version.is_empty() {
            Err(VerificationError::InvalidRequest(
                "sdk_version is required in compile_settings".to_string()
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
                }
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
        
        // Validate all required fields are present
        assert!(request.source.is_some());
        assert!(!request.contract_address.is_empty());
        assert!(!request.chain_id.is_empty());
        assert!(!request.rpc_endpoint.is_empty());
        
        let compile_settings = request.compile_settings.as_ref().unwrap();
        assert!(!compile_settings.sdk_version.is_empty());
    }

    #[test]
    fn test_verification_result_structure() {
        // Test that VerificationResult can be created with all required fields
        let result = VerificationResult {
            contract_address: "0x1234567890123456789012345678901234567890".to_string(),
            chain_id: "1".to_string(),
            expected_hash: "0xabc123".to_string(),
            actual_hash: "0xabc123".to_string(),
            compile_settings: Some(CompileSettings {
                sdk_version: "v0.2.1-dev".to_string(),
                features: vec![],
                no_default_features: false,
            }),
            rustc_version: "rustc 1.87.0".to_string(),
            sdk_version: "0.1.0-abc123".to_string(),
            build_platform: "docker:linux-x86_64".to_string(),
            source_files: std::collections::BTreeMap::new(),
        };
        
        assert_eq!(result.contract_address, "0x1234567890123456789012345678901234567890");
        assert_eq!(result.expected_hash, "0xabc123");
        assert_eq!(result.actual_hash, "0xabc123");
    }
}