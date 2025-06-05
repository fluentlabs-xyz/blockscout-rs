use blockscout_service_launcher::{
    launcher::{ConfigSettings, MetricsSettings, ServerSettings},
    tracing::{JaegerSettings, TracingSettings},
};
use serde::Deserialize;
use std::{num::NonZeroUsize, path::PathBuf};
use url::Url;

// Main settings structure for the Fluent Verifier service.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub server: ServerSettings,
    pub metrics: MetricsSettings,
    pub tracing: TracingSettings,
    pub jaeger: JaegerSettings,
    pub docker_api: DockerApiSettings,
    pub verification: VerificationSettings,
    pub compilers_concurrency: CompilersConcurrencySettings,
}

impl ConfigSettings for Settings {
    const SERVICE_NAME: &'static str = "FLUENT_VERIFIER";

    fn validate(&self) -> anyhow::Result<()> {
        // Basic validation for critical verification settings.
        if self.verification.max_archive_size_bytes == 0 {
            anyhow::bail!("verification.max_archive_size_bytes must be greater than 0");
        }
        if self.verification.max_concurrent_jobs == 0 {
            anyhow::bail!("verification.max_concurrent_jobs must be greater than 0");
        }
        if self.verification.job_timeout_seconds == 0 {
            anyhow::bail!("verification.job_timeout_seconds must be greater than 0");
        }
        if self.verification.default_rustc_version.is_empty() {
            anyhow::bail!("verification.default_rustc_version must not be empty");
        }
        // Validate default_rustc_version is a valid semver (optional, but good practice)
        if semver::Version::parse(&self.verification.default_rustc_version).is_err() {
            anyhow::bail!(
                "verification.default_rustc_version ('{}') is not a valid semantic version",
                self.verification.default_rustc_version
            );
        }
        if self.verification.docker_image_prefix.is_empty() {
            anyhow::bail!("verification.docker_image_prefix must not be empty");
        }

        Ok(())
    }
}

// Implementing Default trait for the main Settings struct.
impl Default for Settings {
    fn default() -> Self {
        Self {
            server: Default::default(),
            metrics: Default::default(),
            tracing: Default::default(),
            jaeger: Default::default(),
            docker_api: Default::default(),
            verification: Default::default(),
            compilers_concurrency: Default::default(),
        }
    }
}

// Settings for connecting to the Docker API.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct DockerApiSettings {
    /// Address of the Docker API endpoint.
    pub addr: Url,
}

impl Default for DockerApiSettings {
    fn default() -> Self {
        Self {
            addr: Url::parse("unix:///var/run/docker.sock")
                .expect("Default Docker API URL ('unix:///var/run/docker.sock') should be valid"),
        }
    }
}

// Settings specific to the WASM verification process.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationSettings {
    /// Global switch to enable/disable WASM verification functionality.
    pub enabled: bool,
    /// Maximum allowed size for uploaded source code archives in bytes.
    pub max_archive_size_bytes: usize,
    /// Timeout for a single verification job (includes unpacking, compilation, and comparison) in seconds.
    pub job_timeout_seconds: u64,
    /// Optional base directory for temporary files (e.g., unpacked archives, build artifacts).
    /// If None, the system's default temporary directory is used.
    pub temp_dir: Option<PathBuf>,
    /// List of explicitly supported rustc versions (e.g., "1.75.0", "1.78.0").
    /// Behavior for unsupported versions depends on `strict_version_matching`.
    pub supported_rustc_versions: Vec<String>,
    /// List of explicitly supported Fluentbase SDK versions or identifiers (e.g., "0.1.0", "path").
    pub supported_fluentbase_sdk_versions: Vec<String>,
    /// If true, requests with rustc/SDK versions not in the supported lists will be rejected.
    /// If false, the service might attempt to use the requested versions (e.g., via rustup in Docker).
    pub strict_version_matching: bool,
    /// Maximum number of concurrent verification jobs (e.g., parallel Docker container runs).
    pub max_concurrent_jobs: usize,
    // /// Optional: Base Docker image for Rust compilation, if a strategy of dynamic rustc installation is used.
    // pub rust_compiler_base_image: String,
    /// Default rustc version to use if not specified by user or rust-toolchain.toml.
    /// e.g., "1.78.0"
    pub default_rustc_version: String,
    /// Prefix for Docker images used for compilation.
    /// e.g., "fluentlabs/verifier" would result in images like "fluentlabs/verifier/fluent-compiler:rust-1.78.0"
    pub docker_image_prefix: String,
}

impl Default for VerificationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_archive_size_bytes: 20 * 1024 * 1024, // 20MB
            job_timeout_seconds: 300,                 // 5 minutes
            temp_dir: None,                           // System default
            supported_rustc_versions: vec![], // Default to empty; recommend configuring explicitly
            supported_fluentbase_sdk_versions: vec![], // TODO(d1r1): configure supported versions
            strict_version_matching: false,   // Be flexible by default
            max_concurrent_jobs: std::thread::available_parallelism().map_or(2, NonZeroUsize::get),
            // rust_compiler_base_image: "rust:latest".to_string(), // Example if used
            default_rustc_version: "1.78.0".to_string(), // Added Default
            docker_image_prefix: "fluentcompile".to_string(), // Added Default (choose a suitable default)
        }
    }
}

// Settings for managing concurrency of compilation processes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompilersConcurrencySettings {
    /// Maximum number of threads a single compilation job (e.g., `cargo build` inside a container)
    /// might try to use. This can be used to set environment variables like `RAYON_NUM_THREADS`.
    pub max_threads_per_job: NonZeroUsize,
}

impl Default for CompilersConcurrencySettings {
    fn default() -> Self {
        let default_threads = std::thread::available_parallelism().unwrap_or_else(|e| {
            tracing::warn!(
                "Failed to get available parallelism for compiler threads, defaulting to 2: {}",
                e
            );
            NonZeroUsize::new(2).expect("2 is a non-zero value")
        });
        Self {
            max_threads_per_job: default_threads,
        }
    }
}
