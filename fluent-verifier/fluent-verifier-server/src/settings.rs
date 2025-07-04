use blockscout_service_launcher::{
    launcher::{ConfigSettings, MetricsSettings, ServerSettings},
    tracing::{JaegerSettings, TracingSettings},
};
use serde::Deserialize;

// Main settings structure for the Fluent Verifier service.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
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

        Ok(())
    }
}

// Implementing Default trait for the main Settings struct.

// Settings for connecting to the Docker API.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct DockerApiSettings {
    /// Address of the Docker API endpoint.
    /// Examples: "unix:///var/run/docker.sock" or "tcp://localhost:2375"
    pub addr: String,
}

impl Default for DockerApiSettings {
    fn default() -> Self {
        Self {
            addr: "unix:///var/run/docker.sock".to_string(),
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
}

impl Default for VerificationSettings {
    fn default() -> Self {
        Self {
            max_archive_size_bytes: 50 * 1024 * 1024, // 50MB (increased from 20MB)
            job_timeout_seconds: 300,                 // 5 minutes
        }
    }
}
