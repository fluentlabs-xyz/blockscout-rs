use crate::{
    proto::{
        self,
        wasm_verifier_server::WasmVerifier,
        ListSupportedVersionsRequest, ListSupportedVersionsResponse,
        Status as VerificationStatusProto, VerifyWasmRequest, VerifyWasmResponse,
        WasmVerificationFailure, WasmVerificationSuccess,
        verify_wasm_request::SourceDetails, 
        ArchiveSourceDetails, GitSourceDetails, 
    },
    settings::Settings as ServerSettings,
};
use async_trait::async_trait;
use fluent_verifier_logic::{
    connect_docker, verify_contract,
    types::{
        VerifyRequest, SourceCode,
        CompileSettings,
        BuildProfile, VerificationSettings as LogicVerificationSettings,
        VerificationSuccess,
    },
    VerificationError,
};
use bollard::Docker;
use bytes::Bytes;
use semver::Version;
use std::{collections::BTreeMap, sync::Arc}; 
use tokio::sync::Semaphore;
use tonic::{Request, Response, Status as TonicStatus};
use url::Url;

pub struct FluentWasmVerifierService {
    docker: Docker,
    verification_settings_logic: LogicVerificationSettings,
    verification_semaphore: Arc<Semaphore>,
    // Added to store configured supported versions
    supported_rustc_versions_config: Vec<String>,
    supported_fluentbase_sdk_versions_config: Vec<String>,
}

impl FluentWasmVerifierService {
    pub async fn new(
        server_settings: &ServerSettings,
        verification_semaphore: Arc<Semaphore>,
    ) -> anyhow::Result<Self> {
        let docker = connect_docker(server_settings.docker_api.addr.as_str()).await
            .map_err(|e| anyhow::anyhow!("Failed to connect to Docker: {}", e))?;

        let default_rustc_ver = Version::parse(&server_settings.verification.default_rustc_version)
            .map_err(|e| anyhow::anyhow!("Invalid default_rustc_version in settings: {}", e))?;

        let verification_settings_logic = LogicVerificationSettings {
            default_rustc_version: default_rustc_ver,
            // docker_url is not part of LogicVerificationSettings if we simplify it,
            // otherwise: server_settings.docker_api.addr.to_string(),
            docker_image_prefix: server_settings.verification.docker_image_prefix.clone(),
            docker_url: server_settings.docker_api.addr.to_string(),
            compile_timeout_secs: server_settings.verification.job_timeout_seconds,
        };

        Ok(Self {
            docker,
            verification_settings_logic,
            verification_semaphore,
            supported_rustc_versions_config: server_settings.verification.supported_rustc_versions.clone(),
            supported_fluentbase_sdk_versions_config: server_settings.verification.supported_fluentbase_sdk_versions.clone(),
        })
    }
}

#[async_trait]
impl WasmVerifier for FluentWasmVerifierService {
    async fn verify_wasm(
        &self,
        request: Request<VerifyWasmRequest>,
    ) -> Result<Response<VerifyWasmResponse>, TonicStatus> {
        let _permit = match self.verification_semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                tracing::warn!("Verification limit reached, request rejected temporarily.");
                return Err(TonicStatus::resource_exhausted(
                    "Service is at maximum capacity, please try again later.",
                ));
            }
        };

        let proto_request = request.into_inner();
        let request_id = uuid::Uuid::new_v4();
        tracing::info!(%request_id, "Received VerifyWasm request");

        let logic_request = match convert_proto_to_logic_request(proto_request, &self.verification_settings_logic) {
            Ok(req) => req,
            Err(e) => {
                tracing::error!(%request_id, error = %e, "Failed to convert request");
                return Ok(Response::new(VerifyWasmResponse {
                    status: VerificationStatusProto::Failure.into(),
                    success_details: None,
                    failure_details: Some(WasmVerificationFailure {
                        error_message: format!("Invalid request: {}", e),
                        compiler_output: None,
                        generated_rwasm_bytecode_hash: None,
                    }),
                }));
            }
        };

        let verification_result = verify_contract(&self.docker, logic_request).await;
        process_logic_result_to_response(verification_result, request_id)
    }

    async fn list_supported_versions(
        &self,
        _request: Request<ListSupportedVersionsRequest>,
    ) -> Result<Response<ListSupportedVersionsResponse>, TonicStatus> {
        let request_id = uuid::Uuid::new_v4();
        tracing::info!(%request_id, "Received ListSupportedVersions request");

        Ok(Response::new(ListSupportedVersionsResponse {
            rustc_versions: self.supported_rustc_versions_config.clone(),
            fluentbase_sdk_versions: self.supported_fluentbase_sdk_versions_config.clone(),
        }))
    }
}

fn convert_proto_to_logic_request(
    proto_request: VerifyWasmRequest,
    default_settings_logic: &LogicVerificationSettings,
) -> anyhow::Result<VerifyRequest> {
    let proto_compile_settings = proto_request.compile_settings
        .ok_or_else(|| anyhow::anyhow!("compile_settings is missing in the request"))?;

    let rustc_version = if !proto_compile_settings.rustc_version.is_empty() {
        let version_parts: Vec<&str> = proto_compile_settings.rustc_version.split_whitespace().collect();
        let version_str = if version_parts.len() >= 2 && version_parts[0].to_lowercase() == "rustc" {
            version_parts[1]
        } else {
            &proto_compile_settings.rustc_version
        }
        .split('-')
        .next()
        .ok_or_else(|| anyhow::anyhow!("Invalid rustc version format (after splitting by '-')"))?;
        Some(Version::parse(version_str).map_err(|e| anyhow::anyhow!("Failed to parse rustc semver '{}': {}", version_str, e))?)
    } else {
        None
    };

    let sdk_version_str = &proto_compile_settings.fluentbase_sdk_version;
    let sdk_version = if sdk_version_str.is_empty() {
        return Err(anyhow::anyhow!("fluentbase_sdk_version is missing in compile_settings"));
    } else {
        Version::parse(sdk_version_str)
            .map_err(|e| anyhow::anyhow!("Invalid fluentbase_sdk_version '{}': {}", sdk_version_str, e))?
    };

    let build_profile = match proto_compile_settings.profile.as_str() {
        "" | "release" => BuildProfile::Release,
        "debug" => BuildProfile::Debug,
        custom => BuildProfile::Custom(custom.to_string()),
    };

    let features = proto_compile_settings.features;
    let cargo_flags = proto_compile_settings.cargo_flags;

    let (source_code_logic, path_to_cargo_toml_for_name) = match proto_request.source_details {
        Some(SourceDetails::ArchiveSource(archive_details)) => {
            if archive_details.source_code_archive.is_empty() {
                return Err(anyhow::anyhow!("archive_source.source_code_archive is empty"));
            }
            let archive_format = fluent_verifier_logic::source::archive::detect_format(
                &Bytes::from(archive_details.source_code_archive.clone()),
            )
            .map_err(|e| anyhow::anyhow!("Failed to detect archive format: {:?}", e))?;
            (
                SourceCode::Archive {
                    content: Bytes::from(archive_details.source_code_archive),
                    format: archive_format,
                },
                archive_details.path_to_cargo_toml_in_archive,
            )
        }
        Some(SourceDetails::GitSource(git_details)) => {
            if git_details.repository_url.is_empty() {
                return Err(anyhow::anyhow!("git_source.repository_url is empty"));
            }
            if git_details.commit_reference.is_empty() {
                return Err(anyhow::anyhow!("git_source.commit_reference is empty"));
            }
            let repo_url = Url::parse(&git_details.repository_url)
                .map_err(|e| anyhow::anyhow!("Invalid repository_url: {}", e))?;
            (
                SourceCode::GitRepository {
                    repository_url: repo_url,
                    commit: git_details.commit_reference,
                },
                git_details.path_to_cargo_toml_in_repository,
            )
        }
        None => return Err(anyhow::anyhow!("source_details (archive_source or git_source) is missing")),
    };

    let contract_name = if !path_to_cargo_toml_for_name.is_empty() {
        std::path::Path::new(&path_to_cargo_toml_for_name)
            .parent()
            .and_then(std::path::Path::file_name)
            .and_then(std::ffi::OsStr::to_str)
            .map(String::from)
    } else {
        None
    };

    Ok(VerifyRequest {
        source: source_code_logic,
        deployed_bytecode_hash: proto_request.deployed_rwasm_bytecode_hash,
        compile_settings: CompileSettings {
            rustc_version,
            fluentbase_sdk_version: sdk_version,
            build_profile,
            features,
            contract_name,
            cargo_flags,
        },
        default_settings: default_settings_logic.clone(),
    })
}

fn process_logic_result_to_response(
    result: Result<VerificationSuccess, VerificationError>,
    request_id: uuid::Uuid,
) -> Result<Response<VerifyWasmResponse>, TonicStatus> {
    match result {
        Ok(success) => {
            tracing::info!(%request_id, contract_name = %success.contract_name, "Verification successful");
            let proto_success = convert_logic_success_to_proto(success);
            Ok(Response::new(VerifyWasmResponse {
                status: VerificationStatusProto::Success.into(),
                success_details: Some(proto_success),
                failure_details: None,
            }))
        }
        Err(error) => {
            tracing::warn!(%request_id, error = %error, "Verification failed");
            let failure_details = convert_logic_error_to_proto(error);
            Ok(Response::new(VerifyWasmResponse {
                status: VerificationStatusProto::Failure.into(),
                success_details: None,
                failure_details: Some(failure_details),
            }))
        }
    }
}

fn convert_logic_success_to_proto(success: VerificationSuccess) -> WasmVerificationSuccess {
    use proto::wasm_verification_success::{BytecodeObject, BuildMetadata};
    use proto::wasm_verification_success::build_metadata::{CompilerInfo, BuildOutputInfo, WasmArtifactInfo, BuildSettingsInfo, ContractBuildInfo};

    let method_identifiers_map = extract_method_identifiers_btreemap(&success.abi);

    let wasm_bytecode_object = BytecodeObject {
        object: format!("0x{}", hex::encode(&success.wasm_bytecode)),
        source_map: None,
    };
    let rwasm_bytecode_object = BytecodeObject {
        object: format!("0x{}", hex::encode(&success.rwasm_bytecode)),
        source_map: None,
    };

    let build_metadata_json = &success.build_metadata;

    let compiler_commit_from_json = build_metadata_json
        .get("compiler_commit")
        .and_then(|v| v.as_str())
        .map(String::from);

    let compiler_info = CompilerInfo {
        name: "rustc".to_string(),
        version: format!("rustc {}", success.rustc_version),
        commit: compiler_commit_from_json,
    };

    let output_info = BuildOutputInfo {
        wasm: Some(WasmArtifactInfo {
            hash: sha256_hex(&success.wasm_bytecode),
            size: success.wasm_bytecode.len() as u64,
        }),
        rwasm: Some(WasmArtifactInfo {
            hash: sha256_hex(&success.rwasm_bytecode),
            size: success.rwasm_bytecode.len() as u64,
        }),
    };

    let path_to_cargo_toml = build_metadata_json
        .get("path_to_cargo_toml")
        .and_then(|v| v.as_str())
        .unwrap_or("Cargo.toml")
        .to_string();

    let contract_version_from_json = build_metadata_json
        .get("contract_version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    
    let features_from_json: Vec<String> = build_metadata_json
        .get("features_used")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let no_default_features_from_json = build_metadata_json
        .get("no_default_features_used")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let cargo_flags_used_from_json: Vec<String> = build_metadata_json
        .get("cargo_flags_used")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let contract_info = ContractBuildInfo {
        path_to_cargo_toml,
        name: success.package_name.clone(),
        version: contract_version_from_json,
        sdk_version: Some(success.fluentbase_sdk_version.to_string()),
    };

    let settings_info = BuildSettingsInfo {
        target_triple: build_metadata_json.get("target").and_then(|v|v.as_str()).unwrap_or("wasm32-unknown-unknown").to_string(),
        profile: build_metadata_json.get("profile").and_then(|v|v.as_str()).unwrap_or("release").to_string(),
        features: features_from_json,
        no_default_features: no_default_features_from_json,
        contract_info: Some(contract_info),
        build_time_utc_seconds: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        cargo_flags_used: cargo_flags_used_from_json,
    };

    let source_files_map = convert_source_files_btreemap(&success.source_files);

    let build_metadata_proto = BuildMetadata {
        compiler: Some(compiler_info),
        language: "Rust".to_string(),
        output: Some(output_info),
        settings: Some(settings_info),
        sources: source_files_map,
        metadata_format_version: 1,
    };

    WasmVerificationSuccess {
        contract_name: success.contract_name.clone(),
        abi_json_string: Some(success.abi.to_string()),
        wasm_bytecode: Some(wasm_bytecode_object),
        rwasm_bytecode: Some(rwasm_bytecode_object),
        method_identifiers: method_identifiers_map,
        build_metadata: Some(build_metadata_proto),
    }
}

fn convert_source_files_btreemap(files: &BTreeMap<String, String>) -> BTreeMap<String, proto::wasm_verification_success::build_metadata::SourceFileInfo> {
    use proto::wasm_verification_success::build_metadata::SourceFileInfo;
    files.iter()
        .map(|(path, content)| {
            (path.clone(), SourceFileInfo {
                content_hash: sha256_hex(content.as_bytes()),
                license_identifier: extract_spdx_license(content),
            })
        })
        .collect()
}

fn convert_logic_error_to_proto(error: VerificationError) -> WasmVerificationFailure {
    match error {
        VerificationError::BytecodeMismatch { expected, actual } => WasmVerificationFailure {
            error_message: format!("Bytecode mismatch: expected 0x{}, got 0x{}", expected, actual),
            compiler_output: None,
            generated_rwasm_bytecode_hash: Some(format!("0x{}", actual)),
        },
        VerificationError::CompilationError(msg) => {
            WasmVerificationFailure {
                error_message: format!("Compilation failed: {}", msg),
                compiler_output: Some(msg),
                generated_rwasm_bytecode_hash: None, // Potentially parse from msg if available
            }
        },
        VerificationError::SourceError(source_err) => WasmVerificationFailure {
            error_message: format!("Source error: {}", source_err),
            compiler_output: None,
            generated_rwasm_bytecode_hash: None,
        },
        VerificationError::VersionError(version_err) => WasmVerificationFailure {
            error_message: format!("Version error: {}", version_err),
            compiler_output: None,
            generated_rwasm_bytecode_hash: None,
        },
        VerificationError::InvalidProject(msg) => WasmVerificationFailure {
            error_message: format!("Invalid project: {}", msg),
            compiler_output: None,
            generated_rwasm_bytecode_hash: None,
        },
        VerificationError::DockerError(err) => WasmVerificationFailure {
            error_message: format!("Docker interaction error: {}", err),
            compiler_output: Some(err.to_string()),
            generated_rwasm_bytecode_hash: None,
        },
        VerificationError::Timeout(secs) => WasmVerificationFailure {
            error_message: format!("Verification timed out after {} seconds", secs),
            compiler_output: None,
            generated_rwasm_bytecode_hash: None,
        },
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(data))
}

fn extract_method_identifiers_btreemap(abi_val: &serde_json::Value) -> BTreeMap<String, String> {
    use sha3::{Digest, Keccak256};
    let mut identifiers = BTreeMap::new();

    if let Some(abi_array) = abi_val.as_array() {
        for item in abi_array {
            if item.get("type").and_then(|t| t.as_str()) == Some("function") {
                if let (Some(name), Some(inputs)) = (
                    item.get("name").and_then(|n| n.as_str()),
                    item.get("inputs").and_then(|i| i.as_array())
                ) {
                    let types: Vec<String> = inputs.iter()
                        .filter_map(|input| input.get("type").and_then(|t| t.as_str()))
                        .map(|s| s.to_string())
                        .collect();

                    let signature = format!("{}({})", name, types.join(","));
                    let hash = Keccak256::digest(signature.as_bytes());
                    let selector = hex::encode(&hash[..4]);

                    identifiers.insert(signature, selector);
                }
            }
        }
    }
    identifiers
}

fn extract_spdx_license(content: &str) -> Option<String> {
    for line in content.lines().take(20) {
        if let Some(pos) = line.to_lowercase().find("spdx-license-identifier:") {
            let license_part = line[pos + "spdx-license-identifier:".len()..].trim();
            let license = license_part
                .trim_start_matches(|c: char| c.is_whitespace() || c == '/' || c == '*' || c == '#' || c == ';')
                .trim_end_matches(|c: char| c.is_whitespace() || c == '*' || c == '/')
                .trim();
            if !license.is_empty() {
                return Some(license.to_string());
            }
        }
    }
    None
}