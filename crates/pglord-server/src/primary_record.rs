//! The primary record: which pod is allowed to accept writes. Stored as an
//! annotation on the spec ConfigMap and only ever changed via CAS on
//! resourceVersion. CAS orders decisions; positive old-primary fencing is also
//! required before handing write authority to a different instance.

use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, Patch, PatchParams};

pub use pglord_core::PRIMARY_ANNOTATION;

/// Durable one-way claim: rollback must not race deletion during finalization.
pub const FINALIZING_ANNOTATION: &str = "pglord.dev/finalizing";

/// The recorded primary pod name; absent annotation = `default_pod`, which
/// callers derive from the spec (`ClusterSpec::instance_name(0)`) so a
/// generation-suffixed cluster bootstraps onto its own ordinal zero.
pub fn recorded_primary(cm: &ConfigMap, default_pod: &str) -> String {
    cm.metadata
        .annotations
        .as_ref()
        .and_then(|a| a.get(PRIMARY_ANNOTATION))
        .cloned()
        .unwrap_or_else(|| default_pod.to_string())
}

/// Compare-and-swap a set of annotations on the spec ConfigMap in one merge
/// patch; a null value removes its key. `expected_rv` must be the
/// resourceVersion the decision was based on, so a concurrent writer makes the
/// apiserver reject the patch (409) and we return Ok(false).
///
/// One patch, not several: a major-version cutover moves the primary record
/// and the upgrade phase together, and an observer that saw one without the
/// other would either send writes to a cluster nobody had cut over to, or
/// treat a completed cutover as still in progress.
pub async fn cas_set_annotations(
    api: &Api<ConfigMap>,
    cm_name: &str,
    expected_rv: &str,
    annotations: serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<bool> {
    let patch = serde_json::json!({
        "metadata": { "resourceVersion": expected_rv, "annotations": annotations }
    });
    match api
        .patch(cm_name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Compare-and-swap the primary record.
pub async fn cas_set_primary(
    api: &Api<ConfigMap>,
    cm_name: &str,
    expected_rv: &str,
    new_primary: &str,
) -> anyhow::Result<bool> {
    let mut annotations = serde_json::Map::new();
    annotations.insert(
        PRIMARY_ANNOTATION.to_string(),
        serde_json::Value::String(new_primary.to_string()),
    );
    cas_set_annotations(api, cm_name, expected_rv, annotations).await
}

/// Parse a Postgres LSN ("X/Y" in hex) into a comparable integer.
pub fn parse_lsn(lsn: &str) -> Option<u64> {
    let (hi, lo) = lsn.split_once('/')?;
    let hi = u64::from_str_radix(hi, 16).ok()?;
    let lo = u64::from_str_radix(lo, 16).ok()?;
    Some((hi << 32) | lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cm_with(annotation: Option<&str>) -> ConfigMap {
        let mut cm = ConfigMap::default();
        if let Some(p) = annotation {
            cm.metadata.annotations =
                Some([(PRIMARY_ANNOTATION.to_string(), p.to_string())].into());
        }
        cm
    }

    #[test]
    fn absent_annotation_defaults_to_ordinal_zero() {
        assert_eq!(recorded_primary(&cm_with(None), "demo-0"), "demo-0");
        assert_eq!(
            recorded_primary(&cm_with(Some("demo-2")), "demo-0"),
            "demo-2"
        );
    }

    #[test]
    fn lsn_parsing_orders_correctly() {
        assert_eq!(parse_lsn("0/3000000"), Some(0x3000000));
        assert_eq!(parse_lsn("1/ABCDEF00"), Some((1u64 << 32) | 0xABCDEF00));
        assert!(parse_lsn("0/3000000").expect("lsn") < parse_lsn("1/0").expect("lsn"));
        assert_eq!(parse_lsn("garbage"), None);
        assert_eq!(parse_lsn("1-2"), None);
    }

    #[test]
    fn lsn_parsing_handles_edge_inputs() {
        assert_eq!(parse_lsn("0/0"), Some(0));
        // Postgres prints uppercase hex; accept either so a future format
        // change cannot silently make every candidate ineligible.
        assert_eq!(parse_lsn("a/ff"), parse_lsn("A/FF"));
        // A high segment must dominate the offset when ordering candidates.
        assert!(parse_lsn("2/0").expect("lsn") > parse_lsn("1/FFFFFFFF").expect("lsn"));
        for bad in [
            "", "/", "0/", "/0", "0//0", "0/1/2", "0x1/0", " 0/1", "zz/1",
        ] {
            assert_eq!(parse_lsn(bad), None, "expected None for {bad:?}");
        }
    }

    #[test]
    fn recorded_primary_ignores_unrelated_annotations() {
        let mut cm = ConfigMap::default();
        cm.metadata.annotations = Some(
            [
                ("other.io/thing".to_string(), "demo-9".to_string()),
                (
                    "kubectl.kubernetes.io/last-applied-configuration".to_string(),
                    "{}".to_string(),
                ),
            ]
            .into(),
        );
        assert_eq!(recorded_primary(&cm, "demo-0"), "demo-0");

        // Empty annotation map behaves like no annotations at all.
        cm.metadata.annotations = Some(Default::default());
        assert_eq!(recorded_primary(&cm, "demo-0"), "demo-0");
    }
}
