# CLAUDE.md — workflow-worker

SDK library for building workers and work factories. This crate is a dependency of any agent that participates in the workflow system.

## Key abstractions

- **`Worker` trait** (`worker.rs`): Implement `execute(job, forgejo) → Result<Outcome>` to define what a worker does. Workers receive jobs from the dispatcher and return an `Outcome` (Complete, Fail, Abandon, or Yield) — they never make lifecycle HTTP calls. Both sim and action workers return `Outcome::Yield` after opening a PR, delegating the InReview transition to the CDC consumer.
- **`DispatchedWorkerLoop`** (`dispatch.rs`): NATS-based worker loop. Registers with the dispatcher, signals idle, receives assignments, runs execute, and reports outcomes — all via NATS pub/sub. No HTTP calls to the sidecar. While idle, periodically re-registers + re-idles every 15s to survive sidecar restarts.
- **`WorkerLoop`** (`worker.rs`): Pull-based worker loop (legacy). Polls the sidecar API for on-deck jobs, claims via HTTP, runs execute, reports outcome via HTTP. Still works but `DispatchedWorkerLoop` is preferred.
- **`WorkFactory` trait** (`factory.rs`): Implement `poll(sidecar, forgejo)` to generate new jobs. Factories inspect state and create Forgejo issues; they never claim or execute.
- **`SidecarClient`** (`client.rs`): Typed HTTP client for the sidecar API. Used by `WorkerLoop` (legacy pull mode), CLI admin commands, and factories. Not used by `DispatchedWorkerLoop`.
- **`ForgejoClient`** (`forgejo.rs`): Wraps the generated `forgejo-api` crate with ergonomic methods. Covers content ops (issues, comments, branches, PRs, file creation), review ops (submit_review, merge_pr, add_pr_reviewer, list_pr_files), Forgejo Actions (dispatch_workflow, list_action_runs, get_action_run), and admin ops (admin_create_user, add_collaborator). Label/assignee mutation is sidecar-only. The reviewer module uses this client for PR reviews and merges.

## Dispatched worker lifecycle (NATS-only)

```
Worker                          Dispatcher (sidecar)
  │                                │
  │── publish WorkerRegistration ─►│  (capabilities, worker_id)
  │── publish IdleEvent ──────────►│
  │                                │── finds matching on-deck job
  │                                │── claims via coord (NATS KV CAS)
  │◄── publish Assignment ────────│  (job + claim on personal subject)
  │                                │
  │  [execute: content ops via Forgejo]
  │── publish WorkerHeartbeat ────►│  (periodic, dispatcher forwards to coord)
  │                                │
  │── publish WorkerOutcome ──────►│  (complete/fail/abandon)
  │                                │── releases claim, updates graph + Forgejo
  │── publish IdleEvent ──────────►│  (ready for next job)
```

Workers communicate exclusively via NATS. The dispatcher handles all claim management, state transitions, and Forgejo label/assignee mutations on the worker's behalf.

## Permission boundary

- **Forgejo API** via `ForgejoClient` — wraps the generated `forgejo-api` crate (workspace member at `crates/forgejo-api`)
- Content ops: create/edit issues, post comments, create branches, create/get PRs, create files (base64 via contents API)
- Review ops: submit_review (APPROVED/REQUEST_CHANGES), merge_pr (merge/squash/rebase), add_pr_reviewer, list_pr_files
- Actions ops: dispatch_workflow, list_action_runs, get_action_run
- Admin ops: admin_create_user, add_collaborator (used by integration tests and init scripts)
- Workers intentionally lack label/assignee mutation methods. State transitions are sidecar-only.
- In dispatched mode, workers don't even need HTTP access to the sidecar.
