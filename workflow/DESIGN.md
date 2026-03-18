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
└──────────────────────────────────────────────────────────────┘
         │ SQLite WAL
┌────────▼─────────┐
│ CDC process       │  watches Forgejo DB, publishes IssueSnapshot
└────────┬─────────┘
         │ NATS JetStream "workflow-changes"
┌────────▼─────────────────────────────────────────────────────┐
│ Sidecar (axum HTTP service)                                   │
│                                                               │
│  Consumer        ─► Graph (IndraDB/RocksDB)                  │
│    ↓ publishes       Coordinator (NATS KV)                   │
│  JobTransition       Forgejo API client                      │
│    ↓                                                          │
│  Dispatcher      ─► assigns jobs to workers via NATS         │
│                  ─► handles worker outcomes (NATS)            │
│                  ─► manages preemption                        │
│  API handler     ─► HTTP endpoints for admin/CLI              │
│  Timeout monitor ─► scans stale claims                        │
└──────────────────────────────────────────────────────────────┘
         ▲ NATS pub/sub only              ▲ sidecar API (HTTP)
         │                                │
┌────────┴──────────┐        ┌────────────┴────────┐
│ Workers            │        │ Work Factories       │
│ (AI agents)        │        │ (job generators)     │
│                    │        │                      │
│ NATS: register,    │        │ sidecar API          │
│   idle, heartbeat, │        │   + Forgejo API      │
│   outcome          │        │     (issue creation) │
│ Forgejo API        │        └────────────────────  │
│   (content ops)    │
└────────────────────┘
```

### Forgejo
Source of truth for job content, history, and human visibility. The CDC process
watches Forgejo's SQLite DB for changes. Workers interact with Forgejo directly
for content operations (reading issue body, posting comments, creating PRs,
updating branches) once they hold an exclusive claim — contention-free by design.

### Sidecar
The single coordination gateway. Uses two Forgejo identities for audit trail clarity:
- **`workflow-sync`** — sync identity: label mutations, dependency resolution, CDC consumption
- **`workflow-dispatcher`** — dispatcher identity: assignee changes, failure comments

Owns:
- All state transitions (label mutations via sync identity)
- Exclusive claim issuance (via NATS KV CAS)
- Task graph (IndraDB/RocksDB): dependency edges, reverse-dep index
- Dispatcher: centralized worker assignment via NATS pub/sub
- Heartbeat monitoring and timeout enforcement
- Dependency resolution: when a job closes, walks reverse deps and unblocks ready jobs
- Failure recording: writes structured failure comments via dispatcher identity
- Transition event stream: publishes `JobTransition` events for reactive subscribers

### Dispatcher (sidecar module)
Centralized worker assignment. Subscribes to:
- `workflow.jobs.transition` — reacts to on-deck jobs (assign or preempt)
- `workflow.dispatch.register` — worker registration with capabilities
- `workflow.dispatch.idle` — worker ready for work
- `workflow.dispatch.heartbeat` — forwards to coordinator for claim keepalive
- `workflow.dispatch.outcome` — handles complete/fail/abandon (release claim, update graph, sync Forgejo)

Publishes to per-worker subjects:
- `workflow.dispatch.assign.{worker_id}` — job assignment
- `workflow.dispatch.preempt.{worker_id}` — preemption notice

### NATS KV (sidecar-internal)
Two buckets, never accessed by workers directly:
- `workflow-claims`: `{owner}/{repo}/{number}` → `ClaimState` JSON
  - Used for: atomic claim CAS, heartbeat updates, timeout detection
- `workflow-webhook-dedup`: delivery ID deduplication (24h TTL)

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
AI agents. Register with the dispatcher via NATS with capability tags.
Receive job assignments pushed by the dispatcher. Execute work using Forgejo
for content ops. Report outcomes (complete/fail/abandon) back via NATS.
Workers never make HTTP calls to the sidecar — the dispatcher manages all
lifecycle state on their behalf.

### Work Factories
Separate processes that generate work. They inspect current job state via the sidecar
and create new Forgejo issues when appropriate (which the CDC picks up and the sidecar
queues). They are not workers — they don't claim or execute jobs.

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
       deps unresolved   │  all deps Done (sidecar, triggered by CDC)
                         │
                    ┌────▼────┐
                    │ On Deck │  claimable; label stays on-deck while unclaimed
                    └────┬────┘
                         │  dispatcher assigns (NATS KV CAS)
                    ┌────▼──────────┐
                    │ On The Stack  │  exclusively held by one worker
                    └──┬────────┬──┘
                       │        │
            complete   │        │  fail  OR  heartbeat timeout
                  ┌────▼──┐  ┌──▼─────┐
                  │  In   │  │ Failed │  DLQ; error + logs stored
                  │Review │  │ (DLQ)  │  as Forgejo issue comment
                  └──┬─┬──┘  └──┬─────┘
                     │ │        │  manual / factory requeue
     reviewer adds   │ │        └──────────────┐
     status:rework   │ │                       │
               ┌─────▼─┘               On Deck or On Ice
               │    │
          ┌────▼───┐│ merged PR closes issue
          │ Rework ││ (CDC detects closed_by_merge)
          └────┬───┘│
               │    │       closed without merged PR
     dispatcher│  ┌─▼────┐  (no status:done label)
     routes to │  │ Done │  ┌─────────┐
     original  │  └──────┘  │ Revoked │  dependents stay blocked
     worker    │       │    └─────────┘
               └───────┘
            On The Stack     triggers dep resolution:
            (is_rework=true) walk reverse deps in IndraDB,
                             transition unblocked jobs → On Deck
```

### Done vs Revoked detection

When the CDC sees `is_closed=true`, the consumer checks:
1. **`status:done` label present** → Done (human marked it)
2. **`closed_by_merge=true`** (CDC detected merged PR with `Closes #N`) → Done (sidecar adds `status:done` label)
3. **Neither** → Revoked (closed without completion; dependents stay blocked)

Revoked is terminal. `all_deps_done` checks for `state == Done` specifically — Revoked deps do not satisfy it. Since a job was never Done before being Revoked, its dependents were never unblocked.

### Rework routing

When a reviewer adds `status:rework` to an in-review issue:
1. CDC picks up the label change, consumer publishes `JobTransition` to Rework
2. Dispatcher reads the assignee (preserved from the original work cycle)
3. If the original worker is idle → assign immediately with `is_rework: true`
4. If busy → queue in `pending_reworks` map; assigned when the worker next idles
5. Worker receives assignment, creates/updates PR content, completes → back to InReview

### Sidecar behavior on CDC snapshots

The consumer processes each `IssueSnapshot` idempotently. It reads the previous
state from the graph, computes the resolved state, and publishes a `JobTransition`
event if they differ.

| Snapshot condition | Action |
|---|---|
| Closed + (`status:done` label OR `closed_by_merge`) | Set `Done` in graph. Add `status:done` label if absent. Walk reverse deps — unblock any whose deps are all done. Publish transition. |
| Closed + no done label + no merged PR | Set `Revoked` in graph. Publish transition. Dependents stay blocked. |
| `on-ice` label | Set `OnIce` in graph. |
| Has unresolved deps | Set `Blocked`. Sync label to Forgejo if it doesn't match. |
| No deps or all deps done | Set `OnDeck`. Sync label. Publish transition → dispatcher reacts. |
| `on-the-stack`/`in-review`/`failed`/`rework` label | Respect the explicit status label. |

All state changes publish `JobTransition` events. The dispatcher uses `OnDeck`
transitions to assign idle workers or preempt lower-priority workers.

---

## Claim Lifecycle (Dispatched Mode)

```
Worker                          Dispatcher (sidecar)              NATS KV / Forgejo
  │                                │                                │
  │── pub WorkerRegistration ─────►│  (capabilities)                │
  │── pub IdleEvent ──────────────►│                                │
  │                                │── get on-deck jobs from graph  │
  │                                │── try_claim (CAS) ────────────►│
  │                                │◄─ ok (revision) ───────────────│
  │                                │── PATCH label: on-the-stack ──►│ Forgejo
  │◄── pub Assignment ─────────────│  (job + claim)                 │
  │                                │                                │
  │── pub WorkerHeartbeat ────────►│                                │
  │                (periodic)      │── update last_heartbeat (CAS) ►│
  │                                │                                │
  │  [work happens in Forgejo directly — branch, PR, comments]      │
  │                                │                                │
  │── pub WorkerOutcome ──────────►│  (complete)                    │
  │                                │── delete claims:{key} ────────►│
  │                                │── PATCH label: in-review ─────►│ Forgejo
  │                                │── PATCH remove assignee ──────►│ Forgejo
  │                                │── pub JobTransition ───────────│
  │── pub IdleEvent ──────────────►│  (ready for next job)          │
```

### HTTP Claim Lifecycle (Legacy/Admin)

The HTTP endpoints (`/claim`, `/complete`, `/abandon`, `/fail`, `/requeue`) still
exist for the CLI and admin tools. They perform the same state transitions and
publish `JobTransition` events. The dispatched worker loop does not use them.

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
                              # JobTransition, dispatch types (WorkerRegistration,
                              # Assignment, PreemptNotice, WorkerHeartbeat,
                              # WorkerOutcome, OutcomeReport), API req/resp types,
                              # IssueSnapshot, Forgejo webhook types

    sidecar/                  # workflow-sidecar (binary)
      src/
        main.rs               # axum server setup, dispatcher + consumer startup
        config.rs             # env-based config
        error.rs              # AppError, IntoResponse impl
        graph.rs              # TaskGraph: IndraDB/RocksDB wrapper
        coord.rs              # Coordinator: NATS KV claims + publish_transition()
        forgejo.rs            # ForgejoClient: labels, assignees, comments
        consumer.rs           # CDC stream consumer, publishes JobTransition events
        dispatcher.rs         # Centralized worker assignment, handles worker
                              #   lifecycle (heartbeat, outcome) via NATS
        webhook.rs            # Legacy webhook dispatch (CDC is primary)
        api.rs                # HTTP handlers, publish transitions after mutations
        monitor.rs            # Background timeout scan, publishes transitions

    cdc/                      # workflow-cdc (binary)
      src/main.rs             # Watches Forgejo SQLite, publishes IssueSnapshot

    worker/                   # workflow-worker (library)
      src/
        lib.rs
        client.rs             # SidecarClient: HTTP client (legacy/admin/CLI)
        forgejo.rs            # ForgejoClient: content ops only
        worker.rs             # Worker trait: async fn execute(job, forgejo)
        dispatch.rs           # DispatchedWorkerLoop: pure NATS worker loop
        factory.rs            # WorkFactory trait: async fn poll(sidecar, forgejo)

    cli/                      # worker-cli (binary)
      src/main.rs             # Interactive CLI worker, fixture seeding, admin
```

---

## Key Design Principles

1. **Sidecar is the only coordinator.** The dispatcher (part of the sidecar) owns
   all state transitions, claim management, and worker assignment. Workers never
   touch NATS KV or IndraDB directly.
2. **Workers are pure executors.** They receive assignments via NATS, do content ops
   via Forgejo, and report outcomes via NATS. No HTTP calls to the sidecar.
3. **Forgejo is human-visible truth.** All state is mirrored to labels/comments so
   humans can inspect and intervene without tooling.
4. **Event-driven, not polling.** The CDC stream drives state reconciliation. The
   consumer publishes `JobTransition` events that the dispatcher and future reactors
   subscribe to. The transition stream is derived — not a source of truth.
5. **Exclusive ownership guaranteed before content ops.** Workers get a claim before
   touching branches/PRs. The sidecar's CAS guarantee means no two workers race on
   the same job.
6. **Default-deny.** No job moves to `on-deck` without explicit sidecar action.
   `on-ice` is respected as a hold. Failed jobs do not auto-retry.
7. **Work factories are not special.** They are ordinary clients of the sidecar +
   Forgejo APIs. Job generation logic lives in the factory, not in the job system.

---

## Open Questions / Future Work

- **Multi-repo queues**: currently one repo = one queue. Cross-repo deps would require
  qualifying all dep references with `owner/repo/number`. Deferred.
- **Backend swappability**: Forgejo-specific details are isolated in `sidecar/forgejo.rs`
  and `worker/forgejo.rs`. A `ForgejoBackend` trait can be extracted when adding
  GitHub/GitLab support.
- **Webhook secret validation**: Forgejo supports HMAC-signed webhooks. Should be
  wired up before any production use.
