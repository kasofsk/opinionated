# CLAUDE.md — workflow-sidecar

The central coordination service. An Axum HTTP server that owns all state transitions, claim management, dependency resolution, and worker dispatch.

## Module responsibilities

| Module | Owns | Depends on |
|---|---|---|
| `config.rs` | Env-based configuration | — |
| `error.rs` | `AppError` enum → Axum `IntoResponse` | — |
| `graph.rs` | `TaskGraph` — IndraDB/RocksDB wrapper | indradb-lib |
| `coord.rs` | `Coordinator` — NATS JetStream KV for claims, transition event publishing | async-nats |
| `forgejo.rs` | `ForgejoClient` — label/assignee/comment mutations, collaborator queries | reqwest |
| `consumer.rs` | CDC stream consumer — processes `IssueSnapshot`, detects state changes, publishes `JobTransition` events. Handles Done vs Revoked detection (via `closed_by_merge` from CDC) and `propagate_unblock`. | graph, coord, forgejo |
| `dispatcher.rs` | Centralized worker assignment — subscribes to transitions, worker events (register/idle/heartbeat/outcome), manages claims, preemption, and rework routing. `pending_reworks` DashMap tracks job_key→worker_id for deferred rework assignments. | graph, coord, forgejo |
| `webhook.rs` | Legacy webhook dispatch (still wired but CDC is primary path) | graph, coord, forgejo |
| `api.rs` | HTTP handlers for `/jobs/*`, `/factories/*`, `/repos/*/users`; publishes `JobTransition` after state mutations | graph, coord, forgejo |
| `monitor.rs` | Background timeout scan loop; publishes `JobTransition` after timeout failures | coord, forgejo |
| `registry.rs` | In-process factory registry | — |

## Event-driven architecture

State changes flow through a derived `JobTransition` event stream on NATS subject `workflow.jobs.transition`. Events are published by:
- **Consumer** — when CDC snapshots cause state changes (blocked→on-deck, dep unblocking, etc.)
- **API handlers** — after claim/complete/abandon/fail/requeue mutations
- **Monitor** — after heartbeat timeout → failed
- **Dispatcher** — after handling worker outcomes

The dispatcher subscribes to this stream plus worker-specific NATS subjects:
- `workflow.dispatch.register` — worker registration with capabilities
- `workflow.dispatch.idle` — worker ready for new assignment
- `workflow.dispatch.heartbeat` — worker heartbeats (forwarded to coordinator)
- `workflow.dispatch.outcome` — worker reports job result (dispatcher handles claim release, state transition, Forgejo sync)

Workers never make HTTP calls to the sidecar. All worker↔dispatcher communication is NATS pub/sub.

## Invariants

- **Two Forgejo identities.** `state.forgejo` uses the `workflow-sync` user (labels, deps). `state.dispatcher_forgejo` uses the `workflow-dispatcher` user (assignees, comments). This gives a clear audit trail.
- **`pending_reworks` DashMap** on `AppState` maps job_key → preferred worker_id. Populated when a Rework transition fires but the original worker is busy. Consumed on next idle from that worker.
- **Only this crate mutates Forgejo labels and assignees.** Workers do content ops only.
- **NATS KV bucket** `workflow-claims` is internal — no external client should access it.
- **NATS stream** `workflow-changes` is the CDC event source. The sidecar consumes with a durable pull consumer named `sidecar`.
- **`JobTransition` is derived, not authoritative.** The graph and Forgejo labels are the sources of truth. Transition events are fire-and-forget notifications.
- **Cycle detection** runs on every `sync_deps` call in `graph.rs`. If a cycle is detected, the offending edge is rejected and a warning comment is posted to the Forgejo issue.
- **CAS on claims**: `coord.rs` uses NATS KV compare-and-swap for atomic claim acquisition.

## Adding a new API endpoint

1. Add the handler in `api.rs`
2. Add the route in `main.rs` router setup
3. Add request/response types in `crates/types/src/lib.rs` if needed
4. If the handler mutates job state, publish a `JobTransition` via `coord.publish_transition()`

## Static assets

`static/` contains the graph viewer HTML served at `GET /graph`. The viewer includes a worker roster strip, dispatcher journal (newest first), per-node detail panel with linked PRs (fetched from Forgejo API via CORS), and state colors for all states including rework (amber) and revoked (gray).
