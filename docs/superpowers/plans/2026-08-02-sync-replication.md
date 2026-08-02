# Synchronous Replication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `synchronous: bool` spec flag that makes every commit wait for any one standby (`synchronous_standby_names = 'ANY 1 (...)'`), strict (no auto-degrade), reload-only, with degraded-state surfacing in API/UI/metrics.

**Architecture:** A pure render function in `pglord-core` produces a third managed conf include (`postgresql.pglord-sync.conf`); every agent writes it at bootstrap and converges it in the existing 5s watch loop. Each instance's file lists all peer pods (never itself), so a promoted standby already carries the right value — failover needs no sync step and the orchestrator is untouched. Observability rides the single instance-probe path.

**Tech Stack:** Rust (axum, utoipa, kube-rs, tokio-postgres, prometheus), Angular + spartan-ng, Transloco, generated TS client via ng-openapi-gen.

**Design spec:** `docs/superpowers/specs/2026-08-02-sync-replication-design.md`

## Global Constraints

- Rust edition 2024; clippy warns on `unwrap_used`; `make lint` = fmt check + clippy `-D warnings`.
- Never hand-edit `spec/openapi.yaml` or `ui/src/app/api/**` — after DTO changes run `make openapi ui-client` and commit regenerated artifacts with the code.
- All user-visible UI strings go through Transloco (`ui/public/i18n/en.json`).
- `synchronous = true` requires `replicas >= 2` and rejects `parameters.synchronous_commit` of `off` or `local`.
- `synchronous_standby_names` is pglord-owned (reserved), never user-settable.
- Strict semantics: never trim the standby list based on health; no auto-degrade.
- Commit after every task with the repo's conventional prefixes (`feat(core):`, `feat(agent):`, `feat(server):`, `feat(ui):`, `test(e2e):`, `docs:`).

---

### Task 1: Core — `sync` module: peer-list render

**Files:**
- Create: `crates/pglord-core/src/sync.rs`
- Modify: `crates/pglord-core/src/lib.rs` (module list at lines 3-12)

**Interfaces:**
- Produces: `pglord_core::sync::render(cluster: &str, replicas: u8, me: &str, synchronous: bool) -> String` — the entire content of the managed include file. Tasks 3 (agent) depends on this exact signature.

- [ ] **Step 1: Write the failing tests**

Create `crates/pglord-core/src/sync.rs` with the test module only (no `render` yet):

```rust
//! Synchronous replication: the managed conf include that sets
//! `synchronous_standby_names` on every instance.
//!
//! Each instance's file names all pods of the cluster *except itself*, so the
//! value is already correct from any instance's point of view the moment it is
//! promoted — failover needs no sync reconfiguration. Listing a down standby
//! is harmless: postgres only waits on connected ones, and none connected
//! means blocked writes, which is exactly the strict semantics of the flag.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_instance_excludes_itself_from_the_quorum() {
        assert_eq!(
            render("demo", 3, "demo-0", true),
            "# Managed by pglord — do not edit; changes come from the cluster spec.\n\
             synchronous_standby_names = 'ANY 1 (\"demo-1\", \"demo-2\")'\n"
        );
        assert_eq!(
            render("demo", 3, "demo-1", true),
            "# Managed by pglord — do not edit; changes come from the cluster spec.\n\
             synchronous_standby_names = 'ANY 1 (\"demo-0\", \"demo-2\")'\n"
        );
    }

    #[test]
    fn two_replicas_yield_a_single_peer() {
        assert_eq!(
            render("demo", 2, "demo-0", true),
            "# Managed by pglord — do not edit; changes come from the cluster spec.\n\
             synchronous_standby_names = 'ANY 1 (\"demo-1\")'\n"
        );
    }

    #[test]
    fn disabled_renders_an_explicit_empty_value() {
        // '' (not an absent file) so turning the flag off clears the setting
        // on reload rather than leaving the old list in effect.
        assert_eq!(
            render("demo", 3, "demo-0", false),
            "# Managed by pglord — do not edit; changes come from the cluster spec.\n\
             synchronous_standby_names = ''\n"
        );
    }

    #[test]
    fn no_peers_never_renders_an_empty_any_list() {
        // Validation forbids synchronous with replicas < 2, but render is pure
        // and must not emit the syntactically invalid `ANY 1 ()` regardless.
        assert_eq!(
            render("demo", 1, "demo-0", true),
            "# Managed by pglord — do not edit; changes come from the cluster spec.\n\
             synchronous_standby_names = ''\n"
        );
    }

    #[test]
    fn a_foreign_hostname_lists_every_pod() {
        // Outside Kubernetes (docker e2e) the hostname is not a pod name; the
        // render must still be well-formed and simply not exclude anything.
        assert_eq!(
            render("demo", 2, "1f9a2b3c4d5e", true),
            "# Managed by pglord — do not edit; changes come from the cluster spec.\n\
             synchronous_standby_names = 'ANY 1 (\"demo-0\", \"demo-1\")'\n"
        );
    }
}
```

Register the module in `crates/pglord-core/src/lib.rs` (alphabetical, after `spec`):

```rust
pub mod sync;
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pglord-core sync::`
Expected: compile error — `render` not found.

- [ ] **Step 3: Implement `render`**

Add above the test module in `crates/pglord-core/src/sync.rs`:

```rust
/// Render the managed include for one instance. Pod names carry hyphens, so
/// they are double-quoted inside the single-quoted GUC value.
pub fn render(cluster: &str, replicas: u8, me: &str, synchronous: bool) -> String {
    let mut out =
        String::from("# Managed by pglord — do not edit; changes come from the cluster spec.\n");
    let peers: Vec<String> = (0..replicas)
        .map(|i| format!("{cluster}-{i}"))
        .filter(|p| p != me)
        .map(|p| format!("\"{p}\""))
        .collect();
    if synchronous && !peers.is_empty() {
        out.push_str(&format!(
            "synchronous_standby_names = 'ANY 1 ({})'\n",
            peers.join(", ")
        ));
    } else {
        out.push_str("synchronous_standby_names = ''\n");
    }
    out
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pglord-core sync::`
Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/pglord-core/src/sync.rs crates/pglord-core/src/lib.rs
git commit -m "feat(core): sync replication conf render, per-instance peer list"
```

---

### Task 2: Core — wire `synchronous` into ClusterSpec, SpecChange and validation

**Files:**
- Modify: `crates/pglord-core/src/spec.rs`
- Modify: `crates/pglord-core/src/guc.rs` (RESERVED list, lines 49-64)

**Interfaces:**
- Consumes: nothing new.
- Produces: `ClusterSpec.synchronous: bool` (serde default `false`), `SpecChange.synchronous: Option<bool>`, `SpecError::SyncRequiresReplicas`, `SpecError::SyncConflictsSynchronousCommit(String)`. Tasks 3-5 rely on these exact names.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/pglord-core/src/spec.rs`:

```rust
    #[test]
    fn sync_requires_at_least_two_replicas() {
        let mut s = spec("demo");
        s.synchronous = true;
        assert_eq!(s.validate(), Err(SpecError::SyncRequiresReplicas));
        s.replicas = 2;
        assert_eq!(s.validate(), Ok(()));

        // Scaling a sync cluster down to a single instance would block every
        // write forever; the same rule catches it on the change path.
        assert_eq!(
            s.apply_change(&SpecChange {
                replicas: Some(1),
                ..Default::default()
            }),
            Err(SpecError::SyncRequiresReplicas)
        );
    }

    #[test]
    fn sync_rejects_parameters_that_defeat_it() {
        let mut s = spec("demo");
        s.replicas = 2;
        s.synchronous = true;
        for bad in ["off", "local"] {
            s.parameters = BTreeMap::from([("synchronous_commit".into(), bad.into())]);
            assert!(
                matches!(
                    s.validate(),
                    Err(SpecError::SyncConflictsSynchronousCommit(_))
                ),
                "expected rejection for synchronous_commit={bad}"
            );
        }
        // Stronger-or-equal levels stay legal tuning.
        for ok in ["on", "remote_write", "remote_apply"] {
            s.parameters = BTreeMap::from([("synchronous_commit".into(), ok.into())]);
            assert_eq!(s.validate(), Ok(()), "expected ok for synchronous_commit={ok}");
        }
        // With the flag off, 'off' remains an allowed durability trade.
        s.synchronous = false;
        s.parameters = BTreeMap::from([("synchronous_commit".into(), "off".into())]);
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn synchronous_standby_names_is_reserved() {
        let mut s = spec("demo");
        s.parameters =
            BTreeMap::from([("synchronous_standby_names".into(), "ANY 1 (x)".into())]);
        assert!(matches!(s.validate(), Err(SpecError::ReservedParameter(_))));
    }

    #[test]
    fn sync_flag_is_a_day2_toggle() {
        let mut s = spec("demo");
        s.replicas = 2;
        let on = s
            .apply_change(&SpecChange {
                synchronous: Some(true),
                ..Default::default()
            })
            .expect("enable");
        assert!(on.synchronous);
        // Absent leaves it untouched.
        let scaled = on
            .apply_change(&SpecChange {
                replicas: Some(3),
                ..Default::default()
            })
            .expect("scale");
        assert!(scaled.synchronous);
        let off = on
            .apply_change(&SpecChange {
                synchronous: Some(false),
                ..Default::default()
            })
            .expect("disable");
        assert!(!off.synchronous);
    }

    #[test]
    fn specs_without_the_sync_flag_parse_as_async() {
        // Every spec ConfigMap written before this feature lacks the field.
        assert!(!spec("demo").synchronous);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pglord-core spec::`
Expected: compile errors — no `synchronous` field, no new error variants.

- [ ] **Step 3: Implement the wiring**

In `crates/pglord-core/src/spec.rs`:

1. Add the field to `ClusterSpec` right after `storage_size` (line 25):

```rust
    /// Every commit waits until any one standby confirms the WAL flush
    /// (quorum `ANY 1` over all standbys). Strict: with no standby connected,
    /// writes block rather than silently falling back to async. Requires
    /// `replicas >= 2`. Applied with a reload — no restart.
    #[serde(default)]
    pub synchronous: bool,
```

2. Add the error variants to `SpecError` (after `InvalidParameterValue`, line 86):

```rust
    #[error("synchronous replication requires at least 2 replicas")]
    SyncRequiresReplicas,
    #[error(
        "synchronous replication conflicts with synchronous_commit = '{0}'; \
         use 'on', 'remote_write' or 'remote_apply'"
    )]
    SyncConflictsSynchronousCommit(String),
```

3. Add the change field to `SpecChange` after `storage_size` (line 118):

```rust
    /// Toggle synchronous replication. Reload-only: no restart, no roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synchronous: Option<bool>,
```

4. Apply it in `apply_change` (after the `storage_size` block, line 181):

```rust
        if let Some(synchronous) = change.synchronous {
            next.synchronous = synchronous;
        }
```

5. Validate in `validate()` (after the parameters loop, before `hba::validate_rules`, line 227):

```rust
        if self.synchronous {
            if self.replicas < 2 {
                return Err(SpecError::SyncRequiresReplicas);
            }
            if let Some(v) = self.parameters.get("synchronous_commit")
                && matches!(v.as_str(), "off" | "local")
            {
                return Err(SpecError::SyncConflictsSynchronousCommit(v.clone()));
            }
        }
```

6. In `crates/pglord-core/src/guc.rs`, add to `RESERVED` (after `"primary_conninfo"`, line 56):

```rust
    // Driven by the `synchronous` spec flag; a hand-set list would fight the
    // agent's render and break the strict quorum guarantee.
    "synchronous_standby_names",
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pglord-core`
Expected: all pass, including the pre-existing guc catalog/reserved disjointness test.

- [ ] **Step 5: Commit**

```bash
git add crates/pglord-core/src/spec.rs crates/pglord-core/src/guc.rs
git commit -m "feat(core): synchronous flag on the cluster spec, strict validation"
```

---

### Task 3: Agent — own the sync include, converge on change, fix basebackup application_name

**Files:**
- Modify: `crates/pglord-agent/src/params.rs`
- Modify: `crates/pglord-agent/src/bootstrap.rs`
- Modify: `crates/pglord-agent/src/record.rs`
- Modify: `crates/pglord-agent/src/main.rs`

**Interfaces:**
- Consumes: `pglord_core::sync::render(cluster, replicas, me, synchronous)` (Task 1); `spec.synchronous`, `spec.replicas` (Task 2).
- Produces: `params::SYNC_FILE: &str = "postgresql.pglord-sync.conf"`, `params::write_sync_if_changed(pgdata: &Path, rendered: &str) -> std::io::Result<bool>`. `record::apply_parameters` signature gains `me: &str`.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/pglord-agent/src/params.rs`:

```rust
    #[test]
    fn sync_write_is_idempotent_and_targets_the_sync_file() {
        let dir = tempdir("sync");
        let on = pglord_core::sync::render("demo", 2, "demo-0", true);
        assert!(write_sync_if_changed(&dir, &on).expect("write"));
        // Unchanged content must not report a change: a report triggers a
        // reload, and this runs on every 5s poll.
        assert!(!write_sync_if_changed(&dir, &on).expect("write"));
        assert_eq!(
            std::fs::read_to_string(dir.join(SYNC_FILE)).expect("read"),
            on
        );
        let off = pglord_core::sync::render("demo", 2, "demo-0", false);
        assert!(write_sync_if_changed(&dir, &off).expect("write"));
        let _ = std::fs::remove_dir_all(&dir);
    }
```

In `crates/pglord-agent/src/bootstrap.rs`, find the existing test asserting the include order of `postgresql_conf_extra` (test module, around line 340) and extend it — add these assertions to that test (adapt to its local variable name for the rendered string; it asserts `include_if_exists` lines come before the managed settings):

```rust
        // The sync include follows the same rule as params/archive: included
        // before the managed settings so it can never override HA settings.
        assert!(extra.contains("include_if_exists = 'postgresql.pglord-sync.conf'"));
        let sync_pos = extra
            .find("postgresql.pglord-sync.conf")
            .expect("sync include present");
        let managed_pos = extra.find("listen_addresses").expect("managed settings");
        assert!(sync_pos < managed_pos);
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pglord-agent params:: bootstrap::`
Expected: compile error (`SYNC_FILE`, `write_sync_if_changed` missing), then assertion failure on the include line.

- [ ] **Step 3: Implement the agent changes**

1. `crates/pglord-agent/src/params.rs` — after `ARCHIVE_FILE` (line 14):

```rust
/// Synchronous-replication settings, one file per instance: each instance's
/// peer list excludes itself, so a promoted standby already carries the right
/// value (see pglord_core::sync::render).
pub const SYNC_FILE: &str = "postgresql.pglord-sync.conf";
```

and after `write_archive_if_changed` (line 49):

```rust
/// Write the sync-replication file when it differs; returns whether it changed.
pub fn write_sync_if_changed(pgdata: &Path, rendered: &str) -> std::io::Result<bool> {
    write_file_if_changed(&pgdata.join(SYNC_FILE), rendered)
}
```

2. `crates/pglord-agent/src/bootstrap.rs` — extend `postgresql_conf_extra` (lines 25-39) with a third include (order: params, archive, sync, then managed settings):

```rust
pub fn postgresql_conf_extra(port: u16) -> String {
    format!(
        "\n# --- managed by pglord-agent ---\n\
         include_if_exists = '{}'\n\
         include_if_exists = '{}'\n\
         include_if_exists = '{}'\n\
         listen_addresses = '*'\n\
         port = {port}\n\
         wal_level = replica\n\
         max_wal_senders = 10\n\
         hot_standby = on\n\
         wal_log_hints = on\n",
        crate::params::PARAMS_FILE,
        crate::params::ARCHIVE_FILE,
        crate::params::SYNC_FILE,
    )
}
```

3. `crates/pglord-agent/src/bootstrap.rs` — in `bootstrap_standby` (line 296-315), set `application_name` right after a successful basebackup. `pg_basebackup -R` writes `primary_conninfo` without it, and until now it was only corrected on the *next* agent start — leaving a fresh standby invisible to `synchronous_standby_names`. Replace the success branch:

```rust
        if status.success() {
            // -R writes primary_conninfo without application_name; rewrite it
            // now so the standby is a nameable sync peer from its first start.
            let me = std::env::var("HOSTNAME").unwrap_or_default();
            refresh_conninfo(&cfg.pgdata, &primary_conninfo(cfg, &me))
                .context("set application_name in primary_conninfo")?;
            tracing::info!("standby bootstrap complete");
            return Ok(());
        }
```

4. `crates/pglord-agent/src/record.rs` — converge the file in the watch loop. Change `apply_parameters` to take the pod name and render the sync conf; in `follow` (line 116) change the call to `apply_parameters(&reader, &cfg, &me).await;`. In `apply_parameters` change the signature and add the block after the archive block (line 166):

```rust
async fn apply_parameters(reader: &RecordReader, cfg: &Config, me: &str) {
```

```rust
    let sync = pglord_core::sync::render(&cfg.cluster, spec.replicas, me, spec.synchronous);
    match crate::params::write_sync_if_changed(&cfg.pgdata, &sync) {
        Ok(true) => changed = true,
        Ok(false) => {}
        Err(err) => tracing::warn!(%err, "failed to write sync replication settings"),
    }
```

5. `crates/pglord-agent/src/main.rs` — write the initial render after bootstrap, before postgres starts, covering every bootstrap path (initdb, basebackup, restore, already-bootstrapped) uniformly. After the `ensure_bootstrapped` call (line 52), before `spawn_postgres`:

```rust
    let sync_conf = pglord_core::sync::render(
        &cfg.cluster,
        cluster_spec.as_ref().map(|s| s.replicas).unwrap_or(1),
        &hostname,
        cluster_spec.as_ref().map(|s| s.synchronous).unwrap_or(false),
    );
    if let Err(err) = pglord_agent::params::write_sync_if_changed(&cfg.pgdata, &sync_conf) {
        tracing::warn!(%err, "failed to write sync replication settings");
    }
```

(`params` is already used as `crate::params` throughout the agent; ensure `crates/pglord-agent/src/lib.rs` exports it as `pub mod params;` — add if missing.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pglord-agent`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/pglord-agent/src/params.rs crates/pglord-agent/src/bootstrap.rs \
        crates/pglord-agent/src/record.rs crates/pglord-agent/src/main.rs \
        crates/pglord-agent/src/lib.rs
git commit -m "feat(agent): own the sync conf include, name fresh standbys for quorum"
```

---

### Task 4: Agent — report connected sync standbys on /status and /metrics

**Files:**
- Modify: `crates/pglord-agent/src/status.rs`
- Modify: `crates/pglord-agent/src/metrics.rs`

**Interfaces:**
- Produces: `InstanceStatus.sync_standby_count: Option<u32>` (serialized on `/status`; `None` on standbys and when postgres is unreachable), agent metric `pglord_pg_sync_standbys` (primary only). Task 5's `AgentStatus` deserializes the same field name.

- [ ] **Step 1: Write the failing metrics test**

In `crates/pglord-agent/src/metrics.rs` tests, extend `full_sample()` with the new field and add assertions:

```rust
            sync_standbys: Some(1.0),
```

(in `full_sample()`, before the closing brace), and in `a_healthy_primary_renders_every_series`:

```rust
        assert!(out.contains("pglord_pg_sync_standbys 1"));
```

and in `a_standby_reports_lag_seconds_not_replication_rows`:

```rust
        assert!(!out.contains("pglord_pg_sync_standbys"));
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pglord-agent metrics::`
Expected: compile error — no `sync_standbys` field.

- [ ] **Step 3: Implement**

1. `crates/pglord-agent/src/status.rs` — add to `InstanceStatus` (after `pending_restart`, line 20):

```rust
    /// Primary only: connected standbys currently eligible to confirm a
    /// synchronous commit (pg_stat_replication sync_state 'sync'/'quorum').
    /// None on standbys. 0 with the sync flag on means writes are blocking.
    pub sync_standby_count: Option<u32>,
```

Extend the query in `try_query` (change return type to `(bool, Option<String>, bool, Option<u32>)`):

```rust
    let row = client
        .query_one(
            "SELECT pg_is_in_recovery(), \
             CASE WHEN pg_is_in_recovery() \
                  THEN pg_last_wal_receive_lsn()::text \
                  ELSE pg_current_wal_lsn()::text END, \
             EXISTS(SELECT 1 FROM pg_settings WHERE pending_restart), \
             CASE WHEN pg_is_in_recovery() THEN NULL \
                  ELSE (SELECT count(*)::int4 FROM pg_stat_replication \
                        WHERE sync_state IN ('sync', 'quorum')) END",
            &[],
        )
        .await?;
    let in_recovery: bool = row.get(0);
    let lsn: Option<String> = row.get(1);
    let pending_restart: bool = row.get(2);
    let sync_standby_count = row
        .get::<_, Option<i32>>(3)
        .map(|c| u32::try_from(c).unwrap_or(0));
    drop(client);
    conn_task.abort();
    Ok((in_recovery, lsn, pending_restart, sync_standby_count))
```

Update `query_status` to destructure the fourth element and set `sync_standby_count` in the success arm, `None` in the error arm.

2. `crates/pglord-agent/src/metrics.rs` — add to `MetricsSample` (after `pending_restart`, line 24):

```rust
    /// Primary only: standbys eligible to confirm a synchronous commit.
    pub sync_standbys: Option<f64>,
```

In `render` (after the `pending_restart` gauge, line 114):

```rust
    if let Some(v) = sample.sync_standbys {
        gauge(
            &reg,
            "pglord_pg_sync_standbys",
            "Primary only: connected standbys with sync_state sync/quorum.",
            v,
        );
    }
```

In `try_collect`, inside the existing `if !in_recovery` block (after the replay-lag loop, line 186):

```rust
        sample.sync_standbys = Some(
            client
                .query_one(
                    "SELECT count(*)::float8 FROM pg_stat_replication \
                     WHERE sync_state IN ('sync', 'quorum')",
                    &[],
                )
                .await?
                .get(0),
        );
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pglord-agent`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/pglord-agent/src/status.rs crates/pglord-agent/src/metrics.rs
git commit -m "feat(agent): report connected sync standbys on /status and /metrics"
```

---

### Task 5: Server — surface sync state on the cluster detail, error codes, metric, contract

**Files:**
- Modify: `crates/pglord-server/src/instance_probe.rs`
- Modify: `crates/pglord-server/src/api/clusters.rs`
- Modify: `crates/pglord-server/src/metrics.rs`
- Modify: `crates/pglord-server/src/orchestrator.rs` (metric only — election untouched)
- Regenerate: `spec/openapi.yaml`, `ui/src/app/api/**`

**Interfaces:**
- Consumes: agent `/status` field `sync_standby_count: Option<u32>` (Task 4); `spec.synchronous`, `SpecError::SyncRequiresReplicas`, `SpecError::SyncConflictsSynchronousCommit` (Task 2).
- Produces: `InstanceObservation.sync_standby_count: Option<u32>`, `InstanceDto.sync_standby_count: Option<u32>`, `ClusterDetail.synchronous: bool`, `ClusterDetail.sync_degraded: bool`, problem+json codes `sync_requires_replicas` / `sync_conflicts_synchronous_commit`, server metric `pglord_sync_degraded{cluster}`. Task 6 consumes the regenerated TS models.

- [ ] **Step 1: Write the failing server-metrics test**

In `crates/pglord-server/src/metrics.rs` tests, add to `recorded_series_appear_in_render`:

```rust
        m.sync_degraded.with_label_values(&["demo"]).set(1);
```

and:

```rust
        assert!(out.contains("pglord_sync_degraded{cluster=\"demo\"} 1"));
```

and in `forgetting_a_cluster_drops_its_series`, before `forget_cluster`:

```rust
        m.sync_degraded.with_label_values(&["gone"]).set(1);
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pglord-server metrics::`
Expected: compile error — no `sync_degraded` handle.

- [ ] **Step 3: Implement**

1. `crates/pglord-server/src/instance_probe.rs` — thread the field through both structs and both match arms:

In `AgentStatus` (after `pending_restart`, line 14):

```rust
    #[serde(default)]
    sync_standby_count: Option<u32>,
```

In `InstanceObservation` (after `pending_restart`, line 24):

```rust
    /// Primary only: connected standbys eligible to confirm a sync commit.
    pub sync_standby_count: Option<u32>,
```

In `observe_cluster`, `Some(s)` arm add `sync_standby_count: s.sync_standby_count,`; `None` arm add `sync_standby_count: None,`.

2. `crates/pglord-server/src/api/clusters.rs`:

`InstanceDto` (after `pending_restart`, line 33):

```rust
    /// Primary only: connected standbys eligible to confirm a synchronous
    /// commit. Null on standbys and unreachable instances.
    pub sync_standby_count: Option<u32>,
```

`ClusterDetail` (after `pending_restart`, line 65):

```rust
    /// Commits wait for any one standby to confirm the WAL flush.
    pub synchronous: bool,
    /// Synchronous replication is on but the primary sees no connected sync
    /// standby: commits are blocking until one reconnects.
    pub sync_degraded: bool,
```

In `get_cluster` (before the `instances` mapping, line 216), compute:

```rust
    let sync_degraded = spec.synchronous
        && observations
            .iter()
            .any(|o| o.pod == primary && o.ready && o.sync_standby_count == Some(0));
```

Add `sync_standby_count: o.sync_standby_count,` to the `InstanceDto` mapping, and `synchronous: spec.synchronous,` + `sync_degraded,` to the `ClusterDetail` literal (before `pending_restart`).

`spec_error` (line 252) — add arms before the catch-all:

```rust
        SpecError::SyncRequiresReplicas => "sync_requires_replicas",
        SpecError::SyncConflictsSynchronousCommit(_) => "sync_conflicts_synchronous_commit",
```

3. `crates/pglord-server/src/metrics.rs` — add the handle following the `instances_ready` pattern exactly: field `pub sync_degraded: IntGaugeVec` on `Metrics`, built in `new()` as

```rust
        let sync_degraded = gauge_vec(
            &registry,
            "pglord_sync_degraded",
            "1 when synchronous replication is on but no sync standby is connected (writes blocking).",
        );
```

added to the struct literal, and in `forget_cluster`:

```rust
        let _ = self.sync_degraded.remove_label_values(&[cluster]);
```

4. `crates/pglord-server/src/orchestrator.rs` — in `run`, after the `instances_ready` gauge set (line 72), add (election and failover logic stay untouched):

```rust
            let degraded = spec.synchronous
                && observations
                    .iter()
                    .any(|o| o.pod == primary && o.ready && o.sync_standby_count == Some(0));
            metrics
                .sync_degraded
                .with_label_values(&[spec.name.as_str()])
                .set(i64::from(degraded));
```

- [ ] **Step 4: Run tests, regenerate the contract**

Run: `cargo test -p pglord-server && make openapi ui-client && make openapi-check ui-client-check`
Expected: tests pass; regenerated `spec/openapi.yaml` and `ui/src/app/api/**` show `synchronous`, `sync_degraded`, `sync_standby_count`; drift checks pass.

- [ ] **Step 5: Commit (code + regenerated artifacts together)**

```bash
git add crates/pglord-server/src crates/pglord-core spec/openapi.yaml ui/src/app/api
git commit -m "feat(server): sync state on the cluster detail, degraded metric, contract"
```

---

### Task 6: UI — sync toggle with confirm, degraded banner

**Files:**
- Modify: `ui/src/app/pages/clusters/cluster-detail.component.ts`
- Modify: `ui/public/i18n/en.json`

**Interfaces:**
- Consumes: regenerated `ClusterDetail` model (`synchronous`, `sync_degraded`, instance `sync_standby_count`), `patchCluster` with `{ synchronous }` (Task 5).

- [ ] **Step 1: Add the i18n strings**

In `ui/public/i18n/en.json`, add a top-level `"sync"` section (next to `"config"` / `"hba"`):

```json
  "sync": {
    "title": "Synchronous replication",
    "enable": "Enable",
    "disable": "Disable",
    "enableTitle": "Enable synchronous replication on {{name}}?",
    "enableWarning": "Every commit will wait until a standby confirms it. If no standby is connected, writes block until one returns. Applies with a reload — no restart.",
    "disableTitle": "Disable synchronous replication on {{name}}?",
    "disableWarning": "Commits will no longer wait for a standby. A primary failure can lose the most recent commits.",
    "on": "On — commits wait for any one standby ({{count}} connected).",
    "off": "Off — replication is asynchronous.",
    "degraded": "No synchronous standby is connected — writes are blocked until one returns."
  },
```

- [ ] **Step 2: Add the confirm dialog component**

In `ui/src/app/pages/clusters/cluster-detail.component.ts`, after `RotateCredentialsDialogComponent` (line 140), following the same pattern:

```typescript
/** Confirm toggling synchronous replication; the warning states the
 * strict-blocking trade-off plainly. */
@Component({
  selector: 'pg-sync-dialog',
  imports: [TranslocoModule, HlmButton, HlmDialogFooter, HlmDialogHeader, HlmDialogTitle],
  template: `
    <div hlmDialogHeader>
      <h2 hlmDialogTitle>
        {{ (enable ? 'sync.enableTitle' : 'sync.disableTitle') | transloco: { name: name } }}
      </h2>
    </div>
    <p class="text-muted-foreground py-2 text-sm">
      {{ (enable ? 'sync.enableWarning' : 'sync.disableWarning') | transloco }}
    </p>
    <div hlmDialogFooter>
      <button hlmBtn variant="ghost" (click)="ref.close(false)">
        {{ 'common.cancel' | transloco }}
      </button>
      <button hlmBtn data-testid="confirm-sync" (click)="ref.close(true)">
        {{ (enable ? 'sync.enable' : 'sync.disable') | transloco }}
      </button>
    </div>
  `,
})
export class SyncDialogComponent {
  protected readonly ref = inject<BrnDialogRef<boolean>>(BrnDialogRef);
  private readonly ctx = injectBrnDialogContext<{ name: string; enable: boolean }>();
  protected readonly name = this.ctx.name;
  protected readonly enable = this.ctx.enable;
}
```

Add `SyncDialogComponent` to nothing else — dialogs are opened via `HlmDialogService`, not the imports array (match how `RotateCredentialsDialogComponent` is used).

- [ ] **Step 3: Add the template section and handler**

In the cluster-detail template, insert between the Configuration tables and the "Client access rules" section (before line 636):

```html
        <!-- Synchronous replication -->
        <div class="mt-6 flex items-center justify-between">
          <h2 class="font-medium">{{ 'sync.title' | transloco }}</h2>
          <button
            hlmBtn
            variant="outline"
            size="sm"
            data-testid="toggle-sync"
            [disabled]="busy()"
            (click)="openSync(d)"
          >
            <ng-icon name="lucideRefreshCw" size="14" />
            {{ (d.synchronous ? 'sync.disable' : 'sync.enable') | transloco }}
          </button>
        </div>

        @if (d.sync_degraded) {
          <div
            class="border-warning mt-2 flex items-center gap-2 rounded-lg border p-3 text-sm"
            data-testid="sync-degraded-banner"
          >
            <ng-icon name="lucideTriangleAlert" size="18" class="text-warning" />
            <span>{{ 'sync.degraded' | transloco }}</span>
          </div>
        }

        <p class="text-muted-foreground mt-2 text-sm" data-testid="sync-state">
          {{
            d.synchronous
              ? ('sync.on' | transloco: { count: syncStandbys(d) ?? '?' })
              : ('sync.off' | transloco)
          }}
        </p>
```

(`lucideRefreshCw` and `lucideTriangleAlert` are already imported and provided.)

In the component class, next to `openHba` (line 868):

```typescript
  /** The primary's connected-sync-standby count, if it is reporting one. */
  protected syncStandbys(d: ClusterDetail): number | null {
    const p = d.instances?.find((i) => i.pod === d.primary);
    return p?.sync_standby_count ?? null;
  }

  async openSync(d: ClusterDetail): Promise<void> {
    const enable = !d.synchronous;
    const confirmed = await firstValueFrom(
      this.dialog.open<boolean>(SyncDialogComponent, {
        context: { name: this.name(), enable },
      }).closed$,
    );
    if (!confirmed) {
      return;
    }
    await this.run(() =>
      this.api.invoke(patchCluster, { name: this.name(), body: { synchronous: enable } }),
    );
  }
```

- [ ] **Step 4: Lint and build**

Run: `make ui-lint ui-build`
Expected: clean. (If `ui-test` covers this page, run `make ui-test` too and keep it green.)

- [ ] **Step 5: Commit**

```bash
git add ui/src/app/pages/clusters/cluster-detail.component.ts ui/public/i18n/en.json
git commit -m "feat(ui): synchronous replication toggle and degraded banner"
```

---

### Task 7: e2e, docs, final verification

**Files:**
- Modify: `scripts/config-e2e.sh`
- Modify: `docs/user-guide.md`
- Modify: `docs/superpowers/specs/2026-08-02-sync-replication-design.md` (one-line metric-name amendment)

**Interfaces:**
- Consumes: everything above, deployed on the dev stand (`make stand-up stand-deploy` if the stand is not running).

- [ ] **Step 1: Append the sync section to config-e2e.sh**

Insert before the final `echo "==> cleanup"` block. The cluster `cfg` has `replicas: 2`; an earlier step performs a rolling restart, after which the primary may be either pod (the orchestrator can depose a primary after a roll) — discover it from the API instead of assuming `cfg-0`:

```bash
echo "==> synchronous replication: validation guard rails"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' \
  -d '{"synchronous":true,"parameters":{"synchronous_commit":"off"}}')
[ "$code" = "422" ] || { echo "FAIL: sync + synchronous_commit=off returned $code"; exit 1; }
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' \
  -d '{"parameters":{"synchronous_standby_names":"ANY 1 (x)"}}')
[ "$code" = "422" ] || { echo "FAIL: reserved synchronous_standby_names returned $code"; exit 1; }
echo "OK rejected as 422"

echo "==> enabling synchronous replication"
prim=$(field "['primary']")
stand=$([ "$prim" = "cfg-0" ] && echo cfg-1 || echo cfg-0)
before0=$(restarts_of cfg-0); before1=$(restarts_of cfg-1)
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' -d '{"synchronous":true}')
[ "$code" = "200" ] || { echo "FAIL: enable sync returned $code"; exit 1; }
for _ in $(seq 1 30); do
  names=$(psql_on "$prim" "SHOW synchronous_standby_names")
  [ "$names" = "ANY 1 (\"$stand\")" ] && break
  sleep 2
done
[ "$names" = "ANY 1 (\"$stand\")" ] \
  || { echo "FAIL: synchronous_standby_names='$names' on $prim"; exit 1; }
state=$(psql_on "$prim" "SELECT sync_state FROM pg_stat_replication LIMIT 1")
[ "$state" = "quorum" ] || { echo "FAIL: sync_state='$state' (want quorum)"; exit 1; }
# A commit must complete while the quorum standby is connected.
psql_on "$prim" "CREATE TABLE IF NOT EXISTS sync_e2e(x int)" >/dev/null
[ "$(psql_on "$prim" "INSERT INTO sync_e2e VALUES (1) RETURNING x")" = "1" ] \
  || { echo "FAIL: sync write did not commit"; exit 1; }
[ "$(field "['synchronous']")" = "True" ] || { echo "FAIL: detail synchronous not true"; exit 1; }
[ "$(field "['sync_degraded']")" = "False" ] || { echo "FAIL: sync_degraded set with a healthy standby"; exit 1; }
[ "$(restarts_of cfg-0)" = "$before0" ] && [ "$(restarts_of cfg-1)" = "$before1" ] \
  || { echo "FAIL: instances restarted for a reload-safe change"; exit 1; }
echo "OK quorum sync active, write committed, no restarts"

echo "==> scaling a sync cluster to 1 replica is rejected"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' -d '{"replicas":1}')
[ "$code" = "422" ] || { echo "FAIL: scale-to-1 with sync on returned $code"; exit 1; }
echo "OK rejected as 422"

echo "==> disabling synchronous replication"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$API/api/v1/clusters/cfg" \
  -H 'content-type: application/json' -d '{"synchronous":false}')
[ "$code" = "200" ] || { echo "FAIL: disable sync returned $code"; exit 1; }
for _ in $(seq 1 30); do
  names=$(psql_on "$prim" "SHOW synchronous_standby_names")
  [ "$names" = "" ] && break
  sleep 2
done
[ "$names" = "" ] || { echo "FAIL: synchronous_standby_names not cleared ('$names')"; exit 1; }
echo "OK cleared"
```

- [ ] **Step 2: Run the e2e**

Run: `make config-e2e`
Expected: every section prints OK including the new sync sections; script exits 0. (Needs the minikube stand: `make stand-up` first if absent.)

- [ ] **Step 3: Update the docs**

`docs/user-guide.md`:

1. Add a "Synchronous replication" section next to the failover material, covering: what `synchronous: true` guarantees (every commit confirmed by at least one standby, quorum `ANY 1` over all standbys); strict semantics (no standby connected → writes block; the cluster page shows a warning banner and `pglord_sync_degraded` fires); requires `replicas >= 2`, and scaling to 1 is rejected while on; the change is reload-only (no restart, no roll); the `replicas: 2` caveat (a rolling restart of the only standby briefly blocks writes — three or more replicas keep quorum through a roll); interaction with `synchronous_commit` tuning (`off`/`local` rejected while on; `remote_apply` available for read-your-writes on standbys); failover implication (with the flag on, promotion of the most advanced standby loses no committed transactions unless the primary and the confirming standby fail together).
2. Update line 171 ("this is asynchronous") to note it applies to asynchronous clusters and link the new section.
3. Delete line 549: "- Asynchronous replication only (synchronous is not exposed yet)."

`docs/superpowers/specs/2026-08-02-sync-replication-design.md`: change the agent metric name `pglord_agent_sync_standbys` to `pglord_pg_sync_standbys` (the implementation follows the agent's existing `pglord_pg_*` naming).

- [ ] **Step 4: Full verification**

Run: `make lint test openapi-check ui-client-check ui-lint`
Expected: all clean.

- [ ] **Step 5: Commit**

```bash
git add scripts/config-e2e.sh docs/user-guide.md \
        docs/superpowers/specs/2026-08-02-sync-replication-design.md
git commit -m "test(e2e): synchronous replication flow; docs: sync replication guide"
```
