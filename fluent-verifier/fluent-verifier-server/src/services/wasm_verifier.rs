use crate::{
    proto::{
        wasm_verifier_server::WasmVerifier, ListAvailableVersionsRequest,
        ListAvailableVersionsResponse, VerifyWasmRequest, VerifyWasmResponse,
    },
    settings::DockerApiSettings,
};
use async_trait::async_trait;
use bollard::Docker;
use fluent_verifier_logic::{connect_docker, verify_contract};
use std::sync::Arc;
use tonic::{Request, Response, Status as TonicStatus};

pub struct FluentWasmVerifierService {
    docker: Arc<Docker>,
    settings: DockerApiSettings,
}

impl FluentWasmVerifierService {
    pub async fn new(docker_api_settings: DockerApiSettings) -> anyhow::Result<Self> {
        let docker = connect_docker(docker_api_settings.addr.as_str())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to Docker: {}", e))?;

        Ok(Self {
            docker: Arc::new(docker),
            settings: docker_api_settings,
        })
    }
}

#[async_trait]
impl WasmVerifier for FluentWasmVerifierService {
    async fn verify_wasm(
        &self,
        request: Request<VerifyWasmRequest>,
    ) -> Result<Response<VerifyWasmResponse>, TonicStatus> {
        let proto_request = request.into_inner();
        let request_id = uuid::Uuid::new_v4();

        tracing::info!(
            %request_id,
            contract_address = %proto_request.contract_address,
            chain_id = %proto_request.chain_id,
            sdk_version = proto_request.compile_settings.as_ref().map(|s| &s.sdk_version).unwrap_or(&String::new()),
            "Received VerifyWasm request"
        );

        // Validate request
        validate_verification_request(&proto_request).map_err(|e| {
            tracing::error!(%request_id, error = %e, "Invalid request");
            TonicStatus::invalid_argument(format!("Invalid request: {e}"))
        })?;

        // Run verification
        let verification_result =
            verify_contract(&self.docker, proto_request, &self.settings.network)
                .await
                .map_err(|e| {
                    tracing::error!(%request_id, error = %e, "Verification error");
                    TonicStatus::internal(format!("Verification failed: {e}"))
                })?;

        tracing::info!(
            %request_id,
            status = ?verification_result.status,
            has_result = verification_result.result.is_some(),
            "Verification completed"
        );

        Ok(Response::new(verification_result))
    }

    async fn list_available_versions(
        &self,
        request: Request<ListAvailableVersionsRequest>,
    ) -> Result<Response<ListAvailableVersionsResponse>, TonicStatus> {
        let request_inner = request.into_inner();
        let request_id = uuid::Uuid::new_v4();

        tracing::info!(
            %request_id,
            include_prerelease = request_inner.include_prerelease,
            "Received ListAvailableVersions request"
        );

        // Get available Docker image tags from registry
        let sdk_versions = list_docker_image_tags(request_inner.include_prerelease)
            .await
            .map_err(|e| {
                tracing::error!(%request_id, error = %e, "Failed to list Docker tags");
                TonicStatus::internal("Failed to retrieve available versions")
            })?;

        // Find latest stable version (non-prerelease)
        let latest_stable = find_latest_stable_version(&sdk_versions);

        Ok(Response::new(ListAvailableVersionsResponse {
            sdk_versions,
            latest_stable,
        }))
    }
}

/// Validates the verification request
fn validate_verification_request(request: &VerifyWasmRequest) -> anyhow::Result<()> {
    // Validate required fields
    if request.contract_address.is_empty() {
        return Err(anyhow::anyhow!("contract_address is required"));
    }

    if request.chain_id.is_empty() {
        return Err(anyhow::anyhow!("chain_id is required"));
    }

    if request.rpc_endpoint.is_empty() {
        return Err(anyhow::anyhow!("rpc_endpoint is required"));
    }

    // Validate source
    if request.source.is_none() {
        return Err(anyhow::anyhow!(
            "source (archive_source or git_source) is required"
        ));
    }

    // Validate compile settings
    let compile_settings = request
        .compile_settings
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("compile_settings is required"))?;

    if compile_settings.sdk_version.is_empty() {
        return Err(anyhow::anyhow!(
            "sdk_version is required in compile_settings"
        ));
    }

    // Validate contract address format (basic check)
    if !request.contract_address.starts_with("0x") || request.contract_address.len() != 42 {
        return Err(anyhow::anyhow!(
            "contract_address must be a valid Ethereum address (0x + 40 hex chars)"
        ));
    }

    Ok(())
}

/// Lists available Docker image tags from the registry
async fn list_docker_image_tags(include_prerelease: bool) -> anyhow::Result<Vec<String>> {
    // Simple implementation using ghcr.io token endpoint and tags API
    let client = reqwest::Client::new();

    // Step 1: Get anonymous token for read access
    let token_url = "https://ghcr.io/token?scope=repository:fluentlabs-xyz/fluentbase-build:pull";
    let token_response = client
        .get(token_url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get registry token: {}", e))?;

    if !token_response.status().is_success() {
        return Err(anyhow::anyhow!("Failed to authenticate with registry"));
    }

    #[derive(serde::Deserialize)]
    struct TokenResponse {
        token: String,
    }

    let token_body = token_response
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read token response: {}", e))?;
    let token: TokenResponse = serde_json::from_str(&token_body)
        .map_err(|e| anyhow::anyhow!("Failed to parse token response: {}", e))?;

    // Step 2: List tags using the token
    let tags_url = "https://ghcr.io/v2/fluentlabs-xyz/fluentbase-build/tags/list";
    let tags_response = client
        .get(tags_url)
        .bearer_auth(&token.token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to list tags: {}", e))?;

    if !tags_response.status().is_success() {
        return Err(anyhow::anyhow!("Failed to retrieve tags from registry"));
    }

    #[derive(serde::Deserialize)]
    struct TagsResponse {
        tags: Vec<String>,
    }

    let tags_body = tags_response
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read tags response: {}", e))?;
    let mut tags: TagsResponse = serde_json::from_str(&tags_body)
        .map_err(|e| anyhow::anyhow!("Failed to parse tags response: {}", e))?;

    // Sort tags for consistent ordering
    tags.tags.sort();

    if !include_prerelease {
        // Filter out pre-release versions (containing -dev, -beta, -alpha, etc.)
        tags.tags
            .retain(|v| v == "latest" || (!v.contains('-') && v.starts_with('v')));
    }

    tracing::info!("Found {} SDK versions in registry", tags.tags.len());
    Ok(tags.tags)
}

/// Finds the latest stable version from the list
fn find_latest_stable_version(versions: &[String]) -> String {
    // Filter stable versions (v-prefixed without pre-release suffix)
    let mut stable_versions: Vec<&String> = versions
        .iter()
        .filter(|v| v.starts_with('v') && !v.contains('-'))
        .collect();

    // Sort by semantic version (simple implementation)
    stable_versions.sort_by(|a, b| {
        // Remove 'v' prefix and compare
        let a_parts: Vec<u32> = a[1..].split('.').filter_map(|s| s.parse().ok()).collect();
        let b_parts: Vec<u32> = b[1..].split('.').filter_map(|s| s.parse().ok()).collect();

        for i in 0..3 {
            let a_val = a_parts.get(i).unwrap_or(&0);
            let b_val = b_parts.get(i).unwrap_or(&0);
            match a_val.cmp(b_val) {
                std::cmp::Ordering::Equal => continue,
                other => return other,
            }
        }
        std::cmp::Ordering::Equal
    });

    stable_versions
        .last()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "latest".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn test_find_latest_stable_version() {
        let versions = vec![
            "v0.1.0".to_string(),
            "v0.2.0".to_string(),
            "v0.2.1-dev".to_string(),
            "v0.3.0".to_string(),
            "v0.3.1-beta".to_string(),
            "latest".to_string(),
        ];

        assert_eq!(find_latest_stable_version(&versions), "v0.3.0");
    }

    #[test]
    fn test_validate_verification_request() {
        use crate::proto::CompileSettings;

        // Valid request
        let valid_request = VerifyWasmRequest {
            source: Some(crate::proto::verify_wasm_request::Source::ArchiveSource(
                crate::proto::ArchiveSource {
                    content: Bytes::new(),
                },
            )),
            contract_address: "0x1234567890123456789012345678901234567890".to_string(),
            chain_id: "9999".to_string(),
            rpc_endpoint: "https://mainnet.fluent.xyz".to_string(),
            compile_settings: Some(CompileSettings {
                sdk_version: "v0.2.1-dev".to_string(),
                features: vec![],
                no_default_features: false,
                rust_flags: vec![],
                rust_toolchain: "".to_string(),
                manifest_path: "".to_string(),
            }),
        };

        assert!(validate_verification_request(&valid_request).is_ok());

        // Invalid address
        let mut invalid_request = valid_request.clone();
        invalid_request.contract_address = "invalid".to_string();
        assert!(validate_verification_request(&invalid_request).is_err());

        // Missing SDK version
        let mut invalid_request = valid_request.clone();
        invalid_request.compile_settings = Some(CompileSettings {
            sdk_version: String::new(),
            features: vec![],
            no_default_features: false,
            rust_flags: vec![],
            rust_toolchain: "".to_string(),
            manifest_path: "".to_string(),
        });
        assert!(validate_verification_request(&invalid_request).is_err());
    }
}
