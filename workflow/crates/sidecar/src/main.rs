use axum::{
    response::Html,
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod api;
mod config;
mod consumer;
mod coord;
mod dispatcher;
mod error;
mod forgejo;
mod graph;
mod monitor;
mod registry;
mod reviewer;
mod webhook;

use config::SidecarConfig;
use coord::Coordinator;
use dashmap::DashMap;
use dispatcher::Dispatcher;
use forgejo::ForgejoClient;
use graph::TaskGraph;
use registry::FactoryRegistry;
use reviewer::Reviewer;
use workflow_types::{JournalEntry, WorkerInfo};

/// Shared application state threaded through all axum handlers.
pub struct AppState {
    pub graph: TaskGraph,
    pub coord: Coordinator,
    /// Sync identity — labels, deps, claim management.
    pub forgejo: ForgejoClient,
    /// Dispatcher identity — assignees, comments.
    pub dispatcher_forgejo: ForgejoClient,
    pub config: SidecarConfig,
    pub registry: Arc<FactoryRegistry>,
    /// Dispatcher's worker registry — shared so the API can read it.
    pub dispatch_registry: Arc<DashMap<String, WorkerInfo>>,
    /// Rework routing: job_key → preferred worker_id (the original assignee).
    pub pending_reworks: DashMap<String, String>,
}

impl AppState {
    pub async fn journal(&self, action: &str, comment: &str, job_key: Option<&str>, worker_id: Option<&str>) {
        let entry = JournalEntry {
            timestamp: chrono::Utc::now(),
            action: action.to_string(),
            comment: comment.to_string(),
            job_key: job_key.map(|s| s.to_string()),
            worker_id: worker_id.map(|s| s.to_string()),
        };
        self.coord.append_journal(&entry).await;
    }
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
    let dispatcher_forgejo = ForgejoClient::new(
        &config.dispatcher.forgejo_url,
        &config.dispatcher.forgejo_token,
    );
    let registry = Arc::new(FactoryRegistry::new());

    let reviewer_forgejo = workflow_worker::ForgejoClient::new(
        &config.reviewer.forgejo_url,
        &config.reviewer.forgejo_token,
    );
    let reviewer_human_login = config.reviewer.human_login.clone();
    let reviewer_delay_secs = config.reviewer.delay_secs;

    let dispatch_registry = Arc::new(DashMap::new());
    let pending_reworks = DashMap::new();
    let state = Arc::new(AppState {
        graph, coord, forgejo, dispatcher_forgejo, config,
        registry, dispatch_registry, pending_reworks,
    });

    // Start dispatcher (subscribes to NATS events independently)
    let dispatcher = Arc::new(Dispatcher::new(Arc::clone(&state)));
    Arc::clone(&dispatcher).start().await?;

    // Start reviewer (subscribes to InReview transitions)
    let reviewer = Arc::new(Reviewer::new(
        Arc::clone(&state),
        reviewer_forgejo,
        reviewer_human_login,
        reviewer_delay_secs,
    ));
    Arc::clone(&reviewer).start().await?;

    // Start background tasks
    tokio::spawn(monitor::run_monitor(Arc::clone(&state)));
    consumer::start(Arc::clone(&state)).await?;
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
        // Users
        .route("/repos/:owner/:repo/users", get(api::list_users))
        // Dispatch observability
        .route("/dispatch/workers", get(api::list_dispatch_workers))
        .route("/dispatch/journal", get(api::get_dispatch_journal))
        // Graph viewer
        .route("/graph", get(|| async {
            Html(include_str!("../static/graph.html"))
        }))
        .with_state(state)
}
