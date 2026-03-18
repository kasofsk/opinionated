//! Dispatcher: centralized worker assignment via NATS pub/sub.
//!
//! Workers register with capability tags and signal when idle. The dispatcher
//! picks the best matching on-deck job, claims it through the coordinator, and
//! pushes an assignment. When a high-priority job arrives and all capable
//! workers are busy on lower-priority work, the dispatcher preempts the
//! lowest-priority worker.

use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::Utc;
use futures::StreamExt;
use workflow_types::{
    Assignment, FailureKind, FailureRecord, IdleEvent, JobState, JobTransition, OutcomeReport,
    PreemptNotice, WorkerHeartbeat, WorkerInfo, WorkerOutcome, WorkerRegistration, WorkerState,
};

use crate::AppState;

// ── NATS subjects ────────────────────────────────────────────────────────────

const SUBJECT_REGISTER: &str = "workflow.dispatch.register";
const SUBJECT_IDLE: &str = "workflow.dispatch.idle";
const SUBJECT_TRANSITION: &str = "workflow.jobs.transition";
const SUBJECT_HEARTBEAT: &str = "workflow.dispatch.heartbeat";
const SUBJECT_OUTCOME: &str = "workflow.dispatch.outcome";

fn subject_assign(worker_id: &str) -> String {
    format!("workflow.dispatch.assign.{worker_id}")
}

fn subject_preempt(worker_id: &str) -> String {
    format!("workflow.dispatch.preempt.{worker_id}")
}

pub struct Dispatcher {
    state: Arc<AppState>,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("worker_count", &self.state.dispatch_registry.len())
            .finish()
    }
}

impl Dispatcher {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Start background NATS subscriptions for register, idle, and transition events.
    pub async fn start(self: Arc<Self>) -> Result<()> {
        let nats = self.state.coord.nats_client().clone();

        let mut reg_sub = nats
            .subscribe(String::from(SUBJECT_REGISTER))
            .await
            .context("subscribe to dispatch.register")?;

        let mut idle_sub = nats
            .subscribe(String::from(SUBJECT_IDLE))
            .await
            .context("subscribe to dispatch.idle")?;

        let mut transition_sub = nats
            .subscribe(String::from(SUBJECT_TRANSITION))
            .await
            .context("subscribe to jobs.transition")?;

        // Registration handler — uses request/reply so workers wait for ack.
        let dispatcher = Arc::clone(&self);
        let nats_reg = nats.clone();
        tokio::spawn(async move {
            while let Some(msg) = reg_sub.next().await {
                let reg: WorkerRegistration = match serde_json::from_slice(&msg.payload) {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("bad registration payload: {e:#}");
                        continue;
                    }
                };
                let caps_str = if reg.capabilities.is_empty() {
                    "none".to_string()
                } else {
                    reg.capabilities.join(", ")
                };
                dispatcher.state.journal(
                    "register",
                    &format!("Worker registered with capabilities: [{caps_str}]"),
                    None,
                    Some(&reg.worker_id),
                ).await;
                tracing::info!(
                    worker_id = %reg.worker_id,
                    capabilities = ?reg.capabilities,
                    "worker registered"
                );
                let now = Utc::now();
                dispatcher.state.dispatch_registry.insert(
                    reg.worker_id.clone(),
                    WorkerInfo {
                        worker_id: reg.worker_id,
                        capabilities: reg.capabilities,
                        state: WorkerState::Idle,
                        current_job_key: None,
                        current_job_priority: None,
                        last_seen: now,
                    },
                );
                // Reply so the worker knows registration is complete
                // before it sends its first idle event.
                if let Some(reply) = msg.reply {
                    let _ = nats_reg.publish(reply, Bytes::new()).await;
                }
            }
        });

        // Idle handler
        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = idle_sub.next().await {
                let idle: IdleEvent = match serde_json::from_slice(&msg.payload) {
                    Ok(i) => i,
                    Err(e) => {
                        tracing::warn!("bad idle payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_idle(&idle.worker_id).await {
                    tracing::error!(
                        worker_id = %idle.worker_id,
                        error = %e,
                        "failed to handle idle event"
                    );
                }
            }
        });

        // Transition handler — reacts to job state changes
        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = transition_sub.next().await {
                let transition: JobTransition = match serde_json::from_slice(&msg.payload) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("bad transition payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_transition(&transition).await {
                    tracing::error!(
                        job = %transition.job.key(),
                        error = %e,
                        "failed to handle transition"
                    );
                }
            }
        });

        // Heartbeat handler — workers publish heartbeats via NATS
        let mut hb_sub = nats
            .subscribe(String::from(SUBJECT_HEARTBEAT))
            .await
            .context("subscribe to dispatch.heartbeat")?;

        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = hb_sub.next().await {
                let hb: WorkerHeartbeat = match serde_json::from_slice(&msg.payload) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!("bad heartbeat payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_heartbeat(&hb).await {
                    tracing::warn!(
                        worker_id = %hb.worker_id,
                        job_key = %hb.job_key,
                        error = %e,
                        "failed to forward heartbeat"
                    );
                }
            }
        });

        // Outcome handler — workers report job results via NATS
        let mut outcome_sub = nats
            .subscribe(String::from(SUBJECT_OUTCOME))
            .await
            .context("subscribe to dispatch.outcome")?;

        let dispatcher = Arc::clone(&self);
        tokio::spawn(async move {
            while let Some(msg) = outcome_sub.next().await {
                let outcome: WorkerOutcome = match serde_json::from_slice(&msg.payload) {
                    Ok(o) => o,
                    Err(e) => {
                        tracing::warn!("bad outcome payload: {e:#}");
                        continue;
                    }
                };
                if let Err(e) = dispatcher.handle_outcome(&outcome).await {
                    tracing::error!(
                        worker_id = %outcome.worker_id,
                        job_key = %outcome.job_key,
                        error = %e,
                        "failed to handle worker outcome"
                    );
                }
            }
        });

        Ok(())
    }

    /// Called when a worker signals idle. Check pending reworks first, then normal assign.
    async fn handle_idle(&self, worker_id: &str) -> Result<()> {
        // Update status to idle.
        if let Some(mut entry) = self.state.dispatch_registry.get_mut(worker_id) {
            entry.state = WorkerState::Idle;
            entry.current_job_key = None;
            entry.current_job_priority = None;
            entry.last_seen = Utc::now();
        } else {
            tracing::warn!(worker_id, "idle from unregistered worker, ignoring");
            return Ok(());
        }

        // Check if this worker has a pending rework assignment.
        if let Some(job_key) = self.find_pending_rework(worker_id) {
            if let Ok(Some(job)) = self.state.graph.get_job(&job_key) {
                if job.state == JobState::Rework {
                    return self.try_assign_rework(worker_id, &job).await;
                }
            }
            // Job no longer in Rework state — clean up stale entry.
            self.state.pending_reworks.remove(&job_key);
        }

        self.try_assign(worker_id).await
    }

    /// Try to assign an on-deck job to a specific idle worker.
    async fn try_assign(&self, worker_id: &str) -> Result<()> {
        let caps = match self.state.dispatch_registry.get(worker_id) {
            Some(entry) => entry.capabilities.clone(),
            None => return Ok(()),
        };

        let mut jobs = self.state.graph.get_all_jobs(Some(&JobState::OnDeck))?;
        jobs.sort_by(|a, b| b.priority.cmp(&a.priority));

        for job in &jobs {
            if !capabilities_match(&job.capabilities, &caps) {
                continue;
            }

            // Guard: verify all declared deps are actually Done. The graph may
            // mark a job on-deck before CDC has synced all dependency edges.
            if !job.dependency_numbers.is_empty() {
                let deps_ok = self.state.graph.all_declared_deps_done(
                    &job.repo_owner, &job.repo_name, &job.dependency_numbers,
                ).unwrap_or(false);
                if !deps_ok {
                    tracing::debug!(
                        job_key = %job.key(),
                        "skipping — deps not all done"
                    );
                    continue;
                }
            }

            // Try to claim.
            let timeout = job
                .timeout_secs
                .unwrap_or(self.state.config.default_timeout_secs);
            let claim = self
                .state
                .coord
                .try_claim(&job.key(), worker_id.to_string(), timeout)
                .await?;

            let claim = match claim {
                Some(c) => c,
                None => continue, // already claimed by someone else
            };

            // Transition state and set assignee to the worker's Forgejo user.
            self.state.graph.set_state(&job.key(), &JobState::OnTheStack)?;
            self.state
                .forgejo
                .set_job_state(
                    &job.repo_owner,
                    &job.repo_name,
                    job.number,
                    &JobState::OnTheStack,
                )
                .await?;
            self.state
                .dispatcher_forgejo
                .set_assignees(
                    &job.repo_owner,
                    &job.repo_name,
                    job.number,
                    vec![worker_id.to_string()],
                )
                .await?;

            // Mark worker as busy.
            if let Some(mut entry) = self.state.dispatch_registry.get_mut(worker_id) {
                entry.state = WorkerState::Busy;
                entry.current_job_key = Some(job.key());
                entry.current_job_priority = Some(job.priority);
                entry.last_seen = Utc::now();
            }

            // Publish assignment.
            let assignment = Assignment {
                job: job.clone(),
                claim,
                is_rework: false,
            };
            self.state
                .coord
                .nats_client()
                .publish(
                    subject_assign(worker_id),
                    Bytes::from(serde_json::to_vec(&assignment)?),
                )
                .await
                .context("publish assignment")?;

            self.state.journal(
                "assign",
                &format!(
                    "Assigned {} (priority {}) to worker",
                    job.key(), job.priority
                ),
                Some(&job.key()),
                Some(worker_id),
            ).await;

            tracing::info!(
                worker_id,
                job_key = %job.key(),
                priority = job.priority,
                "assigned job to worker"
            );

            return Ok(());
        }

        tracing::debug!(worker_id, "no matching on-deck jobs for idle worker");
        Ok(())
    }

    /// Forward a worker heartbeat to the coordinator.
    async fn handle_heartbeat(&self, hb: &WorkerHeartbeat) -> Result<()> {
        if let Some(mut entry) = self.state.dispatch_registry.get_mut(&hb.worker_id) {
            entry.last_seen = Utc::now();
        }
        let ok = self.state.coord.heartbeat(&hb.job_key, &hb.worker_id).await?;
        if !ok {
            tracing::warn!(
                worker_id = %hb.worker_id,
                job_key = %hb.job_key,
                "heartbeat rejected — worker is not the claim holder"
            );
        }
        Ok(())
    }

    /// Handle a worker's outcome report: release claim, update state, sync Forgejo.
    async fn handle_outcome(&self, wo: &WorkerOutcome) -> Result<()> {
        let key = &wo.job_key;

        // Parse owner/repo/number from the job key.
        let parts: Vec<&str> = key.splitn(3, '/').collect();
        if parts.len() != 3 {
            anyhow::bail!("malformed job key: {key}");
        }
        let (owner, repo) = (parts[0], parts[1]);
        let number: u64 = parts[2].parse().context("parse issue number")?;

        // Release the claim.
        self.state.coord.release(key).await?;

        let new_state = match &wo.outcome {
            OutcomeReport::Complete => {
                self.state.graph.set_state(key, &JobState::InReview)?;
                self.state
                    .forgejo
                    .set_job_state(owner, repo, number, &JobState::InReview)
                    .await?;
                self.state.journal(
                    "complete",
                    &format!("Worker completed job → in-review"),
                    Some(key),
                    Some(&wo.worker_id),
                ).await;
                tracing::info!(
                    worker_id = %wo.worker_id,
                    job_key = key,
                    "worker completed job"
                );
                JobState::InReview
            }
            OutcomeReport::Fail { reason, logs } => {
                let failure = FailureRecord {
                    worker_id: wo.worker_id.clone(),
                    kind: FailureKind::WorkerReported,
                    reason: reason.clone(),
                    logs: logs.clone(),
                    failed_at: Utc::now(),
                };

                self.state.graph.set_state(key, &JobState::Failed)?;
                self.state
                    .forgejo
                    .set_job_state(owner, repo, number, &JobState::Failed)
                    .await?;
                self.state
                    .dispatcher_forgejo
                    .post_failure_comment(owner, repo, number, &failure)
                    .await?;
                self.state.journal(
                    "fail",
                    &format!("Worker reported failure: {reason}"),
                    Some(key),
                    Some(&wo.worker_id),
                ).await;
                tracing::warn!(
                    worker_id = %wo.worker_id,
                    job_key = key,
                    reason,
                    "worker reported failure"
                );
                JobState::Failed
            }
            OutcomeReport::Abandon => {
                self.state.graph.set_state(key, &JobState::OnDeck)?;
                self.state
                    .forgejo
                    .set_job_state(owner, repo, number, &JobState::OnDeck)
                    .await?;
                self.state.journal(
                    "abandon",
                    &format!("Worker abandoned job → on-deck"),
                    Some(key),
                    Some(&wo.worker_id),
                ).await;
                tracing::info!(
                    worker_id = %wo.worker_id,
                    job_key = key,
                    "worker abandoned job"
                );
                JobState::OnDeck
            }
        };

        // Mark worker as transitioning — it will send IdleEvent when ready.
        if let Some(mut entry) = self.state.dispatch_registry.get_mut(&wo.worker_id) {
            entry.state = WorkerState::Transitioning;
            entry.current_job_key = None;
            entry.current_job_priority = None;
            entry.last_seen = Utc::now();
        }

        // Publish transition event for other reactors.
        if let Ok(Some(job)) = self.state.graph.get_job(key) {
            self.state.coord.publish_transition(&JobTransition {
                job,
                previous_state: Some(JobState::OnTheStack),
                new_state,
            }).await;
        }

        Ok(())
    }

    /// React to a state-transition event from the NATS stream.
    async fn handle_transition(&self, t: &JobTransition) -> Result<()> {
        match &t.new_state {
            JobState::OnDeck => {
                // A job became on-deck — try idle workers, then preemption.
                if let Some(worker_id) = self.find_idle_worker(&t.job.capabilities) {
                    return self.try_assign(&worker_id).await;
                }

                if let Some((victim_id, victim_priority)) =
                    self.find_preemption_candidate(&t.job.capabilities, t.job.priority)
                {
                    self.state.journal(
                        "preempt",
                        &format!(
                            "Preempting {} (priority {victim_priority}) for higher-priority job {} (priority {})",
                            victim_id, t.job.key(), t.job.priority
                        ),
                        Some(&t.job.key()),
                        Some(&victim_id),
                    ).await;
                    tracing::info!(
                        job_key = %t.job.key(),
                        job_priority = t.job.priority,
                        victim_worker = %victim_id,
                        victim_priority,
                        "preempting worker for higher-priority job"
                    );

                    let notice = PreemptNotice {
                        reason: format!(
                            "higher-priority job {} (priority {}) arrived",
                            t.job.key(),
                            t.job.priority
                        ),
                        new_job: t.job.clone(),
                    };

                    self.state
                        .coord
                        .nats_client()
                        .publish(
                            subject_preempt(&victim_id),
                            Bytes::from(serde_json::to_vec(&notice)?),
                        )
                        .await
                        .context("publish preempt notice")?;
                }
            }
            JobState::OnTheStack => {
                // A worker claimed this job — check if already tracked.
                let job_key = t.job.key();
                for entry in self.state.dispatch_registry.iter() {
                    if entry.current_job_key.as_deref() == Some(&job_key) {
                        return Ok(());
                    }
                }
            }
            JobState::Rework => {
                // A reviewer requested changes. Route back to the original worker.
                let job_key = t.job.key();
                let original_worker = t.job.assignees.first().cloned();

                if let Some(ref worker_id) = original_worker {
                    // Check if the original worker is currently idle.
                    let is_idle = self.state.dispatch_registry.get(worker_id)
                        .map(|e| matches!(e.state, WorkerState::Idle))
                        .unwrap_or(false);

                    if is_idle {
                        // Assign immediately.
                        self.try_assign_rework(worker_id, &t.job).await?;
                    } else {
                        // Worker is busy — queue for when it becomes idle.
                        self.state.pending_reworks.insert(job_key.clone(), worker_id.clone());
                        self.state.journal(
                            "rework",
                            &format!("Rework queued for worker {worker_id} (currently busy)"),
                            Some(&job_key),
                            Some(worker_id),
                        ).await;
                        tracing::info!(
                            job_key = %job_key,
                            worker_id = %worker_id,
                            "rework queued — original worker is busy"
                        );
                    }
                } else {
                    tracing::warn!(
                        job_key = %job_key,
                        "rework requested but no assignee to route back to"
                    );
                }
            }
            JobState::Done | JobState::Failed | JobState::InReview | JobState::Revoked => {
                // Worker is no longer busy on this job, but we don't mark it
                // Idle — the worker itself will publish an IdleEvent when it's
                // actually ready for new work.  Just clear the Busy tracking.
                let job_key = t.job.key();
                self.state.pending_reworks.remove(&job_key);
                for mut entry in self.state.dispatch_registry.iter_mut() {
                    if entry.current_job_key.as_deref() == Some(&job_key) {
                        entry.state = WorkerState::Transitioning;
                        entry.current_job_key = None;
                        entry.current_job_priority = None;
                        entry.last_seen = Utc::now();
                        break;
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Assign a rework job to the original worker. Claim, transition Rework → OnTheStack,
    /// and publish an Assignment with `is_rework: true`.
    async fn try_assign_rework(&self, worker_id: &str, job: &workflow_types::Job) -> Result<()> {
        let job_key = job.key();

        let timeout = job.timeout_secs.unwrap_or(self.state.config.default_timeout_secs);
        let claim = self.state.coord.try_claim(&job_key, worker_id.to_string(), timeout).await?;
        let claim = match claim {
            Some(c) => c,
            None => {
                tracing::warn!(job_key = %job_key, "rework claim failed — already claimed");
                return Ok(());
            }
        };

        self.state.graph.set_state(&job_key, &JobState::OnTheStack)?;
        self.state
            .forgejo
            .set_job_state(&job.repo_owner, &job.repo_name, job.number, &JobState::OnTheStack)
            .await?;
        // Assignee is already set from the original work cycle — no need to re-set.

        if let Some(mut entry) = self.state.dispatch_registry.get_mut(worker_id) {
            entry.state = WorkerState::Busy;
            entry.current_job_key = Some(job_key.clone());
            entry.current_job_priority = Some(job.priority);
            entry.last_seen = Utc::now();
        }

        let assignment = Assignment {
            job: job.clone(),
            claim,
            is_rework: true,
        };
        self.state
            .coord
            .nats_client()
            .publish(
                subject_assign(worker_id),
                Bytes::from(serde_json::to_vec(&assignment)?),
            )
            .await
            .context("publish rework assignment")?;

        self.state.pending_reworks.remove(&job_key);

        self.state.journal(
            "rework",
            &format!("Re-assigned {} to original worker for rework", job_key),
            Some(&job_key),
            Some(worker_id),
        ).await;

        tracing::info!(
            worker_id,
            job_key = %job_key,
            "assigned rework to original worker"
        );

        Ok(())
    }

    /// Find a pending rework entry for a given worker.
    fn find_pending_rework(&self, worker_id: &str) -> Option<String> {
        for entry in self.state.pending_reworks.iter() {
            if entry.value() == worker_id {
                return Some(entry.key().clone());
            }
        }
        None
    }

    /// Find the first idle worker whose capabilities are a superset of the required ones.
    fn find_idle_worker(&self, required: &[String]) -> Option<String> {
        for entry in self.state.dispatch_registry.iter() {
            if matches!(entry.state, WorkerState::Idle)
                && capabilities_match(required, &entry.capabilities)
            {
                return Some(entry.worker_id.clone());
            }
        }
        None
    }

    /// Find the busy worker on the lowest-priority job that is lower than `new_priority`
    /// and has the required capabilities.
    fn find_preemption_candidate(
        &self,
        required: &[String],
        new_priority: u32,
    ) -> Option<(String, u32)> {
        let mut best: Option<(String, u32)> = None;

        for entry in self.state.dispatch_registry.iter() {
            if matches!(entry.state, WorkerState::Busy) {
                if let Some(priority) = entry.current_job_priority {
                    if priority < new_priority
                        && capabilities_match(required, &entry.capabilities)
                    {
                        match &best {
                            None => best = Some((entry.worker_id.clone(), priority)),
                            Some((_, best_prio)) if &priority < best_prio => {
                                best = Some((entry.worker_id.clone(), priority));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        best
    }
}

/// Returns true if a worker's capabilities are a superset of the required ones.
/// If `required` is empty, any worker matches.
fn capabilities_match(required: &[String], worker_caps: &[String]) -> bool {
    required.iter().all(|r| worker_caps.contains(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capabilities_match_empty_required() {
        assert!(capabilities_match(&[], &["rust".into()]));
        assert!(capabilities_match(&[], &[]));
    }

    #[test]
    fn test_capabilities_match_subset() {
        assert!(capabilities_match(
            &["rust".into()],
            &["rust".into(), "frontend".into()]
        ));
    }

    #[test]
    fn test_capabilities_match_missing() {
        assert!(!capabilities_match(
            &["rust".into(), "gpu".into()],
            &["rust".into(), "frontend".into()]
        ));
    }
}
