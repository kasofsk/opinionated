use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use serde::Deserialize;
use workflow_types::{Job, JobState, RequeueTarget};
use workflow_worker::{
    client::SidecarClient,
    forgejo::ForgejoClient,
    worker::{Outcome, Worker, WorkerLoop},
    DispatchedWorkerLoop,
};

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "worker-cli",
    about = "Interactive CLI worker for testing the workflow system"
)]
struct Args {
    /// Sidecar base URL
    #[arg(long, env = "SIDECAR_URL", default_value = "http://localhost:3000")]
    sidecar_url: String,

    /// Forgejo base URL (required for content ops)
    #[arg(long, env = "FORGEJO_URL", default_value = "")]
    forgejo_url: String,

    /// Forgejo API token
    #[arg(long, env = "FORGEJO_TOKEN", default_value = "")]
    forgejo_token: String,

    /// Worker ID shown as the Forgejo assignee
    #[arg(long, env = "WORKER_ID", default_value = "cli-worker")]
    worker_id: String,

    /// Heartbeat interval in seconds (background task while a job is held)
    #[arg(long, default_value = "30")]
    heartbeat_secs: u64,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List jobs (optionally filter by state)
    Jobs {
        /// State filter, e.g. "on-deck", "on-the-stack", "failed"
        #[arg(long)]
        state: Option<String>,
    },

    /// Show full details of a single job
    Show {
        /// owner/repo/number
        job: String,
    },

    /// Claim the highest-priority available job and enter an interactive session
    Work {
        /// Only accept jobs whose title contains this substring (case-insensitive)
        #[arg(long)]
        filter: Option<String>,
    },

    /// Claim a specific job by owner/repo/number and enter an interactive session
    Claim {
        /// owner/repo/number
        job: String,
    },

    /// Requeue a failed or on-ice job
    Requeue {
        /// owner/repo/number
        job: String,
        /// Target state: "on-deck" or "on-ice"
        #[arg(long, default_value = "on-deck")]
        target: String,
    },

    /// Run a simulated dispatched worker that auto-completes jobs after a delay
    Sim {
        /// NATS server URL
        #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4223")]
        nats_url: String,
        /// Seconds to "work" on each job before completing
        #[arg(long, default_value = "10")]
        delay_secs: u64,
    },

    /// Run an action worker that triggers Forgejo Actions workflows for each job
    Action {
        /// NATS server URL
        #[arg(long, env = "NATS_URL", default_value = "nats://localhost:4223")]
        nats_url: String,
        /// Workflow filename to dispatch (e.g. "agent-work.yml")
        #[arg(long, env = "ACTION_WORKFLOW", default_value = "agent-work.yml")]
        workflow: String,
        /// Runner label this worker targets (only accepts jobs with matching runner:X label)
        #[arg(long, env = "ACTION_RUNNER")]
        runner: Option<String>,
        /// Git ref to dispatch the workflow on
        #[arg(long, default_value = "main")]
        git_ref: String,
        /// Poll interval in seconds when waiting for the action run to complete
        #[arg(long, default_value = "10")]
        poll_secs: u64,
        /// Maximum time in seconds to wait for an action run before failing
        #[arg(long, default_value = "3600")]
        timeout_secs: u64,
    },

    /// Seed a Forgejo repo with jobs from a fixture file
    Seed {
        /// owner/repo to create issues in
        repo: String,
        /// Path to a fixture JSON file (e.g. demo/fixtures/chain.json)
        fixture: String,
        /// Create the repo and webhook if they don't exist
        #[arg(long)]
        create_repo: bool,
        /// Webhook target URL (used with --create-repo).
        /// Defaults to SIDECAR_WEBHOOK_URL or http://host.docker.internal:8080
        #[arg(long, env = "SIDECAR_WEBHOOK_URL")]
        webhook_url: Option<String>,
    },
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("worker_cli=info".parse().unwrap()),
        )
        .init();

    let args = Args::parse();
    let sidecar = SidecarClient::new(&args.sidecar_url);

    match &args.command {
        Cmd::Jobs { state } => cmd_jobs(&sidecar, state.as_deref()).await,

        Cmd::Show { job } => cmd_show(&sidecar, job).await,

        Cmd::Work { filter } => {
            let worker = CliWorker::new(args.worker_id.clone(), filter.clone());
            let loop_ = WorkerLoop::new(
                worker,
                &args.sidecar_url,
                &args.forgejo_url,
                &args.forgejo_token,
                Duration::from_secs(args.heartbeat_secs),
            );
            match loop_.run_once().await? {
                true => Ok(()),
                false => {
                    println!("No available jobs.");
                    Ok(())
                }
            }
        }

        Cmd::Claim { job } => {
            let (owner, repo, number) = parse_job_ref(job)?;
            cmd_claim(&sidecar, &args, owner, repo, number).await
        }

        Cmd::Requeue { job, target } => {
            let (owner, repo, number) = parse_job_ref(job)?;
            let target = match target.as_str() {
                "on-deck" => RequeueTarget::OnDeck,
                "on-ice" => RequeueTarget::OnIce,
                other => bail!("unknown target: {other}; use 'on-deck' or 'on-ice'"),
            };
            sidecar.requeue(owner, repo, number, target).await?;
            println!("Requeued {job}");
            Ok(())
        }

        Cmd::Sim {
            nats_url,
            delay_secs,
        } => {
            let worker = SimWorker::new(args.worker_id.clone(), *delay_secs);
            let loop_ = DispatchedWorkerLoop::new(
                worker,
                &args.forgejo_url,
                &args.forgejo_token,
                nats_url,
                Duration::from_secs(args.heartbeat_secs),
            )
            .await?;
            println!(
                "Starting sim worker '{}' (delay={}s, nats={})",
                args.worker_id, delay_secs, nats_url
            );
            loop_.run().await
        }

        Cmd::Action {
            nats_url,
            workflow,
            runner,
            git_ref,
            poll_secs,
            timeout_secs,
        } => {
            let worker = ActionWorker::new(
                args.worker_id.clone(),
                workflow.clone(),
                runner.clone(),
                git_ref.clone(),
                *poll_secs,
                *timeout_secs,
            );
            let runner_desc = runner.as_deref().unwrap_or("any");
            let loop_ = DispatchedWorkerLoop::new(
                worker,
                &args.forgejo_url,
                &args.forgejo_token,
                nats_url,
                Duration::from_secs(args.heartbeat_secs),
            )
            .await?;
            println!(
                "Starting action worker '{}' (workflow={}, runner={}, nats={})",
                args.worker_id, workflow, runner_desc, nats_url
            );
            loop_.run().await
        }

        Cmd::Seed {
            repo,
            fixture,
            create_repo,
            webhook_url,
        } => {
            let forgejo = ForgejoClient::new(&args.forgejo_url, &args.forgejo_token);
            let parts: Vec<&str> = repo.splitn(2, '/').collect();
            if parts.len() != 2 {
                bail!("expected owner/repo, got: {repo}");
            }
            if *create_repo {
                // Create the repo (ignores 409 if it already exists).
                match forgejo.create_repo(parts[1]).await {
                    Ok(()) => println!("Created repo {repo}"),
                    Err(e) => {
                        let msg = format!("{e}");
                        if msg.contains("409") {
                            println!("Repo {repo} already exists");
                        } else {
                            return Err(e.context("create repo"));
                        }
                    }
                }
                // Set up the sidecar webhook.
                let wh = webhook_url
                    .as_deref()
                    .unwrap_or("http://host.docker.internal:8080");
                let target = format!("{}/webhook", wh.trim_end_matches('/'));
                forgejo
                    .create_webhook(parts[0], parts[1], &target)
                    .await
                    .context("create webhook")?;
                println!("Webhook → {target}");
            }
            cmd_seed(&forgejo, parts[0], parts[1], fixture).await
        }
    }
}

// ── commands ──────────────────────────────────────────────────────────────────

async fn cmd_jobs(sidecar: &SidecarClient, state: Option<&str>) -> Result<()> {
    let label = state.map(|s| {
        if s.starts_with("status:") {
            s.to_string()
        } else {
            format!("status:{s}")
        }
    });
    let jobs = sidecar.list_jobs(label.as_deref()).await?;

    if jobs.is_empty() {
        println!("No jobs found.");
        return Ok(());
    }

    print_job_table(&jobs);
    Ok(())
}

async fn cmd_show(sidecar: &SidecarClient, job_ref: &str) -> Result<()> {
    let (owner, repo, number) = parse_job_ref(job_ref)?;
    let resp = sidecar.get_job(owner, repo, number).await?;
    print_job_detail(&resp.job);
    if let Some(claim) = resp.claim {
        println!(
            "Claimed by: {}  (heartbeat: {})",
            claim.worker_id,
            claim.last_heartbeat.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    if let Some(failure) = resp.failure {
        println!("\nFailure ({:?}): {}", failure.kind, failure.reason);
    }
    Ok(())
}

async fn cmd_claim(
    sidecar: &SidecarClient,
    args: &Args,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<()> {
    let resp = sidecar
        .claim(owner, repo, number, &args.worker_id)
        .await?
        .context("job is already claimed or not on-deck")?;

    // Inline interactive session (not going through WorkerLoop here so we
    // have a concrete owner/repo/number to work with rather than picking
    // from the queue).
    let outcome = interactive_session(&resp.job);

    match outcome {
        Outcome::Complete => {
            sidecar
                .complete(owner, repo, number, &args.worker_id)
                .await?;
            println!("✓ Job marked complete.");
        }
        Outcome::Fail { reason, .. } => {
            sidecar
                .fail(owner, repo, number, &args.worker_id, reason, None)
                .await?;
            println!("✗ Job marked failed.");
        }
        Outcome::Abandon => {
            sidecar
                .abandon(owner, repo, number, &args.worker_id)
                .await?;
            println!("→ Job returned to on-deck.");
        }
        Outcome::Yield => {
            sidecar
                .abandon(owner, repo, number, &args.worker_id)
                .await?;
            println!("⏳ Yielded — waiting for external transition.");
        }
    }
    Ok(())
}

// ── seed command ──────────────────────────────────────────────────────────────

/// A job definition as stored in a fixture JSON file.
#[derive(Deserialize)]
struct FixtureJob {
    /// Stable symbolic identifier used to express dependencies within the file.
    id: String,
    title: String,
    /// Issue body (plain text or Markdown).
    #[serde(default)]
    body: String,
    /// Forgejo label names to apply (e.g. `["priority:80"]`).
    /// Currently stored for reference; label creation via the Forgejo API is future work.
    #[serde(default)]
    #[allow(dead_code)]
    labels: Vec<String>,
    /// IDs of jobs (within this fixture) that must complete first.
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Deserialize)]
struct Fixture {
    name: String,
    #[allow(dead_code)]
    description: String,
    jobs: Vec<FixtureJob>,
}

async fn cmd_seed(
    forgejo: &ForgejoClient,
    owner: &str,
    repo: &str,
    fixture_path: &str,
) -> Result<()> {
    let raw =
        std::fs::read_to_string(fixture_path).with_context(|| format!("read {fixture_path}"))?;
    let fixture: Fixture =
        serde_json::from_str(&raw).with_context(|| format!("parse {fixture_path}"))?;

    // Derive sidecar URL from SIDECAR_URL env (default localhost:8080).
    let sidecar_url =
        std::env::var("SIDECAR_URL").unwrap_or_else(|_| "http://localhost:8080".into());
    let sidecar = SidecarClient::new(&sidecar_url);

    println!("Seeding \"{}\" into {owner}/{repo} …", fixture.name);

    // Fetch repo labels so we can apply status labels at issue creation time.
    let repo_labels = forgejo.list_repo_labels(owner, repo).await?;
    let blocked_label_id = repo_labels
        .iter()
        .find(|l| l.name.as_deref() == Some("status:blocked"))
        .and_then(|l| l.id.map(|id| id as u64));
    let on_deck_label_id = repo_labels
        .iter()
        .find(|l| l.name.as_deref() == Some("status:on-deck"))
        .and_then(|l| l.id.map(|id| id as u64));

    // Single pass — create each issue with deps and labels in one shot.
    // Fixtures are DAGs so all dep IDs have already been created by the time
    // we reach a job that depends on them.
    let mut id_to_number: HashMap<String, u64> = HashMap::new();

    for job in &fixture.jobs {
        // Resolve dep IDs → issue numbers (all known since deps reference earlier jobs).
        let dep_numbers: Vec<u64> = job
            .depends_on
            .iter()
            .filter_map(|dep_id| {
                let n = id_to_number.get(dep_id).copied();
                if n.is_none() {
                    eprintln!("warning: unknown dep id '{dep_id}' in job '{}'", job.id);
                }
                n
            })
            .collect();

        // Build body with dep marker included from the start.
        let base_body = if job.body.is_empty() {
            job.title.clone()
        } else {
            job.body.clone()
        };
        let body = if dep_numbers.is_empty() {
            base_body
        } else {
            let dep_csv: Vec<String> = dep_numbers.iter().map(|n| n.to_string()).collect();
            let dep_links: Vec<String> = dep_numbers.iter().map(|n| format!("- #{n}")).collect();
            format!(
                "{base_body}\n\n## Dependencies\n\n{}\n\n<!-- workflow:deps:{} -->",
                dep_links.join("\n"),
                dep_csv.join(","),
            )
        };

        // Apply initial status label: on-deck if no deps, blocked if has deps.
        let label_ids: Vec<u64> = if dep_numbers.is_empty() {
            on_deck_label_id.into_iter().collect()
        } else {
            blocked_label_id.into_iter().collect()
        };

        let number = forgejo
            .create_issue_with_labels(owner, repo, &job.title, &body, &label_ids)
            .await?;

        if dep_numbers.is_empty() {
            println!("  #{number}  {}", job.title);
        } else {
            let dep_str: Vec<String> = dep_numbers.iter().map(|n| format!("#{n}")).collect();
            println!("  #{number}  {} (deps: {})", job.title, dep_str.join(", "));
        }

        id_to_number.insert(job.id.clone(), number);

        // Wait for sidecar to acknowledge this issue and its deps.
        wait_for_job(&sidecar, owner, repo, number).await?;
        if !dep_numbers.is_empty() {
            wait_for_deps(&sidecar, owner, repo, number, &dep_numbers).await?;
        }
    }

    println!("Done. {} issues created.", fixture.jobs.len());
    Ok(())
}

/// Poll the sidecar until a job appears in the graph.
async fn wait_for_job(sidecar: &SidecarClient, owner: &str, repo: &str, number: u64) -> Result<()> {
    for _ in 0..120 {
        if sidecar.get_job(owner, repo, number).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("timed out waiting for sidecar to sync #{number}");
}

/// Poll the sidecar until a job's dependency list matches expectations.
async fn wait_for_deps(
    sidecar: &SidecarClient,
    owner: &str,
    repo: &str,
    number: u64,
    expected: &[u64],
) -> Result<()> {
    for _ in 0..120 {
        if let Ok(resp) = sidecar.get_job(owner, repo, number).await {
            let mut got = resp.job.dependency_numbers.clone();
            let mut want = expected.to_vec();
            got.sort();
            want.sort();
            if got == want {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("timed out waiting for deps on #{number}");
}

// ── CliWorker ─────────────────────────────────────────────────────────────────

struct CliWorker {
    worker_id: String,
    title_filter: Option<String>,
}

impl CliWorker {
    fn new(worker_id: String, title_filter: Option<String>) -> Self {
        Self {
            worker_id,
            title_filter,
        }
    }
}

#[async_trait]
impl Worker for CliWorker {
    fn worker_id(&self) -> &str {
        &self.worker_id
    }

    fn accepts(&self, job: &Job) -> bool {
        match &self.title_filter {
            Some(f) => job.title.to_lowercase().contains(&f.to_lowercase()),
            None => true,
        }
    }

    async fn execute(&self, job: &Job, _forgejo: &ForgejoClient) -> Result<Outcome> {
        Ok(interactive_session(job))
    }
}

// ── SimWorker ────────────────────────────────────────────────────────────────

struct SimWorker {
    worker_id: String,
    delay_secs: u64,
}

impl SimWorker {
    fn new(worker_id: String, delay_secs: u64) -> Self {
        Self {
            worker_id,
            delay_secs,
        }
    }
}

#[async_trait]
impl Worker for SimWorker {
    fn worker_id(&self) -> &str {
        &self.worker_id
    }

    async fn execute(&self, job: &Job, forgejo: &ForgejoClient) -> Result<Outcome> {
        tracing::info!(
            key = %job.key(),
            title = %job.title,
            priority = job.priority,
            "simulating work for {}s",
            self.delay_secs
        );

        // Create a branch, commit a file, and open a PR so there's something to review.
        let branch_name = format!("work/{}/{}", self.worker_id, job.number);
        let file_path = format!("work/{}.md", job.number);
        let timestamp = chrono::Utc::now().to_rfc3339();
        let file_content = format!(
            "# {}\n\nWorker: {}\nTimestamp: {}\n",
            job.title, self.worker_id, timestamp
        );
        let commit_msg = format!(
            "work: {} (#{}) by {}",
            job.title, job.number, self.worker_id
        );
        let pr_body = format!(
            "Closes #{}\n\nSimulated work by {}",
            job.number, self.worker_id
        );

        match forgejo
            .create_branch(&job.repo_owner, &job.repo_name, &branch_name, "main")
            .await
        {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(key = %job.key(), error = %e, "branch creation failed (may already exist from rework), continuing");
            }
        }

        match forgejo
            .create_file(
                &job.repo_owner,
                &job.repo_name,
                &file_path,
                &file_content,
                &commit_msg,
                &branch_name,
            )
            .await
        {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(key = %job.key(), error = %e, "file creation failed, continuing");
            }
        }

        match forgejo
            .create_pr(
                &job.repo_owner,
                &job.repo_name,
                &job.title,
                &pr_body,
                &branch_name,
                "main",
            )
            .await
        {
            Ok(pr) => {
                tracing::info!(key = %job.key(), pr_number = pr.number, "opened PR");
            }
            Err(e) => {
                tracing::warn!(key = %job.key(), error = %e, "PR creation failed (may already exist), continuing");
            }
        }

        tokio::time::sleep(Duration::from_secs(self.delay_secs)).await;
        tracing::info!(key = %job.key(), "sim complete, yielding for review");
        Ok(Outcome::Yield)
    }
}

// ── ActionWorker ──────────────────────────────────────────────────────────────

struct ActionWorker {
    worker_id: String,
    workflow: String,
    runner: Option<String>,
    git_ref: String,
    poll_secs: u64,
    timeout_secs: u64,
}

impl ActionWorker {
    fn new(
        worker_id: String,
        workflow: String,
        runner: Option<String>,
        git_ref: String,
        poll_secs: u64,
        timeout_secs: u64,
    ) -> Self {
        Self {
            worker_id,
            workflow,
            runner,
            git_ref,
            poll_secs,
            timeout_secs,
        }
    }
}

#[async_trait]
impl Worker for ActionWorker {
    fn worker_id(&self) -> &str {
        &self.worker_id
    }

    async fn execute(&self, job: &Job, forgejo: &ForgejoClient) -> Result<Outcome> {
        let runner_label = self.runner.as_deref().unwrap_or("ubuntu-latest");

        // Create a work branch so the action run is associated with it (not main).
        let branch_name = format!("work/action/{}", job.number);
        match forgejo
            .create_branch(&job.repo_owner, &job.repo_name, &branch_name, &self.git_ref)
            .await
        {
            Ok(()) => {}
            Err(e) => {
                tracing::debug!(key = %job.key(), error = %e, "branch may already exist, continuing");
            }
        }

        tracing::info!(
            key = %job.key(),
            title = %job.title,
            runner = runner_label,
            workflow = %self.workflow,
            branch = %branch_name,
            "dispatching action"
        );

        // Build workflow dispatch inputs.
        let mut inputs = std::collections::HashMap::new();
        inputs.insert("issue_number".to_string(), job.number.to_string());
        inputs.insert("runner_label".to_string(), runner_label.to_string());
        inputs.insert(
            "forgejo_url".to_string(),
            std::env::var("FORGEJO_URL").unwrap_or_default(),
        );

        // Dispatch the workflow on the work branch so the run is tied to it.
        let run_id = forgejo
            .dispatch_workflow(
                &job.repo_owner,
                &job.repo_name,
                &self.workflow,
                &branch_name,
                &inputs,
            )
            .await?
            .unwrap_or(0);

        tracing::info!(key = %job.key(), run_id, "tracking action run");

        // Poll until the run completes or we time out.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(self.timeout_secs);
        loop {
            if tokio::time::Instant::now() > deadline {
                return Ok(Outcome::Fail {
                    reason: format!("action run {run_id} timed out after {}s", self.timeout_secs),
                    logs: None,
                });
            }

            tokio::time::sleep(Duration::from_secs(self.poll_secs)).await;

            let run = forgejo
                .get_action_run(&job.repo_owner, &job.repo_name, run_id)
                .await?;

            if run.is_completed() {
                if run.is_success() {
                    tracing::info!(key = %job.key(), run_id, "action run succeeded, yielding to CDC");
                    return Ok(Outcome::Yield);
                } else {
                    let reason =
                        format!("action run {run_id} finished with status={}", run.status,);
                    tracing::warn!(key = %job.key(), run_id, %reason, "action run failed");
                    return Ok(Outcome::Fail { reason, logs: None });
                }
            }

            tracing::debug!(
                key = %job.key(),
                run_id,
                status = %run.status,
                "action still running"
            );
        }
    }
}

// ── interactive session ───────────────────────────────────────────────────────

fn interactive_session(job: &Job) -> Outcome {
    print_job_detail(job);
    println!();
    println!("Heartbeat running in background.");
    println!();
    print_session_help();

    loop {
        print!("> ");
        let _ = io::stdout().flush();

        let mut line = String::new();
        match io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => return Outcome::Abandon, // EOF
            Ok(_) => {}
        }

        let input = line.trim();
        let parts: Vec<&str> = input.splitn(2, ' ').collect();

        match parts.as_slice() {
            [] => {}
            ["done"] | ["d"] => {
                return Outcome::Complete;
            }
            ["fail"] => {
                print!("Reason: ");
                let _ = io::stdout().flush();
                let mut reason = String::new();
                let _ = io::stdin().lock().read_line(&mut reason);
                let reason = reason.trim().to_string();
                let reason = if reason.is_empty() {
                    "no reason given".to_string()
                } else {
                    reason
                };
                return Outcome::Fail { reason, logs: None };
            }
            ["fail", reason] => {
                return Outcome::Fail {
                    reason: reason.to_string(),
                    logs: None,
                };
            }
            ["abandon"] | ["a"] => {
                return Outcome::Abandon;
            }
            ["show"] | ["s"] => {
                print_job_detail(job);
            }
            ["help"] | ["h"] | ["?"] => {
                print_session_help();
            }
            [""] => {}
            [cmd, ..] => {
                println!("Unknown command: {cmd}  (type 'help' for commands)");
            }
        }
    }
}

fn print_session_help() {
    println!("Commands:");
    println!("  done              Mark job complete → in-review");
    println!("  fail [reason]     Mark job failed (prompts for reason if omitted)");
    println!("  abandon           Return job to on-deck");
    println!("  show              Print job details again");
    println!("  help              Show this message");
}

// ── display helpers ───────────────────────────────────────────────────────────

fn print_job_table(jobs: &[Job]) {
    let w_ref = 24usize;
    let w_title = 38usize;
    let w_state = 14usize;
    let w_pri = 4usize;
    let w_deps = 4usize;

    println!(
        "{:<w_ref$}  {:<w_title$}  {:<w_state$}  {:>w_pri$}  {:>w_deps$}",
        "Ref", "Title", "State", "Prio", "Deps"
    );
    println!(
        "{}",
        "─".repeat(w_ref + w_title + w_state + w_pri + w_deps + 8)
    );

    for job in jobs {
        let job_ref = format!("{}/{}/#{}", job.repo_owner, job.repo_name, job.number);
        let title = truncate(&job.title, w_title);
        let state = state_label(&job.state);
        println!(
            "{:<w_ref$}  {:<w_title$}  {:<w_state$}  {:>w_pri$}  {:>w_deps$}",
            truncate(&job_ref, w_ref),
            title,
            state,
            job.priority,
            job.dependency_numbers.len(),
        );
    }
}

fn print_job_detail(job: &Job) {
    let separator = "─".repeat(60);
    println!("{separator}");
    println!(
        "Job:      {}/{}/#{} ",
        job.repo_owner, job.repo_name, job.number
    );
    println!("Title:    {}", job.title);
    println!("State:    {}", state_label(&job.state));
    println!("Priority: {}", job.priority);
    if let Some(t) = job.timeout_secs {
        println!("Timeout:  {t}s");
    }
    if !job.dependency_numbers.is_empty() {
        let deps: Vec<String> = job
            .dependency_numbers
            .iter()
            .map(|n| format!("#{n}"))
            .collect();
        println!("Deps:     {}", deps.join(", "));
    }
    if !job.assignees.is_empty() {
        println!("Assigned: {}", job.assignees.join(", "));
    }
    println!("{separator}");
}

fn state_label(state: &JobState) -> &'static str {
    match state {
        JobState::OnIce => "on-ice",
        JobState::Blocked => "blocked",
        JobState::OnDeck => "on-deck",
        JobState::OnTheStack => "on-the-stack",
        JobState::InReview => "in-review",
        JobState::Rework => "rework",
        JobState::Done => "done",
        JobState::Failed => "failed",
        JobState::Revoked => "revoked",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_job_ref(s: &str) -> Result<(&str, &str, u64)> {
    let parts: Vec<&str> = s.splitn(3, '/').collect();
    if parts.len() != 3 {
        bail!("expected owner/repo/number, got: {s}");
    }
    let number: u64 = parts[2]
        .parse()
        .with_context(|| format!("invalid issue number: {}", parts[2]))?;
    Ok((parts[0], parts[1], number))
}
