/// Integration tests for the full seed → sidecar pipeline.
///
/// # Prerequisites
///
/// Before running these tests, apply the test Terraform environment:
///
///   cd infra/test
///   terraform init
///   terraform apply -var-file=test.tfvars -var-file=test.secrets.tfvars
///
/// Then export the required env vars (or source the helper output):
///
///   source <(terraform output -raw env_exports)
///
/// The tests create and close issues within the pre-provisioned TEST_REPO.
/// They do not create or delete repositories.
///
/// Run with:
///   cargo test -p workflow-integration-tests -- --include-ignored
use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use workflow_types::JobState;
use workflow_worker::{client::SidecarClient, forgejo::ForgejoClient};

// ── Fixture types (mirrors demo/fixtures/*.json) ───────────────────────────

#[derive(Deserialize)]
struct FixtureJob {
    id: String,
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Deserialize)]
struct Fixture {
    jobs: Vec<FixtureJob>,
}

// ── Environment ────────────────────────────────────────────────────────────

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

// ── Test context ───────────────────────────────────────────────────────────

/// Wraps the shared pre-provisioned repo.
/// On drop, closes all issues created during the test so they don't
/// accumulate across runs.
struct TestContext {
    forgejo: ForgejoClient,
    sidecar: SidecarClient,
    owner: String,
    repo: String,
    /// Issue numbers created by this test, to be closed during teardown.
    created: Vec<u64>,
}

impl TestContext {
    fn new() -> Self {
        let forgejo_url = env("FORGEJO_URL", "http://localhost:3000");
        let forgejo_token = env("FORGEJO_TOKEN", "");
        let sidecar_url = env("SIDECAR_URL", "http://localhost:8080");
        let owner = env("TEST_OWNER", "admin");
        let repo = env("TEST_REPO", "workflow-test");
        Self {
            forgejo: ForgejoClient::new(&forgejo_url, &forgejo_token),
            sidecar: SidecarClient::new(&sidecar_url),
            owner,
            repo,
            created: Vec::new(),
        }
    }

    /// Close all issues created by this test context.
    async fn teardown(self) {
        for number in self.created {
            let _ = self.forgejo.close_issue(&self.owner, &self.repo, number).await;
        }
    }
}

// ── Seed helper ────────────────────────────────────────────────────────────

/// Seed a fixture into the test repo, returning a map of symbolic id → issue number.
async fn seed(ctx: &mut TestContext, fixture: &Fixture) -> Result<HashMap<String, u64>> {
    let mut id_to_number: HashMap<String, u64> = HashMap::new();

    // Phase 1: create all issues.
    for job in &fixture.jobs {
        let tmp = if job.body.is_empty() { job.title.as_str() } else { job.body.as_str() };
        let number = ctx.forgejo.create_issue(&ctx.owner, &ctx.repo, &job.title, tmp).await?;
        id_to_number.insert(job.id.clone(), number);
        ctx.created.push(number);
    }

    // Phase 2: patch bodies with <!-- workflow:deps:N,N --> markers.
    for job in &fixture.jobs {
        if job.depends_on.is_empty() {
            continue;
        }
        let mut missing = Vec::new();
        let dep_numbers: Vec<String> = job
            .depends_on
            .iter()
            .map(|dep_id: &String| {
                id_to_number.get(dep_id).map(|n| n.to_string()).unwrap_or_else(|| {
                    missing.push(dep_id.clone());
                    dep_id.clone()
                })
            })
            .collect();
        if !missing.is_empty() {
            bail!("fixture '{}' references unknown dep ids: {:?}", job.id, missing);
        }
        let number = *id_to_number.get(&job.id).unwrap();
        let marker = format!("<!-- workflow:deps:{} -->", dep_numbers.join(","));
        let new_body = if job.body.is_empty() {
            marker
        } else {
            format!("{}\n\n{marker}", job.body)
        };
        ctx.forgejo.edit_issue_body(&ctx.owner, &ctx.repo, number, &new_body).await?;
    }

    Ok(id_to_number)
}

// ── Polling helper ─────────────────────────────────────────────────────────

async fn poll_until<F, Fut, T>(timeout: Duration, f: F) -> Option<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f().await {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn load_fixture(filename: &str) -> Result<Fixture> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../demo/fixtures")
        .join(filename);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read fixture {}", path.display()))?;
    serde_json::from_str(&raw).context("parse fixture")
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// After seeding the linear chain, only the first job (no deps) should be
/// on-deck; every other job should be blocked.
#[tokio::test]
#[ignore = "requires: terraform apply infra/test + env vars from terraform output -raw env_exports"]
async fn chain_initial_states() {
    let mut ctx = TestContext::new();
    let fixture = load_fixture("chain.json").unwrap();

    let id_map = seed(&mut ctx, &fixture).await.unwrap();

    let jobs = poll_until(Duration::from_secs(30), || async {
        let all = ctx.sidecar.list_jobs(None).await.ok()?;
        // Filter to the exact issue numbers we just created.
        let mine: Vec<_> = all
            .into_iter()
            .filter(|j| ctx.created.contains(&j.number)
                && j.repo_owner == ctx.owner
                && j.repo_name == ctx.repo)
            .collect();
        let stable = mine.iter().all(|j| {
            j.state == JobState::OnDeck || j.state == JobState::Blocked
        });
        if mine.len() == fixture.jobs.len() && stable { Some(mine) } else { None }
    })
    .await
    .expect("timed out waiting for sidecar to process chain jobs");

    let by_number: HashMap<u64, _> = jobs.iter().map(|j| (j.number, j)).collect();

    assert_eq!(by_number[&id_map["setup"]].state, JobState::OnDeck, "setup should be on-deck");
    for id in &["schema", "api", "tests", "docs", "release"] {
        assert_eq!(
            by_number[&id_map[*id]].state,
            JobState::Blocked,
            "'{id}' should be blocked"
        );
    }

    ctx.teardown().await;
}

/// Completing the first job in the chain should unblock only the immediate
/// dependent; everything further down remains blocked.
#[tokio::test]
#[ignore = "requires: terraform apply infra/test + env vars from terraform output -raw env_exports"]
async fn chain_completing_head_unblocks_next() {
    let mut ctx = TestContext::new();
    let fixture = load_fixture("chain.json").unwrap();

    let id_map = seed(&mut ctx, &fixture).await.unwrap();

    // Wait for stable initial state.
    poll_until(Duration::from_secs(30), || async {
        let all = ctx.sidecar.list_jobs(None).await.ok()?;
        let mine: Vec<_> = all
            .into_iter()
            .filter(|j| ctx.created.contains(&j.number))
            .collect();
        let stable = mine.iter().all(|j| {
            j.state == JobState::OnDeck || j.state == JobState::Blocked
        });
        if mine.len() == fixture.jobs.len() && stable { Some(()) } else { None }
    })
    .await
    .expect("timed out waiting for initial state");

    // Claim and complete "setup".
    let setup_n = id_map["setup"];
    let worker = env("TEST_WORKER", "test-worker");
    ctx.sidecar
        .claim(&ctx.owner, &ctx.repo, setup_n, &worker)
        .await
        .unwrap()
        .expect("setup should be claimable");
    ctx.sidecar.complete(&ctx.owner, &ctx.repo, setup_n, &worker).await.unwrap();

    // "schema" (direct dependent) should become on-deck.
    let schema_n = id_map["schema"];
    poll_until(Duration::from_secs(30), || async {
        let resp = ctx.sidecar.get_job(&ctx.owner, &ctx.repo, schema_n).await.ok()?;
        if resp.job.state == JobState::OnDeck { Some(()) } else { None }
    })
    .await
    .expect("timed out waiting for schema to become on-deck");

    // "api" should still be blocked (schema not done yet).
    let api_resp = ctx.sidecar.get_job(&ctx.owner, &ctx.repo, id_map["api"]).await.unwrap();
    assert_eq!(api_resp.job.state, JobState::Blocked, "api should still be blocked");

    ctx.teardown().await;
}

/// After seeding the hub, exactly the four root jobs should be on-deck;
/// all downstream jobs should be blocked.
#[tokio::test]
#[ignore = "requires: terraform apply infra/test + env vars from terraform output -raw env_exports"]
async fn hub_initial_states() {
    let mut ctx = TestContext::new();
    let fixture = load_fixture("hub.json").unwrap();

    let id_map = seed(&mut ctx, &fixture).await.unwrap();

    let jobs = poll_until(Duration::from_secs(60), || async {
        let all = ctx.sidecar.list_jobs(None).await.ok()?;
        let mine: Vec<_> = all
            .into_iter()
            .filter(|j| ctx.created.contains(&j.number))
            .collect();
        let stable = mine.iter().all(|j| {
            j.state == JobState::OnDeck || j.state == JobState::Blocked
        });
        if mine.len() == fixture.jobs.len() && stable { Some(mine) } else { None }
    })
    .await
    .expect("timed out waiting for sidecar to process hub jobs");

    let by_number: HashMap<u64, _> = jobs.iter().map(|j| (j.number, j)).collect();

    for id in &["infra-plan", "auth-design", "fe-wireframes", "data-model"] {
        assert_eq!(
            by_number[&id_map[*id]].state,
            JobState::OnDeck,
            "'{id}' should be on-deck"
        );
    }

    let spoke_ids = [
        "infra-terraform", "infra-ci",
        "auth-impl", "auth-tests",
        "fe-components", "fe-integration",
        "data-migrations", "data-seed",
        "hub-integration", "load-test", "security-audit", "final-release",
    ];
    for id in &spoke_ids {
        assert_eq!(
            by_number[&id_map[*id]].state,
            JobState::Blocked,
            "'{id}' should be blocked"
        );
    }

    ctx.teardown().await;
}
