use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── Job state ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    OnIce,
    Blocked,
    OnDeck,
    OnTheStack,
    InReview,
    Rework,
    Done,
    Failed,
    Revoked,
}

impl JobState {
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "status:on-ice"       => Some(Self::OnIce),
            "status:blocked"      => Some(Self::Blocked),
            "status:on-deck"      => Some(Self::OnDeck),
            "status:on-the-stack" => Some(Self::OnTheStack),
            "status:in-review"    => Some(Self::InReview),
            "status:rework"       => Some(Self::Rework),
            "status:done"         => Some(Self::Done),
            "status:failed"       => Some(Self::Failed),
            "status:revoked"      => Some(Self::Revoked),
            _                     => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::OnIce      => "status:on-ice",
            Self::Blocked    => "status:blocked",
            Self::OnDeck     => "status:on-deck",
            Self::OnTheStack => "status:on-the-stack",
            Self::InReview   => "status:in-review",
            Self::Rework     => "status:rework",
            Self::Done       => "status:done",
            Self::Failed     => "status:failed",
            Self::Revoked    => "status:revoked",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Revoked)
    }
}

// ── Job ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub repo_owner: String,
    pub repo_name: String,
    /// Forgejo issue number (per-repo, used in API paths)
    pub number: u64,
    pub title: String,
    pub state: JobState,
    pub assignees: Vec<String>,
    /// Issue numbers (same repo) this job depends on
    pub dependency_numbers: Vec<u64>,
    /// 0–100, higher = more urgent. Sourced from `priority:N` label; default 50.
    pub priority: u32,
    /// Timeout override in seconds. None = use sidecar default.
    pub timeout_secs: Option<u64>,
    /// Required capabilities from `capability:X` labels. Empty = any worker.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Max retry attempts from `retry:N` label. Default 3.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_max_retries() -> u32 {
    3
}

impl Job {
    pub fn key(&self) -> String {
        format!("{}/{}/{}", self.repo_owner, self.repo_name, self.number)
    }
}

// ── Claim state (NATS KV) ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimState {
    pub worker_id: String,
    pub claimed_at: DateTime<Utc>,
    pub last_heartbeat: DateTime<Utc>,
    /// Resolved timeout for this claim (job override or sidecar default)
    pub timeout_secs: u64,
}

impl ClaimState {
    pub fn new(worker_id: String, timeout_secs: u64) -> Self {
        let now = Utc::now();
        Self { worker_id, claimed_at: now, last_heartbeat: now, timeout_secs }
    }

    pub fn is_timed_out(&self) -> bool {
        let elapsed = Utc::now()
            .signed_duration_since(self.last_heartbeat)
            .num_seconds();
        elapsed > self.timeout_secs as i64
    }
}

// ── Failure record ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    WorkerReported,
    HeartbeatTimeout,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureRecord {
    pub worker_id: String,
    pub kind: FailureKind,
    pub reason: String,
    pub logs: Option<String>,
    pub failed_at: DateTime<Utc>,
}

impl FailureRecord {
    /// Renders the structured comment body for posting to Forgejo.
    pub fn to_comment_body(&self) -> String {
        let json = serde_json::to_string_pretty(self).unwrap_or_default();
        let kind = match self.kind {
            FailureKind::HeartbeatTimeout => "heartbeat_timeout",
            FailureKind::WorkerReported => "worker_reported",
        };
        format!(
            "<!-- workflow:failure\n{json}\n-->\n\n\
             ⚠️ **Job failed** — `{kind}` by worker `{worker}` at {at}\n\n\
             **Reason:** {reason}",
            worker = self.worker_id,
            at = self.failed_at.to_rfc3339(),
            reason = self.reason,
        )
    }
}

// ── Factory status ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactoryStatus {
    pub name: String,
    pub enabled: bool,
    pub poll_interval_secs: Option<u64>,
    pub last_poll: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FactoryListResponse {
    pub factories: Vec<FactoryStatus>,
}

// ── User info ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub login: String,
    #[serde(default)]
    pub full_name: String,
    #[serde(default)]
    pub avatar_url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserListResponse {
    pub users: Vec<UserInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LabelListResponse {
    pub labels: Vec<ForgejoLabel>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateIssueRequest {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub labels: Vec<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateIssueResponse {
    pub number: u64,
}

// ── API: request / response types ────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaimRequest {
    pub worker_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaimResponse {
    pub job: Job,
    pub claim: ClaimState,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub worker_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CompleteRequest {
    pub worker_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AbandonRequest {
    pub worker_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FailRequest {
    pub worker_id: String,
    pub reason: String,
    pub logs: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequeueTarget {
    OnDeck,
    OnIce,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RequeueRequest {
    pub target: RequeueTarget,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobListResponse {
    pub jobs: Vec<Job>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobResponse {
    pub job: Job,
    pub claim: Option<ClaimState>,
    pub failure: Option<FailureRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DepsResponse {
    pub dependencies: Vec<Job>,
    pub all_done: bool,
}

// ── Transition events ────────────────────────────────────────────────────

/// Published to "workflow.jobs.transition" when the sidecar detects or
/// causes a state change.  This is a derived notification stream — the
/// graph and Forgejo labels remain the sources of truth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobTransition {
    pub job: Job,
    /// `None` when the job is first seen (new vertex in graph).
    pub previous_state: Option<JobState>,
    pub new_state: JobState,
}

// ── Dispatch types ───────────────────────────────────────────────────────

/// Worker announces itself to the dispatcher with its capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRegistration {
    pub worker_id: String,
    pub capabilities: Vec<String>,
}

/// Worker signals it is ready for a new assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdleEvent {
    pub worker_id: String,
}

/// Dispatcher pushes a job assignment to a specific worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub job: Job,
    pub claim: ClaimState,
    #[serde(default)]
    pub is_rework: bool,
}

/// Dispatcher tells a worker to yield its current job for a higher-priority one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreemptNotice {
    pub reason: String,
    pub new_job: Job,
}

/// Worker publishes a heartbeat to NATS so the dispatcher can forward it
/// to the coordinator without the worker needing an HTTP client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerHeartbeat {
    pub worker_id: String,
    pub job_key: String,
}

/// The result a worker reports after executing a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum OutcomeReport {
    Complete,
    Fail { reason: String, logs: Option<String> },
    Abandon,
    /// Release claim without changing state; an external signal handles the transition.
    Yield,
}

/// Worker reports the outcome of an assigned job via NATS.
/// The dispatcher handles claim release, state transitions, and Forgejo sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerOutcome {
    pub worker_id: String,
    pub job_key: String,
    pub outcome: OutcomeReport,
}

// ── Dispatch observability ───────────────────────────────────────────────────

/// Current state of a worker in the dispatcher's registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Idle,
    Busy,
    Transitioning,
}

/// Snapshot of a registered worker's status, exposed via `GET /dispatch/workers`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub worker_id: String,
    pub state: WorkerState,
    pub capabilities: Vec<String>,
    pub current_job_key: Option<String>,
    pub current_job_priority: Option<u32>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerListResponse {
    pub workers: Vec<WorkerInfo>,
}

/// A single dispatcher journal entry — records an action the dispatcher took.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub timestamp: DateTime<Utc>,
    pub action: String,
    pub comment: String,
    /// Job key if the action relates to a specific job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_key: Option<String>,
    /// Worker involved, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JournalResponse {
    pub entries: Vec<JournalEntry>,
}

// ── CDC: issue snapshot from database ────────────────────────────────────────

/// A fully denormalized issue snapshot produced by the CDC process.
/// One message per changed issue, published to the NATS stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueSnapshot {
    /// Forgejo internal issue ID (globally unique across repos).
    pub issue_id: u64,
    /// Repository owner username.
    pub repo_owner: String,
    /// Repository name.
    pub repo_name: String,
    /// Issue number within the repo (the human-visible `#N`).
    pub number: u64,
    pub title: String,
    pub body: String,
    pub is_closed: bool,
    /// True if a merged PR in the same repo references `Closes #N` for this issue.
    #[serde(default)]
    pub closed_by_merge: bool,
    /// True if an open (unmerged) PR referencing `Closes #N` exists for this issue.
    #[serde(default)]
    pub has_open_pr: bool,
    /// Label names attached to this issue.
    pub labels: Vec<String>,
    /// Assignee login names.
    pub assignees: Vec<String>,
    /// Unix timestamp of the last update (used as stream position).
    pub updated_unix: i64,
}

// ── Forgejo webhook payload ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct WebhookPayload {
    pub action: WebhookAction,
    pub issue: ForgejoIssue,
    pub repository: ForgejoRepo,
    pub label: Option<ForgejoLabel>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebhookAction {
    Opened,
    Edited,
    Closed,
    Reopened,
    Labeled,
    Unlabeled,
    Assigned,
    Unassigned,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgejoIssue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub labels: Vec<ForgejoLabel>,
    pub assignees: Option<Vec<ForgejoUser>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ForgejoLabel {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub color: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ForgejoUser {
    pub login: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgejoRepo {
    pub owner: ForgejoUser,
    pub name: String,
}

// ── Label parsing helpers ────────────────────────────────────────────────────

/// Parse `priority:N` label, returning 50 if absent.
pub fn parse_priority(labels: &[ForgejoLabel]) -> u32 {
    for label in labels {
        if let Some(rest) = label.name.strip_prefix("priority:") {
            if let Ok(n) = rest.trim().parse::<u32>() {
                return n.min(100);
            }
        }
    }
    50
}

/// Parse `timeout:N` label, returning None if absent.
pub fn parse_timeout(labels: &[ForgejoLabel]) -> Option<u64> {
    for label in labels {
        if let Some(rest) = label.name.strip_prefix("timeout:") {
            if let Ok(n) = rest.trim().parse::<u64>() {
                return Some(n);
            }
        }
    }
    None
}

/// Parse `retry:N` label, returning N (default 3 if absent).
pub fn parse_retries(labels: &[ForgejoLabel]) -> u32 {
    for label in labels {
        if let Some(rest) = label.name.strip_prefix("retry:") {
            if let Ok(n) = rest.trim().parse::<u32>() {
                return n;
            }
        }
    }
    3 // default
}

/// Parse `capability:X` labels, returning all capability tags.
pub fn parse_capabilities(labels: &[ForgejoLabel]) -> Vec<String> {
    labels
        .iter()
        .filter_map(|l| l.name.strip_prefix("capability:").map(|s| s.trim().to_string()))
        .collect()
}

/// Parse dep issue numbers from an issue body.
///
/// Convention: `<!-- workflow:deps:1,2,3 -->` anywhere in the body.
pub fn parse_deps(body: &str) -> Vec<u64> {
    let prefix = "<!-- workflow:deps:";
    if let Some(start) = body.find(prefix) {
        let rest = &body[start + prefix.len()..];
        if let Some(end) = rest.find("-->") {
            return rest[..end]
                .trim()
                .split(',')
                .filter_map(|s| s.trim().parse::<u64>().ok())
                .collect();
        }
    }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_priority() {
        let labels = vec![
            ForgejoLabel { id: 1, name: "priority:75".into(), color: String::new() },
            ForgejoLabel { id: 2, name: "status:on-deck".into(), color: String::new() },
        ];
        assert_eq!(parse_priority(&labels), 75);
    }

    #[test]
    fn test_parse_priority_default() {
        let labels: Vec<ForgejoLabel> = vec![];
        assert_eq!(parse_priority(&labels), 50);
    }

    #[test]
    fn test_parse_deps() {
        let body = "Some description\n<!-- workflow:deps:1,2,3 -->\nMore text";
        assert_eq!(parse_deps(body), vec![1, 2, 3]);
    }

    #[test]
    fn test_parse_deps_empty() {
        assert_eq!(parse_deps("no deps here"), Vec::<u64>::new());
    }

    #[test]
    fn test_job_state_label_roundtrip() {
        for state in [
            JobState::OnIce,
            JobState::Blocked,
            JobState::OnDeck,
            JobState::OnTheStack,
            JobState::InReview,
            JobState::Rework,
            JobState::Done,
            JobState::Failed,
            JobState::Revoked,
        ] {
            assert_eq!(JobState::from_label(state.label()), Some(state));
        }
    }

    #[test]
    fn test_claim_timeout() {
        let claim = ClaimState::new("worker-1".into(), 3600);
        assert!(!claim.is_timed_out());
    }

    #[test]
    fn test_parse_capabilities() {
        let labels = vec![
            ForgejoLabel { id: 1, name: "capability:rust".into(), color: String::new() },
            ForgejoLabel { id: 2, name: "capability:frontend".into(), color: String::new() },
            ForgejoLabel { id: 3, name: "status:on-deck".into(), color: String::new() },
        ];
        let mut caps = parse_capabilities(&labels);
        caps.sort();
        assert_eq!(caps, vec!["frontend", "rust"]);
    }

    #[test]
    fn test_parse_capabilities_empty() {
        let labels: Vec<ForgejoLabel> = vec![
            ForgejoLabel { id: 1, name: "status:on-deck".into(), color: String::new() },
        ];
        assert!(parse_capabilities(&labels).is_empty());
    }
}
