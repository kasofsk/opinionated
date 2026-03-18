# CLAUDE.md — workflow

## What this is

Issues-based workflow orchestration for AI agents. Forgejo issues are jobs, a sidecar service coordinates, workers claim and execute. See `DESIGN.md` for full architecture.

## Quick start

```bash
./scripts/init.sh          # full setup: infra, users, build, CDC, sidecar, seed fixture
```

This single command tears everything down and rebuilds from scratch. It starts all services and seeds a default fixture. When it finishes, the graph viewer is at `http://localhost:8080/graph` and Forgejo at `http://localhost:3000`.

To start workers after init: `./scripts/workers.sh`

To stop: `pkill -f workflow-sidecar; pkill -f workflow-cdc` and `./scripts/workers.sh --down`

## Build & test

```bash
cargo build                                                # build all crates
cargo test -p workflow-types                               # unit tests (fast, no infra needed)
cargo test -p workflow-integration-tests -- --include-ignored  # integration tests (requires init.sh)
```

## Workspace layout

| Crate | Kind | Purpose |
|---|---|---|
| `types` | lib | Shared types: Job, JobState, ClaimState, JobTransition, dispatch types, API request/response types |
| `sidecar` | bin | Coordination gateway: CDC consumer, dispatcher, graph, claims, HTTP API, timeout monitor |
| `cdc` | bin | Change data capture: watches Forgejo's SQLite DB, publishes IssueSnapshot to NATS |
| `worker` | lib | SDK for building workers (NATS-based dispatched loop) and work factories |
| `cli` | bin | Interactive CLI worker for manual testing, fixture seeding, admin commands |
| `integration` | test | Integration tests against live Forgejo + sidecar |

## Data flow

```
Forgejo SQLite (bind-mounted at .data/forgejo/)
    ↓ watches WAL file + polls as fallback
CDC process (workflow-cdc)
    ↓ publishes IssueSnapshot to NATS JetStream
Stream "workflow-changes"
    ↓ durable pull consumer
Sidecar consumer
    ↓ reconciles graph, detects state changes
    ↓ publishes JobTransition to "workflow.jobs.transition"
    ↓ sets labels back on Forgejo
    ↓
Dispatcher (subscribes to transitions + worker events)
    → assigns jobs to idle workers via NATS
    → handles worker outcomes (complete/fail/abandon)
    → manages preemption for high-priority jobs
```

The CDC process reads Forgejo's SQLite DB read-only and publishes full issue snapshots to a NATS JetStream stream. The sidecar consumes the stream with a durable consumer that tracks its position automatically. On restart, both resume from where they left off. No webhooks required.

The consumer publishes `JobTransition` events whenever a job changes state. The dispatcher and future reactors subscribe to this derived event stream. Workers communicate with the dispatcher exclusively via NATS pub/sub — they never make HTTP calls to the sidecar.

## NATS subjects

| Subject | Publisher | Subscriber | Payload |
|---|---|---|---|
| `workflow.changes` | CDC | Consumer | `IssueSnapshot` |
| `workflow.jobs.transition` | Consumer, API, Monitor, Dispatcher | Dispatcher | `JobTransition` |
| `workflow.dispatch.register` | Worker | Dispatcher | `WorkerRegistration` |
| `workflow.dispatch.idle` | Worker | Dispatcher | `IdleEvent` |
| `workflow.dispatch.heartbeat` | Worker | Dispatcher | `WorkerHeartbeat` |
| `workflow.dispatch.outcome` | Worker | Dispatcher | `WorkerOutcome` |
| `workflow.dispatch.assign.{id}` | Dispatcher | Worker | `Assignment` |
| `workflow.dispatch.preempt.{id}` | Dispatcher | Worker | `PreemptNotice` |

## Key conventions

- **Two Forgejo identities in one process.** The sidecar uses `workflow-sync` for label/dep operations (sync role) and `workflow-dispatcher` for assignee/comment operations (dispatcher role). This gives a clear audit trail in Forgejo.
- **Sidecar owns all state transitions.** Workers never mutate labels, assignees, or NATS KV directly. Content ops (comments, branches, PRs) are fine once a claim is held.
- **Dispatcher owns the worker lifecycle.** Workers communicate purely via NATS — register, idle, heartbeat, outcome. The dispatcher handles claims, state transitions, and Forgejo sync on their behalf.
- **Forgejo is the human-visible truth.** Every state change must be mirrored to Forgejo labels/comments so a human can inspect the system without tooling.
- **CDC is the source of events.** The sidecar learns about issue changes via the NATS stream, not webhooks. The CDC process watches Forgejo's SQLite WAL for changes.
- **Transitions are derived events.** `JobTransition` events are published when state changes, but the graph and Forgejo labels remain the sources of truth.
- **Default-deny.** Jobs don't move to `on-deck` without explicit sidecar action. Failed jobs don't auto-retry. `on-ice` is a hold.
- **Deterministic UUIDs.** IndraDB vertex IDs are UUID v5 from `{owner}/{repo}/{number}` — no separate index, no rebuild on restart.
- **Dependency metadata lives in issue bodies** as `<!-- workflow:deps:1,2,3 -->` HTML comments, parsed by the sidecar from CDC snapshots.
- **Review lifecycle.** Workers create PRs with `Closes #N`. After completion (in-review), a reviewer can add `status:rework` to route the job back to its original worker. Merging the PR auto-closes the issue → Done. Closing without a merged PR → Revoked (dependents stay blocked).
- **Worker re-announcement.** Idle workers periodically re-register with the dispatcher (every 15s). This ensures recovery after sidecar restarts without manual intervention.

## Environment variables

### Sidecar

| Var | Default | Notes |
|---|---|---|
| `FORGEJO_URL` | (required) | |
| `FORGEJO_TOKEN` | (required) | Sync identity token (labels, deps) |
| `DISPATCHER_FORGEJO_TOKEN` | (required) | Dispatcher identity token (assignees, comments) |
| `NATS_URL` | `nats://localhost:4222` | |
| `DB_PATH` | `./workflow.db` | RocksDB path |
| `LISTEN_ADDR` | `0.0.0.0:3000` | |
| `DEFAULT_TIMEOUT_SECS` | `3600` | |
| `MONITOR_INTERVAL_SECS` | `60` | |

### CDC

| Var | Default | Notes |
|---|---|---|
| `FORGEJO_DB_PATH` | `.data/forgejo/gitea/gitea.db` | Path to Forgejo's SQLite DB |
| `NATS_URL` | `nats://localhost:4223` | |
| `CDC_POLL_MS` | `250` | Fallback poll interval (file watcher is primary) |

## Infrastructure

- **Docker Compose**: Forgejo (port 3000, bind-mounted to `.data/forgejo/`) + NATS with JetStream (port 4223). Worker containers defined in `docker-compose.workers.yml`.
- **Forgejo config**: `infra/forgejo/app.ini` is copied into `.data/` before first boot. Includes CORS for the graph viewer's cross-origin API calls.
- **Terraform** (`infra/`): Provisions repos, users, labels, collaborator permissions. Uses Lerentis/gitea provider.
- **`scripts/init.sh`**: One-command setup. Teardown → infra → Terraform → build → CDC + sidecar → seed → verify.
- **`scripts/workers.sh`**: Creates Forgejo API tokens and launches worker containers. Supports `--delay`, `--count`, `--build`, `--down`.

## Gitignored runtime artifacts

- `.sidecar.env` — generated credentials
- `.data/` — RocksDB, Forgejo bind-mount
- `target/` — build output
