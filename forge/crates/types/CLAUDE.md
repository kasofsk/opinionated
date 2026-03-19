# CLAUDE.md — workflow-types

Shared type definitions used by all crates. No business logic beyond parsing and serialization.

## What lives here

- **`Job`**, **`JobState`** (OnIce, Blocked, OnDeck, OnTheStack, InReview, Rework, Done, Failed, Revoked), **`ClaimState`**, **`FailureRecord`** — core domain types
- **`JobTransition`** — derived event published when job state changes (previous + new state)
- **Dispatch types**: `WorkerRegistration`, `IdleEvent`, `Assignment`, `PreemptNotice` — dispatcher↔worker messages
- **Worker lifecycle types**: `WorkerHeartbeat`, `WorkerOutcome`, `OutcomeReport` (Complete, Fail, Abandon, Yield) — workers report heartbeats and outcomes via NATS, not HTTP
- **API types**: `ClaimRequest`, `ClaimResponse`, `HeartbeatRequest`, `CompleteRequest`, `DepsResponse`, `RequeueRequest`, `LabelListResponse`, `CreateIssueRequest`, `CreateIssueResponse`, etc.
- **`IssueSnapshot`** — CDC-produced denormalized issue state, includes `closed_by_merge` and `has_open_pr` booleans
- **`WebhookPayload`** — Forgejo webhook event structure (legacy, CDC is primary)
- **Label parsing**: `priority:N`, `timeout:N`, `capability:X`, `retry:N` from label names; `<!-- workflow:deps:1,2,3 -->` from issue body
- **`FactoryStatus`** — observability metadata for work factories

## Conventions

- All types derive `Serialize`/`Deserialize`. Most also derive `Debug`, `Clone`.
- `JobState` round-trips through label strings (`on-deck`, `on-the-stack`, `rework`, `revoked`, etc.) — keep these stable as they're Forgejo label names managed by Terraform.
- `is_terminal()` returns true for `Done` and `Revoked`. However, `all_deps_done` in graph.rs checks for `Done` specifically — Revoked deps block dependents.
- `Assignment` has `is_rework: bool` to signal the worker that this is a rework cycle (branch/PR may already exist).
- `IssueSnapshot` has `closed_by_merge: bool` and `has_open_pr: bool`, both populated by CDC from SQLite queries.
- `Job` has `max_retries: u32` (default 3) parsed from `retry:N` labels via `parse_retries()`.
- `OutcomeReport` has a `Yield` variant — releases claim without changing state; CDC owns the transition.
- `OutcomeReport` uses `#[serde(tag = "kind")]` for tagged enum serialization.
- This crate has zero async dependencies. Keep it that way.
