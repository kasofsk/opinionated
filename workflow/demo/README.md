# Workflow demo fixtures

Two fixture files that show different dependency topologies.
Seed them into a Forgejo repo with the `worker-cli seed` command.

```
worker-cli \
  --forgejo-url http://localhost:3000 \
  --forgejo-token <TOKEN> \
  seed owner/repo fixtures/chain.json
```

---

## chain — linear dependency chain

Each job must complete before the next can start.

```
setup
  └─► schema
        └─► api
              └─► tests
                    └─► docs
                          └─► release
```

| Priority | Job |
|----------|-----|
| 80 | Set up development environment |
| 70 | Design database schema |
| 60 | Implement REST API |
| 60 | Write integration tests |
| 40 | Write API documentation |
| 90 | Cut v1.0 release |

---

## hub — hub-and-spoke with multiple work streams

Four independent work streams (infra, auth, frontend, data) run in parallel.
They converge on a central integration milestone, which gates the final release.

```
infra-plan ──► infra-terraform ──► infra-ci ──────────────┐
                                                           │
auth-design ──► auth-impl ──► auth-tests ─────────────────┤
                    │                                      ├──► hub-integration ──► load-test ──────┐
                    └──────────────────────────────────────┤         │              security-audit ─┴──► final-release
                                                           │         │
fe-wireframes ──► fe-components ──► fe-integration ────────┘         │
                                                                      │
data-model ──► data-migrations ──► data-seed ─────────────────────────┘
```

| Priority | Job |
|----------|-----|
| 70 | [Infra] Plan cloud architecture |
| 65 | [Infra] Write Terraform modules |
| 60 | [Infra] Set up CI pipeline |
| 75 | [Auth] Design auth flow |
| 70 | [Auth] Implement auth middleware |
| 60 | [Auth] Auth integration tests |
| 55 | [Frontend] UX wireframes |
| 50 | [Frontend] Build component library |
| 55 | [Frontend] Wire frontend to API |
| 80 | [Data] Define core data model |
| 75 | [Data] Write DB migrations |
| 50 | [Data] Seed data & fixtures |
| 95 | Integration milestone: all streams merged |
| 65 | Load testing & performance tuning |
| 80 | Security audit |
| 100 | Production release |

---

## How seeding works

The `seed` command does two passes:

1. **Create issues** — each fixture job becomes a Forgejo issue.
   The temporary body is created without dependency markers so Forgejo
   assigns a real issue number.

2. **Patch bodies** — once all issue numbers are known, each issue that
   has dependencies gets its body updated with an HTML comment:

   ```
   <!-- workflow:deps:3,7,12 -->
   ```

   The sidecar picks this up on the next `edited` webhook and wires the
   edges in the task graph.

The task graph enforces DAG invariants: if any dependency would form a
cycle the sidecar rejects it and posts a warning comment on the issue.
