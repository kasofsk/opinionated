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
    Done,
    Failed,
}

impl JobState {
    pub fn from_label(s: &str) -> Option<Self> {
        match s {
            "status:on-ice"       => Some(Self::OnIce),
            "status:blocked"      => Some(Self::Blocked),
            "status:on-deck"      => Some(Self::OnDeck),
            "status:on-the-stack" => Some(Self::OnTheStack),
            "status:in-review"    => Some(Self::InReview),
            "status:done"         => Some(Self::Done),
            "status:failed"       => Some(Self::Failed),
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
            Self::Done       => "status:done",
            Self::Failed     => "status:failed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done)
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
            JobState::Done,
            JobState::Failed,
        ] {
            assert_eq!(JobState::from_label(state.label()), Some(state));
        }
    }

    #[test]
    fn test_claim_timeout() {
        let claim = ClaimState::new("worker-1".into(), 3600);
        assert!(!claim.is_timed_out());
    }
}
