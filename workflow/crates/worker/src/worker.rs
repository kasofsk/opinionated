use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use workflow_types::Job;

use crate::client::SidecarClient;
use crate::forgejo::ForgejoClient;

// ── Outcome ───────────────────────────────────────────────────────────────────

/// The result of executing a job.
///
/// Workers return this from `execute` to indicate what should happen next.
/// The loop (pull-based or dispatched) converts each variant into the
/// appropriate lifecycle action.
#[derive(Debug)]
pub enum Outcome {
    /// Job finished successfully; transition to `in-review`.
    Complete,
    /// Job failed; record a failure comment and transition to `failed`.
    Fail {
        reason: String,
        logs: Option<String>,
    },
    /// Voluntarily return the job to `on-deck` (e.g. worker is shutting down).
    Abandon,
}

// ── Execution context ────────────────────────────────────────────────────────

/// Context passed alongside [`Worker::execute`] in dispatched mode.
///
/// Bundles the forgejo client with a cancellation token that fires when
/// the dispatcher preempts this worker for a higher-priority job.
pub struct ExecutionContext {
    pub forgejo: ForgejoClient,
    pub cancellation: CancellationToken,
}

// ── Worker trait ──────────────────────────────────────────────────────────────

/// Core trait for agents that execute jobs.
///
/// The [`WorkerLoop`] handles the claim–heartbeat–outcome lifecycle; implementors
/// only need to provide the execution logic.
#[async_trait]
pub trait Worker: Send + Sync {
    /// Unique identifier for this worker instance, used for claims and assignees.
    fn worker_id(&self) -> &str;

    /// Capability tags this worker supports (e.g. `["rust", "frontend"]`).
    ///
    /// Used by the dispatcher to match workers to jobs with `capability:X` labels.
    /// Default: empty (accepts any job regardless of capability requirements).
    fn capabilities(&self) -> Vec<String> {
        vec![]
    }

    /// Return `false` to skip a job without claiming it.
    ///
    /// Use this to implement worker specialization — e.g. only accept jobs
    /// whose title contains a certain tag. The default accepts every job.
    fn accepts(&self, _job: &Job) -> bool {
        true
    }

    /// Execute a claimed job.
    ///
    /// - The claim is already held when this is called.
    /// - The loop maintains a background heartbeat task for the duration of
    ///   this call; do **not** send heartbeats manually.
    /// - Use `forgejo` for content operations (comments, branches, PRs).
    /// - Return [`Outcome::Complete`], [`Outcome::Fail`], or [`Outcome::Abandon`].
    async fn execute(
        &self,
        job: &Job,
        forgejo: &ForgejoClient,
    ) -> Result<Outcome>;
}

// ── WorkerLoop ────────────────────────────────────────────────────────────────

/// Runs the claim–heartbeat–outcome lifecycle for a [`Worker`].
///
/// This is the pull-based worker loop. For dispatcher-driven operation, use
/// [`crate::dispatch::DispatchedWorkerLoop`].
pub struct WorkerLoop<W> {
    worker: W,
    sidecar: SidecarClient,
    forgejo: ForgejoClient,
    heartbeat_interval: Duration,
}

impl<W: Worker> WorkerLoop<W> {
    pub fn new(
        worker: W,
        sidecar_url: &str,
        forgejo_url: &str,
        forgejo_token: &str,
        heartbeat_interval: Duration,
    ) -> Self {
        Self {
            worker,
            sidecar: SidecarClient::new(sidecar_url),
            forgejo: ForgejoClient::new(forgejo_url, forgejo_token),
            heartbeat_interval,
        }
    }

    /// Run one iteration: find and claim an acceptable job, execute it, report
    /// the outcome.
    ///
    /// Returns `true` if a job was executed, `false` if no suitable jobs exist.
    pub async fn run_once(&self) -> Result<bool> {
        let jobs = self.sidecar.available_jobs().await?;

        // Pick the first job this worker accepts (jobs are sorted by priority desc)
        let job = match jobs.into_iter().find(|j| self.worker.accepts(j)) {
            Some(j) => j,
            None => return Ok(false),
        };

        let owner = job.repo_owner.clone();
        let repo = job.repo_name.clone();
        let number = job.number;
        let worker_id = self.worker.worker_id().to_string();

        let claim_resp = self.sidecar.claim(&owner, &repo, number, &worker_id).await?;
        let _claim = match claim_resp {
            Some(c) => c,
            None => return Ok(false), // race — another worker got there first
        };

        tracing::info!(worker_id, owner, repo, number, "claimed job");

        // Spawn background heartbeat task
        let (hb_cancel, hb_rx) = tokio::sync::oneshot::channel::<()>();
        {
            let sidecar = self.sidecar.clone();
            let owner_c = owner.clone();
            let repo_c = repo.clone();
            let worker_c = worker_id.clone();
            let interval = self.heartbeat_interval;
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                let mut rx = hb_rx;
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            if let Err(e) = sidecar
                                .heartbeat(&owner_c, &repo_c, number, &worker_c)
                                .await
                            {
                                tracing::warn!("heartbeat failed: {e:#}");
                            }
                        }
                        _ = &mut rx => break,
                    }
                }
            });
        }

        let outcome = self
            .worker
            .execute(&job, &self.forgejo)
            .await;

        let _ = hb_cancel.send(()); // stop heartbeat

        match outcome {
            Ok(Outcome::Complete) => {
                self.sidecar.complete(&owner, &repo, number, &worker_id).await?;
                tracing::info!(worker_id, owner, repo, number, "job complete");
            }
            Ok(Outcome::Fail { reason, logs }) => {
                tracing::warn!(worker_id, owner, repo, number, "job failed: {reason}");
                let _ = self
                    .sidecar
                    .fail(&owner, &repo, number, &worker_id, reason, logs)
                    .await;
            }
            Ok(Outcome::Abandon) => {
                tracing::info!(worker_id, owner, repo, number, "job abandoned");
                let _ = self.sidecar.abandon(&owner, &repo, number, &worker_id).await;
            }
            Err(e) => {
                tracing::error!(worker_id, owner, repo, number, "execute error: {e:#}");
                let _ = self
                    .sidecar
                    .fail(
                        &owner,
                        &repo,
                        number,
                        &worker_id,
                        format!("unexpected error: {e:#}"),
                        None,
                    )
                    .await;
            }
        }

        Ok(true)
    }

    /// Poll for jobs in a loop, sleeping `idle_delay` when none are available.
    pub async fn run_loop(&self, idle_delay: Duration) -> Result<()> {
        loop {
            match self.run_once().await {
                Ok(true) => {}
                Ok(false) => tokio::time::sleep(idle_delay).await,
                Err(e) => {
                    tracing::error!("worker loop error: {e:#}");
                    tokio::time::sleep(idle_delay).await;
                }
            }
        }
    }
}
