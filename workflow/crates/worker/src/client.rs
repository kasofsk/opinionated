use anyhow::{Context, Result};
use reqwest::Client;
use workflow_types::{
    AbandonRequest, ClaimRequest, ClaimResponse, CompleteRequest, DepsResponse,
    FactoryListResponse, FailRequest, HeartbeatRequest, Job, JobListResponse, JobResponse,
    RequeueRequest, RequeueTarget,
};

/// Typed HTTP client for the sidecar API.
#[derive(Clone)]
pub struct SidecarClient {
    base_url: String,
    http: Client,
}

impl SidecarClient {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    // ── Discovery ─────────────────────────────────────────────────────────────

    /// List all jobs, optionally filtered by state label (e.g. `"status:on-deck"`).
    pub async fn list_jobs(&self, state: Option<&str>) -> Result<Vec<Job>> {
        let mut url = self.url("/jobs");
        if let Some(s) = state {
            url = format!("{url}?state={s}");
        }
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .context("list jobs")?
            .error_for_status()
            .context("list jobs response")?;
        let body: JobListResponse = resp.json().await?;
        Ok(body.jobs)
    }

    pub async fn get_job(&self, owner: &str, repo: &str, number: u64) -> Result<JobResponse> {
        let resp = self
            .http
            .get(self.url(&format!("/jobs/{owner}/{repo}/{number}")))
            .send()
            .await
            .context("get job")?
            .error_for_status()
            .context("get job response")?;
        Ok(resp.json().await?)
    }

    pub async fn get_deps(&self, owner: &str, repo: &str, number: u64) -> Result<DepsResponse> {
        let resp = self
            .http
            .get(self.url(&format!("/jobs/{owner}/{repo}/{number}/deps")))
            .send()
            .await
            .context("get deps")?
            .error_for_status()
            .context("get deps response")?;
        Ok(resp.json().await?)
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────────

    pub async fn claim(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        worker_id: &str,
    ) -> Result<Option<ClaimResponse>> {
        let resp = self
            .http
            .post(self.url(&format!("/jobs/{owner}/{repo}/{number}/claim")))
            .json(&ClaimRequest {
                worker_id: worker_id.to_string(),
            })
            .send()
            .await
            .context("claim")?;

        if resp.status() == reqwest::StatusCode::CONFLICT {
            return Ok(None);
        }
        resp.error_for_status_ref().context("claim response")?;
        Ok(Some(resp.json().await?))
    }

    pub async fn heartbeat(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        worker_id: &str,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!("/jobs/{owner}/{repo}/{number}/heartbeat")))
            .json(&HeartbeatRequest {
                worker_id: worker_id.to_string(),
            })
            .send()
            .await
            .context("heartbeat")?
            .error_for_status()
            .context("heartbeat response")?;
        Ok(())
    }

    pub async fn complete(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        worker_id: &str,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!("/jobs/{owner}/{repo}/{number}/complete")))
            .json(&CompleteRequest {
                worker_id: worker_id.to_string(),
            })
            .send()
            .await
            .context("complete")?
            .error_for_status()
            .context("complete response")?;
        Ok(())
    }

    pub async fn abandon(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        worker_id: &str,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!("/jobs/{owner}/{repo}/{number}/abandon")))
            .json(&AbandonRequest {
                worker_id: worker_id.to_string(),
            })
            .send()
            .await
            .context("abandon")?
            .error_for_status()
            .context("abandon response")?;
        Ok(())
    }

    pub async fn fail(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        worker_id: &str,
        reason: String,
        logs: Option<String>,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!("/jobs/{owner}/{repo}/{number}/fail")))
            .json(&FailRequest {
                worker_id: worker_id.to_string(),
                reason,
                logs,
            })
            .send()
            .await
            .context("fail")?
            .error_for_status()
            .context("fail response")?;
        Ok(())
    }

    pub async fn requeue(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        target: RequeueTarget,
    ) -> Result<()> {
        self.http
            .post(self.url(&format!("/jobs/{owner}/{repo}/{number}/requeue")))
            .json(&RequeueRequest { target })
            .send()
            .await
            .context("requeue")?
            .error_for_status()
            .context("requeue response")?;
        Ok(())
    }

    // ── Factory endpoints ─────────────────────────────────────────────────────

    pub async fn list_factories(&self) -> Result<FactoryListResponse> {
        let resp = self
            .http
            .get(self.url("/factories"))
            .send()
            .await
            .context("list factories")?
            .error_for_status()
            .context("list factories response")?;
        Ok(resp.json().await?)
    }

    pub async fn trigger_factory(&self, name: &str) -> Result<()> {
        self.http
            .post(self.url(&format!("/factories/{name}/poll")))
            .send()
            .await
            .context("trigger factory")?
            .error_for_status()
            .context("trigger factory response")?;
        Ok(())
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Return all `on-deck` jobs sorted by descending priority.
    pub async fn available_jobs(&self) -> Result<Vec<Job>> {
        let mut jobs = self.list_jobs(Some("on-deck")).await?;
        jobs.sort_by(|a, b| b.priority.cmp(&a.priority));
        Ok(jobs)
    }
}
