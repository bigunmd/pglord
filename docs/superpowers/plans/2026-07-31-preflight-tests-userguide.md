# Permission Preflight + Core Tests + User Guide Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** (P0) Stop the create dialog resizing for good. (P1) Let end users see exactly which cluster permissions pglord is missing. (P2) Harden the failover-critical code paths with tests. (P3) A real end-user "How to run" guide, including how to connect an application to a created cluster.

**Architecture:** Preflight uses Kubernetes `SelfSubjectAccessReview` (SSAR) — the server asks the apiserver "may I do X?" for every permission it needs, so the answer is authoritative rather than a guess from parsing Roles. Default RBAC grants every authenticated identity the right to create SSARs, so no extra permission is needed to run the check. Results surface at `GET /api/v1/preflight` and on a UI page; a startup log line summarizes them too.

## Global Constraints

- Dialog fields use **persistent helper text** (same string always rendered, recolored on error) — the only layout-shift-proof approach when messages can wrap.
- Preflight must never fail the server: SSAR errors render as `unknown` for that check, not a 500.
- API change ⇒ regenerate `spec/openapi.yaml` + `ui/src/app/api` in the same commit.
- New tests must cover behavior, not implementation details; every failover-critical pure function gets edge cases.

## Tasks

### P0: Dialog layout-shift fix
- `cluster-create-dialog.component.ts`: replace the conditional error paragraphs with always-rendered helper text (`clusters.nameHelp`, `clusters.storageHelp`) that gets `text-destructive` when the value is present and invalid. Same DOM, same text, same height in every state.
- i18n: add `nameHelp` ("Lowercase letters, digits and hyphens. Max 40 characters."), `storageHelp` ("Size with unit, for example 1Gi or 500Mi."); drop the now-unused `nameInvalid`/`storageInvalid`.
- Verify with Playwright at 380px and 1280px widths: dialog height identical before/after entering an invalid value in each field.

### P1: Permission preflight
- `server/src/preflight.rs`:
  ```rust
  pub struct PermissionCheck { pub group: &'static str, pub resource: &'static str, pub verb: &'static str }
  pub struct CheckResult { pub resource: String, pub verb: String, pub allowed: Option<bool>, pub reason: Option<String> }
  pub fn required() -> Vec<PermissionCheck>;              // configmaps/secrets/services/pods/serviceaccounts (core), statefulsets (apps), roles/rolebindings (rbac) x get,list,watch,create,update,patch,delete as applicable
  pub async fn run(client: &kube::Client, ns: &str) -> Vec<CheckResult>;  // one SSAR per check; Err -> allowed: None + reason
  pub fn summarize(results: &[CheckResult]) -> (bool, usize);            // (all_allowed, denied_count)
  ```
  Unit-test `required()` shape (non-empty, no duplicates) and `summarize` over hand-built results (all-allowed, one-denied, one-unknown).
- `api/preflight.rs`: `GET /api/v1/preflight` → `PreflightReport { namespace, ok, denied: i32, checks: [PreflightCheck { resource, verb, allowed: Option<bool>, reason: Option<String> }] }`; 503 `kube_unavailable` without a cluster. Route registered in `app.rs`.
- `main.rs`: after client init, run preflight once and log a single line (`preflight: N/M permissions granted`), warning-level with the denied list when incomplete.
- UI `pages/preflight/preflight.component.ts` at `/preflight`: status banner (all granted / N missing), table of resource · verb · state (granted green / denied red / unknown warning) with the reason as muted text; link from the shell nav; i18n `preflight.*`.

### P2: Core critical-path tests
Add to existing `#[cfg(test)]` modules (no new files):
- `primary_record`: `parse_lsn` — uppercase/lowercase hex, `0/0`, max-ish values, empty string, missing slash, non-hex, extra slash; `recorded_primary` — empty annotation map, unrelated annotations present.
- `orchestrator::elect_candidate`: single candidate; equal LSNs (deterministic pick, first-max); candidate with `None` LSN skipped even when ready; empty observation list; all pods unready.
- `spec`: name boundaries (1 char ok, 40 ok, 41 rejected), digit-only name ok, embedded double hyphen ok; replicas 1 and 5 boundaries; storage `0Gi` accepted (unit present), `1gi` rejected (case-sensitive units).
- `render`: replicas propagate to StatefulSet; `pod_fqdn` shape; rw selector follows the passed primary (already), plus headless service name derivation.
- `spec_store::configmap_to_spec`: malformed JSON is an error; unknown extra keys tolerated (forward compatibility).
- `agent::bootstrap`: `primary_conninfo` contains the replication user when a password is configured and the superuser otherwise (currently untested and used by the rejoin path); `has_standby_signal` detection.
- `agent::rolestrategy`: multi-hyphen names, large ordinals.

### P3: End-user guide
- `docs/user-guide.md`: what pglord is; install (namespace, RBAC, deployment, port-forward/Ingress note); check permissions page; create a cluster in the UI; **connect an application** — service DNS (`<cluster>-rw.<ns>.svc.cluster.local:5432`, read-only `<cluster>-pods`), fetching credentials from the `<cluster>-credentials` Secret, a copy-paste `psql` and a Kubernetes Deployment env snippet; monitor via the detail page; what failover looks like from the app's perspective (reconnect on error); delete a cluster; troubleshooting table (pods pending → storage class; instance stuck unknown → agent logs; permission denied → preflight page).
- README links to it near the top.

## Verification
1. `make lint test openapi-check ui-client-check ui-build ui-lint ui-test` green.
2. Playwright: dialog height unchanged with invalid input at 380px and 1280px; `/preflight` page shows all-granted on the stand; deliberately break one permission (patch the Role to drop `statefulsets`) and confirm the page reports it, then restore.
