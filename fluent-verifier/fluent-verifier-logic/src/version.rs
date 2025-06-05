use crate::error::VersionError;
use crate::types::{CompileSettings, ResolvedVersions, RustcVersionSource, VerificationSettings};
use semver::Version;
use std::path::Path;
use tokio::fs;

/// Resolves the rustc version to use based on priority rules
pub async fn resolve_versions(
    project_dir: &Path,
    compile_settings: &CompileSettings,
    default_settings: &VerificationSettings,
) -> Result<ResolvedVersions, VersionError> {
    // Step 1: Resolve rustc version
    let (rustc_version, rustc_source) = resolve_rustc_version(
        project_dir,
        compile_settings.rustc_version.as_ref(),
        &default_settings.default_rustc_version,
    )
    .await?;

    // Step 2: Validate rustc version is supported
    validate_rustc_version(&rustc_version)?;

    Ok(ResolvedVersions {
        rustc_version,
        rustc_source,
        fluentbase_sdk_version: compile_settings.fluentbase_sdk_version.clone(),
    })
}

async fn resolve_rustc_version(
    project_dir: &Path,
    user_version: Option<&Version>,
    default_version: &Version,
) -> Result<(Version, RustcVersionSource), VersionError> {
    // Priority 1: User-provided version
    if let Some(version) = user_version {
        return Ok((version.clone(), RustcVersionSource::UserRequest));
    }

    // Priority 2: rust-toolchain.toml or rust-toolchain file
    if let Some(version) = read_toolchain_version(project_dir).await? {
        return Ok((version, RustcVersionSource::RustToolchainFile));
    }

    // Priority 3: Server default
    Ok((default_version.clone(), RustcVersionSource::ServerDefault))
}

async fn read_toolchain_version(project_dir: &Path) -> Result<Option<Version>, VersionError> {
    // Try rust-toolchain.toml first
    let toolchain_toml = project_dir.join("rust-toolchain.toml");
    if toolchain_toml.exists() {
        let content = fs::read_to_string(&toolchain_toml).await.map_err(|e| {
            VersionError::ToolchainParseError(format!("Failed to read file: {e}"))
        })?;

        return parse_toolchain_toml(&content);
    }

    // Try rust-toolchain file
    let toolchain_file = project_dir.join("rust-toolchain");
    if toolchain_file.exists() {
        let content = fs::read_to_string(&toolchain_file).await.map_err(|e| {
            VersionError::ToolchainParseError(format!("Failed to read file: {e}"))
        })?;

        return parse_toolchain_file(&content);
    }

    Ok(None)
}

fn parse_toolchain_toml(content: &str) -> Result<Option<Version>, VersionError> {
    let toml: toml::Value = toml::from_str(content)
        .map_err(|e| VersionError::ToolchainParseError(format!("Invalid TOML: {e}")))?;

    // Extract channel from [toolchain] section
    let channel = toml
        .get("toolchain")
        .and_then(|t| t.get("channel"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| {
            VersionError::ToolchainParseError("Missing toolchain.channel".to_string())
        })?;

    parse_rustc_channel(channel).map(Some)
}

fn parse_toolchain_file(content: &str) -> Result<Option<Version>, VersionError> {
    let channel = content.trim();
    parse_rustc_channel(channel).map(Some)
}

fn parse_rustc_channel(channel: &str) -> Result<Version, VersionError> {
    // Reject generic channels
    if channel == "stable" || channel == "beta" || channel == "nightly" {
        return Err(VersionError::InvalidToolchainVersion(
            "Must specify exact version, not generic channel".to_string(),
        ));
    }

    // Handle nightly-YYYY-MM-DD format
    if channel.starts_with("nightly-") {
        return Err(VersionError::InvalidToolchainVersion(
            "Nightly versions not supported for reproducible builds".to_string(),
        ));
    }

    // Parse as semver
    Version::parse(channel)
        .map_err(|e| VersionError::InvalidToolchainVersion(format!("Invalid version: {e}")))
}

fn validate_rustc_version(version: &Version) -> Result<(), VersionError> {
    // Example validation: ensure version is >= 1.70.0
    let min_version = Version::new(1, 70, 0);
    if version < &min_version {
        return Err(VersionError::UnsupportedRustcVersion {
            version: version.clone(),
            reason: format!("Minimum supported version is {min_version}"),
        });
    }

    Ok(())
}
