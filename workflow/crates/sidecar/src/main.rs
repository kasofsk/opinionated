use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod api;
mod config;
mod coord;
mod error;
mod forgejo;
mod graph;
mod monitor;
mod registry;
mod webhook;

use config::SidecarConfig;
use coord::Coordinator;
use forgejo::ForgejoClient;
use graph::TaskGraph;
use registry::FactoryRegistry;

/// Shared application state threaded through all axum handlers.
pub struct AppState {
    pub graph: TaskGraph,
    pub coord: Coordinator,
    pub forgejo: ForgejoClient,
    pub config: SidecarConfig,
    pub registry: Arc<FactoryRegistry>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let config = SidecarConfig::from_env()?;

    let graph = TaskGraph::open(&config.db_path)?;
    let coord = Coordinator::new(&config.nats.url).await?;
    let forgejo = ForgejoClient::new(&config.forgejo.url, &config.forgejo.token);
    let registry = Arc::new(FactoryRegistry::new());

    let state = Arc::new(AppState { graph, coord, forgejo, config, registry });

    // Start background tasks
    tokio::spawn(monitor::run_monitor(Arc::clone(&state)));
    Arc::clone(&state.registry).start(Arc::clone(&state)).await;

    let app = build_router(Arc::clone(&state));

    let addr: std::net::SocketAddr = state.config.listen_addr.parse()?;
    tracing::info!("listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        // Webhook
        .route("/webhook", post(api::receive_webhook))
        // Job discovery
        .route("/jobs", get(api::list_jobs))
        .route("/jobs/:owner/:repo/:number", get(api::get_job))
        .route("/jobs/:owner/:repo/:number/deps", get(api::get_deps))
        // Job lifecycle
        .route("/jobs/:owner/:repo/:number/claim", post(api::claim_job))
        .route("/jobs/:owner/:repo/:number/heartbeat", post(api::heartbeat))
        .route("/jobs/:owner/:repo/:number/complete", post(api::complete_job))
        .route("/jobs/:owner/:repo/:number/abandon", post(api::abandon_job))
        .route("/jobs/:owner/:repo/:number/fail", post(api::fail_job))
        .route("/jobs/:owner/:repo/:number/requeue", post(api::requeue_job))
        // Factory observability
        .route("/factories", get(api::list_factories))
        .route("/factories/:name/poll", post(api::poll_factory))
        .with_state(state)
}
