use bytes::Bytes;
use semver::Version;
use std::collections::BTreeMap;
use url::Url;

/// Source code input method
#[derive(Debug, Clone)]
pub enum SourceCode {
    /// Git repository with URL and commit
    GitRepository { repository_url: Url, commit: String },
    /// Archive file (.tar.gz or .zip)
    Archive {
        content: Bytes,
        format: ArchiveFormat,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum ArchiveFormat {
    TarGz,
    Zip,
}

/// Main verification request
pub struct VerifyRequest {
    /// Source code to verify
    pub source: SourceCode,

    /// Expected rWASM bytecode hash (deployed on-chain)
    pub deployed_bytecode_hash: String,

    /// Compilation settings
    pub compile_settings: CompileSettings,

    /// Server-level default settings (fallback values)
    pub default_settings: VerificationSettings,
}

/// Compilation settings from the request
#[derive(Debug, Clone)]
pub struct CompileSettings {
    /// Rust compiler version (optional - if not set, will be determined)
    pub rustc_version: Option<Version>,

    /// Fluentbase SDK version to use
    pub fluentbase_sdk_version: Version,

    /// Build profile
    pub build_profile: BuildProfile,

    /// Features to enable
    pub features: Vec<String>,

    /// Specific contract name (if multiple in workspace)
    pub contract_name: Option<String>,

    /// Additional cargo flags
    pub cargo_flags: Vec<String>,
}

/// Server-level default settings
#[derive(Debug, Clone)]
pub struct VerificationSettings {
    /// Default Rust compiler version
    pub default_rustc_version: Version,

    /// Docker daemon URL
    pub docker_url: String,

    /// Base Docker image repository
    pub docker_image_prefix: String,

    /// Compilation timeout
    pub compile_timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub enum BuildProfile {
    Debug,
    Release,
    Custom(String),
}

/// Successful verification result
pub struct VerificationSuccess {
    /// Contract metadata
    pub contract_name: String,
    pub package_name: String,

    /// Generated artifacts
    pub wasm_bytecode: Bytes,
    pub rwasm_bytecode: Bytes,
    pub abi: serde_json::Value,
    pub build_metadata: serde_json::Value,

    /// Source files for transparency
    pub source_files: BTreeMap<String, String>,

    /// Versions actually used
    pub rustc_version: Version,
    pub fluentbase_sdk_version: Version,

    /// Compilation logs
    pub compile_logs: String,
}

/// Version resolution result
pub struct ResolvedVersions {
    pub rustc_version: Version,
    pub rustc_source: RustcVersionSource,
    pub fluentbase_sdk_version: Version,
}

#[derive(Debug, Clone)]
pub enum RustcVersionSource {
    UserRequest,
    RustToolchainFile,
    ServerDefault,
}
