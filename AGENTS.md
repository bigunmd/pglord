# Working on pglord

pglord manages HA PostgreSQL on Kubernetes. It is a Rust control plane and per-instance
agent with an Angular UI. Read the [user guide](docs/user-guide.md) for supported
operations, the [C4 architecture](docs/architecture/c4.md) for boundaries and risks,
and [development guide](docs/development.md) for the GOLD delivery process.

## Start here

- Confirm the requested outcome, scope, acceptance checks and allowed side effects before
  implementation. A review authorizes findings, not repairs. Reuse approval already given.
- Prefer the smallest complete change. Do not add speculative frameworks or dependencies.
- Keep local plans, evidence and handoff notes in gitignored `.gold/`; never store secrets
  there. Shared architecture and operating knowledge belong in tracked documentation.
- Do not commit, push, publish images, deploy, delete data or change Kubernetes resources
  without explicit authorization for that action and target. Tests can have side effects.
- For parallel work, assign disjoint write scopes; the lead integrates and verifies.
- Present current verification results and limitations for human acceptance before closure.

## Repository map

| Area | Responsibility |
| --- | --- |
| [core](crates/pglord-core/src/lib.rs) | Pure domain validation, Kubernetes rendering, SQL/config builders |
| [server](crates/pglord-server/src/main.rs) | REST API, auth, desired-state storage, reconciliation, failover, backup scheduling |
| [agent](crates/pglord-agent/src/main.rs) | PostgreSQL bootstrap/supervision, role following, backups, restore and upgrades |
| [UI routes](ui/src/app/app.routes.ts) | Angular standalone pages, signals, generated API client |
| [Makefile](Makefile) | Build, generation, validation and state-changing stand/E2E commands |
| [Helm values](charts/pglord/values.yaml) | Configurable installation |
| [stand deployment](deploy/k8s/deployment.yaml) | Local minikube development installation |

Default deployment is namespaced and ConfigMap-backed. Optional CRD storage and
cluster-wide operation already exist; cluster-wide requires CRD mode. Do not describe
these as future work. Keep namespace context in every lookup, mutation and metric.

## Commands and generated contracts

Use the [Makefile](Makefile), not guessed commands:

```sh
make build
make lint test
cargo test -p pglord-core <test_name>
make ui-build ui-lint
# Noninteractive Angular unit tests (dependencies must already be available):
npm --prefix ui test -- --watch=false
make openapi ui-client crd
make openapi-check ui-client-check crd-check
```

Generators and drift checks may write files/build output; inspect the diff afterward.
Never hand-edit [OpenAPI](spec/openapi.yaml), the generated client rooted at
[api.ts](ui/src/app/api/api.ts), or the [CRD](deploy/crds/postgresclusters.pglord.dev.yaml).
Change Rust source, regenerate affected contracts, and deliver them together.
[app::build](crates/pglord-server/src/app.rs) composes both the router and OpenAPI;
wire new public routes there. API errors use stable codes in RFC 9457 problem responses.

Rust uses edition 2024; propagate errors and keep clippy clean. Pure policy belongs in
core; I/O belongs in server/agent. Do not weaken validation of reserved HA GUCs,
managed labels, identifiers or secret references to make a test pass.

UI uses standalone Angular, Transloco for user-facing text and spartan primitives.
Use the generated client for pglord APIs; OIDC discovery/token exchange is a separate
external protocol. Update adjacent unit tests for changed behavior.

## High-risk boundaries

- The primary annotation is updated using resourceVersion compare-and-swap. That does
  not prove the former primary stopped accepting writes. Test partitioned-live-primary
  behavior, not just deleted pods, when changing failover.
- Upgrade, restore, credential rotation and rolling restart cross several durable
  systems. Check interruption/retry at each write boundary; a happy-path E2E is not proof
  of recoverability. Never initialize over uncertain existing data.
- Kubernetes Secrets supply credentials; do not copy values into specs, logs or docs.
  Agent control HTTP is an internal trust boundary, not protected by public API auth.
- StatefulSets use OnDelete updates and explicit rolls. Their PVC policy retains
  scale-down volumes; orphan cleanup requires explicit operator action;
  StatefulSet deletion/finalization is destructive.
- Compare both install paths when changing RBAC, config, images or resources; preserve
  their intentional development/production differences.

`make stand-*`, Docker tests and E2E scripts create/change/delete resources. In particular,
`make stand-image` scales the server down. Obtain target-specific approval before running
these; never assume the current kube context is disposable.

## Finish

Run checks proportionate to the change and report exact outcomes, including checks not
run. [CI](.github/workflows/ci.yaml) currently checks Rust formatting/clippy/tests,
OpenAPI drift, UI lint/build and client drift; it does not run UI tests, CRD drift or E2E.
Assess C4 on every delivery: update affected views for architecture changes or proven
inaccuracy, otherwise state why no update is needed. A reviewer verdict does not replace
human acceptance. Do not invent historical decisions or claim unexecuted tests passed.
