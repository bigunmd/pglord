//! Synchronous replication: the managed conf include that sets
//! `synchronous_standby_names` on every instance.
//!
//! Each instance's file names all pods of the cluster *except itself*, so the
//! value is already correct from any instance's point of view the moment it is
//! promoted — failover needs no sync reconfiguration. Listing a down standby
//! is harmless: postgres only waits on connected ones, and none connected
//! means blocked writes, which is exactly the strict semantics of the flag.

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
