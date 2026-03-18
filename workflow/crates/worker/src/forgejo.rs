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
    pub html_url: String,
    pub state: String,
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
        #[derive(serde::Serialize)]
        struct Body<'a> {
            title: &'a str,
            body: &'a str,
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
            .json(&Body { title, body })
            .send()
            .await
            .context("create issue")?
            .error_for_status()
            .context("create issue response")?
            .json()
            .await?;
        Ok(resp.number)
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
}
