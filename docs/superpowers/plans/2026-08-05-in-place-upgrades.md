# In-Place Major Upgrades (pg_upgrade) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `mode: in_place` to the existing upgrade surface, converting a cluster's data directory with `pg_upgrade --link` instead of building a second copy — minutes of downtime and almost no extra disk, gated on a backup.

**Architecture:** `POST /upgrade` gains a `mode` field. In-place records `UpgradeState { mode: InPlace, phase: Upgrading, from_major }`, patches the single StatefulSet to the target image plus an initContainer that copies the old PostgreSQL tree into an emptyDir mounted **at its original absolute paths**, then rolls the pods. Each agent boots, sees an older PGDATA plus `PGLORD_AGENT_UPGRADE_IN_PLACE`, and either runs `pg_upgrade --link` (primary) or wipes and re-clones (standby). The server watches for every instance ready on the target major, then persists the image and clears the state.

**Tech Stack:** Rust (axum + utoipa, kube-rs, tokio-postgres), `pg_upgrade`, Angular + spartan-ng hlm + Transloco, minikube e2e shell scripts.

**Spec:** `docs/superpowers/specs/2026-08-05-in-place-upgrades-design.md` (its spike is already done — the approach is proven on the stand's images).

## Global Constraints

- Never hand-edit `spec/openapi.yaml`, `ui/src/app/api/**`, or `deploy/crds/*.yaml` — regenerate with `make openapi ui-client crd` and commit alongside the code.
- Rust edition 2024; clippy denies warnings and `unwrap_used` — `expect` only in tests.
- All user-visible UI strings go through Transloco (`ui/public/i18n/en.json`).
- `UpgradeMode` **must** default to `BlueGreen` under `#[serde(default)]`: existing stored `UpgradeState` annotations and existing API callers carry no `mode`, and both must keep working unchanged.
- The old tree mounts at `/usr/lib/postgresql/<old>` and `/usr/share/postgresql/<old>` — **not** an arbitrary path. Debian compiles the share path in absolutely; any other location breaks `initdb` with `could not open version file`.
- `$PGDATA.new` must be a sibling of `$PGDATA` on the same volume — `--link` uses hard links, which cannot cross filesystems.
- Per task: `cargo test -p <crate>` for the touched crate, then `make lint` before each commit.
- Blue-green behavior must not change. Its e2e (`make upgrade-e2e`) is the regression gate and must still pass.

---

### Task 1: Core — upgrade mode, phase and old-major parsing

**Files:**
- Modify: `crates/pglord-core/src/upgrade.rs`
- Regenerate: `deploy/crds/postgresclusters.pglord.dev.yaml` (`make crd`)

**Interfaces:**
- Produces: `UpgradeMode { BlueGreen, InPlace }` (serde `snake_case`, `Default` = `BlueGreen`); `UpgradePhase::Upgrading`; `UpgradeState.mode: UpgradeMode` (`#[serde(default)]`) and `UpgradeState.from_major: Option<u32>`; `pub fn major_from_image(image: &str) -> Option<u32>`.

- [ ] **Step 1: Write the failing tests** (append to `upgrade.rs`'s `mod tests`):

```rust
#[test]
fn mode_defaults_to_blue_green_for_stored_state() {
    // State written before in-place existed carries no mode; it must keep
    // parsing, as blue-green, or every in-flight upgrade breaks on deploy.
    let st: UpgradeState = serde_json::from_str(
        r#"{"target_image":"a:18","next_generation":"g2","phase":"migrating"}"#,
    )
    .expect("parse");
    assert_eq!(st.mode, UpgradeMode::BlueGreen);
    assert_eq!(st.from_major, None);
}

#[test]
fn in_place_state_roundtrips() {
    let st = UpgradeState {
        target_image: "pglord-agent:17".into(),
        next_generation: "g2".into(),
        phase: UpgradePhase::Upgrading,
        previous_primary: None,
        mode: UpgradeMode::InPlace,
        from_major: Some(16),
    };
    let json = serde_json::to_string(&st).expect("serialize");
    assert!(json.contains("\"in_place\""), "{json}");
    assert!(json.contains("\"upgrading\""), "{json}");
    assert_eq!(serde_json::from_str::<UpgradeState>(&json).expect("parse"), st);
}

#[test]
fn major_parses_from_conventional_image_tags() {
    assert_eq!(major_from_image("pglord-agent:17"), Some(17));
    assert_eq!(major_from_image("ghcr.io/o/pglord-agent:18-timescaledb"), Some(18));
    assert_eq!(major_from_image("ghcr.io/o/pglord-agent:v1.2.3-pg16"), Some(16));
    // Tags are arbitrary: an unreadable one is not an error, it just means
    // the running server's reported version is the only authority.
    assert_eq!(major_from_image("pglord-agent:dev"), None);
    assert_eq!(major_from_image("pglord-agent"), None);
    assert_eq!(major_from_image("registry:5000/pglord-agent"), None);
}
```

- [ ] **Step 2: Run to verify failure**: `cargo test -p pglord-core upgrade::` — expect compile errors (`UpgradeMode` undefined).

- [ ] **Step 3: Implement** in `upgrade.rs`:

```rust
/// How a major upgrade is performed. Absent in stored state written before
/// in-place existed, which must keep meaning blue-green.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[cfg_attr(feature = "crd", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum UpgradeMode {
    /// Build a second instance set and replicate into it. No downtime, needs
    /// a full second copy of the cluster.
    #[default]
    BlueGreen,
    /// Convert this cluster's data directory with pg_upgrade --link. Minutes
    /// of downtime, almost no extra disk, recoverable only from a backup.
    InPlace,
}
```

Add `Upgrading` to `UpgradePhase`:

```rust
    /// In-place: the instances are converting their data directory.
    Upgrading,
```

Add to `UpgradeState`:

```rust
    #[serde(default)]
    pub mode: UpgradeMode,
    /// The major being upgraded from, resolved once at start so the render
    /// stays stable across reconciles. In-place only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_major: Option<u32>,
```

And the tag parser:

```rust
/// The PostgreSQL major an image tag advertises, when it says so.
///
/// Tags are arbitrary strings, so this is a convenience, never an authority —
/// the running server's own `server_version_num` decides. Recognises a bare
/// major ("…:17"), a leading major with a suffix ("…:18-timescaledb") and a
/// trailing "-pgNN" ("…:v1.2.3-pg16").
pub fn major_from_image(image: &str) -> Option<u32> {
    // Strip any registry host:port before looking for the tag separator.
    let tag = image.rsplit_once(':').and_then(|(host, tag)| {
        (!tag.contains('/')).then_some(tag).filter(|_| !host.is_empty())
    })?;
    if let Some((_, pg)) = tag.rsplit_once("-pg") {
        return pg.parse().ok();
    }
    let head: String = tag.chars().take_while(char::is_ascii_digit).collect();
    head.parse().ok()
}
```

- [ ] **Step 4: Run**: `cargo test -p pglord-core` — expect PASS. Fix the existing `UpgradeState` literals the compiler flags in `render.rs` and `api/upgrade.rs` tests by adding `mode: UpgradeMode::BlueGreen, from_major: None`.
- [ ] **Step 5: Regenerate the CRD**: `make crd`.
- [ ] **Step 6: `make lint` and commit**:

```bash
git add crates/pglord-core deploy/crds
git commit -m "feat(core): upgrade mode, in-place phase and image-tag major parsing"
```

---

### Task 2: Core — render the in-place upgrade pod

**Files:**
- Modify: `crates/pglord-core/src/render.rs`

**Interfaces:**
- Consumes: `UpgradeState { mode, from_major }` (Task 1).
- Produces: `render_statefulset` gains in-place awareness via a new parameter-free path — the *spec's* image is replaced and the initContainer added when the caller passes an in-place upgrade. Exact signature: `pub fn render_statefulset_for_upgrade(spec: &ClusterSpec, ns: &str, owner: &OwnerRef, upgrade: Option<&crate::upgrade::UpgradeState>) -> StatefulSet`, with `render_statefulset(spec, ns, owner)` kept as `render_statefulset_for_upgrade(spec, ns, owner, None)` so every existing caller is untouched.
- Produces constants: `OLD_BIN_VOLUME = "pglord-oldbin"`, `pub fn old_lib_path(major: u32) -> String` → `/usr/lib/postgresql/{major}`, `pub fn old_share_path(major: u32) -> String` → `/usr/share/postgresql/{major}`.

- [ ] **Step 1: Write the failing tests** (append to `render.rs`'s `mod tests`):

```rust
fn in_place_state() -> crate::upgrade::UpgradeState {
    crate::upgrade::UpgradeState {
        target_image: "pglord-agent:17".into(),
        next_generation: "g2".into(),
        phase: crate::upgrade::UpgradePhase::Upgrading,
        previous_primary: None,
        mode: crate::upgrade::UpgradeMode::InPlace,
        from_major: Some(16),
    }
}

#[test]
fn in_place_render_adds_the_old_tree_at_its_original_paths() {
    let (spec, owner) = demo();
    let sts = render_statefulset_for_upgrade(&spec, "pglord", &owner, Some(&in_place_state()));
    // Same instance set: no generation suffix, no second StatefulSet.
    assert_eq!(sts.metadata.name.as_deref(), Some("demo"));
    let pod = sts.spec.expect("spec").template.spec.expect("pod");

    let init = pod.init_containers.expect("init containers");
    assert_eq!(init.len(), 1);
    // The initContainer runs the CURRENT image — that is where PG 16 lives.
    assert_eq!(init[0].image.as_deref(), Some("pglord-agent:dev"));

    let main = &pod.containers[0];
    assert_eq!(main.image.as_deref(), Some("pglord-agent:17"));
    let mounts = main.volume_mounts.clone().expect("mounts");
    // Load-bearing: Debian compiles the share path in absolutely, so the old
    // tree must appear exactly where it was installed or initdb cannot run.
    let lib = mounts.iter().find(|m| m.mount_path == "/usr/lib/postgresql/16").expect("lib mount");
    let share = mounts.iter().find(|m| m.mount_path == "/usr/share/postgresql/16").expect("share mount");
    assert_eq!(lib.name, OLD_BIN_VOLUME);
    assert_eq!(lib.sub_path.as_deref(), Some("lib"));
    assert_eq!(share.sub_path.as_deref(), Some("share"));

    let env = main.env.clone().expect("env");
    let marker = env.iter().find(|e| e.name == "PGLORD_AGENT_UPGRADE_IN_PLACE").expect("var");
    assert_eq!(marker.value.as_deref(), Some("16"));
}

#[test]
fn a_blue_green_or_absent_upgrade_renders_exactly_as_before() {
    let (spec, owner) = demo();
    let plain = render_statefulset(&spec, "pglord", &owner);
    assert_eq!(
        render_statefulset_for_upgrade(&spec, "pglord", &owner, None),
        plain
    );
    // Blue-green renders the second set through render_green_statefulset; the
    // main set must stay untouched while one is running.
    let bg = crate::upgrade::UpgradeState {
        mode: crate::upgrade::UpgradeMode::BlueGreen,
        from_major: None,
        ..in_place_state()
    };
    assert_eq!(
        render_statefulset_for_upgrade(&spec, "pglord", &owner, Some(&bg)),
        plain
    );
}

#[test]
fn in_place_without_a_known_old_major_renders_unchanged() {
    // from_major is resolved at start; if it is somehow absent there is
    // nothing to mount and guessing a path would break the pod.
    let (spec, owner) = demo();
    let st = crate::upgrade::UpgradeState { from_major: None, ..in_place_state() };
    assert_eq!(
        render_statefulset_for_upgrade(&spec, "pglord", &owner, Some(&st)),
        render_statefulset(&spec, "pglord", &owner)
    );
}
```

- [ ] **Step 2: Run**: `cargo test -p pglord-core render::` — FAIL (`render_statefulset_for_upgrade` undefined).

- [ ] **Step 3: Implement.** Add near `AGENT_HTTP_PORT`:

```rust
pub const OLD_BIN_VOLUME: &str = "pglord-oldbin";

/// Where a PostgreSQL major's binaries live in the agent images.
pub fn old_lib_path(major: u32) -> String {
    format!("/usr/lib/postgresql/{major}")
}

/// Where its share tree lives. The binaries hold this path as an absolute
/// compiled-in constant, so the old tree has to be mounted here and nowhere
/// else — see the in-place upgrade design.
pub fn old_share_path(major: u32) -> String {
    format!("/usr/share/postgresql/{major}")
}
```

Rename the body of the current `render_statefulset` to `render_statefulset_for_upgrade` with the extra parameter, and keep the old name as a thin wrapper:

```rust
pub fn render_statefulset(spec: &ClusterSpec, ns: &str, owner: &OwnerRef) -> StatefulSet {
    render_statefulset_for_upgrade(spec, ns, owner, None)
}
```

Inside `render_statefulset_for_upgrade`, immediately after the existing `let container = Container { … };` binding is built, add:

```rust
    // An in-place upgrade runs the target image over this cluster's own data
    // directory, with the previous major's tree mounted alongside so
    // pg_upgrade has both bindirs.
    let in_place = upgrade.filter(|u| u.mode == crate::upgrade::UpgradeMode::InPlace);
    let old_major = in_place.and_then(|u| u.from_major);
    let mut container = container;
    let mut init_containers = None;
    if let (Some(u), Some(major)) = (in_place, old_major) {
        container.image = Some(u.target_image.clone());
        if let Some(env) = container.env.as_mut() {
            env.push(EnvVar {
                name: "PGLORD_AGENT_UPGRADE_IN_PLACE".to_string(),
                value: Some(major.to_string()),
                ..Default::default()
            });
        }
        let old_mounts = vec![
            VolumeMount {
                name: OLD_BIN_VOLUME.to_string(),
                mount_path: old_lib_path(major),
                sub_path: Some("lib".to_string()),
                read_only: Some(true),
                ..Default::default()
            },
            VolumeMount {
                name: OLD_BIN_VOLUME.to_string(),
                mount_path: old_share_path(major),
                sub_path: Some("share".to_string()),
                read_only: Some(true),
                ..Default::default()
            },
        ];
        if let Some(mounts) = container.volume_mounts.as_mut() {
            mounts.extend(old_mounts.clone());
        }
        init_containers = Some(vec![Container {
            name: "pglord-oldbin".to_string(),
            // The cluster's CURRENT image: the only place the old major is.
            image: Some(spec.image.clone()),
            image_pull_policy: Some("IfNotPresent".to_string()),
            command: Some(vec!["sh".to_string(), "-c".to_string(), format!(
                "cp -a {}/. /oldbin/lib/ && cp -a {}/. /oldbin/share/",
                old_lib_path(major),
                old_share_path(major)
            )]),
            volume_mounts: Some(vec![VolumeMount {
                name: OLD_BIN_VOLUME.to_string(),
                mount_path: "/oldbin".to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        }]);
    }
```

In the `PodSpec` literal add `init_containers,` and extend the `volumes` block so the emptyDir appears when in-place is active:

```rust
                    volumes: {
                        let mut volumes = init_script_volumes(spec);
                        volumes.extend(tls_volumes(spec));
                        if old_major.is_some() && in_place.is_some() {
                            volumes.push(Volume {
                                name: OLD_BIN_VOLUME.to_string(),
                                empty_dir: Some(Default::default()),
                                ..Default::default()
                            });
                        }
                        (!volumes.is_empty()).then_some(volumes)
                    },
```

Add `EmptyDirVolumeSource` to the `k8s_openapi::api::core::v1` import list.

- [ ] **Step 4: Run**: `cargo test -p pglord-core` — PASS, including every pre-existing render test (they all go through the `None` path and must be byte-identical).
- [ ] **Step 5: `make lint` and commit**: `git add crates/pglord-core && git commit -m "feat(core): render the in-place upgrade pod with the old tree mounted"`

---

### Task 3: Agent — pg_upgrade argument builder and config

**Files:**
- Modify: `crates/pglord-agent/src/config.rs`
- Create: `crates/pglord-agent/src/pgupgrade.rs` (registered in `lib.rs` next to `upgrade`)

**Interfaces:**
- Produces: `Config.upgrade_in_place: String` (env `PGLORD_AGENT_UPGRADE_IN_PLACE`, empty = not an in-place upgrade), plus `Config::in_place_major(&self) -> Option<u32>`.
- Produces: `pgupgrade::plan(old_major: u32, new_bindir: &str, pgdata: &Path) -> UpgradePlan` where `pub struct UpgradePlan { pub new_datadir: PathBuf, pub old_bindir: String, pub new_bindir: String, pub check_args: Vec<String>, pub run_args: Vec<String> }`.

Keeping the argument construction pure is the point: the destructive command is then unit-tested without running it.

- [ ] **Step 1: Write the failing tests** (in `pgupgrade.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_new_datadir_is_a_sibling_of_pgdata() {
        // --link makes hard links, which cannot cross filesystems: the new
        // directory has to live on the same volume as the old one.
        let p = plan(16, "/usr/lib/postgresql/17/bin", Path::new("/var/lib/postgresql/data/pgdata"));
        assert_eq!(p.new_datadir, Path::new("/var/lib/postgresql/data/pgdata.new"));
        assert_eq!(p.new_datadir.parent(), Path::new("/var/lib/postgresql/data/pgdata").parent());
    }

    #[test]
    fn old_bindir_points_at_the_mounted_tree() {
        let p = plan(16, "/usr/lib/postgresql/17/bin", Path::new("/d/pgdata"));
        assert_eq!(p.old_bindir, "/usr/lib/postgresql/16/bin");
        assert_eq!(p.new_bindir, "/usr/lib/postgresql/17/bin");
    }

    #[test]
    fn check_args_are_the_run_args_plus_check() {
        let p = plan(16, "/nb", Path::new("/d/pgdata"));
        assert!(p.check_args.contains(&"--check".to_string()));
        assert!(!p.run_args.contains(&"--check".to_string()));
        // The real run links; anything else defeats the point of the mode.
        assert!(p.run_args.contains(&"--link".to_string()));
        assert!(p.check_args.contains(&"--link".to_string()));
        for args in [&p.check_args, &p.run_args] {
            assert!(args.contains(&"--old-datadir=/d/pgdata".to_string()), "{args:?}");
            assert!(args.contains(&"--new-datadir=/d/pgdata.new".to_string()), "{args:?}");
            assert!(args.contains(&"--old-bindir=/usr/lib/postgresql/16/bin".to_string()), "{args:?}");
            assert!(args.contains(&"--new-bindir=/nb".to_string()), "{args:?}");
        }
    }
}
```

- [ ] **Step 2: Run**: `cargo test -p pglord-agent pgupgrade` — FAIL.

- [ ] **Step 3: Implement `pgupgrade.rs`**:

```rust
//! In-place major upgrades: converting this instance's data directory with
//! pg_upgrade --link.
//!
//! Argument construction is pure and unit-tested, because the command it
//! builds rewrites a live database and there is no way back except a backup.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePlan {
    /// Freshly initdb'd directory pg_upgrade converts into. A sibling of
    /// PGDATA so --link's hard links stay on one filesystem.
    pub new_datadir: PathBuf,
    pub old_bindir: String,
    pub new_bindir: String,
    /// Non-destructive compatibility check.
    pub check_args: Vec<String>,
    /// The real conversion.
    pub run_args: Vec<String>,
}

pub fn plan(old_major: u32, new_bindir: &str, pgdata: &Path) -> UpgradePlan {
    let new_datadir = pgdata.with_file_name(format!(
        "{}.new",
        pgdata.file_name().unwrap_or_default().to_string_lossy()
    ));
    let old_bindir = format!("/usr/lib/postgresql/{old_major}/bin");
    let base = vec![
        format!("--old-datadir={}", pgdata.display()),
        format!("--new-datadir={}", new_datadir.display()),
        format!("--old-bindir={old_bindir}"),
        format!("--new-bindir={new_bindir}"),
        "--link".to_string(),
    ];
    let mut check_args = base.clone();
    check_args.push("--check".to_string());
    UpgradePlan {
        new_datadir,
        old_bindir,
        new_bindir: new_bindir.to_string(),
        check_args,
        run_args: base,
    }
}
```

Add to `config.rs`:

```rust
    /// In-place major upgrade: the major this instance's data directory is
    /// being converted *from*. Empty = ordinary boot.
    #[serde(default = "default_empty")]
    pub upgrade_in_place: String,
```

and on `impl Config`:

```rust
    pub fn in_place_major(&self) -> Option<u32> {
        self.upgrade_in_place.parse().ok()
    }
```

- [ ] **Step 4: Run**: `cargo test -p pglord-agent` — PASS. **Step 5:** `make lint` and commit: `git add crates/pglord-agent && git commit -m "feat(agent): pure pg_upgrade plan builder and in-place config"`

---

### Task 4: Agent — run the conversion at bootstrap

**Files:**
- Modify: `crates/pglord-agent/src/bootstrap.rs`, `crates/pglord-agent/src/pgupgrade.rs`

**Interfaces:**
- Consumes: `pgupgrade::plan`, `Config::in_place_major` (Task 3).
- Produces: `pgupgrade::run(cfg: &Config, old_major: u32) -> anyhow::Result<()>` (initdb the new dir, `--check`, real run, swap, re-apply managed conf); `bootstrap::in_place_pending(pgdata: &Path, cfg: &Config) -> Option<u32>` (pure decision, unit-tested).

- [ ] **Step 1: Write the failing test** (in `bootstrap.rs`'s `mod tests`):

```rust
#[test]
fn in_place_is_pending_only_for_an_older_pgdata_with_the_marker() {
    let dir = std::env::temp_dir().join("pglord-agent-test-inplace");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let cfg_with = |v: &str| -> Config {
        let mut c: Config =
            serde_json::from_value(serde_json::json!({ "role": "primary" })).expect("config");
        c.upgrade_in_place = v.to_string();
        c.pgdata = dir.clone();
        c
    };

    // No data directory yet: a fresh instance initdbs, it does not upgrade.
    assert_eq!(in_place_pending(&dir, &cfg_with("16")), None);

    std::fs::write(dir.join("PG_VERSION"), "16\n").expect("write");
    assert_eq!(in_place_pending(&dir, &cfg_with("16")), Some(16));
    // No marker: the ordinary major-mismatch refusal must still apply.
    assert_eq!(in_place_pending(&dir, &cfg_with("")), None);
    // Already converted (a pod restart after success): nothing to do.
    std::fs::write(dir.join("PG_VERSION"), "17\n").expect("write");
    assert_eq!(in_place_pending(&dir, &cfg_with("16")), None);

    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Run**: `cargo test -p pglord-agent in_place_is_pending` — FAIL.

- [ ] **Step 3: Implement.** In `bootstrap.rs`:

```rust
/// Whether this boot should convert the data directory in place, and from
/// which major. Some only when the marker env var names a major, PGDATA
/// exists, and its version still matches that major — so a pod that restarts
/// after a successful conversion simply starts normally.
pub fn in_place_pending(pgdata: &Path, cfg: &Config) -> Option<u32> {
    let want = cfg.in_place_major()?;
    let on_disk: u32 = std::fs::read_to_string(pgdata.join("PG_VERSION"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (on_disk == want).then_some(want)
}
```

Wire it into `ensure_bootstrapped`, **before** `check_major_version`:

```rust
    if is_bootstrapped(&cfg.pgdata) {
        if let Some(old_major) = in_place_pending(&cfg.pgdata, cfg) {
            return match cfg.role {
                // The primary converts; standbys cannot be pg_upgraded, so
                // they throw their data away and clone the converted primary
                // through the ordinary basebackup path.
                Role::Primary => crate::pgupgrade::run(cfg, old_major).await,
                _ => {
                    tracing::warn!(old_major, "in-place upgrade: discarding standby data to re-clone");
                    std::fs::remove_dir_all(&cfg.pgdata).context("wipe standby PGDATA")?;
                    std::fs::create_dir_all(&cfg.pgdata).context("recreate PGDATA")?;
                    bootstrap_standby(cfg).await
                }
            };
        }
        check_major_version(&cfg.pgdata).await?;
```

In `pgupgrade.rs` add the runner:

```rust
/// Convert PGDATA to the running binary's major. Destructive on success and
/// a no-op on failure: --check gates the real run, and the original directory
/// is only removed once pg_upgrade has reported success.
pub async fn run(cfg: &crate::config::Config, old_major: u32) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    let new_bindir = std::env::var("PGLORD_AGENT_NEW_BINDIR")
        .unwrap_or_else(|_| default_new_bindir());
    let p = plan(old_major, &new_bindir, &cfg.pgdata);

    // A retried pod must not inherit a half-built directory.
    if p.new_datadir.exists() {
        std::fs::remove_dir_all(&p.new_datadir).context("clear stale upgrade directory")?;
    }
    let mut initdb = tokio::process::Command::new(format!("{}/initdb", p.new_bindir));
    initdb.arg("-D").arg(&p.new_datadir).args(["-U", &cfg.superuser, "-A", "trust"]);
    if !initdb.status().await.context("spawn initdb")?.success() {
        bail!("initdb for the upgrade target failed");
    }

    for (label, args) in [("check", &p.check_args), ("upgrade", &p.run_args)] {
        let out = tokio::process::Command::new(format!("{}/pg_upgrade", p.new_bindir))
            .args(args)
            // pg_upgrade writes its logs into the working directory.
            .current_dir(cfg.pgdata.parent().unwrap_or(&cfg.pgdata))
            .output()
            .await
            .with_context(|| format!("spawn pg_upgrade --{label}"))?;
        if !out.status.success() {
            bail!(
                "pg_upgrade {label} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        tracing::info!(label, "pg_upgrade stage complete");
    }

    // --link means both directories name the same files; removing the old one
    // keeps the data under the new name.
    std::fs::remove_dir_all(&cfg.pgdata).context("remove the pre-upgrade directory")?;
    std::fs::rename(&p.new_datadir, &cfg.pgdata).context("promote the upgraded directory")?;
    tracing::warn!(old_major, "in-place upgrade complete");
    Ok(())
}

/// The bindir of the major this image ships, discovered from the binary's own
/// location so it never has to be configured.
fn default_new_bindir() -> String {
    std::env::var("PGLORD_AGENT_BINDIR").unwrap_or_else(|_| {
        std::process::Command::new("pg_config")
            .arg("--bindir")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "/usr/local/bin".to_string())
    })
}
```

The freshly initdb'd directory carries none of pglord's configuration, so after the rename re-apply it — reuse the existing helpers rather than duplicating them:

```rust
    append(&cfg.pgdata.join("postgresql.conf"), &crate::bootstrap::postgresql_conf_extra(cfg.port))?;
```

Make `bootstrap::append` and `bootstrap::postgresql_conf_extra` `pub(crate)` as needed, and write `pg_hba.conf` with `pglord_core::hba::render(hba_rules, &cfg.repl_user, !cfg.superuser_password.is_empty())` — thread `hba_rules` into `pgupgrade::run` as a third parameter `hba_rules: &[pglord_core::hba::HbaRule]`, passed straight through from `ensure_bootstrapped`.

- [ ] **Step 4: Run**: `cargo test -p pglord-agent` — PASS.
- [ ] **Step 5: Statistics.** In `main.rs`, after the supervisor starts and only when `cfg.upgrade_in_place` was set, spawn a background `vacuumdb --all --analyze-in-stages` against the local socket. pg_upgrade transfers no optimizer statistics, so without this the cluster returns correct but slow, which reads as a failed upgrade:

```rust
    if !cfg.upgrade_in_place.is_empty() && cfg.role == Role::Primary {
        let cfg_v = cfg.clone();
        tokio::spawn(async move { pglord_agent::pgupgrade::analyze(&cfg_v).await });
    }
```

with `pub async fn analyze(cfg: &Config)` running `vacuumdb -U <superuser> -h <socket_dir> -p <port> --all --analyze-in-stages`, logging failures at warn (statistics are an optimisation, not correctness).

- [ ] **Step 6: `make lint` and commit**: `git add crates/pglord-agent && git commit -m "feat(agent): convert PGDATA in place with pg_upgrade --link"`

---

### Task 5: Server — mode plumbing, preconditions and completion

**Files:**
- Modify: `crates/pglord-server/src/api/upgrade.rs`, `crates/pglord-server/src/reconciler.rs`, `crates/pglord-server/src/orchestrator.rs`, `crates/pglord-agent/src/status.rs`, `crates/pglord-server/src/instance_probe.rs`
- Regenerate: `spec/openapi.yaml`, `ui/src/app/api/**`

**Interfaces:**
- Produces: `UpgradeRequest.mode: UpgradeMode` (`#[serde(default)]`); `UpgradeStatusDto.mode`; new codes `backups_required`, `backup_required`, `target_not_newer`.
- Produces: `InstanceStatus.server_version_num: Option<i32>` and `InstanceObservation.server_version_num: Option<i32>` (the completion signal).
- Produces: `pub(crate) fn in_place_complete(obs: &[InstanceObservation], target_major: u32) -> bool`.
- `reconciler::apply_cluster` passes its `upgrade` argument to `render_statefulset_for_upgrade` for the main set, and still renders the green set only for `BlueGreen` (the existing generation guard already excludes in-place, since in-place never changes `generation` — add an explicit `mode == BlueGreen` condition so intent is not implied).

- [ ] **Step 1: Write the failing tests** (in `api/upgrade.rs`'s `mod tests`):

```rust
fn obs(pod: &str, ready: bool, ver: Option<i32>) -> crate::instance_probe::InstanceObservation {
    crate::instance_probe::InstanceObservation {
        pod: pod.into(), ready, in_recovery: Some(false), lsn: None,
        pending_restart: false, sync_standby_count: None, ssl: None,
        wal_level: None, server_version_num: ver,
    }
}

#[test]
fn in_place_completes_only_when_every_instance_is_ready_on_the_target() {
    let done = vec![obs("demo-0", true, Some(170010)), obs("demo-1", true, Some(170010))];
    assert!(in_place_complete(&done, 17));

    // One still on the old major: the roll has not reached it.
    let mixed = vec![obs("demo-0", true, Some(170010)), obs("demo-1", true, Some(160013))];
    assert!(!in_place_complete(&mixed, 17));
    // One not ready yet (converting, or re-cloning).
    let booting = vec![obs("demo-0", true, Some(170010)), obs("demo-1", false, None)];
    assert!(!in_place_complete(&booting, 17));
    // Unreachable instances report no version: never conclude success.
    let silent = vec![obs("demo-0", true, None)];
    assert!(!in_place_complete(&silent, 17));
    // An empty observation set is not a completed upgrade.
    assert!(!in_place_complete(&[], 17));
}
```

- [ ] **Step 2: Run**: `cargo test -p pglord-server in_place_complete` — FAIL.

- [ ] **Step 3: Implement.**
  1. `status.rs`: add `pub server_version_num: Option<i32>`, selected as `current_setting('server_version_num')::int4` alongside `wal_level`; `None` in the fenced and error paths.
  2. `instance_probe.rs`: mirror the field in `AgentStatus` and `InstanceObservation`.
  3. `api/upgrade.rs`:

```rust
/// Every instance is up and reporting the major we upgraded to.
pub(crate) fn in_place_complete(
    obs: &[crate::instance_probe::InstanceObservation],
    target_major: u32,
) -> bool {
    !obs.is_empty()
        && obs.iter().all(|o| {
            o.ready
                && o.server_version_num
                    .is_some_and(|v| v as u32 / 10_000 == target_major)
        })
}
```

  4. `UpgradeRequest` gains `#[serde(default)] pub mode: UpgradeMode`; `UpgradeStatusDto` gains `pub mode: UpgradeMode`.
  5. `start_upgrade` branches after the existing hibernated/empty-image/in-progress checks:

```rust
    if req.mode == UpgradeMode::InPlace {
        if !spec.backup.enabled {
            return Err(ApiError::conflict("backups_required").with_detail(
                "An in-place upgrade rewrites the data directory and can only be undone by \
                 restoring a backup. Configure a backup repository for this cluster first.",
            ));
        }
        let runs = crate::backup_runs::list(&ops.kube, &ns, &name).await;
        if !runs.iter().any(|r| r.state == crate::backup_runs::RunState::Succeeded) {
            return Err(ApiError::conflict("backup_required").with_detail(
                "Take a backup before upgrading in place: it is the only way back.",
            ));
        }
    }
```

  6. Resolve the majors. The running server is authoritative; the tag is a hint:

```rust
    let running_major = observations
        .iter()
        .find(|o| o.pod == primary)
        .and_then(|o| o.server_version_num)
        .map(|v| v as u32 / 10_000);
    let target_major = pglord_core::upgrade::major_from_image(&target_image);
    if let (Some(from), Some(to)) = (running_major, target_major)
        && to <= from
    {
        return Err(ApiError::conflict("target_not_newer").with_detail(format!(
            "This cluster runs PostgreSQL {from} and the target image is {to}. \
             Upgrades only go forward."
        )));
    }
```

  For in-place, `running_major` is required — without it there is no way to know which tree to mount, so return `ApiError::conflict("target_not_newer")` with a detail saying the primary did not report its version. Store it as `from_major`.

  7. The `wal_level` precondition applies to blue-green only — guard it with `if req.mode == UpgradeMode::BlueGreen`. Likewise the WAL-retention `prepare` call.
  8. In-place `state`: `phase: UpgradePhase::Upgrading, mode: InPlace, from_major: Some(from)`. After the CAS, `apply_cluster` (which now renders the in-place pod), then spawn a roll over `roll_order(&spec.sts_name(), spec.replicas, &primary)` so standbys are replaced before the primary converts.
  9. `cutover_upgrade`, `finalize_upgrade`, `rollback_upgrade`: reject non-blue-green up front —

```rust
    if upgrade.mode != UpgradeMode::BlueGreen {
        return Err(ApiError::conflict("invalid_upgrade_phase")
            .with_detail("This operation applies to a blue-green upgrade."));
    }
```

  10. `abort_upgrade`: accept `Upgrading` as well as `Migrating`. For in-place, revert `spec.image` to the pre-upgrade image (recorded implicitly: the spec's image is still the old one until completion, so simply clearing the state and re-applying suffices) and re-roll; do **not** delete any StatefulSet.
  11. `status_dto`: for in-place, report `phase`, `mode`, `target_image` and no databases/lag; `ready_to_cutover: false`.
  12. Completion, in `get_cluster`'s upgrade branch **and** in the reconciler pass so it converges without a viewer: when `mode == InPlace` and `in_place_complete(&observations, target)`, persist `image = target_image` and clear the state. Put it in one place — `pub async fn settle_in_place(ops, ns, spec, state, obs) -> anyhow::Result<bool>` in `api/upgrade.rs`, called from `get_cluster`; the reconciler is level-triggered and calls the same function each pass. Before clearing the state, `settle_in_place` posts `backup/stanza-upgrade` to the primary (best-effort, warn on failure) — the stanza still describes the old major, exactly as at blue-green's finalize, and in-place always has backups enabled because the gate requires it.
  13. **Failover stays off for the duration.** `orchestrator::failover_enabled` currently excludes only `Soaking`; the instances are deliberately down and mid-conversion during `Upgrading` too:

```rust
pub fn failover_enabled(upgrade: Option<&pglord_core::upgrade::UpgradeState>) -> bool {
    !matches!(
        upgrade.map(|u| u.phase),
        Some(pglord_core::upgrade::UpgradePhase::Soaking)
            | Some(pglord_core::upgrade::UpgradePhase::Upgrading)
    )
}
```

Extend the existing `no_failover_during_an_upgrade_soak` test with an `Upgrading` case asserting `!failover_enabled(...)`.

- [ ] **Step 4: Run**: `cargo test --workspace` — PASS. `make upgrade-e2e` must still pass (blue-green regression).
- [ ] **Step 5: Regenerate**: `make openapi ui-client crd`.
- [ ] **Step 6: `make lint` and commit**:

```bash
git add crates spec/openapi.yaml ui/src/app/api deploy/crds
git commit -m "feat(server): in-place upgrade mode with backup gate and completion detection"
```

---

### Task 6: UI — mode choice and in-place card

**Files:**
- Modify: `ui/src/app/pages/clusters/cluster-upgrade-dialog.component.ts`, `ui/src/app/pages/clusters/cluster-detail.component.ts`, `ui/public/i18n/en.json`

**Interfaces:**
- Consumes: `UpgradeStatusDto.mode`, `startUpgrade` with `body: { target_image, mode }`.

- [ ] **Step 1: i18n.** Add under the existing `upgrade` key:

```json
"mode": "How should this upgrade run?",
"modeBlueGreen": "Blue-green — no downtime",
"modeBlueGreenHint": "Builds a second copy of the whole cluster and replicates into it. Needs double the disk and compute until you finalize.",
"modeInPlace": "In place — minutes of downtime",
"modeInPlaceHint": "Converts this cluster's data directory with pg_upgrade. Almost no extra disk, but the only way back is restoring a backup.",
"inPlaceNeedsBackup": "In-place upgrades need a backup first — take one from the Backups section.",
"upgrading": "Converting the data directory",
"inPlaceRunning": "Instances are converting and re-cloning. This is the downtime window.",
"abortInPlaceWarning": "Clears the upgrade and puts the previous image back. If the data directory has already been converted, the old version cannot start on it and you will need to restore your backup."
```

- [ ] **Step 2: Start dialog** — `UpgradeStartDialogComponent` gains a mode radio group above the image field, defaulting to `blue_green`, each option showing its hint. `ref.close(...)` returns `{ image, mode }` instead of a bare string; update `openUpgrade` accordingly:

```ts
const result = await firstValueFrom(
  this.dialog.open<{ image: string; mode: 'blue_green' | 'in_place' } | undefined>(
    UpgradeStartDialogComponent,
    { context: { name: d.name }, contentClass: FORM_DIALOG_CLASS },
  ).closed$,
);
if (!result) { return; }
await this.run(() =>
  this.api.invoke(startUpgrade, {
    ns: this.ns(), name: this.name(),
    body: { target_image: result.image, mode: result.mode },
  }),
);
```

- [ ] **Step 3: Card** — in the upgrade section, when `u.mode === 'in_place'` render the `upgrade.upgrading` heading, the `upgrade.inPlaceRunning` note and only an **Abort** button (`data-testid="upgrade-abort"`); the cutover/finalize/rollback buttons stay behind `u.mode === 'blue_green'`. The abort confirmation uses `upgrade.abortInPlaceWarning` when the mode is in-place.
- [ ] **Step 4: Verify**: `make ui-lint ui-build ui-test`.
- [ ] **Step 5: Commit**: `git add ui && git commit -m "feat(ui): choose an upgrade mode; in-place progress card"`

---

### Task 7: E2E and docs

**Files:**
- Create: `scripts/upgrade-inplace-e2e.sh` (chmod +x)
- Modify: `Makefile`, `docs/user-guide.md`, `CLAUDE.md`

- [ ] **Step 1: Script.** Same harness as `scripts/upgrade-e2e.sh` (port `18106`, `kill_pf`, `trap cleanup EXIT`, python3 JSON extraction, image build + staleness check for `pglord-agent:dev16`/`dev17`). Flow:

```bash
# create on 16 with app_rw/appdb, 100 rows, a sequence and pgcrypto (as upgrade-e2e.sh)
# 1. in-place start is REFUSED without backups
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/api/v1/clusters/$CLUSTER/upgrade" \
  -H 'content-type: application/json' \
  -d '{"target_image":"pglord-agent:dev17","mode":"in_place"}')
[ "$code" = "409" ] || { echo "FAIL: in-place without backups returned $code"; exit 1; }
# 2. enable backups against the dev MinIO, take one, wait for it to succeed
# 3. start in place -> 202; poll until the upgrade block clears
# 4. assert: server_version starts with 17; 100 rows intact; sequence continues;
#    pgcrypto works; pod names UNCHANGED (updemo-0, no -g2); sts count is 1;
#    cluster image == pglord-agent:dev17; ready_replicas == 2
```

Assert the names explicitly — that no generation suffix appears is the observable difference from blue-green:

```bash
$KUBECTL -n pglord get sts "$CLUSTER" >/dev/null 2>&1 || { echo "FAIL: the original StatefulSet is gone"; exit 1; }
$KUBECTL -n pglord get sts "$CLUSTER-g2" >/dev/null 2>&1 && { echo "FAIL: in-place created a second instance set"; exit 1; }
```

Apply the harness pitfalls: `| head -1` on any `RETURNING` value, response *bodies* printed on failure, no `pkill` pattern that could match the caller.

- [ ] **Step 2: Makefile**: `upgrade-inplace-e2e: ## Minikube e2e: in-place pg_upgrade 16->17 (needs dev MinIO)` + `./scripts/upgrade-inplace-e2e.sh`. Add it to the e2e list in `CLAUDE.md`.
- [ ] **Step 3: Run it**: `make upgrade-inplace-e2e`. Iterate until green without weakening assertions.
- [ ] **Step 4: Docs.** Rewrite the user guide's "Major version upgrades" opening to present the two modes and when to pick each — blue-green for clusters that cannot take downtime and have the headroom, in-place for large clusters where a second copy is impractical — then split the existing detail under each. Document the backup requirement, the downtime window, that standbys are re-cloned (time and reduced redundancy), and that abort after conversion means restoring the backup.
- [ ] **Step 5: Commit**: `git add scripts Makefile docs CLAUDE.md && git commit -m "test(e2e)+docs: in-place upgrade suite and two-mode guide"`

---

## Self-review

**Spec coverage:** mode field (T1, T5) · initContainer + original-path mounts (T2) · backup gate (T5) · direction check (T5) · agent decision table incl. standby re-clone (T4) · `--link` and same-volume new datadir (T3, T4) · conf/hba re-apply (T4) · vacuumdb (T4) · completion detection (T5) · stanza-upgrade — **gap found**: the spec calls for it at completion. Fold into T5 step 12: `settle_in_place` posts `backup/stanza-upgrade` to the primary when `spec.backup.enabled`, which is guaranteed true for in-place. · abort semantics (T5 step 10) · failover disabled — **gap found**: `failover_enabled` currently only excludes `Soaking`. Fold into T5: exclude `Upgrading` too, and extend the existing `no_failover_during_an_upgrade_soak` test with an `Upgrading` case. · UI (T6) · tests (throughout, T7).

**Type consistency:** `UpgradeMode`/`UpgradePhase::Upgrading`/`from_major` (T1) are used with those exact names in T2, T5, T6. `render_statefulset_for_upgrade` (T2) is called only from `reconciler::apply_cluster` (T5). `in_place_complete` and `settle_in_place` (T5) are the only completion entry points. `InstanceObservation.server_version_num` (T5) is the field the e2e's assertions rest on.

**Placeholders:** none — every step carries its code or its exact command.
