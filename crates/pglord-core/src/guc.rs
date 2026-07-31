//! Catalog of tunable PostgreSQL settings.
//!
//! Deliberately curated rather than exhaustive: these are the settings people
//! actually tune, each with the metadata the UI needs to guide the change —
//! most importantly whether it applies live or needs a restart. The catalog's
//! `apply` field is advisory (it drives the UI hint); Postgres'
//! `pg_settings.pending_restart` remains the source of truth for what is
//! actually outstanding on a running instance.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ApplyMethod {
    /// Takes effect on `pg_ctl reload`.
    Reload,
    /// Requires a PostgreSQL restart.
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum GucType {
    Integer,
    Real,
    Bool,
    /// Size with unit, e.g. `128MB`.
    Memory,
    /// Time with unit, e.g. `30s`, or a bare integer in the setting's unit.
    Duration,
    /// One of a fixed set of words.
    Enum,
    /// Free-form text.
    StringLike,
}

#[derive(Debug, Clone, Copy, serde::Serialize, utoipa::ToSchema)]
pub struct Guc {
    pub name: &'static str,
    pub kind: GucType,
    pub apply: ApplyMethod,
    pub category: &'static str,
    pub summary: &'static str,
    pub default: &'static str,
}

/// Settings pglord manages itself. HA correctness depends on them, so they are
/// never user-overridable.
pub const RESERVED: &[&str] = &[
    "listen_addresses",
    "port",
    "wal_level",
    "max_wal_senders",
    "hot_standby",
    "wal_log_hints",
    "primary_conninfo",
    "hba_file",
    "data_directory",
];

macro_rules! guc {
    ($name:literal, $kind:ident, $apply:ident, $cat:literal, $default:literal, $summary:literal) => {
        Guc {
            name: $name,
            kind: GucType::$kind,
            apply: ApplyMethod::$apply,
            category: $cat,
            summary: $summary,
            default: $default,
        }
    };
}

const CATALOG: &[Guc] = &[
    // Memory
    guc!(
        "shared_buffers",
        Memory,
        Restart,
        "Memory",
        "128MB",
        "Memory for shared data cache. Commonly 25% of container memory."
    ),
    guc!(
        "work_mem",
        Memory,
        Reload,
        "Memory",
        "4MB",
        "Memory per sort or hash operation, per query node. Raise for analytics."
    ),
    guc!(
        "maintenance_work_mem",
        Memory,
        Reload,
        "Memory",
        "64MB",
        "Memory for VACUUM, CREATE INDEX and similar maintenance work."
    ),
    guc!(
        "effective_cache_size",
        Memory,
        Reload,
        "Memory",
        "4GB",
        "Planner's estimate of cache available. Does not allocate memory."
    ),
    guc!(
        "wal_buffers",
        Memory,
        Restart,
        "Memory",
        "-1",
        "Shared memory for WAL not yet written to disk."
    ),
    // Connections
    guc!(
        "max_connections",
        Integer,
        Restart,
        "Connections",
        "100",
        "Maximum concurrent connections. Prefer a pooler over a large value."
    ),
    guc!(
        "idle_in_transaction_session_timeout",
        Duration,
        Reload,
        "Connections",
        "0",
        "Terminate sessions idle in a transaction beyond this time. 0 disables."
    ),
    guc!(
        "statement_timeout",
        Duration,
        Reload,
        "Connections",
        "0",
        "Abort any statement running longer than this. 0 disables."
    ),
    guc!(
        "lock_timeout",
        Duration,
        Reload,
        "Connections",
        "0",
        "Abort statements waiting longer than this for a lock. 0 disables."
    ),
    // Write-ahead log and checkpoints
    guc!(
        "synchronous_commit",
        Enum,
        Reload,
        "Durability",
        "on",
        "Whether commits wait for WAL flush. 'off' trades durability for speed."
    ),
    guc!(
        "max_wal_size",
        Memory,
        Reload,
        "Durability",
        "1GB",
        "WAL size that triggers a checkpoint. Larger means fewer checkpoints."
    ),
    guc!(
        "min_wal_size",
        Memory,
        Reload,
        "Durability",
        "80MB",
        "WAL kept for reuse instead of being removed."
    ),
    guc!(
        "checkpoint_timeout",
        Duration,
        Reload,
        "Durability",
        "5min",
        "Maximum time between automatic checkpoints."
    ),
    guc!(
        "checkpoint_completion_target",
        Real,
        Reload,
        "Durability",
        "0.9",
        "Fraction of the interval over which a checkpoint spreads its writes."
    ),
    // Planner
    guc!(
        "random_page_cost",
        Real,
        Reload,
        "Planner",
        "4.0",
        "Cost of a random page read. Around 1.1 suits SSDs."
    ),
    guc!(
        "effective_io_concurrency",
        Integer,
        Reload,
        "Planner",
        "1",
        "Concurrent disk operations the planner assumes are possible."
    ),
    guc!(
        "default_statistics_target",
        Integer,
        Reload,
        "Planner",
        "100",
        "Detail of column statistics collected by ANALYZE."
    ),
    guc!(
        "max_worker_processes",
        Integer,
        Restart,
        "Planner",
        "8",
        "Background worker processes the cluster supports."
    ),
    guc!(
        "max_parallel_workers",
        Integer,
        Reload,
        "Planner",
        "8",
        "Workers available for parallel queries."
    ),
    guc!(
        "max_parallel_workers_per_gather",
        Integer,
        Reload,
        "Planner",
        "2",
        "Parallel workers a single query node may use."
    ),
    // Autovacuum
    guc!(
        "autovacuum_max_workers",
        Integer,
        Restart,
        "Autovacuum",
        "3",
        "Autovacuum processes that may run at once."
    ),
    guc!(
        "autovacuum_naptime",
        Duration,
        Reload,
        "Autovacuum",
        "1min",
        "Delay between autovacuum runs."
    ),
    guc!(
        "autovacuum_vacuum_scale_factor",
        Real,
        Reload,
        "Autovacuum",
        "0.2",
        "Fraction of a table that must change before VACUUM. Lower for big tables."
    ),
    guc!(
        "autovacuum_analyze_scale_factor",
        Real,
        Reload,
        "Autovacuum",
        "0.1",
        "Fraction of a table that must change before ANALYZE."
    ),
    // Logging and observability
    guc!(
        "log_min_duration_statement",
        Duration,
        Reload,
        "Logging",
        "-1",
        "Log statements slower than this. 0 logs all, -1 disables."
    ),
    guc!(
        "log_statement",
        Enum,
        Reload,
        "Logging",
        "none",
        "Which statements to log: none, ddl, mod or all."
    ),
    guc!(
        "log_connections",
        Bool,
        Reload,
        "Logging",
        "off",
        "Log each successful connection."
    ),
    guc!(
        "log_disconnections",
        Bool,
        Reload,
        "Logging",
        "off",
        "Log the end of each session and its duration."
    ),
    guc!(
        "log_lock_waits",
        Bool,
        Reload,
        "Logging",
        "off",
        "Log sessions waiting longer than deadlock_timeout for a lock."
    ),
    guc!(
        "track_io_timing",
        Bool,
        Reload,
        "Logging",
        "off",
        "Collect I/O timing statistics. Small overhead, useful for tuning."
    ),
    // Locale
    guc!(
        "timezone",
        StringLike,
        Reload,
        "Locale",
        "UTC",
        "Time zone for displaying and interpreting timestamps."
    ),
];

pub fn catalog() -> &'static [Guc] {
    CATALOG
}

pub fn lookup(name: &str) -> Option<&'static Guc> {
    CATALOG.iter().find(|g| g.name == name)
}

pub fn is_reserved(name: &str) -> bool {
    RESERVED.contains(&name)
}

/// Validate a value against its setting's type. Deliberately permissive about
/// ranges (Postgres enforces those authoritatively) and strict about shape,
/// which is what catches typos before they reach a crashloop.
pub fn validate_value(guc: &Guc, value: &str) -> Result<(), String> {
    let v = value.trim();
    if v.is_empty() {
        return Err("value must not be empty".to_string());
    }
    match guc.kind {
        GucType::Integer => v
            .parse::<i64>()
            .map(|_| ())
            .map_err(|_| format!("'{v}' is not an integer")),
        GucType::Real => v
            .parse::<f64>()
            .map(|_| ())
            .map_err(|_| format!("'{v}' is not a number")),
        GucType::Bool => match v.to_ascii_lowercase().as_str() {
            "on" | "off" | "true" | "false" | "yes" | "no" | "1" | "0" => Ok(()),
            _ => Err(format!("'{v}' is not a boolean (use on or off)")),
        },
        GucType::Memory => validate_unit(v, &["B", "kB", "KB", "MB", "GB", "TB"], "size"),
        GucType::Duration => validate_unit(v, &["us", "ms", "s", "min", "h", "d"], "duration"),
        // Enum values differ per setting and Postgres rejects bad ones clearly;
        // shape validation here would just duplicate that badly.
        GucType::Enum | GucType::StringLike => Ok(()),
    }
}

fn validate_unit(v: &str, units: &[&str], what: &str) -> Result<(), String> {
    if v.parse::<i64>().is_ok() {
        return Ok(()); // bare number: the setting's own unit
    }
    let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return Err(format!("'{v}' is not a {what} (expected e.g. 128MB)"));
    }
    let unit = v[digits.len()..].trim();
    if units.contains(&unit) {
        Ok(())
    } else {
        Err(format!(
            "'{v}' has an unknown unit '{unit}' (expected one of {})",
            units.join(", ")
        ))
    }
}

/// Render parameters as a postgresql.conf fragment. Sorted so the file is
/// stable — the agent compares content to decide whether to reload.
pub fn render_parameters(params: &BTreeMap<String, String>) -> String {
    if params.is_empty() {
        return String::new();
    }
    let mut out = String::from("# Managed by pglord. Edited through the pglord API/UI.\n");
    for (name, value) in params {
        let needs_quotes = lookup(name)
            .map(|g| matches!(g.kind, GucType::StringLike | GucType::Enum))
            .unwrap_or(true);
        if needs_quotes {
            out.push_str(&format!("{name} = '{}'\n", value.replace('\'', "''")));
        } else {
            out.push_str(&format!("{name} = {value}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_well_formed() {
        let mut names: Vec<&str> = CATALOG.iter().map(|g| g.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate settings in the catalog");
        for g in CATALOG {
            assert!(
                !is_reserved(g.name),
                "{} is reserved and must not be tunable",
                g.name
            );
            assert!(!g.summary.is_empty());
        }
        // The settings people reach for first must be present.
        for expected in ["shared_buffers", "work_mem", "max_connections"] {
            assert!(lookup(expected).is_some(), "missing {expected}");
        }
    }

    #[test]
    fn restart_classification_matches_postgres() {
        assert_eq!(
            lookup("shared_buffers").expect("guc").apply,
            ApplyMethod::Restart
        );
        assert_eq!(
            lookup("max_connections").expect("guc").apply,
            ApplyMethod::Restart
        );
        assert_eq!(lookup("work_mem").expect("guc").apply, ApplyMethod::Reload);
        assert_eq!(
            lookup("log_min_duration_statement").expect("guc").apply,
            ApplyMethod::Reload
        );
    }

    #[test]
    fn value_shapes_are_validated() {
        let mem = lookup("shared_buffers").expect("guc");
        assert!(validate_value(mem, "128MB").is_ok());
        assert!(validate_value(mem, "2GB").is_ok());
        assert!(validate_value(mem, "1024").is_ok()); // bare number = setting's unit
        assert!(validate_value(mem, "128 megs").is_err());
        assert!(validate_value(mem, "lots").is_err());
        assert!(validate_value(mem, "").is_err());

        let int = lookup("max_connections").expect("guc");
        assert!(validate_value(int, "200").is_ok());
        assert!(validate_value(int, "200MB").is_err());

        let boolean = lookup("log_connections").expect("guc");
        assert!(validate_value(boolean, "on").is_ok());
        assert!(validate_value(boolean, "FALSE").is_ok());
        assert!(validate_value(boolean, "maybe").is_err());

        let dur = lookup("statement_timeout").expect("guc");
        assert!(validate_value(dur, "30s").is_ok());
        assert!(validate_value(dur, "5min").is_ok());
        assert!(validate_value(dur, "30 secs").is_err());

        let real = lookup("random_page_cost").expect("guc");
        assert!(validate_value(real, "1.1").is_ok());
        assert!(validate_value(real, "fast").is_err());
    }

    #[test]
    fn rendering_is_stable_and_quotes_text_values() {
        let params = BTreeMap::from([
            ("work_mem".to_string(), "16MB".to_string()),
            ("log_statement".to_string(), "ddl".to_string()),
        ]);
        let rendered = render_parameters(&params);
        // Sorted order keeps the file byte-identical across reconciles.
        let body: Vec<&str> = rendered.lines().skip(1).collect();
        assert_eq!(body, vec!["log_statement = 'ddl'", "work_mem = 16MB"]);
        assert_eq!(render_parameters(&BTreeMap::new()), "");
    }

    #[test]
    fn reserved_settings_are_recognized() {
        assert!(is_reserved("wal_level"));
        assert!(is_reserved("listen_addresses"));
        assert!(!is_reserved("work_mem"));
    }
}
