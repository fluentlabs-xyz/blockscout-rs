use crate::{
    proto::{
        self,
        wasm_verifier_server::WasmVerifier,
        ListSupportedVersionsRequest, ListSupportedVersionsResponse,
        VerifyWasmRequest, VerifyWasmResponse,
    },
    settings::{DockerApiSettings, VerificationSettings},
};
use async_trait::async_trait;
use fluent_verifier_logic::{
    connect_docker, verify_contract,
    VerifyWasmRequest as LogicRequest,
    VerifyWasmResponse as LogicResponse,
    VerificationStatus,
};
use bollard::Docker;
use std::sync::Arc;
use tonic::{Request, Response, Status as TonicStatus};

pub struct FluentWasmVerifierService {
    docker: Arc<Docker>,
    supported_rustc_versions: Vec<String>,
    supported_fluentbase_sdk_versions: Vec<String>,
    default_rustc_version: String,
    default_sdk_version: String,
}

impl FluentWasmVerifierService {
    pub async fn new(
        docker_api_settings: DockerApiSettings,
        verification_settings: VerificationSettings,
    ) -> anyhow::Result<Self> {
        let docker = connect_docker(docker_api_settings.addr.as_str()).await
            .map_err(|e| anyhow::anyhow!("Failed to connect to Docker: {}", e))?;

        Ok(Self {
            docker: Arc::new(docker),
            supported_rustc_versions: verification_settings.supported_rustc_versions,
            supported_fluentbase_sdk_versions: verification_settings.supported_fluentbase_sdk_versions.clone(),
            default_rustc_version: verification_settings.default_rustc_version,
            default_sdk_version: verification_settings.supported_fluentbase_sdk_versions
                .first()
                .cloned()
                .unwrap_or_else(|| "0.1.0".to_string()),
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
        tracing::info!(%request_id, "Received VerifyWasm request");

        // Validate and convert proto request to logic request
        let logic_request = convert_proto_to_logic_request(proto_request)
            .map_err(|e| {
                tracing::error!(%request_id, error = %e, "Failed to convert request");
                TonicStatus::invalid_argument(format!("Invalid request: {}", e))
            })?;

        // Run verification
        let verification_result = verify_contract(&self.docker, logic_request).await
            .map_err(|e| {
                tracing::error!(%request_id, error = %e, "Verification error");
                TonicStatus::internal(format!("Verification failed: {}", e))
            })?;

        tracing::info!(%request_id, status = ?verification_result.status, "Verification completed");
        
        Ok(Response::new(verification_result))
    }

    async fn list_supported_versions(
        &self,
        _request: Request<ListSupportedVersionsRequest>,
    ) -> Result<Response<ListSupportedVersionsResponse>, TonicStatus> {
        let request_id = uuid::Uuid::new_v4();
        tracing::info!(%request_id, "Received ListSupportedVersions request");

        Ok(Response::new(ListSupportedVersionsResponse {
            rustc_versions: self.supported_rustc_versions.clone(),
            sdk_versions: self.supported_fluentbase_sdk_versions.clone(),
            default_rustc_version: self.default_rustc_version.clone(),
            default_sdk_version: self.default_sdk_version.clone(),
        }))
    }
}

/// Converts proto request to logic request
fn convert_proto_to_logic_request(
    proto_request: VerifyWasmRequest,
) -> anyhow::Result<LogicRequest> {
    // Validate required fields
    if proto_request.contract_address.is_empty() {
        return Err(anyhow::anyhow!("contract_address is required"));
    }
    if proto_request.chain_id.is_empty() {
        return Err(anyhow::anyhow!("chain_id is required"));
    }
    if proto_request.rpc_endpoint.is_empty() {
        return Err(anyhow::anyhow!("rpc_endpoint is required"));
    }

    // Validate source
    if proto_request.source.is_none() {
        return Err(anyhow::anyhow!("source (archive_source or git_source) is required"));
    }

    // We can pass the proto request directly since logic uses the same proto types
    Ok(proto_request)
}