use std::sync::Arc;
use workflow_types::{FailureKind, FailureRecord};

use crate::AppState;

/// Background task: scan all active claims periodically and time out stale ones.
pub async fn run_monitor(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(
        state.config.monitor_interval_secs,
    ));

    loop {
        interval.tick().await;
        if let Err(e) = check_timeouts(&state).await {
            tracing::error!("monitor error: {e:#}");
        }
    }
}

async fn check_timeouts(state: &Arc<AppState>) -> anyhow::Result<()> {
    let claims = state.coord.all_claims().await?;

    for (key, claim) in claims {
        if !claim.is_timed_out() {
            continue;
        }

        // Parse owner/repo/number from the claim key
        let parts: Vec<&str> = key.splitn(3, '/').collect();
        if parts.len() != 3 {
            tracing::warn!(key, "malformed claim key, skipping");
            continue;
        }
        let (owner, repo, num_str) = (parts[0], parts[1], parts[2]);
        let number: u64 = match num_str.parse() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!(key, "could not parse issue number, skipping");
                continue;
            }
        };

        tracing::info!(
            key,
            worker_id = claim.worker_id,
            "heartbeat timeout — failing job"
        );

        let elapsed = chrono::Utc::now()
            .signed_duration_since(claim.last_heartbeat)
            .num_seconds();

        let failure = FailureRecord {
            worker_id: claim.worker_id.clone(),
            kind: FailureKind::HeartbeatTimeout,
            reason: format!(
                "No heartbeat for {elapsed}s (timeout: {}s)",
                claim.timeout_secs
            ),
            logs: None,
            failed_at: chrono::Utc::now(),
        };

        // Release claim, update graph, update Forgejo in parallel
        let release = state.coord.release(&key);
        let set_state = state.graph.set_state(&key, &workflow_types::JobState::Failed);

        // Release NATS claim
        if let Err(e) = release.await {
            tracing::error!(key, "failed to release timed-out claim: {e:#}");
        }

        // Update graph state
        if let Err(e) = set_state {
            tracing::error!(key, "failed to set failed state in graph: {e:#}");
        }

        // Update Forgejo label and post failure comment
        if let Err(e) = state
            .forgejo
            .set_job_state(owner, repo, number, &workflow_types::JobState::Failed)
            .await
        {
            tracing::error!(key, "failed to set Forgejo label on timeout: {e:#}");
        }

        if let Err(e) = state
            .forgejo
            .post_failure_comment(owner, repo, number, &failure)
            .await
        {
            tracing::error!(key, "failed to post failure comment: {e:#}");
        }
    }

    Ok(())
}
