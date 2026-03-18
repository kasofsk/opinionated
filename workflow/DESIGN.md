# Workflow — Design Document

## Overview

An issues-based workflow orchestration system for AI agents. Jobs are Forgejo issues.
Agents are workers that claim and execute jobs. A sidecar service is the single
coordination point — workers never touch NATS or IndraDB directly.

The backend is intentionally swappable (Forgejo-first, with progressive feature
activation for GitHub/GitLab). All job-management intelligence lives in the sidecar;
the issue tracker is a display and persistence layer for human visibility.

---

## Components

```
┌──────────────────────────────────────────────────────────────┐
│ Forgejo                                                      │
│  Issues = jobs   Labels = state   Assignees = audit trail    │
│  Webhooks ──────────────────────────────────┐               │
└─────────────────────────────────────────────│───────────────┘
                                              │
┌─────────────────────────────────────────────▼───────────────┐
│ Sidecar (axum HTTP service, co-deployed with Forgejo)        │
│                                                              │
│  Webhook handler  ─► Graph (IndraDB/RocksDB)                │
│  API handler      ─► Coordinator (NATS KV)                  │
│  Timeout monitor  ─► Forgejo API client                     │
│                                                              │
│  POST /webhook                                               │
│  GET  /jobs                        (all states, for context) │
│  GET  /jobs/:owner/:repo/:number   (single job + claim)      │
│  GET  /jobs/:owner/:repo/:number/deps                        │
│  POST /jobs/:owner/:repo/:number/claim                       │
│  POST /jobs/:owner/:repo/:number/heartbeat                   │
│  POST /jobs/:owner/:repo/:number/complete                    │
│  POST /jobs/:owner/:repo/:number/abandon                     │
│  POST /jobs/:owner/:repo/:number/fail                        │
│  POST /jobs/:owner/:repo/:number/requeue                     │
└──────────────────────────────────────────────────────────────┘
         ▲                              ▲
         │ sidecar API                  │ sidecar API
         │                              │
┌────────┴──────────┐        ┌──────────┴──────────┐
│ Workers            │        │ Work Factories       │
│ (AI agents)        │        │ (schedulers/         │
│                    │        │  job generators)     │
│ sidecar API        │        │                      │
│   + Forgejo API    │        │ sidecar API          │
│     (content ops)  │        │   + Forgejo API      │
└────────────────────┘        │     (issue creation) │
                              └──────────────────────┘
```

### Forgejo
Source of truth for job content, history, and human visibility. The sidecar is
configured as a webhook consumer. Workers interact with Forgejo directly for
content operations (reading issue body, posting comments, creating PRs, updating
branches) once they hold an exclusive claim — contention-free by design.

### Sidecar
The single coordination gateway. Owns:
- All state transitions (label mutations in Forgejo)
- Exclusive claim issuance (via NATS KV CAS)
- Task graph (IndraDB/RocksDB): dependency edges, reverse-dep index
- Heartbeat monitoring and timeout enforcement
- Dependency resolution: when a job closes, walks reverse deps and unblocks ready jobs
- Failure recording: writes structured failure comments to Forgejo issues

### NATS KV (sidecar-internal)
Two buckets, never accessed by workers directly:
- `workflow-claims`: `{owner}/{repo}/{number}` → `ClaimState` JSON
  - Used for: atomic claim CAS, heartbeat updates, timeout detection
- (token bucket bucket removed — superseded by Work Factories)

### IndraDB / RocksDB (sidecar-internal)
Persistent task graph. Vertices = jobs, edges = dependencies.

**Vertex properties:**
- `job_key`: `{owner}/{repo}/{number}` (string)
- `state`: `JobState` (serialized)
- `timeout_secs`: optional u64

**Edge type:** `depends_on` (outbound = dependent, inbound = dependency)

Vertex UUIDs are derived deterministically via UUID v5 from the job key —
no separate index required, no rebuild on restart.

**Key graph queries:**
- Reverse deps of job X → inbound `depends_on` edges to X's vertex
- All deps of job X → outbound `depends_on` edges from X's vertex
- All deps done? → check `state == Done` on all inbound vertices

### Workers
AI agents. Discover jobs via sidecar API (all states visible for context). Claim via
sidecar. Once claimed exclusively, interact with Forgejo directly for content ops.
Must send periodic heartbeats or the sidecar will time out the claim.

### Work Factories
Separate processes that generate work. They inspect current job state via the sidecar
and create new Forgejo issues when appropriate (which the sidecar then picks up via
webhook and queues). They are not workers — they don't claim or execute jobs.

Examples:
- "Keep at least N `on-deck` jobs of type X"
- "When job Y completes, create follow-up jobs Z1..Zn"
- "Create a daily audit job every morning"

Factories implement a `WorkFactory` trait: `async fn poll(sidecar, forgejo) -> Result<()>`.

---

## Job State Machine

```
                    ┌─────────┐
         manual /   │  On Ice │  intentionally withheld;
         factory    └────┬────┘  set by creator or requeue
                         │
                    ┌────▼────┐
       sidecar sets │ Blocked │  has unresolved deps (sidecar sets on open)
       on open if   └────┬────┘
       deps unresolved   │  all deps Done (sidecar, triggered by webhook)
                         │
                    ┌────▼────┐
                    │ On Deck │  claimable; label stays on-deck while unclaimed
                    └────┬────┘
                         │  POST /claim  (NATS KV CAS)
                    ┌────▼──────────┐
                    │ On The Stack  │  exclusively held by one worker
                    └──┬────────┬──┘
                       │        │
            complete   │        │  fail  OR  heartbeat timeout
                  ┌────▼──┐  ┌──▼─────┐
                  │  In   │  │ Failed │  DLQ; error + logs stored
                  │Review │  │ (DLQ)  │  as Forgejo issue comment
                  └────┬──┘  └──┬─────┘
                       │        │  manual / factory requeue
         closed/merged │        └──────────────┐
         in Forgejo    │                        │
                  ┌────▼──┐              On Deck or On Ice
                  │ Done  │
                  └───────┘
                       │
              triggers dep resolution:
              walk reverse deps in IndraDB,
              transition unblocked jobs → On Deck
```

### Sidecar behavior on Forgejo webhook events

| Event | Action |
|---|---|
| `opened` | Read deps from Forgejo API. If `on-ice` label present: store as-is. Else if unresolved deps: set `blocked`. Else: set `on-deck`. Upsert vertex in IndraDB. Sync dep edges. |
| `edited` | Re-fetch deps from Forgejo API. Diff edges in IndraDB. Add/remove edges as needed. Re-evaluate state if dep set changed. |
| `closed` | Set state `done` in IndraDB. Walk reverse deps. For each: if all deps done → transition to `on-deck` (update Forgejo label). |
| `labeled` | If `on-ice` added: store `OnIce` in IndraDB. If `on-ice` removed: re-evaluate (blocked or on-deck). |
| `unlabeled` | Same as labeled — re-evaluate when status labels are removed. |

---

## Claim Lifecycle

```
Worker                          Sidecar                         NATS KV / Forgejo
  │                                │                                │
  │── POST /claim {worker_id} ────►│                                │
  │                                │── get claims:{key} ───────────►│
  │                                │◄─ None or existing claim ──────│
  │                                │                                │
  │                                │  if unclaimed:                 │
  │                                │── put claims:{key} (CAS) ─────►│
  │                                │◄─ ok (revision) ───────────────│
  │                                │                                │
  │                                │── PATCH label: on-the-stack ──►│ Forgejo
  │                                │── PATCH assignee: worker_id ──►│ Forgejo
  │◄── 200 ClaimResponse ──────────│                                │
  │                                │                                │
  │── POST /heartbeat (periodic) ──►│                               │
  │                                │── update last_heartbeat (CAS) ►│
  │◄── 200 ────────────────────────│                                │
  │                                │                                │
  │  [work happens in Forgejo directly — branch, PR, comments]      │
  │                                │                                │
  │── POST /complete ──────────────►│                               │
  │                                │── delete claims:{key} ────────►│
  │                                │── PATCH label: in-review ─────►│ Forgejo
  │                                │── PATCH remove assignee ──────►│ Forgejo
  │◄── 200 ────────────────────────│                                │
```

### Timeout monitoring (sidecar background task)
- Runs every N seconds (configurable)
- Scans all NATS KV claim entries
- For each: if `now - last_heartbeat > job.timeout_secs` → trigger failure
- Failure path: delete claim, set `Failed` in IndraDB, PATCH Forgejo label,
  POST structured failure comment to Forgejo issue

### Per-job timeout
- Global default in sidecar config: `default_task_timeout_secs`
- Override via label: `timeout:3600` on the Forgejo issue
- Sidecar reads timeout label on claim, stores in NATS KV claim entry

---

## Failure Record

Posted as a Forgejo issue comment (machine-readable block + human-readable summary):

```
<!-- workflow:failure
{
  "worker_id": "agent-42",
  "kind": "heartbeat_timeout" | "worker_reported",
  "reason": "...",
  "logs": "...",
  "failed_at": "2026-03-17T..."
}
-->

⚠️ **Job failed** — `heartbeat_timeout` by worker `agent-42` at 2026-03-17T...

**Reason:** ...
```

Failed jobs are not automatically retried. A human or work factory must call
`POST /requeue` with `{ "target": "on-deck" | "on-ice" }`.

---

## Sidecar API Reference

All responses are JSON. Errors use standard HTTP status codes with a
`{ "error": "..." }` body.

### Job discovery
```
GET /jobs?state=on-deck|on-the-stack|blocked|...
  → { jobs: [Job] }
  All states visible; workers use this for context before choosing a task.

GET /jobs/:owner/:repo/:number
  → { job: Job, claim: ClaimState | null, failure: FailureRecord | null }

GET /jobs/:owner/:repo/:number/deps
  → { dependencies: [Job], all_done: bool }
```

### Job lifecycle
```
POST /jobs/:owner/:repo/:number/claim
  Body: { worker_id: string }
  → 200 ClaimResponse | 409 Conflict | 400 Bad Request

POST /jobs/:owner/:repo/:number/heartbeat
  Body: { worker_id: string }
  → 200 | 403 Forbidden (not the current holder)

POST /jobs/:owner/:repo/:number/complete
  Body: { worker_id: string }
  → 200 | 403 Forbidden

POST /jobs/:owner/:repo/:number/abandon
  Body: { worker_id: string }
  → 200 (job returns to on-deck) | 403 Forbidden

POST /jobs/:owner/:repo/:number/fail
  Body: { worker_id: string, reason: string, logs?: string }
  → 200 | 403 Forbidden

POST /jobs/:owner/:repo/:number/requeue
  Body: { target: "on-deck" | "on-ice" }
  → 200 (no auth — admin/factory operation)
```

### Webhook (Forgejo → Sidecar)
```
POST /webhook
  Body: ForgejoWebhookPayload (configured in Forgejo repo settings)
  → 200
```

---

## Crate Structure

```
workflow/
  Cargo.toml                  # workspace

  crates/
    types/                    # workflow-types
      src/lib.rs              # Job, JobState, ClaimState, FailureRecord,
                              # all API req/resp types, Forgejo webhook types

    sidecar/                  # workflow-sidecar (binary)
      src/
        main.rs               # axum server setup, background timeout task
        config.rs             # env-based config (Forgejo URL/token, NATS URL,
                              #   RocksDB path, default timeout, heartbeat interval)
        error.rs              # AppError, IntoResponse impl
        graph.rs              # TaskGraph: IndraDB/RocksDB wrapper
                              #   upsert_job, sync_deps, get_dependents,
                              #   all_deps_done, set_state, get_job
        coord.rs              # Coordinator: NATS KV wrapper
                              #   try_claim (CAS), heartbeat, release, get_claim
        forgejo.rs            # ForgejoClient: label transitions, assignee
                              #   mutations, dep fetching, comment posting
        webhook.rs            # handle_webhook → dispatch on action
        api.rs                # all HTTP handler fns
        monitor.rs            # background timeout scan loop

    worker/                   # workflow-worker (library)
      src/
        lib.rs
        client.rs             # SidecarClient: typed HTTP client for sidecar API
        forgejo.rs            # ForgejoClient: content ops (read body, post
                              #   comments, create PRs, branch ops)
        worker.rs             # Worker trait: async fn execute(job, sidecar, forgejo)
        factory.rs            # WorkFactory trait: async fn poll(sidecar, forgejo)
```

---

## Key Design Principles

1. **Sidecar is the only coordinator.** Workers and factories never touch NATS or IndraDB.
2. **Forgejo is human-visible truth.** All state is mirrored to labels/comments so
   humans can inspect and intervene without tooling.
3. **All state in the issue DB.** No central scheduler process — the sidecar is
   event-driven (webhooks) + reactive (timeout monitor). The monitor is a lightweight
   background task, not a scheduler.
4. **Exclusive ownership guaranteed before content ops.** Workers get a claim before
   touching branches/PRs. The sidecar's CAS guarantee means no two workers race on
   the same job.
5. **Default-deny.** No job moves to `on-deck` without explicit sidecar action.
   `on-ice` is respected as a hold. Failed jobs do not auto-retry.
6. **Work factories are not special.** They are ordinary clients of the sidecar +
   Forgejo APIs. Job generation logic lives in the factory, not in the job system.

---

## Open Questions / Future Work

- **Multi-repo queues**: currently one repo = one queue. Cross-repo deps would require
  qualifying all dep references with `owner/repo/number`. Deferred.
- **Priority**: no priority ordering on `on-deck` queue currently. Workers choose
  freely. Could add a `priority:N` label and sort in the sidecar API response.
- **Backend swappability**: Forgejo-specific details are isolated in `sidecar/forgejo.rs`
  and `worker/forgejo.rs`. A `ForgejoBackend` trait can be extracted when adding
  GitHub/GitLab support.
- **Work factory scheduling**: factories are polled externally (cron, tokio interval).
  A factory registry in the sidecar could manage this centrally. Deferred.
- **Webhook secret validation**: Forgejo supports HMAC-signed webhooks. Should be
  wired up before any production use.
