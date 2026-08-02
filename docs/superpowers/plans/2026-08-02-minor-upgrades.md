# Minor Version Upgrades Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `image` becomes a day-2 spec change: pglord rolls the cluster standbys-first-primary-last onto the new image, with an agent-side guard against accidental major-version jumps.

**Architecture:** StatefulSet switches to `updateStrategy: OnDelete`; the k8s controller stops rolling pods on template changes. `patch_cluster` detects a pod-template difference (image, env, workload alike), synchronously applies the new StatefulSet, then spawns the existing `roll_pods(roll_order(...))`. The agent refuses to start postgres when PGDATA's major differs from the binary's.

**Tech Stack:** Rust axum/utoipa contract pipeline (`make openapi ui-client` after DTO changes — never hand-edit `spec/openapi.yaml` or `ui/src/app/api/**`), Angular UI with Transloco strings, bash e2e on the minikube stand.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-02-minor-upgrades-design.md`.
- Cluster name and `credentials_secret` stay immutable.
- Roll order is always standbys first, primary last (`roll_order`).
- Existing env/workload e2e must keep passing — they now roll via the server, not the k8s controller.
- `make lint`, `make test`, `make openapi-check`, `make ui-client-check` green before finishing.

---

### Task 1: Core — mutable image + OnDelete strategy

**Files:**
- Modify: `crates/pglord-core/src/spec.rs` (SpecChange field, apply_change arm, validate, `SpecError::InvalidImage`, doc comment rewrite, tests)
- Modify: `crates/pglord-core/src/render.rs` (`update_strategy` on the StatefulSetSpec, test)

**Interfaces:**
- Produces: `SpecChange.image: Option<String>`; `SpecError::InvalidImage`; `render_statefulset(...)` emitting `updateStrategy: OnDelete`.

- [ ] **Step 1: Failing tests in spec.rs**

```rust
#[test]
fn image_change_is_applied() {
    let s = spec("demo");
    let next = s
        .apply_change(&SpecChange { image: Some("pglord-agent:v2".into()), ..Default::default() })
        .expect("apply");
    assert_eq!(next.image, "pglord-agent:v2");
}

#[test]
fn empty_image_is_rejected() {
    let mut s = spec("demo");
    s.image = "  ".into();
    assert!(matches!(s.validate(), Err(SpecError::InvalidImage)));
}
```

Run: `cargo test -p pglord-core image` — expect compile failure (no `image` field / variant).

- [ ] **Step 2: Implement**

- `SpecError`: add `#[error("image must not be empty")] InvalidImage`.
- `SpecChange`: add after `storage_size`:

```rust
/// Change the instance image. Rolls every instance, standbys first,
/// primary last. Same-major images only — the agent refuses to start a
/// data directory created by a different major version.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub image: Option<String>,
```

- `apply_change`: `if let Some(image) = &change.image { next.image = image.clone(); }` (validation happens in the `validate()` call the server already performs on the result — confirm `patch_cluster` path validates; if `apply_change` is not followed by `validate()`, call it explicitly there).
- `validate()`: `if self.image.trim().is_empty() { return Err(SpecError::InvalidImage); }`.
- Rewrite the `SpecChange` doc comment: name stays immutable (renaming orphans derived objects); image is now a rolling change; keep the credentials_secret rationale.
- `render.rs`: in `render_statefulset`'s `StatefulSetSpec`, add

```rust
update_strategy: Some(StatefulSetUpdateStrategy {
    // pglord owns rolls (standbys first, primary last, ready-gated);
    // the controller must not race it on template changes.
    type_: Some("OnDelete".to_string()),
    rolling_update: None,
}),
```

with the import added to the existing `apps::v1` use list, plus a test asserting `sts.spec.unwrap().update_strategy.unwrap().type_ == Some("OnDelete".into())`.

- [ ] **Step 3: Run and commit**

Run: `cargo test -p pglord-core` — green. `make lint`.

```bash
git add crates/pglord-core
git commit -m "feat(core): mutable image on the cluster spec, OnDelete statefulset strategy"
```

### Task 2: Server — template-change detection, synchronous apply, roll

**Files:**
- Modify: `crates/pglord-server/src/api/clusters.rs` (`patch_cluster`, `spec_error` arm, helper + unit test)
- Regenerate: `spec/openapi.yaml` (`make openapi`)

**Interfaces:**
- Consumes: `SpecChange.image` (Task 1); `reconciler::apply_cluster(client, ns, spec, owner, primary_pod)`; `roll_order`/`roll_pods`.
- Produces: `fn pod_template_changed(current: &ClusterSpec, next: &ClusterSpec, ns: &str) -> bool`.

- [ ] **Step 1: Helper + unit test**

```rust
/// Whether a spec change alters the rendered pod template (image, env,
/// workload...). Owner content cancels out — any fixed value works here.
fn pod_template_changed(
    current: &pglord_core::spec::ClusterSpec,
    next: &pglord_core::spec::ClusterSpec,
    ns: &str,
) -> bool {
    let owner = pglord_core::render::OwnerRef {
        api_version: "v1".into(),
        kind: "ConfigMap".into(),
        name: format!("{}-spec", current.name),
        uid: "template-diff".into(),
    };
    pglord_core::render::render_statefulset(current, ns, &owner).spec.map(|s| s.template)
        != pglord_core::render::render_statefulset(next, ns, &owner).spec.map(|s| s.template)
}
```

(Match `OwnerRef`'s actual field set — check `render.rs:131`.) Unit test in the existing `mod tests`: an image change and an env change return true; a parameters-only change returns false.

- [ ] **Step 2: Wire into patch_cluster**

- Keep the `(current, owner)` from `store.get` (stop discarding `_owner`).
- After `ops.store.put(&next)` and the orphan-PVC block:

```rust
if pod_template_changed(&current, &next, &ops.ns) {
    let primary = ops
        .store
        .primary(&name)
        .await
        .map_err(ApiError::internal)?
        .unwrap_or_else(|| format!("{name}-0"));
    // The StatefulSet must carry the new template before any pod dies, or
    // a deleted pod would come back on the old one. Same field manager as
    // the reconciler, so the level-triggered loop stays idempotent on top.
    crate::reconciler::apply_cluster(&ops.kube, &ops.ns, &next, &owner, &primary)
        .await
        .map_err(ApiError::internal)?;
    let ops = ops.clone();
    let roll_spec = next.clone();
    tokio::spawn(async move {
        let order = roll_order(&roll_spec.name, roll_spec.replicas, &primary);
        tracing::info!(cluster = %roll_spec.name, ?order, "template change: rolling instances");
        roll_pods(&ops, &roll_spec, order).await;
        tracing::info!(cluster = %roll_spec.name, "template roll finished");
    });
}
```

- `spec_error`: add `SpecError::InvalidImage => "invalid_image",`.
- Confirm the patch path validates the merged spec (`next.validate()`); add it right after `apply_change` if absent.

- [ ] **Step 3: Contract + verify + commit**

Run: `make openapi && make test && make lint && make openapi-check`.

```bash
git add crates/pglord-server spec/openapi.yaml
git commit -m "feat(server): image change rolls the cluster via OnDelete-controlled roll"
```

### Task 3: Agent — major-version preflight

**Files:**
- Modify: `crates/pglord-agent/src/bootstrap.rs` (pure check + call in `ensure_bootstrapped` when PGDATA exists, tests)

**Interfaces:**
- Produces: `pub fn major_version_conflict(pgdata_version: &str, binary_version_output: &str) -> Option<(String, String)>` returning `(data_major, binary_major)` on mismatch.

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn major_versions_compared_from_pgdata_and_binary() {
    // PG_VERSION holds "16"; `postgres --version` prints
    // "postgres (PostgreSQL) 16.4 (Debian ...)".
    assert_eq!(major_version_conflict("16\n", "postgres (PostgreSQL) 16.4"), None);
    assert_eq!(
        major_version_conflict("15\n", "postgres (PostgreSQL) 16.4"),
        Some(("15".into(), "16".into()))
    );
    // Unparseable input must not block startup: postgres itself is the
    // final authority and refuses a real mismatch anyway.
    assert_eq!(major_version_conflict("16\n", "gibberish"), None);
}
```

- [ ] **Step 2: Implement**

```rust
/// Compare PGDATA's major (PG_VERSION file) with the running binary's.
/// Some((data, binary)) on a definite mismatch; None when they match or
/// either side is unreadable (postgres is the final authority).
pub fn major_version_conflict(
    pgdata_version: &str,
    binary_version_output: &str,
) -> Option<(String, String)> {
    let data = pgdata_version.trim().split('.').next()?.to_string();
    let binary = binary_version_output
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))?
        .split('.')
        .next()?
        .to_string();
    if data.is_empty() || binary.is_empty() || data == binary {
        None
    } else {
        Some((data, binary))
    }
}
```

In `ensure_bootstrapped`, on the already-bootstrapped path (`is_bootstrapped(pgdata)` true), read `pgdata/PG_VERSION` and run `postgres --version` (tokio Command, capture stdout); on `Some((data, binary))` bail:

```rust
anyhow::bail!(
    "PGDATA was created by PostgreSQL {data} but this image ships PostgreSQL {binary}; \
     major upgrades are not supported — revert the cluster image to a PostgreSQL {data} build"
);
```

- [ ] **Step 3: Run and commit**

Run: `cargo test -p pglord-agent major_version && make lint`.

```bash
git add crates/pglord-agent/src/bootstrap.rs
git commit -m "feat(agent): refuse to start a PGDATA from a different postgres major"
```

### Task 4: UI — image edit on the cluster detail

**Files:**
- Modify: `ui/src/app/pages/clusters/cluster-detail.component.ts` (ImageDialogComponent, button next to the cluster header/config area, handler)
- Modify: `ui/public/i18n/en.json` ("image" section)
- Regenerate: `ui/src/app/api/**` (`make ui-client`)

**Interfaces:**
- Consumes: regenerated `SpecChange` with `image?: string`; existing `patchCluster` service call and dialog patterns (see SyncDialogComponent in the same file).

- [ ] **Step 1: Regenerate the client**

Run: `make ui-client` — `SpecChange` gains `image`.

- [ ] **Step 2: Dialog + handler + template**

Follow the SyncDialogComponent pattern in the same file: an `ImageDialogComponent` with a text input pre-filled with the current image, warning copy, confirm/cancel. Handler:

```typescript
async openImage(d: ClusterDetail): Promise<void> {
  const ref = this.dialog.open(ImageDialogComponent, { data: { image: d.image } });
  const next = await firstValueFrom(ref.closed);
  if (!next || next === d.image) return;
  await firstValueFrom(this.api.patchCluster({ name: d.name, body: { image: next } }));
  this.refresh();
}
```

(`ClusterDetail.image` must exist on the DTO — it already carries the spec's image; verify and add to the server DTO + regenerate if absent.) Template: show the current image with an edit (pencil) button in the cluster meta section; all strings via Transloco keys `clusters.detail.image.*` (`title`, `label`, `warning`, `confirm`, `cancel`).

- [ ] **Step 3: i18n**

Add the `image` section to `ui/public/i18n/en.json` next to the existing detail sections; run the repo's JSON round-trip (prettier) so formatting stays clean.

- [ ] **Step 4: Verify and commit**

Run: `make ui-lint && make ui-test && make ui-build && make ui-client-check`.

```bash
git add ui
git commit -m "feat(ui): cluster image edit with rolling-upgrade confirm"
```

### Task 5: E2e + docs

**Files:**
- Modify: `scripts/day2-e2e.sh` (upgrade section)
- Modify: `docs/user-guide.md` (upgrade section; drop the immutable-image limitation; mid-roll recovery note)

- [ ] **Step 1: E2e section**

In `scripts/day2-e2e.sh`, after the existing day-2 sections:

```bash
echo "--- image change: rolling upgrade, standbys first"
# Same binary under a second tag: proves the roll, not postgres.
minikube -p pglord image tag pglord-agent:dev pglord-agent:dev-upgrade
api PATCH "/clusters/$CLUSTER" '{"image":"pglord-agent:dev-upgrade"}' >/dev/null
PRIMARY=$(api GET "/clusters/$CLUSTER" | jq -r .primary)
# The primary's pod must still be on the old image while a standby rolls;
# poll until every pod reports the new image and the cluster is ready.
for i in $(seq 1 60); do
  IMAGES=$(minikube -p pglord kubectl -- -n "$NS" get pods -l pglord.dev/cluster="$CLUSTER" -o jsonpath='{range .items[*]}{.spec.containers[0].image}{"\n"}{end}' | sort -u)
  [ "$IMAGES" = "pglord-agent:dev-upgrade" ] && break
  sleep 5
done
[ "$IMAGES" = "pglord-agent:dev-upgrade" ] || { echo "pods never converged to the new image"; exit 1; }
wait_ready
echo "OK all instances on the new image, cluster healthy"
# Reject an empty image.
CODE=$(api_code PATCH "/clusters/$CLUSTER" '{"image":"  "}')
[ "$CODE" = 422 ] || { echo "empty image not rejected: $CODE"; exit 1; }
```

Adapt helper names, labels, and the standbys-first assertion to the script's actual helpers (read it first; if it lacks an `api_code` helper, use the pattern config-e2e.sh uses for 422 checks). Verify env/workload suites still pass — they exercise the new server-driven roll.

- [ ] **Step 2: Run on the stand**

Run: `make stand-image day2-e2e workload-e2e config-e2e` — all pass.

- [ ] **Step 3: Docs**

`docs/user-guide.md`: new "Minor version upgrades" section (how to change the image, roll order, bad-image recovery = revert the image, major versions refused by the agent, mid-roll server restart recovered by the manual rolling restart). Remove the "Cluster name and image cannot be changed" limitation bullet's image half. Renumber if needed.

- [ ] **Step 4: Final verification + commit**

Run: `make lint test openapi-check ui-client-check`.

```bash
git add scripts/day2-e2e.sh docs/user-guide.md
git commit -m "test(e2e): rolling image upgrade flow; docs: minor upgrades guide"
```
