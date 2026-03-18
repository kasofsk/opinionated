use anyhow::Result;
use std::sync::Arc;
use workflow_types::{
    parse_deps, parse_priority, parse_timeout, Job, JobState, WebhookAction,
    WebhookPayload,
};

use crate::AppState;

/// Dispatch a webhook event.
///
/// This is called only after the delivery ID has been dedup-checked; it will
/// not be invoked twice for the same delivery.
pub async fn handle_webhook(
    state: &Arc<AppState>,
    payload: WebhookPayload,
) -> Result<()> {
    let owner = &payload.repository.owner.login;
    let repo = &payload.repository.name;
    let issue = &payload.issue;

    tracing::debug!(
        action = ?payload.action,
        owner,
        repo,
        number = issue.number,
        "webhook dispatch"
    );

    match payload.action {
        WebhookAction::Opened => {
            handle_opened(state, owner, repo, issue).await?
        }
        WebhookAction::Edited => {
            handle_edited(state, owner, repo, issue).await?
        }
        WebhookAction::Closed => {
            handle_closed(state, owner, repo, issue).await?
        }
        WebhookAction::Labeled | WebhookAction::Unlabeled => {
            handle_label_change(state, owner, repo, issue).await?
        }
        WebhookAction::Reopened => {
            // Treat as re-opened: re-evaluate state from scratch
            handle_opened(state, owner, repo, issue).await?
        }
        _ => {}
    }

    // Publish the event to NATS for factory subscriptions.
    let action_str = match payload.action {
        WebhookAction::Opened => "opened",
        WebhookAction::Edited => "edited",
        WebhookAction::Closed => "closed",
        WebhookAction::Labeled => "labeled",
        WebhookAction::Unlabeled => "unlabeled",
        WebhookAction::Reopened => "reopened",
        WebhookAction::Assigned => "assigned",
        WebhookAction::Unassigned => "unassigned",
        WebhookAction::Other => "other",
    };
    if let Ok(payload_bytes) =
        serde_json::to_vec(&payload.issue).map(bytes::Bytes::from)
    {
        let _ = state.coord.publish_event(action_str, payload_bytes).await;
    }

    Ok(())
}

// ── handlers ──────────────────────────────────────────────────────────────────

async fn handle_opened(
    state: &Arc<AppState>,
    owner: &str,
    repo: &str,
    issue: &workflow_types::ForgejoIssue,
) -> Result<()> {
    let body = issue.body.as_deref().unwrap_or("");
    let dep_numbers = parse_deps(body);
    let priority = parse_priority(&issue.labels);
    let timeout_secs = parse_timeout(&issue.labels);

    // Determine initial state
    let on_ice = issue.labels.iter().any(|l| l.name == "status:on-ice");

    let initial_state = if on_ice {
        JobState::OnIce
    } else {
        // Check existing Forgejo label first (in case sidecar restarted)
        let existing = issue
            .labels
            .iter()
            .find_map(|l| JobState::from_label(&l.name));

        if let Some(s) = existing {
            s
        } else {
            JobState::OnDeck // will be corrected below
        }
    };

    let assignees: Vec<String> = issue
        .assignees
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|u| u.login.clone())
        .collect();

    let dep_keys: Vec<String> = dep_numbers
        .iter()
        .map(|n| format!("{owner}/{repo}/{n}"))
        .collect();

    let job_key = format!("{owner}/{repo}/{}", issue.number);

    // Build job struct; state will be corrected below
    let mut job = Job {
        repo_owner: owner.to_string(),
        repo_name: repo.to_string(),
        number: issue.number,
        title: issue.title.clone(),
        state: initial_state.clone(),
        assignees,
        dependency_numbers: dep_numbers.clone(),
        priority,
        timeout_secs,
        capabilities: workflow_types::parse_capabilities(&issue.labels),
    };

    // Upsert vertex and sync dep edges (enforces DAG)
    state.graph.upsert_job(&job)?;
    let rejected_deps = state.graph.sync_deps(&job_key, &dep_keys)?;
    if !rejected_deps.is_empty() {
        let msg = format!(
            "⚠️ **Cycle detected** — the following dependencies were ignored \
             because they would introduce a cycle in the task graph:\n\n{}\n\n\
             Please fix the dependency list.",
            rejected_deps.iter().map(|k| format!("- `{k}`")).collect::<Vec<_>>().join("\n")
        );
        let _ = state.forgejo.post_comment(owner, repo, issue.number, &msg).await;
    }

    // Determine correct state unless explicitly held
    if !on_ice {
        let resolved_state = if !dep_numbers.is_empty()
            && !state.graph.all_deps_done(&job_key)?
        {
            JobState::Blocked
        } else {
            JobState::OnDeck
        };

        if resolved_state != initial_state {
            job.state = resolved_state.clone();
            state.graph.upsert_job(&job)?;
        }

        // Sync Forgejo label unless it already matches
        let forgejo_state = issue
            .labels
            .iter()
            .find_map(|l| JobState::from_label(&l.name));
        if forgejo_state.as_ref() != Some(&job.state) {
            state.forgejo.set_job_state(owner, repo, issue.number, &job.state).await?;
        }
    }

    Ok(())
}

async fn handle_edited(
    state: &Arc<AppState>,
    owner: &str,
    repo: &str,
    issue: &workflow_types::ForgejoIssue,
) -> Result<()> {
    let body = issue.body.as_deref().unwrap_or("");
    let dep_numbers = parse_deps(body);
    let priority = parse_priority(&issue.labels);
    let timeout_secs = parse_timeout(&issue.labels);

    let dep_keys: Vec<String> = dep_numbers
        .iter()
        .map(|n| format!("{owner}/{repo}/{n}"))
        .collect();

    let job_key = format!("{owner}/{repo}/{}", issue.number);

    // Update stored job (re-fetch current state from graph to preserve it)
    let current_state = state
        .graph
        .get_job(&job_key)?
        .map(|j| j.state)
        .unwrap_or(JobState::OnDeck);

    let assignees: Vec<String> = issue
        .assignees
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|u| u.login.clone())
        .collect();

    let job = Job {
        repo_owner: owner.to_string(),
        repo_name: repo.to_string(),
        number: issue.number,
        title: issue.title.clone(),
        state: current_state.clone(),
        assignees,
        dependency_numbers: dep_numbers.clone(),
        priority,
        timeout_secs,
        capabilities: workflow_types::parse_capabilities(&issue.labels),
    };

    state.graph.upsert_job(&job)?;
    let rejected_deps = state.graph.sync_deps(&job_key, &dep_keys)?;
    if !rejected_deps.is_empty() {
        let msg = format!(
            "⚠️ **Cycle detected** — the following dependencies were ignored \
             because they would introduce a cycle:\n\n{}\n\nPlease fix the dependency list.",
            rejected_deps.iter().map(|k| format!("- `{k}`")).collect::<Vec<_>>().join("\n")
        );
        let _ = state.forgejo.post_comment(owner, repo, issue.number, &msg).await;
    }

    // Re-evaluate state if the dep set changed and job isn't terminal/claimed
    let should_re_evaluate = matches!(
        current_state,
        JobState::Blocked | JobState::OnDeck | JobState::OnIce
    );

    if should_re_evaluate
        && !issue.labels.iter().any(|l| l.name == "status:on-ice")
    {
        let new_state = if !dep_numbers.is_empty()
            && !state.graph.all_deps_done(&job_key)?
        {
            JobState::Blocked
        } else {
            JobState::OnDeck
        };

        if new_state != current_state {
            state.graph.set_state(&job_key, &new_state)?;
            state.forgejo.set_job_state(owner, repo, issue.number, &new_state).await?;
        }
    }

    Ok(())
}

async fn handle_closed(
    state: &Arc<AppState>,
    owner: &str,
    repo: &str,
    issue: &workflow_types::ForgejoIssue,
) -> Result<()> {
    let job_key = format!("{owner}/{repo}/{}", issue.number);

    state.graph.set_state(&job_key, &JobState::Done)?;

    // Walk reverse deps: transition any that are now fully unblocked to OnDeck
    let dependents = state.graph.get_dependents(&job_key)?;
    for dep_key in dependents {
        if state.graph.all_deps_done(&dep_key)? {
            if let Some(dep_job) = state.graph.get_job(&dep_key)? {
                if dep_job.state == JobState::Blocked {
                    state.graph.set_state(&dep_key, &JobState::OnDeck)?;
                    state
                        .forgejo
                        .set_job_state(
                            &dep_job.repo_owner,
                            &dep_job.repo_name,
                            dep_job.number,
                            &JobState::OnDeck,
                        )
                        .await?;
                }
            }
        }
    }

    Ok(())
}

async fn handle_label_change(
    state: &Arc<AppState>,
    owner: &str,
    repo: &str,
    issue: &workflow_types::ForgejoIssue,
) -> Result<()> {
    let job_key = format!("{owner}/{repo}/{}", issue.number);

    let on_ice = issue.labels.iter().any(|l| l.name == "status:on-ice");

    if on_ice {
        // Explicitly held; store as-is
        state.graph.set_state(&job_key, &JobState::OnIce)?;
    } else {
        // on-ice removed: re-evaluate
        let current = state.graph.get_job(&job_key)?;
        if let Some(job) = current {
            if job.state == JobState::OnIce {
                let dep_keys: Vec<String> = job
                    .dependency_numbers
                    .iter()
                    .map(|n| format!("{owner}/{repo}/{n}"))
                    .collect();

                let new_state = if !dep_keys.is_empty()
                    && !state.graph.all_deps_done(&job_key)?
                {
                    JobState::Blocked
                } else {
                    JobState::OnDeck
                };

                state.graph.set_state(&job_key, &new_state)?;
                state
                    .forgejo
                    .set_job_state(owner, repo, issue.number, &new_state)
                    .await?;
            }
        }
    }

    Ok(())
}
