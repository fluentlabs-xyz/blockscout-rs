use crate::{
    docker_runner::run_compiler,
    error::VerificationError,
    source,
    types::*,
    version::resolve_versions,
};
use bollard::Docker;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Main entry point for contract verification
pub async fn verify_contract(
    docker: &Docker,
    request: VerifyRequest,
) -> Result<VerificationSuccess, VerificationError> {
    // Step 1: Prepare source code
    tracing::info!("Preparing source code");
    let source_dir = source::prepare_source(&request.source).await?;
    let project_path = source_dir.path();

    // Step 2: Validate project structure
    tracing::info!("Validating project structure");
    source::validate_project_structure(project_path).await?;

    // Step 3: Resolve versions
    tracing::info!("Resolving compiler versions");
    let resolved_versions = resolve_versions(
        project_path,
        &request.compile_settings,
        &request.default_settings,
    )
    .await?;

    tracing::info!(
        "Using rustc {} (source: {:?}), fluentbase-sdk {}",
        resolved_versions.rustc_version,
        resolved_versions.rustc_source,
        resolved_versions.fluentbase_sdk_version
    );

    // Step 4: Run compilation in Docker
    tracing::info!("Running compilation");
    let compiler_output = run_compiler(
        docker,
        project_path,
        &request.compile_settings,
        &resolved_versions,
        &request.default_settings.docker_image_prefix,
        request.default_settings.compile_timeout_secs,
    )
    .await
    .map_err(|e| VerificationError::CompilationError(e.to_string()))?;

    // Step 5: Verify bytecode hash
    tracing::info!("Verifying bytecode hash");
    let computed_hash = compute_bytecode_hash(&compiler_output.rwasm_bytecode);
    if computed_hash != request.deployed_bytecode_hash {
        return Err(VerificationError::BytecodeMismatch {
            expected: request.deployed_bytecode_hash,
            actual: computed_hash,
        });
    }

    // Step 6: Collect source files
    tracing::info!("Collecting source files");
    let source_files = collect_source_files(project_path).await?;

    // Step 7: Build success response
    Ok(VerificationSuccess {
        contract_name: compiler_output.contract_name,
        package_name: compiler_output.package_name,
        wasm_bytecode: compiler_output.wasm_bytecode,
        rwasm_bytecode: compiler_output.rwasm_bytecode,
        abi: compiler_output.abi,
        build_metadata: compiler_output.metadata,
        source_files,
        rustc_version: resolved_versions.rustc_version,
        fluentbase_sdk_version: resolved_versions.fluentbase_sdk_version,
        compile_logs: compiler_output.compile_logs,
    })
}

fn compute_bytecode_hash(bytecode: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytecode))
}

async fn collect_source_files(
    project_dir: &std::path::Path,
) -> Result<BTreeMap<String, String>, VerificationError> {
    use tokio::fs;
    use walkdir::WalkDir;

    let mut files = BTreeMap::new();

    for entry in WalkDir::new(project_dir)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();

        // Skip directories and non-relevant files
        if !path.is_file() {
            continue;
        }

        // Skip target and .git directories
        if path
            .components()
            .any(|c| matches!(c.as_os_str().to_str(), Some("target") | Some(".git")))
        {
            continue;
        }

        // Include relevant files
        let include = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext == "rs" || ext == "toml" || ext == "lock")
            .unwrap_or(false);

        if include {
            let relative_path = path
                .strip_prefix(project_dir)
                .unwrap()
                .to_string_lossy()
                .to_string();

            let content = fs::read_to_string(path).await.map_err(|e| {
                VerificationError::InvalidProject(format!(
                    "Failed to read {relative_path}: {e}"
                ))
            })?;

            files.insert(relative_path, content);
        }
    }

    Ok(files)
}
