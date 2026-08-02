# Hot-standby GUC Clamp Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Standbys clamp `max_connections`, `max_worker_processes`, `max_prepared_transactions`, `max_locks_per_transaction` to at least the primary's live values so rebuilds never crashloop on a lowered restart-required GUC.

**Architecture:** A new pure `pglord-core::standby` module parses `pg_controldata` output and renders a fourth managed include file, `postgresql.pglord-standby.conf`, placed after the user parameter include so it overrides it. The agent writes it before first postgres start (control file from basebackup carries the primary's values) and converges it in the 5s watch loop.

**Tech Stack:** Rust (edition 2024, clippy denies `unwrap_used`), existing agent conf-include plumbing, bash e2e against the minikube stand.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-08-02-hot-standby-guc-clamp-design.md`.
- Four clamped GUCs only; `max_wal_senders` is pinned to 10 by `postgresql_conf_extra` and stays out.
- PostgreSQL defaults when the spec omits a GUC: `max_connections` 100, `max_worker_processes` 8, `max_prepared_transactions` 0, `max_locks_per_transaction` 64.
- No API/UI/contract changes.
- `make lint` and `make test` green before every commit.

---

### Task 1: `pglord-core::standby` — control-file parse + clamp render

**Files:**
- Create: `crates/pglord-core/src/standby.rs`
- Modify: `crates/pglord-core/src/lib.rs` (add `pub mod standby;`)

**Interfaces:**
- Produces: `pub struct ControlFileValues { pub max_connections: u32, pub max_worker_processes: u32, pub max_prepared_transactions: u32, pub max_locks_per_transaction: u32 }`; `pub fn parse_controldata(output: &str) -> Option<ControlFileValues>`; `pub fn render(params: &BTreeMap<String, String>, live: Option<&ControlFileValues>) -> String`.

- [ ] **Step 1: Write the module with failing-first tests**

```rust
//! Hot-standby GUC floor. PostgreSQL refuses standby recovery when
//! max_connections-class settings are below the primary's *live* values
//! (which the standby's own control file tracks via WAL). This module
//! renders an include that raises them just enough, so a rebuilt standby
//! can always start; the spec keeps recording the user's intent.

use std::collections::BTreeMap;

/// The primary's live values as recorded in the standby's control file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlFileValues {
    pub max_connections: u32,
    pub max_worker_processes: u32,
    pub max_prepared_transactions: u32,
    pub max_locks_per_transaction: u32,
}

/// Parse `pg_controldata` output. None when any of the four lines is
/// missing or malformed — the caller then skips clamping entirely.
pub fn parse_controldata(output: &str) -> Option<ControlFileValues> {
    let field = |label: &str| -> Option<u32> {
        output.lines().find_map(|l| {
            let rest = l.strip_prefix(label)?;
            rest.trim_start_matches(':').trim().parse().ok()
        })
    };
    Some(ControlFileValues {
        max_connections: field("max_connections setting")?,
        max_worker_processes: field("max_worker_processes setting")?,
        max_prepared_transactions: field("max_prepared_xacts setting")?,
        max_locks_per_transaction: field("max_locks_per_xact setting")?,
    })
}

const HEADER: &str =
    "# Managed by pglord — hot-standby floor from the primary's live settings.\n";

/// Render the clamp include: for each GUC, emit the live value only when it
/// exceeds what the spec (or the PostgreSQL default) would set. `live: None`
/// (primary, or control file unreadable) renders the header alone, which the
/// agent's content compare treats as "nothing to do".
pub fn render(params: &BTreeMap<String, String>, live: Option<&ControlFileValues>) -> String {
    let mut out = String::from(HEADER);
    let Some(live) = live else {
        return out;
    };
    let spec_or = |name: &str, default: u32| -> u32 {
        params
            .get(name)
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(default)
    };
    let gucs: [(&str, u32, u32); 4] = [
        ("max_connections", spec_or("max_connections", 100), live.max_connections),
        ("max_worker_processes", spec_or("max_worker_processes", 8), live.max_worker_processes),
        ("max_prepared_transactions", spec_or("max_prepared_transactions", 0), live.max_prepared_transactions),
        ("max_locks_per_transaction", spec_or("max_locks_per_transaction", 64), live.max_locks_per_transaction),
    ];
    for (name, effective, live_value) in gucs {
        if live_value > effective {
            out.push_str(&format!("{name} = {live_value}\n"));
        }
    }
    out
}
```

Tests in the same file (`#[cfg(test)] mod tests`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const CONTROLDATA: &str = "\
pg_control version number:            1300
Database cluster state:               in archive recovery
wal_level setting:                    replica
wal_log_hints setting:                on
max_connections setting:              150
max_worker_processes setting:         8
max_wal_senders setting:              10
max_prepared_xacts setting:           0
max_locks_per_xact setting:           64
";

    #[test]
    fn parses_real_controldata_output() {
        let v = parse_controldata(CONTROLDATA).expect("parse");
        assert_eq!(v.max_connections, 150);
        assert_eq!(v.max_worker_processes, 8);
        assert_eq!(v.max_prepared_transactions, 0);
        assert_eq!(v.max_locks_per_transaction, 64);
    }

    #[test]
    fn missing_line_means_no_clamp() {
        assert!(parse_controldata("Database cluster state: in production\n").is_none());
    }

    #[test]
    fn clamps_only_gucs_where_live_exceeds_spec_or_default() {
        let live = parse_controldata(CONTROLDATA).expect("parse");
        // Spec cleared: default 100 < live 150 → clamp; the other three equal.
        let out = render(&BTreeMap::new(), Some(&live));
        assert!(out.contains("max_connections = 150\n"));
        assert!(!out.contains("max_worker_processes"));
        assert!(!out.contains("max_prepared_transactions"));
        assert!(!out.contains("max_locks_per_transaction"));
    }

    #[test]
    fn spec_at_or_above_live_needs_no_clamp() {
        let params = BTreeMap::from([("max_connections".to_string(), "200".to_string())]);
        let live = parse_controldata(CONTROLDATA).expect("parse");
        assert_eq!(render(&params, Some(&live)), super::HEADER);
    }

    #[test]
    fn unparseable_spec_value_falls_back_to_default() {
        // "not-a-number" can't be compared, so the default (100) is the
        // effective floor and live 150 must win.
        let params = BTreeMap::from([("max_connections".to_string(), "oops".to_string())]);
        let live = parse_controldata(CONTROLDATA).expect("parse");
        assert!(render(&params, Some(&live)).contains("max_connections = 150\n"));
    }

    #[test]
    fn primary_renders_header_only() {
        assert_eq!(render(&BTreeMap::new(), None), HEADER);
    }
}
```

- [ ] **Step 2: Register the module and run the tests**

Add `pub mod standby;` to `crates/pglord-core/src/lib.rs` (alphabetical order with the existing modules). Run: `cargo test -p pglord-core standby` — expect all 6 pass.

- [ ] **Step 3: Commit**

```bash
git add crates/pglord-core/src/standby.rs crates/pglord-core/src/lib.rs
git commit -m "feat(core): hot-standby GUC floor render from control-file values"
```

### Task 2: Agent wiring — include, controldata runner, first-start and watch-loop writes

**Files:**
- Create: `crates/pglord-agent/src/controldata.rs`
- Modify: `crates/pglord-agent/src/lib.rs` (add `pub mod controldata;`)
- Modify: `crates/pglord-agent/src/params.rs` (STANDBY_FILE const + `write_standby_if_changed` + test)
- Modify: `crates/pglord-agent/src/bootstrap.rs` (`postgresql_conf_extra`: 4th `include_if_exists` + test assertion)
- Modify: `crates/pglord-agent/src/main.rs` (write before `spawn_postgres`)
- Modify: `crates/pglord-agent/src/record.rs` (`apply_parameters` gains `in_recovery: bool`, renders the clamp in the shared changed→reload flow)

**Interfaces:**
- Consumes: `pglord_core::standby::{render, parse_controldata, ControlFileValues}` from Task 1.
- Produces: `pub async fn live_values(pgdata: &Path) -> Option<ControlFileValues>` in `pglord_agent::controldata`; `pub const STANDBY_FILE: &str = "postgresql.pglord-standby.conf"` and `pub fn write_standby_if_changed(pgdata: &Path, rendered: &str) -> std::io::Result<bool>` in `params`.

- [ ] **Step 1: controldata runner**

`crates/pglord-agent/src/controldata.rs`:

```rust
//! Thin wrapper over `pg_controldata`: the pure parsing lives in
//! pglord_core::standby so it can be unit-tested without a PGDATA.

use std::path::Path;

use pglord_core::standby::ControlFileValues;
use tokio::process::Command;

/// The primary's live hot-standby settings as tracked by this PGDATA's
/// control file. None (with a warning) when pg_controldata is missing or
/// its output is unreadable — the caller then applies no clamp, which is
/// exactly the pre-clamp behavior.
pub async fn live_values(pgdata: &Path) -> Option<ControlFileValues> {
    let out = match Command::new("pg_controldata").arg("-D").arg(pgdata).output().await {
        Ok(out) if out.status.success() => out.stdout,
        Ok(out) => {
            tracing::warn!(status = %out.status, "pg_controldata failed; skipping hot-standby clamp");
            return None;
        }
        Err(err) => {
            tracing::warn!(%err, "pg_controldata not runnable; skipping hot-standby clamp");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&out);
    let parsed = pglord_core::standby::parse_controldata(&text);
    if parsed.is_none() {
        tracing::warn!("pg_controldata output unparseable; skipping hot-standby clamp");
    }
    parsed
}
```

Register `pub mod controldata;` in `crates/pglord-agent/src/lib.rs`.

- [ ] **Step 2: params.rs — file const, writer, idempotency test**

After the `SYNC_FILE` const:

```rust
/// Hot-standby GUC floor (see pglord_core::standby::render); included after
/// the user parameter file so the clamp always wins.
pub const STANDBY_FILE: &str = "postgresql.pglord-standby.conf";
```

Next to `write_sync_if_changed`:

```rust
/// Write the hot-standby floor file when it differs; returns whether it changed.
pub fn write_standby_if_changed(pgdata: &Path, rendered: &str) -> std::io::Result<bool> {
    write_file_if_changed(&pgdata.join(STANDBY_FILE), rendered)
}
```

Test (same pattern as `sync_write_is_idempotent_and_targets_the_sync_file`):

```rust
#[test]
fn standby_write_is_idempotent_and_targets_the_standby_file() {
    let dir = tempdir("standby");
    let live = pglord_core::standby::parse_controldata("max_connections setting: 150\nmax_worker_processes setting: 8\nmax_prepared_xacts setting: 0\nmax_locks_per_xact setting: 64\n").expect("parse");
    let clamped = pglord_core::standby::render(&Default::default(), Some(&live));
    assert!(write_standby_if_changed(&dir, &clamped).expect("write"));
    // Unchanged content must not report a change: a report triggers a
    // reload, and this runs on every 5s poll.
    assert!(!write_standby_if_changed(&dir, &clamped).expect("write"));
    assert_eq!(std::fs::read_to_string(dir.join(STANDBY_FILE)).expect("read"), clamped);
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 3: bootstrap.rs — fourth include**

In `postgresql_conf_extra`, add a fourth `include_if_exists = '{}'` line after the sync one, fed with `crate::params::STANDBY_FILE`. Extend the existing conf test with `assert!(conf.contains("include_if_exists = 'postgresql.pglord-standby.conf'"));` and assert it appears *after* the `PARAMS_FILE` include (compare `conf.find(...)` offsets) so the clamp can override user parameters.

- [ ] **Step 4: main.rs — write before first start**

Directly after the sync-conf write, before `spawn_postgres`:

```rust
// A rebuilt standby carries the primary's control file, so the clamp is
// correct exactly when postgres first validates hot-standby settings.
let empty = std::collections::BTreeMap::new();
let live = if cfg.role == Role::Standby {
    pglord_agent::controldata::live_values(&cfg.pgdata).await
} else {
    None
};
let standby_conf = pglord_core::standby::render(
    cluster_spec.as_ref().map(|s| &s.parameters).unwrap_or(&empty),
    live.as_ref(),
);
if let Err(err) = pglord_agent::params::write_standby_if_changed(&cfg.pgdata, &standby_conf) {
    tracing::warn!(%err, "failed to write hot-standby floor settings");
}
```

- [ ] **Step 5: record.rs — converge in the watch loop**

Change the call in `follow()` to `apply_parameters(&reader, &cfg, &me, st.in_recovery == Some(true)).await;` and the signature to `async fn apply_parameters(reader: &RecordReader, cfg: &Config, me: &str, in_recovery: bool)`. Add before the `if spec.backup.enabled` block:

```rust
// Standbys clamp restart-required hot-standby GUCs to the primary's live
// values (control file); a primary renders the header-only file, clearing
// any clamp left over from before its promotion.
let live = if in_recovery {
    crate::controldata::live_values(&cfg.pgdata).await
} else {
    None
};
let standby = pglord_core::standby::render(&spec.parameters, live.as_ref());
match crate::params::write_standby_if_changed(&cfg.pgdata, &standby) {
    Ok(true) => changed = true,
    Ok(false) => {}
    Err(err) => tracing::warn!(%err, "failed to write hot-standby floor settings"),
}
```

- [ ] **Step 6: Verify and commit**

Run: `make lint && cargo test -p pglord-agent && cargo test -p pglord-core` — all green.

```bash
git add crates/pglord-agent crates/pglord-core
git commit -m "feat(agent): clamp hot-standby GUCs to the primary's live values"
```

### Task 3: E2e proof + docs

**Files:**
- Modify: `scripts/config-e2e.sh` (new final section)
- Modify: `docs/user-guide.md` (note in the configuration section)

**Interfaces:**
- Consumes: the stand helpers already in `config-e2e.sh` (`api`, `wait_ready`, kubectl invocations, DIAG dump).

- [ ] **Step 1: Add the rebuild-under-downgrade section to `scripts/config-e2e.sh`**

Placed at the END (after the rolling-restart sections — this section deliberately performs the downgrade that used to brick rebuilds). Shape, adapted to the script's existing helpers and style:

```bash
echo "--- hot-standby clamp: rebuild survives a lowered restart GUC"
# Raise max_connections and make it live everywhere.
api PATCH "/clusters/$CLUSTER" '{"parameters":{"max_connections":"150"}}' >/dev/null
api POST "/clusters/$CLUSTER/restart" '' >/dev/null
wait_ready   # existing helper: all instances ready, no pending roll
# Clear it: reload-only, so the primary keeps 150 live with pending_restart.
api PATCH "/clusters/$CLUSTER" '{"parameters":{}}' >/dev/null
sleep 8
# Force a standby rebuild: drop its PVC and pod.
PRIMARY=$(api GET "/clusters/$CLUSTER" | jq -r .primary)
STAND=$(api GET "/clusters/$CLUSTER" | jq -r --arg p "$PRIMARY" '.instances[] | select(.pod != $p) | .pod' | head -1)
minikube -p pglord kubectl -- -n "$NS" delete pvc "data-$STAND" --wait=false
minikube -p pglord kubectl -- -n "$NS" delete pod "$STAND"
wait_ready
echo "OK rebuilt standby healthy while primary still runs the higher value"
# A full roll converges everything back to the spec value.
api POST "/clusters/$CLUSTER/restart" '' >/dev/null
wait_ready
```

Adjust helper names/JSON paths to what the script actually defines (read it first); keep the DIAG-on-failure trap covering the new section.

- [ ] **Step 2: Run the suite on the stand**

Run: `make config-e2e` — expect `config e2e passed` including the new `OK rebuilt standby healthy...` line. (Requires `make stand-up stand-deploy` beforehand if the stand is cold; rebuild the image via `make stand-image` so the new agent binary is on the stand.)

- [ ] **Step 3: User guide note**

In the configuration-management section of `docs/user-guide.md`, add a short paragraph: lowering `max_connections`, `max_worker_processes`, `max_prepared_transactions`, or `max_locks_per_transaction` takes effect on each instance at its next restart; until the primary restarts, standbys keep running with the primary's higher live value (pglord raises it automatically) so replicas can always rebuild.

- [ ] **Step 4: Commit and update memory**

```bash
git add scripts/config-e2e.sh docs/user-guide.md
git commit -m "test(e2e): standby rebuild under lowered restart GUC; docs: clamp note"
```

Update `~/.claude/projects/-home-bigunmd-github-pglord/memory/standby-rebuild-fails-on-lowered-restart-guc.md`: the clamp is implemented; the e2e ordering constraint is now only historical context.
