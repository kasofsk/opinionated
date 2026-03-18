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
};

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "worker-cli", about = "Interactive CLI worker for testing the workflow system")]
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

    /// Seed a Forgejo repo with jobs from a fixture file
    Seed {
        /// owner/repo to create issues in
        repo: String,
        /// Path to a fixture JSON file (e.g. demo/fixtures/chain.json)
        fixture: String,
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

        Cmd::Seed { repo, fixture } => {
            let forgejo = ForgejoClient::new(&args.forgejo_url, &args.forgejo_token);
            let parts: Vec<&str> = repo.splitn(2, '/').collect();
            if parts.len() != 2 {
                bail!("expected owner/repo, got: {repo}");
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
        println!(
            "\nFailure ({:?}): {}",
            failure.kind, failure.reason
        );
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
            sidecar.complete(owner, repo, number, &args.worker_id).await?;
            println!("✓ Job marked complete.");
        }
        Outcome::Fail { reason, .. } => {
            sidecar
                .fail(owner, repo, number, &args.worker_id, reason, None)
                .await?;
            println!("✗ Job marked failed.");
        }
        Outcome::Abandon => {
            sidecar.abandon(owner, repo, number, &args.worker_id).await?;
            println!("→ Job returned to on-deck.");
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

    println!("Seeding \"{}\" into {owner}/{repo} …", fixture.name);

    // Phase 1 — create all issues without dep markers so each gets a real number.
    // Map symbolic id → Forgejo issue number.
    let mut id_to_number: HashMap<String, u64> = HashMap::new();

    for job in &fixture.jobs {
        // Build a temporary body without deps (added in phase 2).
        let tmp_body = if job.body.is_empty() {
            job.title.clone()
        } else {
            job.body.clone()
        };

        let number = forgejo.create_issue(owner, repo, &job.title, &tmp_body).await?;
        println!("  #{number}  {}", job.title);
        id_to_number.insert(job.id.clone(), number);
    }

    // Phase 2 — patch each issue body to inject the `<!-- workflow:deps:... -->` marker.
    for job in &fixture.jobs {
        if job.depends_on.is_empty() {
            // Optionally add labels-only header; skip dep injection.
            continue;
        }

        let dep_numbers: Vec<String> = job
            .depends_on
            .iter()
            .map(|dep_id: &String| {
                id_to_number
                    .get(dep_id)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| {
                        eprintln!("warning: unknown dep id '{dep_id}' in job '{}'", job.id);
                        dep_id.clone()
                    })
            })
            .collect();

        let number = *id_to_number.get(&job.id).unwrap();
        let dep_marker = format!("<!-- workflow:deps:{} -->", dep_numbers.join(","));

        let new_body = if job.body.is_empty() {
            dep_marker
        } else {
            format!("{}\n\n{dep_marker}", job.body)
        };

        forgejo.edit_issue_body(owner, repo, number, &new_body).await?;
        println!("  #{number}  deps → [{}]", dep_numbers.join(", "));
    }

    println!("Done. {} issues created.", fixture.jobs.len());
    Ok(())
}

// ── CliWorker ─────────────────────────────────────────────────────────────────

struct CliWorker {
    worker_id: String,
    title_filter: Option<String>,
}

impl CliWorker {
    fn new(worker_id: String, title_filter: Option<String>) -> Self {
        Self { worker_id, title_filter }
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

    async fn execute(
        &self,
        job: &Job,
        _sidecar: &SidecarClient,
        _forgejo: &ForgejoClient,
    ) -> Result<Outcome> {
        Ok(interactive_session(job))
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
                let reason =
                    if reason.is_empty() { "no reason given".to_string() } else { reason };
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
    println!("{}", "─".repeat(w_ref + w_title + w_state + w_pri + w_deps + 8));

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
        let deps: Vec<String> =
            job.dependency_numbers.iter().map(|n| format!("#{n}")).collect();
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
        JobState::Done => "done",
        JobState::Failed => "failed",
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
