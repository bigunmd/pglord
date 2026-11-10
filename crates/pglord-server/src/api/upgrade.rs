//! Major version upgrades: a blue-green migration driven by logical
//! replication, gated at every irreversible step.
//!
//! The whole flow is five verbs. `start` builds a second instance set on the
//! target image and subscribes it to the running one; `cutover` fences writes,
//! drains and moves the primary record to the new set; `finalize` deletes the
//! old set and makes the new generation the cluster; `rollback` undoes a
//! cutover; `abort` throws the new set away before anything user-visible has
//! happened.

use std::time::Duration;

use anyhow::Context;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use k8s_openapi::api::apps::v1::StatefulSet;
use kube::api::Api;
use pglord_core::render::{AGENT_HTTP_PORT, pod_fqdn};
use pglord_core::spec::ClusterSpec;
use pglord_core::upgrade::{UpgradeMode, UpgradePhase, UpgradeState, next_generation};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::app::ClusterOps;
use crate::error::ApiError;

/// Lag we still call "caught up". Logical replication never reaches exactly
/// zero on a live cluster, and the cutover fences writes before draining, so
/// this only has to be small enough to drain inside the fence window.
const READY_LAG_BYTES: i64 = 1_048_576;

const CUTOVER_TIMEOUT: Duration = Duration::from_secs(180);
const AGENT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Deserialize, ToSchema)]
pub struct UpgradeRequest {
    /// Image to upgrade to — a pglord agent image built on the PostgreSQL
    /// major you want, shipping every extension this cluster uses.
    pub target_image: String,
    /// How to perform the upgrade. Defaults to blue-green, so existing
    /// callers keep their behaviour.
    #[serde(default)]
    pub mode: UpgradeMode,
}

#[derive(Serialize, ToSchema)]
pub struct UpgradeDbDto {
    pub name: String,
    /// Tables the migration covers in this database.
    pub tables_total: i64,
    /// Tables whose initial copy has finished and that are now streaming.
    pub tables_ready: i64,
}

#[derive(Serialize, ToSchema)]
pub struct UpgradeStatusDto {
    /// "blue_green" (a second instance set) or "in_place" (pg_upgrade on this
    /// cluster's own data directory). Decides which actions apply.
    pub mode: UpgradeMode,
    /// Blue-green: "migrating" then "soaking". In-place: "upgrading".
    pub phase: UpgradePhase,
    pub target_image: String,
    /// Generation the new instance set renders as; its pods are named
    /// `<cluster>-<generation>-<ordinal>`.
    pub next_generation: String,
    /// Every table is streaming and replication lag is negligible.
    pub ready_to_cutover: bool,
    /// Worst-case bytes of WAL the new set has not confirmed. Null while
    /// migrating means the new set is not reachable yet.
    pub lag_bytes: Option<i64>,
    pub databases: Vec<UpgradeDbDto>,
    /// What is currently blocking progress, when anything is.
    pub error: Option<String>,
}

/// The agent's `/upgrade/status` JSON. A separate type from the agent's own
/// struct on purpose: the crates do not depend on each other, so the wire
/// field names are the contract between them.
#[derive(Deserialize, Default)]
pub(crate) struct AgentUpgradeStatus {
    #[serde(default)]
    pub established: bool,
    #[serde(default)]
    pub complete: bool,
    #[serde(default)]
    pub databases: Vec<AgentDbSync>,
    #[serde(default)]
    pub lag_bytes: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct AgentDbSync {
    pub name: String,
    pub tables_total: i64,
    pub tables_ready: i64,
}

/// Cutting over before every table is streaming would silently leave rows
/// behind, so this is the one gate the API will not let a user talk it out of.
pub(crate) fn ready_to_cutover(st: &AgentUpgradeStatus) -> bool {
    st.established
        && st
            .databases
            .iter()
            .all(|d| d.tables_ready == d.tables_total)
        && st.lag_bytes.is_some_and(|l| l <= READY_LAG_BYTES)
}

/// Every instance is up and past the major we upgraded from.
///
/// Compares against the recorded *source* major rather than a major parsed
/// out of the target image: tags are arbitrary strings, and a perfectly good
/// image called `:dev17` or `:latest` would otherwise leave the upgrade
/// running forever with the cluster already converted.
///
/// Deliberately strict about silence — an unreachable instance reports no
/// version, and concluding success from that would clear the upgrade while a
/// pod is still converting.
pub(crate) fn in_place_complete(
    obs: &[crate::instance_probe::InstanceObservation],
    from_major: u32,
) -> bool {
    !obs.is_empty()
        && obs.iter().all(|o| {
            o.ready
                && o.server_version_num
                    .is_some_and(|v| (v as u32) / 10_000 > from_major)
        })
}

fn ops(state: &Option<ClusterOps>) -> Result<&ClusterOps, ApiError> {
    state.as_ref().ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "kube_unavailable")
            .with_detail("No Kubernetes environment available; upgrades are unavailable.")
    })
}

/// Ordinal zero of the new instance set — the pod that runs the subscription
/// and, after cutover, the cluster's primary.
fn green_primary_pod(spec: &ClusterSpec, state: &UpgradeState) -> String {
    format!("{}-{}-0", spec.name, state.next_generation)
}

fn green_sts_name(spec: &ClusterSpec, state: &UpgradeState) -> String {
    format!("{}-{}", spec.name, state.next_generation)
}

fn agent_url(pod: &str, cluster: &str, ns: &str, path: &str) -> String {
    format!(
        "http://{}:{}/{}",
        pod_fqdn(pod, cluster, ns),
        AGENT_HTTP_PORT,
        path
    )
}

async fn fetch_agent_status(
    ops: &ClusterOps,
    ns: &str,
    spec: &ClusterSpec,
    state: &UpgradeState,
) -> AgentUpgradeStatus {
    let url = agent_url(
        &green_primary_pod(spec, state),
        &spec.name,
        ns,
        "upgrade/status",
    );
    match ops.http.get(&url).timeout(AGENT_TIMEOUT).send().await {
        Ok(resp) => resp.json::<AgentUpgradeStatus>().await.unwrap_or_default(),
        Err(err) => AgentUpgradeStatus {
            error: Some(format!("new instances not reachable yet: {err}")),
            ..Default::default()
        },
    }
}

/// The status block `GET /clusters/{name}` carries while an upgrade runs.
pub async fn status_dto(
    ops: &ClusterOps,
    ns: &str,
    spec: &ClusterSpec,
    state: &UpgradeState,
) -> UpgradeStatusDto {
    // In-place has no second set to poll: progress is the instances coming
    // back on the new major, which the caller already sees in the instance
    // list.
    if state.mode == UpgradeMode::InPlace {
        return UpgradeStatusDto {
            mode: state.mode,
            phase: state.phase,
            target_image: state.target_image.clone(),
            next_generation: state.next_generation.clone(),
            ready_to_cutover: false,
            lag_bytes: None,
            databases: Vec::new(),
            error: None,
        };
    }
    // After cutover the new set is detached from the old one on purpose, so
    // there is no sync progress left to report — only the pending decision.
    if state.phase == UpgradePhase::Soaking {
        return UpgradeStatusDto {
            mode: state.mode,
            phase: state.phase,
            target_image: state.target_image.clone(),
            next_generation: state.next_generation.clone(),
            ready_to_cutover: false,
            lag_bytes: None,
            databases: Vec::new(),
            error: None,
        };
    }
    let agent = fetch_agent_status(ops, ns, spec, state).await;
    UpgradeStatusDto {
        mode: state.mode,
        phase: state.phase,
        target_image: state.target_image.clone(),
        next_generation: state.next_generation.clone(),
        ready_to_cutover: ready_to_cutover(&agent),
        lag_bytes: agent.lag_bytes,
        databases: agent
            .databases
            .iter()
            .map(|d| UpgradeDbDto {
                name: d.name.clone(),
                tables_total: d.tables_total,
                tables_ready: d.tables_ready,
            })
            .collect(),
        error: agent.error,
    }
}

/// Cutover, rollback and finalize are blue-green concepts; an in-place
/// upgrade has no second set to switch between.
fn reject_non_blue_green(state: &UpgradeState) -> Result<(), ApiError> {
    if state.mode != UpgradeMode::BlueGreen {
        return Err(ApiError::conflict("invalid_upgrade_phase")
            .with_detail("This operation applies to a blue-green upgrade."));
    }
    Ok(())
}

/// Load the cluster and its upgrade state, requiring the state to be in one of
/// the phases this verb accepts.
async fn require_phase(
    ops: &ClusterOps,
    ns: &str,
    name: &str,
    allowed: UpgradePhase,
) -> Result<
    (
        ClusterSpec,
        pglord_core::render::OwnerRef,
        UpgradeState,
        String,
    ),
    ApiError,
> {
    let Some((spec, owner, revision)) = ops
        .store
        .get_versioned(ns, name)
        .await
        .map_err(ApiError::from_kube)?
    else {
        return Err(ApiError::not_found());
    };
    let Some(state) = ops
        .store
        .upgrade_state(ns, name)
        .await
        .map_err(ApiError::from_kube)?
    else {
        return Err(ApiError::conflict("no_upgrade_in_progress")
            .with_detail("This cluster has no major version upgrade in progress."));
    };
    if state.phase != allowed {
        return Err(
            ApiError::conflict("invalid_upgrade_phase").with_detail(format!(
                "The upgrade is {:?} and this operation applies to a {:?} upgrade.",
                state.phase, allowed
            )),
        );
    }
    Ok((spec, owner, state, revision))
}

/// Delete an instance set. Its PVCs follow through the StatefulSet's
/// whenDeleted: Delete retention policy, so no volume is orphaned.
async fn delete_instance_set(ops: &ClusterOps, ns: &str, sts_name: &str) {
    let api: Api<StatefulSet> = Api::namespaced(ops.kube.clone(), ns);
    match api.delete(sts_name, &Default::default()).await {
        Ok(_) => tracing::info!(sts = %sts_name, "instance set deleted"),
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(err) => tracing::warn!(sts = %sts_name, %err, "instance set delete failed"),
    }
}

/// Best-effort POST to an agent: used for the steps whose failure must not
/// block the decision the user already made (unfencing, cleanup).
async fn post_agent(ops: &ClusterOps, ns: &str, cluster: &str, pod: &str, path: &str) -> bool {
    let url = agent_url(pod, cluster, ns, path);
    match ops.http.post(&url).timeout(AGENT_TIMEOUT).send().await {
        Ok(resp) if resp.status().is_success() => true,
        Ok(resp) => {
            tracing::warn!(%pod, %path, status = %resp.status(), "agent call refused");
            false
        }
        Err(err) => {
            tracing::warn!(%pod, %path, %err, "agent call failed");
            false
        }
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/namespaces/{ns}/clusters/{name}/upgrade",
    tag = "clusters",
    params(
        ("ns" = String, Path, description = "Namespace"),
        ("name" = String, Path, description = "Cluster name")
    ),
    request_body = UpgradeRequest,
    responses(
        (status = 202, description = "Upgrade started; the new instance set is being built"),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 409, description = "Already upgrading, hibernated, or wal_level is not logical", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 422, description = "Invalid target image", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn start_upgrade(
    State(state): State<Option<ClusterOps>>,
    Path((ns, name)): Path<(String, String)>,
    axum::Json(req): axum::Json<UpgradeRequest>,
) -> Result<StatusCode, ApiError> {
    let ops = ops(&state)?;
    let Some((mut spec, owner, revision)) = ops
        .store
        .get_versioned(&ns, &name)
        .await
        .map_err(ApiError::from_kube)?
    else {
        return Err(ApiError::not_found());
    };
    crate::api::clusters::reject_hibernated(&spec)?;
    let target_image = req.target_image.trim().to_string();
    if target_image.is_empty() {
        return Err(
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_image")
                .with_detail("The target image must not be empty."),
        );
    }
    if ops
        .store
        .upgrade_state(&ns, &name)
        .await
        .map_err(ApiError::from_kube)?
        .is_some()
    {
        return Err(ApiError::conflict("upgrade_in_progress")
            .with_detail("This cluster already has a major version upgrade in progress."));
    }

    let primary = ops
        .store
        .primary(&ns, &name)
        .await
        .map_err(ApiError::from_kube)?
        .unwrap_or_else(|| spec.instance_name(0));

    let observations = crate::instance_probe::observe_cluster(&ops.http, &spec, &ns).await;
    let running_major = observations
        .iter()
        .find(|o| o.pod == primary)
        .and_then(|o| o.server_version_num)
        .map(|v| (v as u32) / 10_000);

    // Upgrades only go forward. The running server is authoritative; the
    // image tag is a hint, so a tag we cannot read is not an error.
    if let (Some(from), Some(to)) = (
        running_major,
        pglord_core::upgrade::major_from_image(&target_image),
    ) && to <= from
    {
        return Err(ApiError::conflict("target_not_newer").with_detail(format!(
            "This cluster runs PostgreSQL {from} and the target image is PostgreSQL {to}."
        )));
    }

    if req.mode == UpgradeMode::InPlace {
        // An in-place upgrade rewrites the data directory with no way back
        // except a restore, so the backup is a precondition, not advice.
        if !spec.backup.enabled {
            return Err(ApiError::conflict("backups_required").with_detail(
                "An in-place upgrade rewrites the data directory and can only be undone by \
                 restoring a backup. Configure a backup repository for this cluster first.",
            ));
        }
        let runs = crate::backup_runs::list(&ops.kube, &ns, &name)
            .await
            .map_err(ApiError::from_kube)?;
        if !runs
            .iter()
            .any(|r| r.state == crate::backup_runs::RunState::Succeeded)
        {
            return Err(ApiError::conflict("backup_required")
                .with_detail("Take a backup before upgrading in place: it is the only way back."));
        }
        // Without the running major there is no way to know which tree to
        // mount for pg_upgrade, and guessing produces a pod that cannot start.
        if running_major.is_none() {
            return Err(ApiError::conflict("target_not_newer").with_detail(
                "The primary did not report its PostgreSQL version; it may be unreachable. \
                 Check the cluster is healthy and retry.",
            ));
        }
    } else {
        // Logical replication cannot be built on wal_level = replica, and the
        // level only changes at server start — so this is a restart the user
        // has to take before, not during, the upgrade.
        match observations
            .iter()
            .find(|o| o.pod == primary)
            .and_then(|o| o.wal_level.as_deref())
        {
            Some("logical") => {}
            Some(other) => {
                return Err(
                    ApiError::conflict("wal_level_not_logical").with_detail(format!(
                        "The primary runs with wal_level={other}. Restart the cluster \
                         (Restart cluster) to pick up wal_level=logical, then start the upgrade."
                    )),
                );
            }
            None => {
                return Err(ApiError::conflict("wal_level_not_logical").with_detail(
                    "The primary did not report its wal_level; it may be unreachable. \
                     Check the cluster is healthy and retry.",
                ));
            }
        }
    }

    // Bound the WAL the migration may pin here, *before* any slot exists.
    // Without it the initial copy of a large database retains WAL on the
    // running cluster's volume for as long as it takes, which can fill the
    // disk of the database this upgrade is supposed to leave alone.
    if req.mode == UpgradeMode::BlueGreen
        && let Some(size) = pglord_core::upgrade::wal_retention_for(&spec.storage_size)
    {
        let url = agent_url(&primary, &spec.name, &ns, "upgrade/prepare");
        let prepared = ops
            .http
            .post(&url)
            .timeout(AGENT_TIMEOUT)
            .json(&serde_json::json!({ "max_slot_wal_keep_size": size }))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success());
        if !prepared {
            return Err(
                ApiError::new(StatusCode::BAD_GATEWAY, "upgrade_prepare_failed").with_detail(
                    "Could not limit how much WAL the migration may retain on the current \
                     primary. Starting anyway risks filling its volume, so nothing was changed.",
                ),
            );
        }
    }

    let in_place = req.mode == UpgradeMode::InPlace;
    if in_place && !spec.parallel_pod_management {
        // Persist before any immutable migration. Both ordinary reconciliation
        // and upgrade retries must keep Parallel after the upgrade settles.
        spec.parallel_pod_management = true;
        ops.store
            .update(&ns, &spec, &revision)
            .await
            .map_err(ApiError::from_kube)?;
    }
    let state = UpgradeState {
        target_image,
        next_generation: next_generation(&spec.generation),
        phase: if in_place {
            UpgradePhase::Upgrading
        } else {
            UpgradePhase::Migrating
        },
        previous_primary: None,
        mode: req.mode,
        // Resolved once here so the render stays stable across reconciles.
        from_major: if in_place { running_major } else { None },
    };
    if !ops
        .store
        .set_upgrade(&ns, &name, Some(&state), None, None)
        .await
        .map_err(ApiError::from_kube)?
    {
        return Err(ApiError::conflict("upgrade_in_progress")
            .with_detail("The cluster changed while starting the upgrade; retry."));
    }
    // Synchronously, so the new instance set exists by the time the caller
    // sees 202 and starts polling its progress.
    crate::reconciler::apply_cluster(
        &ops.kube,
        &ops.http,
        &ns,
        &spec,
        &owner,
        &primary,
        Some(&state),
    )
    .await
    .map_err(ApiError::from_kube)?;
    // In-place replacement is driven by the orchestrator's durable-state poll,
    // not a request-owned task that disappears on a server restart.
    tracing::warn!(
        cluster = %name, image = %state.target_image, mode = ?state.mode,
        "major upgrade started"
    );
    Ok(StatusCode::ACCEPTED)
}

#[utoipa::path(
    post,
    path = "/api/v1/namespaces/{ns}/clusters/{name}/upgrade/cutover",
    tag = "clusters",
    params(
        ("ns" = String, Path, description = "Namespace"),
        ("name" = String, Path, description = "Cluster name")
    ),
    responses(
        (status = 202, description = "Cut over; applications now write to the new instance set"),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 409, description = "No upgrade running, wrong phase, or not caught up", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 502, description = "Cutover failed; the cluster is unchanged", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn cutover_upgrade(
    State(state): State<Option<ClusterOps>>,
    Path((ns, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let ops = ops(&state)?;
    let (spec, owner, upgrade, _revision) =
        require_phase(ops, &ns, &name, UpgradePhase::Migrating).await?;
    reject_non_blue_green(&upgrade)?;

    let agent = fetch_agent_status(ops, &ns, &spec, &upgrade).await;
    // An attempt that cut the data over but lost the record patch leaves the
    // new set already detached — which reads as "not caught up", because there
    // is nothing left to catch up to. Resume from there instead of refusing.
    let already_cut_over = agent.complete;
    if !already_cut_over && !ready_to_cutover(&agent) {
        return Err(ApiError::conflict("not_ready_for_cutover").with_detail(
            agent.error.unwrap_or_else(|| {
                "The new instances have not finished copying every table yet.".to_string()
            }),
        ));
    }

    let blue_primary = ops
        .store
        .primary(&ns, &name)
        .await
        .map_err(ApiError::from_kube)?
        .unwrap_or_else(|| spec.instance_name(0));
    let green_primary = green_primary_pod(&spec, &upgrade);

    if !already_cut_over {
        // Fence first: the drain measures the source's final WAL position,
        // which only means anything once nothing can write past it.
        if !post_agent(ops, &ns, &spec.name, &blue_primary, "upgrade/fence").await {
            return Err(ApiError::new(StatusCode::BAD_GATEWAY, "cutover_failed")
                .with_detail("Could not make the current primary read-only; nothing changed."));
        }

        let url = agent_url(&green_primary, &spec.name, &ns, "upgrade/cutover");
        let cut = ops.http.post(&url).timeout(CUTOVER_TIMEOUT).send().await;
        let cut_ok = matches!(&cut, Ok(resp) if resp.status().is_success());
        if !cut_ok {
            // Put the cluster back exactly as it was: the upgrade stays in
            // migrating, so cutover can simply be retried.
            post_agent(ops, &ns, &spec.name, &blue_primary, "upgrade/unfence").await;
            let detail = match cut {
                // Relay what the instance actually said. A bare status code
                // here means the only way to find out why a cutover failed is
                // to reach into the pod.
                Ok(resp) => {
                    let status = resp.status();
                    let reason = resp
                        .json::<serde_json::Value>()
                        .await
                        .ok()
                        .and_then(|v| v.get("error")?.as_str().map(str::to_string))
                        .unwrap_or_else(|| status.to_string());
                    format!("The new instances refused the cutover: {reason}")
                }
                Err(err) => format!("The new instances could not complete the cutover: {err}"),
            };
            tracing::error!(cluster = %name, %detail, "cutover failed");
            return Err(
                ApiError::new(StatusCode::BAD_GATEWAY, "cutover_failed").with_detail(detail)
            );
        }
    }

    let next_state = UpgradeState {
        phase: UpgradePhase::Soaking,
        previous_primary: Some(blue_primary.clone()),
        ..upgrade.clone()
    };
    // One patch moves the primary record and the phase together — the moment
    // that makes the new set the cluster's writer.
    if !ops
        .store
        .set_upgrade(
            &ns,
            &name,
            Some(&next_state),
            Some(&green_primary),
            Some(&upgrade),
        )
        .await
        .map_err(ApiError::from_kube)?
    {
        post_agent(ops, &ns, &spec.name, &blue_primary, "upgrade/unfence").await;
        return Err(
            ApiError::new(StatusCode::BAD_GATEWAY, "cutover_failed").with_detail(
                "The cluster record changed during the cutover; nothing was switched. Retry.",
            ),
        );
    }
    // Repoint the rw Service now instead of waiting for the next resync.
    if let Err(err) = crate::reconciler::apply_cluster(
        &ops.kube,
        &ops.http,
        &ns,
        &spec,
        &owner,
        &green_primary,
        Some(&next_state),
    )
    .await
    {
        tracing::warn!(%err, "post-cutover apply failed (resync will retry)");
    }
    tracing::warn!(cluster = %name, from = %blue_primary, to = %green_primary, "major upgrade cut over");
    Ok(StatusCode::ACCEPTED)
}

#[utoipa::path(
    post,
    path = "/api/v1/namespaces/{ns}/clusters/{name}/upgrade/rollback",
    tag = "clusters",
    params(
        ("ns" = String, Path, description = "Namespace"),
        ("name" = String, Path, description = "Cluster name")
    ),
    responses(
        (status = 202, description = "Rolled back to the previous version; writes made since cutover are gone"),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 409, description = "No upgrade running or wrong phase", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn rollback_upgrade(
    State(state): State<Option<ClusterOps>>,
    Path((ns, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let ops = ops(&state)?;
    let (spec, owner, upgrade, revision) =
        require_phase(ops, &ns, &name, UpgradePhase::Soaking).await?;
    reject_non_blue_green(&upgrade)?;
    let _ = revision;
    let previous = upgrade
        .previous_primary
        .clone()
        .unwrap_or_else(|| spec.instance_name(0));

    // Record back and state cleared in one patch, for the same reason cutover
    // took one: no observer may see the cluster half-rolled-back.
    if !ops
        .store
        .set_upgrade(&ns, &name, None, Some(&previous), Some(&upgrade))
        .await
        .map_err(ApiError::from_kube)?
    {
        return Err(ApiError::conflict("record_changed")
            .with_detail("The cluster record changed; re-read the cluster and retry."));
    }
    if let Err(err) =
        crate::reconciler::apply_cluster(&ops.kube, &ops.http, &ns, &spec, &owner, &previous, None)
            .await
    {
        tracing::warn!(%err, "post-rollback apply failed (resync will retry)");
    }
    post_agent(ops, &ns, &spec.name, &previous, "upgrade/unfence").await;
    post_agent(ops, &ns, &spec.name, &previous, "upgrade/cleanup").await;
    delete_instance_set(ops, &ns, &green_sts_name(&spec, &upgrade)).await;
    tracing::warn!(cluster = %name, primary = %previous, "major upgrade rolled back");
    Ok(StatusCode::ACCEPTED)
}

#[utoipa::path(
    post,
    path = "/api/v1/namespaces/{ns}/clusters/{name}/upgrade/abort",
    tag = "clusters",
    params(
        ("ns" = String, Path, description = "Namespace"),
        ("name" = String, Path, description = "Cluster name")
    ),
    responses(
        (status = 202, description = "Upgrade abandoned; the running cluster was never touched"),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 409, description = "No upgrade running or already cut over", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn abort_upgrade(
    State(state): State<Option<ClusterOps>>,
    Path((ns, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let ops = ops(&state)?;
    let Some((spec, owner)) = ops
        .store
        .get(&ns, &name)
        .await
        .map_err(ApiError::from_kube)?
    else {
        return Err(ApiError::not_found());
    };
    let Some(upgrade) = ops
        .store
        .upgrade_state(&ns, &name)
        .await
        .map_err(ApiError::from_kube)?
    else {
        return Err(ApiError::conflict("no_upgrade_in_progress")
            .with_detail("This cluster has no major version upgrade in progress."));
    };
    if upgrade.phase == UpgradePhase::Soaking {
        return Err(ApiError::conflict("invalid_upgrade_phase").with_detail(
            "This upgrade has already cut over; roll it back or finalize it instead.",
        ));
    }
    if !ops
        .store
        .set_upgrade(&ns, &name, None, None, Some(&upgrade))
        .await
        .map_err(ApiError::from_kube)?
    {
        return Err(ApiError::conflict("record_changed")
            .with_detail("The cluster record changed; re-read the cluster and retry."));
    }
    let primary = ops
        .store
        .primary(&ns, &name)
        .await
        .map_err(ApiError::from_kube)?
        .unwrap_or_else(|| spec.instance_name(0));

    if upgrade.mode == UpgradeMode::InPlace {
        // Clearing the state reverts the render to the cluster's own (old)
        // image; the roll puts the instances back on it. Whether that
        // recovers anything depends on how far the conversion got — if a data
        // directory was already converted the old image refuses it, which is
        // what the confirmation warned about.
        if let Err(err) = crate::reconciler::apply_cluster(
            &ops.kube, &ops.http, &ns, &spec, &owner, &primary, None,
        )
        .await
        {
            tracing::warn!(%err, "post-abort apply failed (resync will retry)");
        }
        let roll_ops = ops.clone();
        let roll_ns = ns.clone();
        let roll_spec = spec.clone();
        let roll_primary = primary.clone();
        tokio::spawn(async move {
            let order = crate::api::clusters::roll_order(
                &roll_spec.sts_name(),
                roll_spec.replicas,
                &roll_primary,
            );
            let _ = crate::api::clusters::roll_pods(&roll_ops, &roll_ns, &roll_spec, order).await;
        });
        tracing::warn!(cluster = %name, "in-place upgrade aborted");
        return Ok(StatusCode::ACCEPTED);
    }

    // Cleanup before the new set goes away: an abandoned replication slot
    // retains WAL forever, filling the volume of a cluster whose upgrade the
    // user believes they cancelled.
    post_agent(ops, &ns, &spec.name, &primary, "upgrade/cleanup").await;
    delete_instance_set(ops, &ns, &green_sts_name(&spec, &upgrade)).await;
    tracing::warn!(cluster = %name, "major upgrade aborted");
    Ok(StatusCode::ACCEPTED)
}

#[utoipa::path(
    post,
    path = "/api/v1/namespaces/{ns}/clusters/{name}/upgrade/finalize",
    tag = "clusters",
    params(
        ("ns" = String, Path, description = "Namespace"),
        ("name" = String, Path, description = "Cluster name")
    ),
    responses(
        (status = 202, description = "Upgrade finalized; the previous instances and their volumes are deleted"),
        (status = 404, description = "Not found", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
        (status = 409, description = "No upgrade running or not cut over yet", body = crate::error::ProblemDetails, content_type = "application/problem+json"),
    )
)]
pub async fn finalize_upgrade(
    State(state): State<Option<ClusterOps>>,
    Path((ns, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let ops = ops(&state)?;
    let (mut spec, owner, upgrade, mut revision) =
        require_phase(ops, &ns, &name, UpgradePhase::Soaking).await?;
    reject_non_blue_green(&upgrade)?;
    // Claim before any irreversible action. Rollback uses the same record CAS.
    let records: Api<k8s_openapi::api::core::v1::ConfigMap> =
        Api::namespaced(ops.kube.clone(), &ns);
    let record_name = crate::spec_store::spec_cm_name(&name);
    let record = records
        .get(&record_name)
        .await
        .map_err(ApiError::from_kube)?;
    if crate::spec_store::upgrade_from_cm(&record).as_ref() != Some(&upgrade) {
        return Err(ApiError::conflict("record_changed"));
    }
    let claim = serde_json::to_string(&upgrade).map_err(ApiError::from_kube)?;
    if record
        .metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(crate::primary_record::FINALIZING_ANNOTATION))
        != Some(&claim)
    {
        let annotations = serde_json::Map::from_iter([(
            crate::primary_record::FINALIZING_ANNOTATION.into(),
            serde_json::Value::String(claim),
        )]);
        if !crate::primary_record::cas_set_annotations(
            &records,
            &record_name,
            record
                .metadata
                .resource_version
                .as_deref()
                .unwrap_or_default(),
            annotations,
        )
        .await
        .map_err(ApiError::from_kube)?
        {
            return Err(ApiError::conflict("record_changed"));
        }
        // Claim changed the ConfigMap RV. Reload the spec rather than stamping an old one.
        let Some((fresh, _, fresh_revision)) = ops
            .store
            .get_versioned(&ns, &name)
            .await
            .map_err(ApiError::from_kube)?
        else {
            return Err(ApiError::not_found());
        };
        spec = fresh;
        revision = fresh_revision;
    }
    let green_primary = green_primary_pod(&spec, &upgrade);
    let old_sts = finalize_source_set(&spec, &upgrade)?;

    // The spec adopts the new generation and image: from here the new set is
    // simply the cluster, and nothing renders the old one any more.
    let mut next = spec.clone();
    next.generation = upgrade.next_generation.clone();
    next.image = upgrade.target_image.clone();
    // The fenced list names pods of the set being deleted.
    next.fenced = Vec::new();
    ops.store
        .update(&ns, &next, &revision)
        .await
        .map_err(ApiError::from_kube)?;

    if next.backup.enabled {
        // The stanza still describes the old major; without this every backup
        // and archive-push fails. A failure here is visible and re-runnable,
        // so it does not block finalizing.
        post_agent(
            ops,
            &ns,
            &next.name,
            &green_primary,
            "backup/stanza-upgrade",
        )
        .await;
    }

    let active = ops
        .store
        .primary(&ns, &name)
        .await
        .map_err(ApiError::from_kube)?;
    if active.as_deref() != Some(green_primary.as_str()) {
        return Err(ApiError::conflict("record_changed")
            .with_detail("The upgraded primary is no longer active; refusing source deletion."));
    }
    let sets: Api<StatefulSet> = Api::namespaced(ops.kube.clone(), &ns);
    match sets.delete(&old_sts, &Default::default()).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(err) => return Err(ApiError::from_kube(err)),
    }
    if !ops
        .store
        .set_upgrade(&ns, &name, None, None, Some(&upgrade))
        .await
        .map_err(ApiError::from_kube)?
    {
        return Err(ApiError::conflict("record_changed")
            .with_detail("The cluster record changed; re-read the cluster and retry."));
    }
    if let Err(err) = crate::reconciler::apply_cluster(
        &ops.kube,
        &ops.http,
        &ns,
        &next,
        &owner,
        &green_primary,
        None,
    )
    .await
    {
        tracing::warn!(%err, "post-finalize apply failed (resync will retry)");
    }

    // Roll the promoted set: its pods still carry the subscriber-mode env var,
    // and dropping it is what re-enables WAL archiving now the stanza matches.
    let roll_ops = ops.clone();
    let roll_ns = ns.clone();
    tokio::spawn(async move {
        let order =
            crate::api::clusters::roll_order(&next.sts_name(), next.replicas, &green_primary);
        if crate::api::clusters::roll_pods(&roll_ops, &roll_ns, &next, order)
            .await
            .is_ok()
        {
            tracing::info!(cluster = %next.name, "post-finalize roll finished");
        }
    });
    tracing::warn!(cluster = %name, deleted = %old_sts, "major upgrade finalized");
    Ok(StatusCode::ACCEPTED)
}

fn finalize_source_set(spec: &ClusterSpec, upgrade: &UpgradeState) -> Result<String, ApiError> {
    // Cutover records this identity before the spec can adopt the green generation.
    let source = upgrade
        .previous_primary
        .as_deref()
        .and_then(|pod| pod.rsplit_once('-'))
        .filter(|(_, ordinal)| ordinal.parse::<u32>().is_ok())
        .map(|(set, _)| set)
        .filter(|set| {
            (*set == spec.name || set.starts_with(&format!("{}-g", spec.name)))
                && *set != green_sts_name(spec, upgrade)
        })
        .ok_or_else(|| {
            ApiError::conflict("invalid_upgrade_state")
                .with_detail("Cannot safely identify the original instance set; refusing cleanup.")
        })?;
    Ok(source.to_string())
}

fn has_in_place_template(pod: &k8s_openapi::api::core::v1::Pod, state: &UpgradeState) -> bool {
    pod.metadata.deletion_timestamp.is_none()
        && pod.spec.as_ref().is_some_and(|s| {
            s.containers.iter().any(|c| {
                c.name == "postgres"
                    && c.image.as_deref() == Some(state.target_image.as_str())
                    && c.env.as_ref().is_some_and(|env| {
                        env.iter().any(|e| {
                            e.name == "PGLORD_AGENT_UPGRADE_IN_PLACE"
                                && e.value.as_deref()
                                    == state.from_major.map(|v| v.to_string()).as_deref()
                        })
                    })
            })
        })
}

/// Restart-safe, bounded pass: replace at most one pod, standbys before primary.
/// Returns true only when every pod already carries the upgrade template.
async fn advance_in_place(
    ops: &ClusterOps,
    ns: &str,
    spec: &ClusterSpec,
    state: &UpgradeState,
) -> anyhow::Result<bool> {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{DeleteParams, Preconditions};
    let Some((mut current, _, revision)) = ops.store.get_versioned(ns, &spec.name).await? else {
        return Ok(false);
    };
    if serde_json::to_value(&current)? != serde_json::to_value(spec)?
        || ops.store.upgrade_state(ns, &spec.name).await?.as_ref() != Some(state)
    {
        return Ok(false);
    }
    if !current.parallel_pod_management {
        current.parallel_pod_management = true;
        ops.store.update(ns, &current, &revision).await?;
        return Ok(false);
    }
    // Reconciliation owns immutable StatefulSet migration. Do not delete any
    // pod until the observed controller can recreate paused standbys in parallel.
    let sets: Api<StatefulSet> = Api::namespaced(ops.kube.clone(), ns);
    let Some(set) = sets.get_opt(&spec.sts_name()).await? else {
        return Ok(false);
    };
    if !in_place_controller_ready(&set, state) {
        return Ok(false);
    }
    let primary = ops
        .store
        .primary(ns, &spec.name)
        .await?
        .unwrap_or_else(|| spec.instance_name(0));
    if !(0..spec.replicas).any(|i| spec.instance_name(i) == primary) {
        anyhow::bail!("in-place primary is outside the current instance set");
    }
    let api: Api<Pod> = Api::namespaced(ops.kube.clone(), ns);
    let order = crate::api::clusters::roll_order(&spec.sts_name(), spec.replicas, &primary);
    for name in order {
        let Some(pod) = api.get_opt(&name).await? else {
            return Ok(false);
        };
        if has_in_place_template(&pod, state) {
            continue;
        }
        if pod.metadata.deletion_timestamp.is_some() {
            return Ok(false);
        }
        if ops.store.upgrade_state(ns, &spec.name).await?.as_ref() != Some(state) {
            return Ok(false);
        }
        let params = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(pod.metadata.uid.context("pod has no UID")?),
                resource_version: Some(
                    pod.metadata
                        .resource_version
                        .context("pod has no resourceVersion")?,
                ),
            }),
            ..Default::default()
        };
        match api.delete(&name, &params).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 || e.code == 409 => {}
            Err(err) => return Err(err.into()),
        }
        return Ok(false);
    }
    Ok(true)
}

fn in_place_controller_ready(set: &StatefulSet, state: &UpgradeState) -> bool {
    set.metadata.deletion_timestamp.is_none()
        && set.spec.as_ref().is_some_and(|spec| {
            spec.pod_management_policy.as_deref() == Some("Parallel")
                && has_in_place_template(
                    &k8s_openapi::api::core::v1::Pod {
                        metadata: Default::default(),
                        spec: spec.template.spec.clone(),
                        ..Default::default()
                    },
                    state,
                )
        })
}

/// Resume from persisted state on every orchestrator pass, including after restart.
pub(crate) async fn drive_in_place(ops: &ClusterOps, ns: &str, name: &str) -> anyhow::Result<()> {
    let Some(state) = ops.store.upgrade_state(ns, name).await? else {
        return Ok(());
    };
    if state.mode != UpgradeMode::InPlace
        || state.phase != UpgradePhase::Upgrading
        || state.from_major.is_none()
    {
        return Ok(());
    }
    let Some((spec, _, _)) = ops.store.get_versioned(ns, name).await? else {
        return Ok(());
    };
    if advance_in_place(ops, ns, &spec, &state).await? {
        // Never reuse observations captured before pod replacement or a spec write.
        let observations = crate::instance_probe::observe_cluster(&ops.http, &spec, ns).await;
        settle_in_place(ops, ns, &spec, &state, &observations).await?;
    }
    Ok(())
}

/// Finish an in-place upgrade once every instance is back on the target
/// major: adopt the image, re-point the backup stanza, clear the state.
///
/// Level-triggered and idempotent, so it can be called from any pass that
/// has fresh observations. Returns whether it settled anything.
pub async fn settle_in_place(
    ops: &ClusterOps,
    ns: &str,
    spec: &ClusterSpec,
    state: &UpgradeState,
    obs: &[crate::instance_probe::InstanceObservation],
) -> anyhow::Result<bool> {
    let Some(from_major) = state.from_major else {
        // Recorded at start for every in-place upgrade; without it there is
        // nothing to compare the observed majors against.
        return Ok(false);
    };
    if state.mode != UpgradeMode::InPlace || state.phase != UpgradePhase::Upgrading {
        return Ok(false);
    }
    if obs.len() != usize::from(spec.replicas) || !in_place_complete(obs, from_major) {
        return Ok(false);
    }

    let Some((current, _, revision)) = ops.store.get_versioned(ns, &spec.name).await? else {
        return Ok(false);
    };
    // Never attach a fresh revision to an old observation's spec.
    if serde_json::to_value(&current)? != serde_json::to_value(spec)?
        || ops.store.upgrade_state(ns, &spec.name).await?.as_ref() != Some(state)
    {
        return Ok(false);
    }
    let mut next = current;
    next.image = state.target_image.clone();
    ops.store.update(ns, &next, &revision).await?;

    if next.backup.enabled {
        // The stanza still describes the old major, exactly as at blue-green's
        // finalize. In-place always has backups: the start gate requires them.
        let primary = ops
            .store
            .primary(ns, &next.name)
            .await?
            .unwrap_or_else(|| next.instance_name(0));
        if !post_agent(ops, ns, &next.name, &primary, "backup/stanza-upgrade").await {
            anyhow::bail!(
                "backup stanza upgrade was not acknowledged; retaining upgrade state for retry"
            );
        }
    }
    if !ops
        .store
        .set_upgrade(ns, &next.name, None, None, Some(state))
        .await?
    {
        anyhow::bail!("upgrade state changed while settling");
    }
    tracing::warn!(cluster = %next.name, image = %next.image, "in-place upgrade settled");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct InPlaceApi {
        cm: serde_json::Value,
        pods: std::collections::BTreeMap<String, serde_json::Value>,
        deleted: Vec<String>,
    }

    fn upgrade_pod(name: &str, target: bool) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "uid": format!("uid-{name}"), "resourceVersion": "7"},
            "spec": {"containers": [{"name": "postgres",
                "image": if target { "pglord:17" } else { "pglord:16" },
                "env": if target { serde_json::json!([{"name": "PGLORD_AGENT_UPGRADE_IN_PLACE", "value": "16"}]) } else { serde_json::json!([]) }
            }]}
        })
    }

    fn in_place_fixture() -> (
        ClusterSpec,
        UpgradeState,
        std::sync::Arc<std::sync::Mutex<InPlaceApi>>,
    ) {
        let mut spec: ClusterSpec =
            serde_json::from_value(serde_json::json!({"name": "demo", "replicas": 3}))
                .expect("spec");
        spec.image = "pglord:16".into();
        spec.parallel_pod_management = true;
        spec.backup.enabled = true;
        let state = UpgradeState {
            target_image: "pglord:17".into(),
            next_generation: "g2".into(),
            phase: UpgradePhase::Upgrading,
            previous_primary: None,
            mode: UpgradeMode::InPlace,
            from_major: Some(16),
        };
        let mut cm = crate::spec_store::spec_to_configmap(&spec, "ns").expect("configmap");
        cm.metadata.uid = Some("spec-uid".into());
        cm.metadata.resource_version = Some("1".into());
        cm.metadata.annotations = Some(
            [
                (
                    pglord_core::upgrade::UPGRADE_ANNOTATION.into(),
                    serde_json::to_string(&state).expect("state"),
                ),
                ("pglord.dev/primary".into(), "demo-0".into()),
            ]
            .into(),
        );
        let api = InPlaceApi {
            cm: serde_json::to_value(cm).expect("json"),
            pods: (0..3)
                .map(|i| {
                    let name = format!("demo-{i}");
                    (name.clone(), upgrade_pod(&name, false))
                })
                .collect(),
            deleted: vec![],
        };
        (spec, state, std::sync::Arc::new(std::sync::Mutex::new(api)))
    }

    fn in_place_ops(shared: std::sync::Arc<std::sync::Mutex<InPlaceApi>>) -> ClusterOps {
        use axum::{
            body::Body,
            http::{Method, Request, Response},
        };
        use http_body_util::BodyExt;
        let service = tower::service_fn(move |req: Request<kube::client::Body>| {
            let shared = shared.clone();
            async move {
                let method = req.method().clone();
                let path = req.uri().path().to_string();
                let bytes = req.into_body().collect().await.expect("body").to_bytes();
                let input: serde_json::Value = if bytes.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&bytes).expect("json")
                };
                let mut api = shared.lock().expect("api");
                let (code, value) = if path.ends_with("/configmaps/demo-spec") {
                    if method == Method::PATCH {
                        assert_eq!(
                            input["metadata"]["resourceVersion"],
                            api.cm["metadata"]["resourceVersion"]
                        );
                        assert!(
                            input["metadata"]["annotations"].is_null(),
                            "failed settlement must not clear persisted upgrade"
                        );
                        api.cm["data"] = input["data"].clone();
                        api.cm["metadata"]["resourceVersion"] = "2".into();
                    } else {
                        assert_eq!(method, Method::GET);
                    }
                    (200, api.cm.clone())
                } else if path.ends_with("/statefulsets/demo") {
                    assert_eq!(method, Method::GET);
                    (
                        200,
                        serde_json::json!({"apiVersion":"apps/v1", "kind":"StatefulSet", "metadata":{"name":"demo"},
                        "spec":{"serviceName":"demo", "selector":{}, "podManagementPolicy":"Parallel",
                        "template":{"spec":upgrade_pod("template", true)["spec"]}}}),
                    )
                } else {
                    assert!(
                        path.starts_with("/api/v1/namespaces/ns/pods/"),
                        "unexpected request: {path}"
                    );
                    let name = path.rsplit('/').next().expect("pod name");
                    if let Some(pod) = api.pods.get(name).cloned() {
                        if method == Method::DELETE {
                            assert_eq!(input["preconditions"]["uid"], pod["metadata"]["uid"]);
                            assert_eq!(
                                input["preconditions"]["resourceVersion"],
                                pod["metadata"]["resourceVersion"]
                            );
                            api.deleted.push(name.into());
                            api.pods.remove(name);
                        } else {
                            assert_eq!(method, Method::GET);
                        }
                        (200, pod)
                    } else {
                        assert_eq!(method, Method::GET);
                        (
                            404,
                            serde_json::json!({"apiVersion":"v1", "kind":"Status", "status":"Failure", "reason":"NotFound", "message":"missing pod", "code":404}),
                        )
                    }
                };
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .status(code)
                        .header("content-type", "application/json")
                        .body(Body::from(value.to_string()))
                        .expect("response"),
                )
            }
        });
        let kube = kube::Client::new(service, "ns");
        ClusterOps {
            store: std::sync::Arc::new(crate::spec_store::ConfigMapSpecStore::new(
                kube.clone(),
                "ns",
            )),
            kube,
            // Force unavailable observations/commands locally, never resolve Kubernetes DNS.
            http: reqwest::Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:1").expect("proxy"))
                .timeout(std::time::Duration::from_millis(100))
                .build()
                .expect("client"),
            ns: "ns".into(),
            default_agent_image: "pglord:16".into(),
            spec_backend: "configmap".into(),
            all_namespaces: false,
            auth_kubernetes: false,
            auth_oidc_rbac: false,
            metrics: crate::metrics::Metrics::new(),
        }
    }

    #[tokio::test]
    async fn persisted_in_place_driver_resumes_one_guarded_pod_per_pass_without_get_handler() {
        let (_, _, api) = in_place_fixture();
        for name in ["demo-1", "demo-2", "demo-0"] {
            let before = api.lock().expect("api").deleted.len();
            // A fresh ops/store each time models a server restart; only Kubernetes state survives.
            drive_in_place(&in_place_ops(api.clone()), "ns", "demo")
                .await
                .expect("drive");
            {
                let live = api.lock().expect("api");
                assert_eq!(live.deleted.len(), before + 1);
                assert_eq!(live.deleted.last().map(String::as_str), Some(name));
            }
            // Missing replacement must stop this pass, including before deleting the primary.
            drive_in_place(&in_place_ops(api.clone()), "ns", "demo")
                .await
                .expect("missing replacement");
            assert_eq!(api.lock().expect("api").deleted.len(), before + 1);
            api.lock()
                .expect("api")
                .pods
                .insert(name.into(), upgrade_pod(name, true));
        }
        drive_in_place(&in_place_ops(api.clone()), "ns", "demo")
            .await
            .expect("unavailable observations retain state");
        let live = api.lock().expect("api");
        assert_eq!(live.deleted, ["demo-1", "demo-2", "demo-0"]);
        assert!(
            live.cm["metadata"]["annotations"][pglord_core::upgrade::UPGRADE_ANNOTATION]
                .is_string()
        );
    }

    #[tokio::test]
    async fn persisted_in_place_driver_waits_for_terminating_first_standby() {
        let (_, _, api) = in_place_fixture();
        api.lock()
            .expect("api")
            .pods
            .get_mut("demo-1")
            .expect("pod")["metadata"]["deletionTimestamp"] = "2026-01-01T00:00:00Z".into();
        drive_in_place(&in_place_ops(api.clone()), "ns", "demo")
            .await
            .expect("wait for deletion");
        assert!(api.lock().expect("api").deleted.is_empty());
    }

    #[tokio::test]
    async fn in_place_settlement_retains_persisted_state_when_stanza_upgrade_fails() {
        let (spec, state, api) = in_place_fixture();
        let observations: Vec<_> = (0..3)
            .map(|i| obs(&format!("demo-{i}"), true, Some(170010)))
            .collect();
        let result = settle_in_place(
            &in_place_ops(api.clone()),
            "ns",
            &spec,
            &state,
            &observations,
        )
        .await;
        assert!(
            result
                .expect_err("unacknowledged stanza")
                .to_string()
                .contains("stanza upgrade was not acknowledged")
        );
        let live = api.lock().expect("api");
        let saved: ClusterSpec =
            serde_json::from_str(live.cm["data"]["spec"].as_str().expect("spec string"))
                .expect("saved spec");
        assert_eq!(saved.image, state.target_image);
        assert_eq!(
            live.cm["metadata"]["annotations"][pglord_core::upgrade::UPGRADE_ANNOTATION],
            serde_json::to_string(&state).expect("state")
        );
        assert!(live.deleted.is_empty());
    }

    #[test]
    fn in_place_replacement_requires_target_image_marker_and_nonterminating_pod() {
        let state = UpgradeState {
            target_image: "pglord:17".into(),
            next_generation: "g2".into(),
            phase: UpgradePhase::Upgrading,
            previous_primary: None,
            mode: UpgradeMode::InPlace,
            from_major: Some(16),
        };
        let mut pod: k8s_openapi::api::core::v1::Pod = serde_json::from_value(serde_json::json!({
            "metadata": {}, "spec": { "containers": [{ "name": "postgres", "image": "pglord:17",
                "env": [{ "name": "PGLORD_AGENT_UPGRADE_IN_PLACE", "value": "16" }] }] }
        }))
        .expect("pod");
        assert!(has_in_place_template(&pod, &state));
        let mut set: StatefulSet = serde_json::from_value(serde_json::json!({
            "metadata": {}, "spec": { "serviceName": "demo", "selector": {},
                "podManagementPolicy": "Parallel", "template": { "spec": pod.spec.clone() } }
        }))
        .expect("statefulset");
        assert!(in_place_controller_ready(&set, &state));
        set.spec.as_mut().expect("spec").pod_management_policy = Some("OrderedReady".into());
        assert!(!in_place_controller_ready(&set, &state));
        set.spec.as_mut().expect("spec").pod_management_policy = Some("Parallel".into());
        set.spec
            .as_mut()
            .expect("spec")
            .template
            .spec
            .as_mut()
            .expect("pod spec")
            .containers[0]
            .image = Some("pglord:16".into());
        assert!(!in_place_controller_ready(&set, &state));
        pod.spec.as_mut().expect("spec").containers[0].image = Some("pglord:16".into());
        assert!(!has_in_place_template(&pod, &state));
        pod.spec.as_mut().expect("spec").containers[0].image = Some("pglord:17".into());
        pod.spec.as_mut().expect("spec").containers[0].env = None;
        assert!(!has_in_place_template(&pod, &state));
    }

    fn agent(established: bool, ready: (i64, i64), lag: Option<i64>) -> AgentUpgradeStatus {
        AgentUpgradeStatus {
            established,
            complete: false,
            databases: vec![AgentDbSync {
                name: "appdb".into(),
                tables_total: ready.0,
                tables_ready: ready.1,
            }],
            lag_bytes: lag,
            error: None,
        }
    }

    #[test]
    fn ready_to_cutover_needs_every_table_and_low_lag() {
        assert!(ready_to_cutover(&agent(true, (3, 3), Some(0))));
        assert!(ready_to_cutover(&agent(
            true,
            (3, 3),
            Some(READY_LAG_BYTES)
        )));
        // A table still copying means rows that would be lost at cutover.
        assert!(!ready_to_cutover(&agent(true, (3, 2), Some(0))));
        assert!(!ready_to_cutover(&agent(
            true,
            (3, 3),
            Some(READY_LAG_BYTES + 1)
        )));
        // No lag reading at all: the source is unreachable, so we know nothing.
        assert!(!ready_to_cutover(&agent(true, (3, 3), None)));
        assert!(!ready_to_cutover(&agent(false, (3, 3), Some(0))));
    }

    #[test]
    fn a_cluster_with_no_user_databases_is_trivially_ready() {
        let st = AgentUpgradeStatus {
            established: true,
            complete: false,
            databases: vec![],
            lag_bytes: Some(0),
            error: None,
        };
        assert!(ready_to_cutover(&st));
    }

    fn obs(pod: &str, ready: bool, ver: Option<i32>) -> crate::instance_probe::InstanceObservation {
        crate::instance_probe::InstanceObservation {
            pod: pod.into(),
            ready,
            in_recovery: Some(false),
            lsn: None,
            pending_restart: false,
            sync_standby_count: None,
            ssl: None,
            wal_level: None,
            server_version_num: ver,
        }
    }

    #[test]
    fn in_place_completes_only_when_every_instance_is_past_the_old_major() {
        // Upgrading FROM 16: both instances now report 17.
        let done = vec![
            obs("demo-0", true, Some(170010)),
            obs("demo-1", true, Some(170010)),
        ];
        assert!(in_place_complete(&done, 16));
        // Skipping a major is just as complete.
        assert!(in_place_complete(&[obs("demo-0", true, Some(180002))], 16));

        // One still on the old major: the roll has not reached it.
        let mixed = vec![
            obs("demo-0", true, Some(170010)),
            obs("demo-1", true, Some(160013)),
        ];
        assert!(!in_place_complete(&mixed, 16));
        // One not ready yet — converting, or re-cloning.
        let booting = vec![
            obs("demo-0", true, Some(170010)),
            obs("demo-1", false, None),
        ];
        assert!(!in_place_complete(&booting, 16));
        // Unreachable instances report no version: never conclude success
        // from silence, or the state clears mid-conversion.
        let silent = vec![obs("demo-0", true, None)];
        assert!(!in_place_complete(&silent, 16));
        // An empty observation set is not a completed upgrade.
        assert!(!in_place_complete(&[], 16));
    }

    #[test]
    fn blue_green_verbs_reject_an_in_place_upgrade() {
        let in_place = UpgradeState {
            target_image: "pglord-agent:17".into(),
            next_generation: "g2".into(),
            phase: UpgradePhase::Upgrading,
            previous_primary: None,
            mode: UpgradeMode::InPlace,
            from_major: Some(16),
        };
        let err = reject_non_blue_green(&in_place).expect_err("must reject");
        assert_eq!(err.code(), "invalid_upgrade_phase");

        let blue_green = UpgradeState {
            mode: UpgradeMode::BlueGreen,
            phase: UpgradePhase::Migrating,
            ..in_place
        };
        assert!(reject_non_blue_green(&blue_green).is_ok());
    }

    #[test]
    fn finalize_retry_keeps_original_set_and_rejects_active_target() {
        let mut spec: ClusterSpec =
            serde_json::from_value(serde_json::json!({ "name": "demo" })).expect("spec");
        let mut state = UpgradeState {
            target_image: "pglord-agent:18".into(),
            next_generation: "g2".into(),
            phase: UpgradePhase::Soaking,
            previous_primary: Some("demo-1".into()),
            mode: UpgradeMode::BlueGreen,
            from_major: None,
        };
        assert_eq!(finalize_source_set(&spec, &state).expect("source"), "demo");
        spec.generation = "g2".into();
        assert_eq!(finalize_source_set(&spec, &state).expect("retry"), "demo");
        state.previous_primary = Some("demo-g2-0".into());
        assert!(finalize_source_set(&spec, &state).is_err());
        state.previous_primary = None;
        assert!(finalize_source_set(&spec, &state).is_err());
    }

    #[test]
    fn green_names_follow_the_generation() {
        let spec: ClusterSpec =
            serde_json::from_value(serde_json::json!({ "name": "demo" })).expect("spec");
        let state = UpgradeState {
            target_image: "pglord-agent:18".into(),
            next_generation: "g2".into(),
            phase: UpgradePhase::Migrating,
            previous_primary: None,
            mode: UpgradeMode::BlueGreen,
            from_major: None,
        };
        assert_eq!(green_sts_name(&spec, &state), "demo-g2");
        assert_eq!(green_primary_pod(&spec, &state), "demo-g2-0");
    }
}
