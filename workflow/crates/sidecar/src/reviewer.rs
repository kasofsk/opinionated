//! Reviewer: automated PR review triggered by InReview transitions.
//!
//! Subscribes to `workflow.jobs.transition` and reacts when a job moves to
//! InReview. Finds the linked PR, then either approves+merges or escalates
//! to a human reviewer (50% random probability).
//!
//! This is NOT a worker — it doesn't claim jobs. It acts in a supervisory
//! capacity, similar to the dispatcher.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use rand::Rng;
use workflow_types::{JobState, JobTransition};
use workflow_worker::forgejo::{ForgejoClient as ReviewerForgejoClient, PullRequest};

use crate::AppState;

const SUBJECT_TRANSITION: &str = "workflow.jobs.transition";

pub struct Reviewer {
    state: Arc<AppState>,
    /// Reviewer's own Forgejo identity (workflow-reviewer).
    forgejo: ReviewerForgejoClient,
    /// Login of the human escalation target.
    human_login: String,
    /// Delay before acting on a review (seconds).
    delay_secs: u64,
}

impl Reviewer {
    pub fn new(
        state: Arc<AppState>,
        forgejo: ReviewerForgejoClient,
        human_login: String,
        delay_secs: u64,
    ) -> Self {
        Self { state, forgejo, human_login, delay_secs }
    }

    /// Start the background NATS subscription.
    pub async fn start(self: Arc<Self>) -> Result<()> {
        let nats = self.state.coord.nats_client().clone();

        let mut sub = nats
            .subscribe(String::from(SUBJECT_TRANSITION))
            .await
            .context("reviewer: subscribe to jobs.transition")?;

        let reviewer = Arc::clone(&self);
        tokio::spawn(async move {
            tracing::info!("reviewer started");
            while let Some(msg) = sub.next().await {
                let transition: JobTransition = match serde_json::from_slice(&msg.payload) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("reviewer: bad transition payload: {e:#}");
                        continue;
                    }
                };

                if transition.new_state != JobState::InReview {
                    continue;
                }

                let reviewer = Arc::clone(&reviewer);
                tokio::spawn(async move {
                    if let Err(e) = reviewer.handle_in_review(&transition).await {
                        tracing::error!(
                            job = %transition.job.key(),
                            error = %e,
                            "reviewer: failed to handle in-review"
                        );
                    }
                });
            }
        });

        Ok(())
    }

    async fn handle_in_review(&self, transition: &JobTransition) -> Result<()> {
        let job = &transition.job;
        let owner = &job.repo_owner;
        let repo = &job.repo_name;
        let job_key = job.key();

        // Delay to avoid racing with PR creation.
        if self.delay_secs > 0 {
            tokio::time::sleep(Duration::from_secs(self.delay_secs)).await;
        }

        // Find the linked PR: search open PRs for "Closes #N".
        let pr = self.find_linked_pr(owner, repo, job.number).await?;
        let pr = match pr {
            Some(pr) => pr,
            None => {
                tracing::debug!(
                    job_key = %job_key,
                    "reviewer: no linked PR found, skipping"
                );
                return Ok(());
            }
        };

        tracing::info!(
            job_key = %job_key,
            pr_number = pr.number,
            "reviewer: reviewing PR #{}", pr.number
        );

        // 50% chance to escalate to human
        let escalate = rand::rng().random_bool(0.5);

        if escalate {
            self.escalate_to_human(owner, repo, job.number, &pr).await?;
        } else {
            self.approve_and_merge(owner, repo, job.number, &pr).await?;
        }

        Ok(())
    }

    /// Find an open PR whose body contains "Closes #N".
    async fn find_linked_pr(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
    ) -> Result<Option<PullRequest>> {
        let prs = self.forgejo.list_prs(owner, repo, "open").await?;
        let pattern = format!("Closes #{issue_number}");

        Ok(prs.into_iter().find(|pr| {
            pr.body
                .as_deref()
                .map(|b| b.contains(&pattern))
                .unwrap_or(false)
        }))
    }

    /// Approve the PR and merge it. Forgejo auto-closes the issue via
    /// "Closes #N", CDC detects closed_by_merge → Done.
    async fn approve_and_merge(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        pr: &PullRequest,
    ) -> Result<()> {
        let job_key = format!("{owner}/{repo}/{issue_number}");

        // Submit approving review.
        self.forgejo
            .submit_review(owner, repo, pr.number, "LGTM — automated review passed.", "APPROVED")
            .await?;

        // Merge with retry — Forgejo may not accept the merge immediately
        // after a review submission. If all retries fail, escalate to human.
        let mut backoff = Duration::from_millis(250);
        let max_attempts = 5;
        let mut merged = false;
        for attempt in 1..=max_attempts {
            match self.forgejo.merge_pr(owner, repo, pr.number, "merge").await {
                Ok(()) => { merged = true; break; }
                Err(e) if attempt < max_attempts => {
                    tracing::debug!(
                        pr_number = pr.number,
                        attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        error = %e,
                        "merge not ready, retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                }
                Err(e) => {
                    tracing::warn!(
                        pr_number = pr.number,
                        error = %e,
                        "merge failed after {max_attempts} attempts, escalating to human"
                    );
                    break;
                }
            }
        }

        if !merged {
            return self.escalate_to_human(owner, repo, issue_number, pr).await;
        }

        self.state.journal(
            "approve",
            &format!("Approved and merged PR #{} for job", pr.number),
            Some(&job_key),
            Some("workflow-reviewer"),
        ).await;

        tracing::info!(
            job_key = %job_key,
            pr_number = pr.number,
            "reviewer: approved and merged PR"
        );

        Ok(())
    }

    /// Escalate to the human reviewer — add them as a PR reviewer and
    /// post a comment explaining why.
    async fn escalate_to_human(
        &self,
        owner: &str,
        repo: &str,
        issue_number: u64,
        pr: &PullRequest,
    ) -> Result<()> {
        let job_key = format!("{owner}/{repo}/{issue_number}");

        // Add the human as a requested reviewer.
        self.forgejo
            .add_pr_reviewer(owner, repo, pr.number, &self.human_login)
            .await?;

        // Post an explanatory comment on the PR.
        let comment = format!(
            "🔍 **Escalated for human review** — @{}\n\n\
             This PR requires manual review. The automated reviewer has \
             flagged it for human attention.",
            self.human_login
        );
        self.forgejo
            .post_comment(owner, repo, pr.number, &comment)
            .await?;

        self.state.journal(
            "escalate",
            &format!(
                "Escalated PR #{} to human reviewer @{}",
                pr.number, self.human_login
            ),
            Some(&job_key),
            Some("workflow-reviewer"),
        ).await;

        tracing::info!(
            job_key = %job_key,
            pr_number = pr.number,
            human = %self.human_login,
            "reviewer: escalated to human reviewer"
        );

        Ok(())
    }
}
