//! Hot-standby GUC floor. PostgreSQL refuses standby recovery when
//! max_connections-class settings are below the primary's *live* values
//! (which the standby's own control file tracks via WAL). This module
//! renders an include that raises them just enough, so a rebuilt standby
//! can always start; the spec keeps recording the user's intent.
//!
//! `max_wal_senders` is subject to the same rule but needs no clamp: the
//! agent pins it to a constant after the user includes on every instance
//! (see the bootstrap conf), so it can never diverge.

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

const HEADER: &str = "# Managed by pglord — hot-standby floor from the primary's live settings.\n";

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
        (
            "max_connections",
            spec_or("max_connections", 100),
            live.max_connections,
        ),
        (
            "max_worker_processes",
            spec_or("max_worker_processes", 8),
            live.max_worker_processes,
        ),
        (
            "max_prepared_transactions",
            spec_or("max_prepared_transactions", 0),
            live.max_prepared_transactions,
        ),
        (
            "max_locks_per_transaction",
            spec_or("max_locks_per_transaction", 64),
            live.max_locks_per_transaction,
        ),
    ];
    for (name, effective, live_value) in gucs {
        if live_value > effective {
            out.push_str(&format!("{name} = {live_value}\n"));
        }
    }
    out
}

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
        assert_eq!(render(&params, Some(&live)), HEADER);
    }

    #[test]
    fn unparseable_spec_value_falls_back_to_default() {
        // "oops" can't be compared, so the default (100) is the effective
        // floor and live 150 must win.
        let params = BTreeMap::from([("max_connections".to_string(), "oops".to_string())]);
        let live = parse_controldata(CONTROLDATA).expect("parse");
        assert!(render(&params, Some(&live)).contains("max_connections = 150\n"));
    }

    #[test]
    fn primary_renders_header_only() {
        assert_eq!(render(&BTreeMap::new(), None), HEADER);
    }
}
