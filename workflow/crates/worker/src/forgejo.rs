use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Forgejo client scoped to content operations safe for workers to perform
/// after they hold an exclusive claim.
///
/// Workers never touch state labels or assignees — those are owned by the
/// sidecar. This client handles: reading issue bodies, posting comments,
/// branch/PR operations, and other content ops.
#[derive(Clone)]
pub struct ForgejoClient {
    base_url: String,
    token: String,
    http: Client,
}

// ── Forgejo API types ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
}

#[derive(Deserialize)]
pub struct Comment {
    pub id: u64,
    pub body: String,
}

#[derive(Deserialize)]
pub struct Branch {
    pub name: String,
}

#[derive(Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub html_url: String,
    pub state: String,
    #[serde(default)]
    pub merged: bool,
}

#[derive(Debug, Deserialize)]
pub struct RepoLabel {
    pub id: u64,
    pub name: String,
}

#[derive(Deserialize)]
pub struct ChangedFile {
    pub filename: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Serialize)]
struct CreateCommentBody<'a> {
    body: &'a str,
}

#[derive(Serialize)]
pub struct CreateBranchBody<'a> {
    pub new_branch_name: &'a str,
    pub old_branch_name: &'a str,
}

#[derive(Serialize)]
pub struct CreatePrBody<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub head: &'a str,
    pub base: &'a str,
}

// ── Implementation ────────────────────────────────────────────────────────────

impl ForgejoClient {
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            http: Client::new(),
        }
    }

    fn api(&self, path: &str) -> String {
        format!("{}/api/v1{}", self.base_url, path)
    }

    fn auth(&self) -> String {
        format!("token {}", self.token)
    }

    // ── Issue creation / editing ──────────────────────────────────────────────

    /// Create a new issue and return its issue number.
    pub async fn create_issue(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
    ) -> Result<u64> {
        self.create_issue_with_labels(owner, repo, title, body, &[]).await
    }

    /// Create a new issue with labels (by ID) and return its issue number.
    pub async fn create_issue_with_labels(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        labels: &[u64],
    ) -> Result<u64> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            title: &'a str,
            body: &'a str,
            #[serde(skip_serializing_if = "<[u64]>::is_empty")]
            labels: &'a [u64],
        }
        #[derive(serde::Deserialize)]
        struct Created {
            number: u64,
        }
        let url = self.api(&format!("/repos/{owner}/{repo}/issues"));
        let resp: Created = self
            .http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&Body { title, body, labels })
            .send()
            .await
            .context("create issue")?
            .error_for_status()
            .context("create issue response")?
            .json()
            .await?;
        Ok(resp.number)
    }

    /// List all labels defined on a repository.
    pub async fn list_repo_labels(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<RepoLabel>> {
        let url = self.api(&format!("/repos/{owner}/{repo}/labels?limit=50"));
        let labels: Vec<RepoLabel> = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("list repo labels")?
            .error_for_status()
            .context("list repo labels response")?
            .json()
            .await?;
        Ok(labels)
    }

    /// Close an issue (sets state to "closed").
    pub async fn close_issue(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body {
            state: &'static str,
        }
        let url = self.api(&format!("/repos/{owner}/{repo}/issues/{number}"));
        self.http
            .patch(&url)
            .header("Authorization", self.auth())
            .json(&Body { state: "closed" })
            .send()
            .await
            .context("close issue")?
            .error_for_status()
            .context("close issue response")?;
        Ok(())
    }

    /// Overwrite the body of an existing issue.
    pub async fn edit_issue_body(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            body: &'a str,
        }
        let url = self.api(&format!("/repos/{owner}/{repo}/issues/{number}"));
        self.http
            .patch(&url)
            .header("Authorization", self.auth())
            .json(&Body { body })
            .send()
            .await
            .context("edit issue body")?
            .error_for_status()
            .context("edit issue body response")?;
        Ok(())
    }

    // ── Issue content ─────────────────────────────────────────────────────────

    pub async fn get_issue(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Issue> {
        let url = self.api(&format!("/repos/{owner}/{repo}/issues/{number}"));
        let issue: Issue = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("get issue")?
            .error_for_status()
            .context("get issue response")?
            .json()
            .await?;
        Ok(issue)
    }

    pub async fn get_issue_body(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<String> {
        let issue = self.get_issue(owner, repo, number).await?;
        Ok(issue.body.unwrap_or_default())
    }

    pub async fn list_issue_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<Comment>> {
        let url =
            self.api(&format!("/repos/{owner}/{repo}/issues/{number}/comments"));
        let comments: Vec<Comment> = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("list comments")?
            .error_for_status()
            .context("list comments response")?
            .json()
            .await?;
        Ok(comments)
    }

    pub async fn post_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        let url =
            self.api(&format!("/repos/{owner}/{repo}/issues/{number}/comments"));
        self.http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&CreateCommentBody { body })
            .send()
            .await
            .context("post comment")?
            .error_for_status()
            .context("post comment response")?;
        Ok(())
    }

    // ── Branch operations ─────────────────────────────────────────────────────

    pub async fn list_branches(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<Branch>> {
        let url = self.api(&format!("/repos/{owner}/{repo}/branches"));
        let branches: Vec<Branch> = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("list branches")?
            .error_for_status()
            .context("list branches response")?
            .json()
            .await?;
        Ok(branches)
    }

    pub async fn create_branch(
        &self,
        owner: &str,
        repo: &str,
        new_branch: &str,
        from_branch: &str,
    ) -> Result<()> {
        let url = self.api(&format!("/repos/{owner}/{repo}/branches"));
        self.http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&CreateBranchBody {
                new_branch_name: new_branch,
                old_branch_name: from_branch,
            })
            .send()
            .await
            .context("create branch")?
            .error_for_status()
            .context("create branch response")?;
        Ok(())
    }

    // ── Pull request operations ───────────────────────────────────────────────

    pub async fn create_pr(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
        head: &str,
        base: &str,
    ) -> Result<PullRequest> {
        let url = self.api(&format!("/repos/{owner}/{repo}/pulls"));
        let pr: PullRequest = self
            .http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&CreatePrBody { title, body, head, base })
            .send()
            .await
            .context("create PR")?
            .error_for_status()
            .context("create PR response")?
            .json()
            .await?;
        Ok(pr)
    }

    // ── File creation ─────────────────────────────────────────────────────────

    /// Create or update a file in a repository via the Forgejo contents API.
    pub async fn create_file(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        content: &str,
        message: &str,
        branch: &str,
    ) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            content: String,
            message: &'a str,
            branch: &'a str,
        }
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(content);
        let url = self.api(&format!("/repos/{owner}/{repo}/contents/{path}"));
        self.http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&Body { content: encoded, message, branch })
            .send()
            .await
            .context("create file")?
            .error_for_status()
            .context("create file response")?;
        Ok(())
    }

    // ── Repository management ─────────────────────────────────────────────────

    /// Create a repository under the authenticated user's account.
    pub async fn create_repo(&self, name: &str) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            name: &'a str,
            auto_init: bool,
            private: bool,
        }
        let url = self.api("/user/repos");
        self.http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&Body { name, auto_init: false, private: false })
            .send()
            .await
            .context("create repo")?
            .error_for_status()
            .context("create repo response")?;
        Ok(())
    }

    /// Delete a repository.
    pub async fn delete_repo(&self, owner: &str, repo: &str) -> Result<()> {
        let url = self.api(&format!("/repos/{owner}/{repo}"));
        self.http
            .delete(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("delete repo")?
            .error_for_status()
            .context("delete repo response")?;
        Ok(())
    }

    /// Register a webhook on a repository that sends `issues` events to `target_url`.
    pub async fn create_webhook(
        &self,
        owner: &str,
        repo: &str,
        target_url: &str,
    ) -> Result<u64> {
        #[derive(serde::Serialize)]
        struct Config<'a> {
            url: &'a str,
            content_type: &'static str,
        }
        #[derive(serde::Serialize)]
        struct Body<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            config: Config<'a>,
            events: &'static [&'static str],
            active: bool,
        }
        #[derive(serde::Deserialize)]
        struct Created {
            id: u64,
        }
        let url = self.api(&format!("/repos/{owner}/{repo}/hooks"));
        let resp: Created = self
            .http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&Body {
                kind: "gitea",
                config: Config { url: target_url, content_type: "json" },
                events: &["issues"],
                active: true,
            })
            .send()
            .await
            .context("create webhook")?
            .error_for_status()
            .context("create webhook response")?
            .json()
            .await?;
        Ok(resp.id)
    }

    pub async fn get_pr(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullRequest> {
        let url = self.api(&format!("/repos/{owner}/{repo}/pulls/{number}"));
        let pr: PullRequest = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("get PR")?
            .error_for_status()
            .context("get PR response")?
            .json()
            .await?;
        Ok(pr)
    }

    /// List open PRs for a repo.
    pub async fn list_prs(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
    ) -> Result<Vec<PullRequest>> {
        let url = self.api(&format!("/repos/{owner}/{repo}/pulls?state={state}&limit=50"));
        let prs: Vec<PullRequest> = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("list PRs")?
            .error_for_status()
            .context("list PRs response")?
            .json()
            .await?;
        Ok(prs)
    }

    /// Submit a review on a pull request.
    /// `event` should be "APPROVED" or "REQUEST_CHANGES".
    pub async fn submit_review(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        body: &str,
        event: &str,
    ) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            body: &'a str,
            event: &'a str,
        }
        let url = self.api(&format!("/repos/{owner}/{repo}/pulls/{pr_number}/reviews"));
        self.http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&Body { body, event })
            .send()
            .await
            .context("submit review")?
            .error_for_status()
            .context("submit review response")?;
        Ok(())
    }

    /// Merge a pull request.
    /// `merge_style` should be "merge", "squash", or "rebase".
    pub async fn merge_pr(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        merge_style: &str,
    ) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            #[serde(rename = "Do")]
            do_action: &'a str,
        }
        let url = self.api(&format!("/repos/{owner}/{repo}/pulls/{pr_number}/merge"));
        self.http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&Body { do_action: merge_style })
            .send()
            .await
            .context("merge PR")?
            .error_for_status()
            .context("merge PR response")?;
        Ok(())
    }

    /// Add a reviewer to a pull request.
    pub async fn add_pr_reviewer(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
        reviewer_login: &str,
    ) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            reviewers: Vec<&'a str>,
        }
        let url = self.api(&format!(
            "/repos/{owner}/{repo}/pulls/{pr_number}/requested_reviewers"
        ));
        self.http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&Body { reviewers: vec![reviewer_login] })
            .send()
            .await
            .context("add PR reviewer")?
            .error_for_status()
            .context("add PR reviewer response")?;
        Ok(())
    }

    /// List changed files in a pull request.
    pub async fn list_pr_files(
        &self,
        owner: &str,
        repo: &str,
        pr_number: u64,
    ) -> Result<Vec<ChangedFile>> {
        let url = self.api(&format!("/repos/{owner}/{repo}/pulls/{pr_number}/files"));
        let files: Vec<ChangedFile> = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("list PR files")?
            .error_for_status()
            .context("list PR files response")?
            .json()
            .await?;
        Ok(files)
    }

    // ── Forgejo Actions API ──────────────────────────────────────────────────

    /// Trigger a workflow via `workflow_dispatch`.
    pub async fn dispatch_workflow(
        &self,
        owner: &str,
        repo: &str,
        workflow: &str,
        git_ref: &str,
        inputs: &std::collections::HashMap<String, String>,
    ) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Body<'a> {
            #[serde(rename = "ref")]
            git_ref: &'a str,
            inputs: &'a std::collections::HashMap<String, String>,
        }
        let url = self.api(&format!(
            "/repos/{owner}/{repo}/actions/workflows/{workflow}/dispatches"
        ));
        self.http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&Body { git_ref, inputs })
            .send()
            .await
            .context("dispatch workflow")?
            .error_for_status()
            .context("dispatch workflow response")?;
        Ok(())
    }

    /// List recent action runs for a repository.
    pub async fn list_action_runs(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<ActionRunList> {
        let url = self.api(&format!(
            "/repos/{owner}/{repo}/actions/runs?limit=10"
        ));
        let resp: ActionRunList = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("list action runs")?
            .error_for_status()
            .context("list action runs response")?
            .json()
            .await?;
        Ok(resp)
    }

    /// Get a specific action run by ID.
    pub async fn get_action_run(
        &self,
        owner: &str,
        repo: &str,
        run_id: u64,
    ) -> Result<ActionRun> {
        let url = self.api(&format!(
            "/repos/{owner}/{repo}/actions/runs/{run_id}"
        ));
        let run: ActionRun = self
            .http
            .get(&url)
            .header("Authorization", self.auth())
            .send()
            .await
            .context("get action run")?
            .error_for_status()
            .context("get action run response")?
            .json()
            .await?;
        Ok(run)
    }
}

// ── Forgejo Actions types ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ActionRunList {
    pub workflow_runs: Vec<ActionRun>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActionRun {
    pub id: u64,
    /// Run status — Forgejo uses "waiting", "running", "success", "failure", "cancelled".
    pub status: String,
    /// The event type that triggered this run.
    #[serde(default)]
    pub trigger_event: String,
    /// JSON-encoded event payload containing workflow_dispatch inputs.
    #[serde(default)]
    pub event_payload: String,
    pub created: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub html_url: Option<String>,
}

impl ActionRun {
    /// Returns true if the run has finished (success, failure, or cancelled).
    pub fn is_completed(&self) -> bool {
        matches!(
            self.status.as_str(),
            "success" | "failure" | "cancelled"
        )
    }

    /// Returns true if the run succeeded.
    pub fn is_success(&self) -> bool {
        self.status == "success"
    }

    /// Extract `issue_number` from the workflow_dispatch event payload inputs.
    pub fn issue_number(&self) -> Option<u64> {
        let payload: serde_json::Value = serde_json::from_str(&self.event_payload).ok()?;
        payload.get("inputs")?.get("issue_number")?.as_str()?.parse().ok()
    }
}
