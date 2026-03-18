use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;
use workflow_types::{
    AbandonRequest, ClaimRequest, ClaimResponse, CompleteRequest, DepsResponse,
    FailRequest, FactoryListResponse, HeartbeatRequest, Job, JobListResponse,
    JobResponse, JobState, RequeueRequest, RequeueTarget,
};

use crate::error::AppError;
use crate::webhook;
use crate::AppState;

type Result<T> = std::result::Result<T, AppError>;

// ── Job discovery ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct JobListQuery {
    pub state: Option<String>,
}

pub async fn list_jobs(
    State(state): State<Arc<AppState>>,
    Query(params): Query<JobListQuery>,
) -> Result<Json<JobListResponse>> {
    let state_filter = params
        .state
        .as_deref()
        .and_then(JobState::from_label)
        .or_else(|| {
            params.state.as_deref().and_then(|s| {
                // Also accept bare names like "on-deck"
                JobState::from_label(&format!("status:{s}"))
            })
        });

    let jobs = state.graph.get_all_jobs(state_filter.as_ref())?;
    Ok(Json(JobListResponse { jobs }))
}

pub async fn get_job(
    State(s): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
) -> Result<Json<JobResponse>> {
    let key = format!("{owner}/{repo}/{number}");
    let job = s.graph.get_job(&key)?.ok_or(AppError::NotFound)?;
    let claim = s.coord.get_claim(&key).await?;
    Ok(Json(JobResponse { job, claim, failure: None }))
}

pub async fn get_deps(
    State(s): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
) -> Result<Json<DepsResponse>> {
    let key = format!("{owner}/{repo}/{number}");
    let job = s.graph.get_job(&key)?.ok_or(AppError::NotFound)?;

    let mut dependencies: Vec<Job> = Vec::new();
    for dep_num in &job.dependency_numbers {
        let dep_key = format!("{owner}/{repo}/{dep_num}");
        if let Some(dep) = s.graph.get_job(&dep_key)? {
            dependencies.push(dep);
        }
    }

    let all_done = dependencies.iter().all(|j| j.state.is_terminal());
    Ok(Json(DepsResponse { dependencies, all_done }))
}

// ── Job lifecycle ─────────────────────────────────────────────────────────────

pub async fn claim_job(
    State(s): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Json(body): Json<ClaimRequest>,
) -> Result<Json<ClaimResponse>> {
    let key = format!("{owner}/{repo}/{number}");

    let job = s.graph.get_job(&key)?.ok_or(AppError::NotFound)?;

    if job.state != JobState::OnDeck {
        return Err(AppError::Conflict(format!(
            "job is in state {:?}, expected on-deck",
            job.state
        )));
    }

    let timeout_secs = job
        .timeout_secs
        .unwrap_or(s.config.default_timeout_secs);

    let claim = s
        .coord
        .try_claim(&key, body.worker_id.clone(), timeout_secs)
        .await?
        .ok_or_else(|| AppError::Conflict("job is already claimed".into()))?;

    // Transition to on-the-stack
    s.graph.set_state(&key, &JobState::OnTheStack)?;
    s.forgejo
        .set_job_state(&owner, &repo, number, &JobState::OnTheStack)
        .await?;
    s.forgejo
        .set_assignees(&owner, &repo, number, vec![body.worker_id])
        .await?;

    let job = s.graph.get_job(&key)?.ok_or(AppError::NotFound)?;
    Ok(Json(ClaimResponse { job, claim }))
}

pub async fn heartbeat(
    State(s): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Json(body): Json<HeartbeatRequest>,
) -> Result<()> {
    let key = format!("{owner}/{repo}/{number}");
    let ok = s.coord.heartbeat(&key, &body.worker_id).await?;
    if !ok {
        return Err(AppError::Forbidden(
            "not the current claim holder".into(),
        ));
    }
    Ok(())
}

pub async fn complete_job(
    State(s): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Json(body): Json<CompleteRequest>,
) -> Result<()> {
    let key = format!("{owner}/{repo}/{number}");

    verify_claim_holder(&s, &key, &body.worker_id).await?;

    s.coord.release(&key).await?;
    s.graph.set_state(&key, &JobState::InReview)?;
    s.forgejo
        .set_job_state(&owner, &repo, number, &JobState::InReview)
        .await?;
    s.forgejo.clear_assignees(&owner, &repo, number).await?;
    Ok(())
}

pub async fn abandon_job(
    State(s): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Json(body): Json<AbandonRequest>,
) -> Result<()> {
    let key = format!("{owner}/{repo}/{number}");

    verify_claim_holder(&s, &key, &body.worker_id).await?;

    s.coord.release(&key).await?;
    s.graph.set_state(&key, &JobState::OnDeck)?;
    s.forgejo.set_job_state(&owner, &repo, number, &JobState::OnDeck).await?;
    s.forgejo.clear_assignees(&owner, &repo, number).await?;
    Ok(())
}

pub async fn fail_job(
    State(s): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Json(body): Json<FailRequest>,
) -> Result<()> {
    let key = format!("{owner}/{repo}/{number}");

    verify_claim_holder(&s, &key, &body.worker_id).await?;

    let failure = workflow_types::FailureRecord {
        worker_id: body.worker_id.clone(),
        kind: workflow_types::FailureKind::WorkerReported,
        reason: body.reason,
        logs: body.logs,
        failed_at: chrono::Utc::now(),
    };

    s.coord.release(&key).await?;
    s.graph.set_state(&key, &JobState::Failed)?;
    s.forgejo.set_job_state(&owner, &repo, number, &JobState::Failed).await?;
    s.forgejo
        .post_failure_comment(&owner, &repo, number, &failure)
        .await?;
    s.forgejo.clear_assignees(&owner, &repo, number).await?;
    Ok(())
}

pub async fn requeue_job(
    State(s): State<Arc<AppState>>,
    Path((owner, repo, number)): Path<(String, String, u64)>,
    Json(body): Json<RequeueRequest>,
) -> Result<()> {
    let key = format!("{owner}/{repo}/{number}");
    let _ = s.graph.get_job(&key)?.ok_or(AppError::NotFound)?;

    let new_state = match body.target {
        RequeueTarget::OnDeck => JobState::OnDeck,
        RequeueTarget::OnIce => JobState::OnIce,
    };

    s.graph.set_state(&key, &new_state)?;
    s.forgejo.set_job_state(&owner, &repo, number, &new_state).await?;
    Ok(())
}

// ── Webhook ───────────────────────────────────────────────────────────────────

pub async fn receive_webhook(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<workflow_types::WebhookPayload>,
) -> Result<()> {
    // Idempotency: deduplicate using the Forgejo delivery ID header.
    let delivery_id = headers
        .get("X-Gitea-Delivery")
        .or_else(|| headers.get("X-GitHub-Delivery"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");

    let is_new = s.coord.try_mark_delivery(delivery_id).await?;
    if !is_new {
        tracing::debug!(delivery_id, "duplicate webhook delivery, skipping");
        return Ok(());
    }

    webhook::handle_webhook(&s, payload)
        .await
        .map_err(AppError::Internal)?;

    Ok(())
}

// ── Factory endpoints ─────────────────────────────────────────────────────────

pub async fn list_factories(
    State(s): State<Arc<AppState>>,
) -> Result<Json<FactoryListResponse>> {
    let factories = s.registry.list_factories().await;
    Ok(Json(FactoryListResponse { factories }))
}

pub async fn poll_factory(
    State(s): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<()> {
    s.registry
        .poll_factory(&name, Arc::clone(&s))
        .await
        .map_err(|_| AppError::NotFound)?;
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

async fn verify_claim_holder(
    s: &Arc<AppState>,
    key: &str,
    worker_id: &str,
) -> Result<()> {
    match s.coord.get_claim(key).await? {
        Some(claim) if claim.worker_id == worker_id => Ok(()),
        Some(_) => Err(AppError::Forbidden("not the current claim holder".into())),
        None => Err(AppError::Forbidden("job is not claimed".into())),
    }
}
