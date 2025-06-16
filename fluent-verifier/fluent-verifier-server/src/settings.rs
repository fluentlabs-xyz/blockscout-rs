use blockscout_service_launcher::{
    launcher::{ConfigSettings, MetricsSettings, ServerSettings},
    tracing::{JaegerSettings, TracingSettings},
};
use serde::Deserialize;
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
}

impl ConfigSettings for Settings {
    const SERVICE_NAME: &'static str = "FLUENT_VERIFIER";

    fn validate(&self) -> anyhow::Result<()> {
        // Basic validation for critical verification settings.
        if self.verification.max_archive_size_bytes == 0 {
            anyhow::bail!("verification.max_archive_size_bytes must be greater than 0");
        }
        if self.verification.job_timeout_seconds == 0 {
            anyhow::bail!("verification.job_timeout_seconds must be greater than 0");
        }
        if self.verification.default_rustc_version.is_empty() {
            anyhow::bail!("verification.default_rustc_version must not be empty");
        }
        // Validate default_rustc_version is a valid semver
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
                .expect("Default Docker API URL should be valid"),
        }
    }
}

// Settings specific to the WASM verification process.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VerificationSettings {
    /// Maximum allowed size for uploaded source code archives in bytes.
    pub max_archive_size_bytes: usize,
    /// Timeout for a single verification job in seconds.
    pub job_timeout_seconds: u64,
    /// List of explicitly supported rustc versions (e.g., "1.75.0", "1.78.0").
    pub supported_rustc_versions: Vec<String>,
    /// List of explicitly supported Fluentbase SDK versions (e.g., "0.1.0", "0.2.0").
    pub supported_fluentbase_sdk_versions: Vec<String>,
    /// Default rustc version to use if not specified by user.
    pub default_rustc_version: String,
    /// Prefix for Docker images used for compilation.
    pub docker_image_prefix: String,
}

impl Default for VerificationSettings {
    fn default() -> Self {
        Self {
            max_archive_size_bytes: 20 * 1024 * 1024, // 20MB
            job_timeout_seconds: 300,                 // 5 minutes
            supported_rustc_versions: vec![
                "1.75.0".to_string(),
                "1.78.0".to_string(),
                "1.79.0".to_string(),
            ],
            supported_fluentbase_sdk_versions: vec!["0.1.0".to_string(), "0.2.0".to_string()],
            default_rustc_version: "1.78.0".to_string(),
            docker_image_prefix: "fluentcompile".to_string(),
        }
    }
}
