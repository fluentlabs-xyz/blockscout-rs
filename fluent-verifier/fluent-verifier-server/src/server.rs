use crate::{
    proto::{
        health_actix::route_health, health_server::HealthServer,
        wasm_verifier_actix::route_wasm_verifier, // http
        wasm_verifier_server::WasmVerifierServer, // grpc
    },
    services::{
        HealthService, FluentWasmVerifierService
    },
    settings::Settings,
};
use blockscout_service_launcher::{
    launcher, launcher::LaunchSettings, tracing};
use std::sync::Arc;
use tokio::sync::Semaphore;



const SERVICE_NAME: &str = "fluent_verifier";

#[derive(Clone)]
struct Router {
    health: Arc<HealthService>,
    wasm_verifier: Arc<FluentWasmVerifierService>,
}

impl Router {
    pub fn grpc_router(&self) -> tonic::transport::server::Router {
        tonic::transport::Server::builder()
            .add_service(HealthServer::from_arc(self.health.clone()))
            .add_service(
                WasmVerifierServer::from_arc(self.wasm_verifier.clone()),
            )
    }
}

impl launcher::HttpRouter for Router {
    fn register_routes(&self, service_config: &mut actix_web::web::ServiceConfig) {
        service_config.configure(|config| route_health(config, self.health.clone()));
        service_config.configure(|config| {
            route_wasm_verifier(config, self.wasm_verifier.clone())
        });
    }
}

pub async fn run(settings: Settings) -> Result<(), anyhow::Error> {
    tracing::init_logs(SERVICE_NAME, &settings.tracing, &settings.jaeger)?;

    let health = Arc::new(HealthService::default());

    // Create the semaphore using max_concurrent_jobs from settings
    // Ensure at least 1 concurrent job is allowed.
    let max_jobs = settings.verification.max_concurrent_jobs;
    let verification_semaphore = Arc::new(Semaphore::new(if max_jobs == 0 { 1 } else { max_jobs }));

    let wasm_verifier = Arc::new(FluentWasmVerifierService::new(&settings, verification_semaphore.clone()).await.expect(
        "Failed to create FluentWasmVerifierService"
    ));

    let router = Router {
        health,
        wasm_verifier,
    };

    let grpc_router = router.grpc_router();
    let http_router = router;

    let launch_settings = LaunchSettings {
        service_name: SERVICE_NAME.to_string(),
        server: settings.server,
        metrics: settings.metrics,
    };

    launcher::launch(&launch_settings, http_router, grpc_router).await
}
