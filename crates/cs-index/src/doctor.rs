//! Corruption detection and diagnostic doctor engine for `cs-index`.
//!
//! # Responsibilities
//!
//! - **Integrity verification**: runs SQLite `PRAGMA quick_check` and `PRAGMA foreign_key_check`.
//! - **Schema and state validation**: checks `meta` keys (`schema_version`, `index_state`, `snapshot_id`).
//! - **Referential consistency**: detects dangling references in `binding_targets`, `edges`, `bindings`, `symbols`, `refs`, and `imports`.
//! - **Index health metrics**: reports parse status breakdown, import resolution rate, ref binding rate, and counts.
//! - **Repair and rebuild**: supports clean rebuild or resetting interrupted runs.

#![forbid(unsafe_code)]

use std::fmt;
use std::path::Path;

use rusqlite::{params, Connection, OpenFlags};

use crate::schema::{initialize, SCHEMA_VERSION};
use crate::{IndexDatabase, IndexError, IndexState};

/// Severity level of a doctor diagnostic finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticSeverity {
    /// Informational check that succeeded or provides operational metrics.
    Info,
    /// Warning indicating non-fatal anomalies (e.g. parse timeouts, interrupted builds).
    Warning,
    /// Severe corruption or integrity failure requiring repair or rebuild.
    Error,
}

impl fmt::Display for DiagnosticSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Info => write!(f, "INFO"),
            Self::Warning => write!(f, "WARN"),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

/// A diagnostic finding reported by [`Doctor::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorDiagnostic {
    /// Machine-readable code for the diagnostic check.
    pub code: &'static str,
    /// Severity level of the finding.
    pub severity: DiagnosticSeverity,
    /// Human-readable message summarizing the check result.
    pub message: String,
    /// Optional list of technical details or affected items.
    pub details: Vec<String>,
}

impl DoctorDiagnostic {
    /// Create an informational diagnostic.
    #[must_use]
    pub fn info(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: DiagnosticSeverity::Info,
            message: message.into(),
            details: Vec::new(),
        }
    }

    /// Create a warning diagnostic.
    #[must_use]
    pub fn warning(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            details: Vec::new(),
        }
    }

    /// Create a warning diagnostic with details.
    #[must_use]
    pub fn warning_with_details(
        code: &'static str,
        message: impl Into<String>,
        details: Vec<String>,
    ) -> Self {
        Self {
            code,
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
            details,
        }
    }

    /// Create an error diagnostic with details.
    #[must_use]
    pub fn error(code: &'static str, message: impl Into<String>, details: Vec<String>) -> Self {
        Self {
            code,
            severity: DiagnosticSeverity::Error,
            message: message.into(),
            details,
        }
    }
}

impl fmt::Display for DoctorDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.severity, self.code, self.message)?;
        for detail in &self.details {
            write!(f, "\n    - {detail}")?;
        }
        Ok(())
    }
}

/// Policy for [`Doctor::repair`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairPolicy {
    /// Drop all tables and recreate a fresh schema at [`IndexState::Empty`].
    Rebuild,
    /// Truncate all fact/derived tables and reset lifecycle state to [`IndexState::Empty`].
    ResetToEmpty,
}

/// Diagnostic and repair engine for ContextSlice SQLite index databases.
pub struct Doctor;

impl Doctor {
    /// Inspect an open SQLite connection and return all diagnostic findings.
    ///
    /// This method is non-destructive: it performs read-only checks and gathers
    /// metrics. Even if queries fail, it records diagnostic findings rather
    /// than aborting early.
    #[must_use]
    pub fn check(conn: &Connection) -> Vec<DoctorDiagnostic> {
        let mut diagnostics = Vec::new();

        // 1. SQLite Quick Check
        Self::check_quick_check(conn, &mut diagnostics);

        // 2. Foreign Key Checks
        Self::check_foreign_keys(conn, &mut diagnostics);

        // 3. Meta Table & Lifecycle State
        Self::check_meta_table(conn, &mut diagnostics);

        // 4. Schema Table Completeness
        let tables_ok = Self::check_required_tables(conn, &mut diagnostics);

        if tables_ok {
            // 5. Referential and Hash Invariants
            Self::check_referential_integrity(conn, &mut diagnostics);

            // 6. Statistics & Health Metrics
            Self::check_health_metrics(conn, &mut diagnostics);
        }

        diagnostics
    }

    /// Open an index file directly (without requiring successful full [`IndexDatabase`] validation)
    /// and run all diagnostic checks.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::Io`] if the file cannot be accessed.
    pub fn check_file(path: &Path) -> Result<Vec<DoctorDiagnostic>, IndexError> {
        if !path.exists() {
            return Ok(vec![DoctorDiagnostic::error(
                "FILE_NOT_FOUND",
                format!("Index file does not exist: {}", path.display()),
                Vec::new(),
            )]);
        }

        let conn = match Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(c) => c,
            Err(e) => {
                return Ok(vec![DoctorDiagnostic::error(
                    "CANNOT_OPEN",
                    format!("Failed to open SQLite database at {}: {e}", path.display()),
                    Vec::new(),
                )]);
            }
        };

        Ok(Self::check(&conn))
    }

    /// Repair an index connection according to the chosen [`RepairPolicy`].
    ///
    /// # Errors
    ///
    /// Returns [`rusqlite::Error`] if any DDL or DML repair command fails.
    pub fn repair(conn: &mut Connection, policy: RepairPolicy) -> Result<(), rusqlite::Error> {
        match policy {
            RepairPolicy::Rebuild => {
                conn.execute_batch(
                    "PRAGMA foreign_keys = OFF;
                     DROP TABLE IF EXISTS edges;
                     DROP TABLE IF EXISTS binding_targets;
                     DROP TABLE IF EXISTS bindings;
                     DROP TABLE IF EXISTS refs;
                     DROP TABLE IF EXISTS symbols;
                     DROP TABLE IF EXISTS imports;
                     DROP TABLE IF EXISTS manifests;
                     DROP TABLE IF EXISTS files;
                     DROP TABLE IF EXISTS packages;
                     DROP TABLE IF EXISTS meta;
                     PRAGMA foreign_keys = ON;",
                )?;
                let txn = conn.transaction()?;
                initialize(&txn)?;
                txn.commit()?;
                conn.execute_batch("VACUUM;")?;
            }
            RepairPolicy::ResetToEmpty => {
                conn.execute_batch(
                    "PRAGMA foreign_keys = OFF;
                     DELETE FROM edges;
                     DELETE FROM binding_targets;
                     DELETE FROM bindings;
                     DELETE FROM refs;
                     DELETE FROM symbols;
                     DELETE FROM imports;
                     DELETE FROM manifests;
                     DELETE FROM files;
                     DELETE FROM packages;
                     UPDATE meta SET value = 'empty' WHERE key = 'index_state';
                     DELETE FROM meta WHERE key = 'snapshot_id';
                     PRAGMA foreign_keys = ON;
                     VACUUM;",
                )?;
            }
        }
        Ok(())
    }

    /// Open an index file and apply the given repair policy.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError`] if opening or repairing fails.
    pub fn repair_file(path: &Path, policy: RepairPolicy) -> Result<(), IndexError> {
        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| IndexDatabase::classify(path, e))?;

        Self::repair(&mut conn, policy).map_err(|e| IndexDatabase::classify(path, e))
    }

    fn check_quick_check(conn: &Connection, diags: &mut Vec<DoctorDiagnostic>) {
        match conn.prepare("PRAGMA quick_check") {
            Ok(mut stmt) => {
                let mut errors = Vec::new();
                match stmt.query([]) {
                    Ok(mut rows) => {
                        while let Ok(Some(row)) = rows.next() {
                            if let Ok(res) = row.get::<_, String>(0) {
                                if res != "ok" {
                                    errors.push(res);
                                }
                            }
                        }
                        if errors.is_empty() {
                            diags.push(DoctorDiagnostic::info(
                                "SQLITE_INTEGRITY",
                                "SQLite quick_check passed without errors",
                            ));
                        } else {
                            diags.push(DoctorDiagnostic::error(
                                "SQLITE_INTEGRITY",
                                "SQLite quick_check reported corruption",
                                errors,
                            ));
                        }
                    }
                    Err(e) => {
                        diags.push(DoctorDiagnostic::error(
                            "SQLITE_INTEGRITY",
                            format!("Failed to execute PRAGMA quick_check: {e}"),
                            Vec::new(),
                        ));
                    }
                }
            }
            Err(e) => {
                diags.push(DoctorDiagnostic::error(
                    "SQLITE_INTEGRITY",
                    format!("Failed to prepare PRAGMA quick_check: {e}"),
                    Vec::new(),
                ));
            }
        }
    }

    fn check_foreign_keys(conn: &Connection, diags: &mut Vec<DoctorDiagnostic>) {
        match conn.prepare("PRAGMA foreign_key_check") {
            Ok(mut stmt) => match stmt.query([]) {
                Ok(mut rows) => {
                    let mut violations = Vec::new();
                    while let Ok(Some(row)) = rows.next() {
                        let table: String = row.get(0).unwrap_or_default();
                        let rowid: i64 = row.get(1).unwrap_or(-1);
                        let parent: String = row.get(2).unwrap_or_default();
                        let fkid: i64 = row.get(3).unwrap_or(-1);
                        violations.push(format!(
                            "table '{table}' rowid {rowid} references missing parent in '{parent}' (fkid {fkid})"
                        ));
                    }
                    if violations.is_empty() {
                        diags.push(DoctorDiagnostic::info(
                            "FOREIGN_KEYS",
                            "Foreign key constraints verified clean",
                        ));
                    } else {
                        diags.push(DoctorDiagnostic::error(
                            "FOREIGN_KEYS",
                            format!("Found {} foreign key violations", violations.len()),
                            violations,
                        ));
                    }
                }
                Err(e) => {
                    diags.push(DoctorDiagnostic::error(
                        "FOREIGN_KEYS",
                        format!("Failed to execute PRAGMA foreign_key_check: {e}"),
                        Vec::new(),
                    ));
                }
            },
            Err(e) => {
                diags.push(DoctorDiagnostic::error(
                    "FOREIGN_KEYS",
                    format!("Failed to prepare PRAGMA foreign_key_check: {e}"),
                    Vec::new(),
                ));
            }
        }
    }

    fn check_meta_table(conn: &Connection, diags: &mut Vec<DoctorDiagnostic>) {
        let has_meta: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);

        if !has_meta {
            diags.push(DoctorDiagnostic::error(
                "META_TABLE_MISSING",
                "Required 'meta' table does not exist",
                Vec::new(),
            ));
            return;
        }

        // Check schema version
        let schema_ver: Result<String, _> = conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        );

        match schema_ver {
            Ok(val) => {
                match val.parse::<i64>() {
                    Ok(v) if v == SCHEMA_VERSION => {
                        diags.push(DoctorDiagnostic::info(
                            "SCHEMA_VERSION",
                            format!("Schema version {v} matches current engine requirements"),
                        ));
                    }
                    Ok(v) => {
                        diags.push(DoctorDiagnostic::error(
                        "SCHEMA_VERSION_MISMATCH",
                        format!("Schema version {v} does not match engine requirement {SCHEMA_VERSION}"),
                        vec!["A rebuild is required: run `contextslice index --rebuild`".into()],
                    ));
                    }
                    Err(_) => {
                        diags.push(DoctorDiagnostic::error(
                            "SCHEMA_VERSION_INVALID",
                            format!("Non-integer schema_version stored in meta: '{val}'"),
                            Vec::new(),
                        ));
                    }
                }
            }
            Err(_) => {
                diags.push(DoctorDiagnostic::error(
                    "SCHEMA_VERSION_MISSING",
                    "Missing 'schema_version' in meta table",
                    Vec::new(),
                ));
            }
        }

        // Check lifecycle state
        let state_val: Result<String, _> = conn.query_row(
            "SELECT value FROM meta WHERE key = 'index_state'",
            [],
            |r| r.get(0),
        );

        let mut current_state = None;
        match state_val {
            Ok(val) => match IndexState::parse(&val) {
                Some(IndexState::Ready) => {
                    current_state = Some(IndexState::Ready);
                    diags.push(DoctorDiagnostic::info(
                        "INDEX_STATE",
                        "Index lifecycle state is 'ready'",
                    ));
                }
                Some(IndexState::Building) => {
                    current_state = Some(IndexState::Building);
                    diags.push(DoctorDiagnostic::warning(
                        "INDEX_STATE_BUILDING",
                        "Index build was interrupted or in progress ('building'); queries will be rejected until index completes",
                    ));
                }
                Some(IndexState::Empty) => {
                    current_state = Some(IndexState::Empty);
                    diags.push(DoctorDiagnostic::info(
                        "INDEX_STATE_EMPTY",
                        "Index is currently empty ('empty'); indexing has not run",
                    ));
                }
                None => {
                    diags.push(DoctorDiagnostic::error(
                        "INDEX_STATE_INVALID",
                        format!("Invalid index_state in meta table: '{val}'"),
                        Vec::new(),
                    ));
                }
            },
            Err(_) => {
                diags.push(DoctorDiagnostic::error(
                    "INDEX_STATE_MISSING",
                    "Missing 'index_state' in meta table",
                    Vec::new(),
                ));
            }
        }

        // Check snapshot_id
        let snapshot_val: Result<String, _> = conn.query_row(
            "SELECT value FROM meta WHERE key = 'snapshot_id'",
            [],
            |r| r.get(0),
        );

        match (current_state, snapshot_val) {
            (Some(IndexState::Ready), Ok(id)) if id.len() == 64 => {
                diags.push(DoctorDiagnostic::info(
                    "SNAPSHOT_ID",
                    format!("Snapshot ID verified: {id}"),
                ));
            }
            (Some(IndexState::Ready), Ok(id)) => {
                diags.push(DoctorDiagnostic::warning(
                    "SNAPSHOT_ID_MALFORMED",
                    format!("Snapshot ID is present but not 64 hex characters: '{id}'"),
                ));
            }
            (Some(IndexState::Ready), Err(_)) => {
                diags.push(DoctorDiagnostic::warning(
                    "SNAPSHOT_ID_MISSING",
                    "Index is marked 'ready' but no 'snapshot_id' was written to meta",
                ));
            }
            _ => {}
        }
    }

    fn check_required_tables(conn: &Connection, diags: &mut Vec<DoctorDiagnostic>) -> bool {
        let expected = [
            "packages",
            "files",
            "manifests",
            "symbols",
            "refs",
            "imports",
            "bindings",
            "binding_targets",
            "edges",
            "meta",
        ];

        let mut missing = Vec::new();
        for tbl in expected {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    params![tbl],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if !exists {
                missing.push(tbl.to_string());
            }
        }

        if missing.is_empty() {
            diags.push(DoctorDiagnostic::info(
                "SCHEMA_TABLES",
                "All 10 expected schema tables exist",
            ));
            true
        } else {
            diags.push(DoctorDiagnostic::error(
                "SCHEMA_TABLES_MISSING",
                format!("Missing {} expected schema tables", missing.len()),
                missing,
            ));
            false
        }
    }

    fn check_referential_integrity(conn: &Connection, diags: &mut Vec<DoctorDiagnostic>) {
        // Files consistency check matching schema constraints:
        // 1. skip = 'too_large' <=> hash IS NULL
        // 2. hash IS NOT NULL OR parse_status = 'skipped'
        let files_inconsistent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files \
                 WHERE (skip = 'too_large' AND hash IS NOT NULL) \
                    OR (skip != 'too_large' AND hash IS NULL) \
                    OR (skip IS NULL AND hash IS NULL) \
                    OR (hash IS NULL AND parse_status != 'skipped')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        if files_inconsistent > 0 {
            diags.push(DoctorDiagnostic::error(
                "FILES_CONSISTENCY",
                format!(
                    "{files_inconsistent} files rows violate hash/skip consistency constraints"
                ),
                Vec::new(),
            ));
        }

        // Dangling binding targets: target file_id missing in files
        let dangling_bt_files: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM binding_targets bt \
                 LEFT JOIN files f ON f.id = bt.file_id \
                 WHERE f.id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        // Dangling edges: src or dst missing in files
        let dangling_edges: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM edges e \
                 LEFT JOIN files f1 ON f1.id = e.src \
                 LEFT JOIN files f2 ON f2.id = e.dst \
                 WHERE f1.id IS NULL OR f2.id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        // Self-edges check: src == dst
        let self_edges: i64 = conn
            .query_row("SELECT COUNT(*) FROM edges WHERE src = dst", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);

        let mut dangling_details = Vec::new();
        if dangling_bt_files > 0 {
            dangling_details.push(format!(
                "{dangling_bt_files} binding_targets reference missing files"
            ));
        }
        if dangling_edges > 0 {
            dangling_details.push(format!("{dangling_edges} edges reference missing files"));
        }
        if self_edges > 0 {
            dangling_details.push(format!(
                "{self_edges} illegal self-edges (src == dst) detected"
            ));
        }

        if dangling_details.is_empty() {
            diags.push(DoctorDiagnostic::info(
                "REFERENTIAL_INTEGRITY",
                "Referential integrity verified: zero dangling rows or self-edges",
            ));
        } else {
            diags.push(DoctorDiagnostic::error(
                "REFERENTIAL_INTEGRITY_VIOLATIONS",
                "Detected referential integrity violations",
                dangling_details,
            ));
        }
    }

    fn check_health_metrics(conn: &Connection, diags: &mut Vec<DoctorDiagnostic>) {
        // Files breakdown
        let file_stats: Result<(i64, i64, i64, i64, i64, i64), _> = conn.query_row(
            "SELECT COUNT(*), \
                    SUM(CASE WHEN parse_status = 'ok' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN parse_status = 'partial' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN parse_status = 'timeout' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN parse_status = 'error' THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN parse_status = 'skipped' THEN 1 ELSE 0 END) \
             FROM files",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        );

        if let Ok((total, ok, partial, timeout, err_count, skipped)) = file_stats {
            diags.push(DoctorDiagnostic::info(
                "FILE_METRICS",
                format!("{total} files: {ok} ok, {partial} partial, {timeout} timeout, {err_count} error, {skipped} skipped"),
            ));

            if timeout > 0 || err_count > 0 {
                let mut problem_files = Vec::new();
                if let Ok(mut stmt) = conn.prepare(
                    "SELECT path, parse_status FROM files WHERE parse_status IN ('timeout', 'error') LIMIT 10",
                ) {
                    if let Ok(mut rows) = stmt.query([]) {
                        while let Ok(Some(row)) = rows.next() {
                            let path: String = row.get(0).unwrap_or_default();
                            let status: String = row.get(1).unwrap_or_default();
                            problem_files.push(format!("{path} ({status})"));
                        }
                    }
                }
                diags.push(DoctorDiagnostic::warning_with_details(
                    "PARSE_ANOMALIES",
                    format!("{timeout} parse timeouts and {err_count} parse errors recorded"),
                    problem_files,
                ));
            }
        }

        // Package, symbol, edge counts
        let packages_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM packages", [], |r| r.get(0))
            .unwrap_or(0);
        let symbols_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
            .unwrap_or(0);
        let refs_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM refs", [], |r| r.get(0))
            .unwrap_or(0);
        let edges_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
            .unwrap_or(0);

        diags.push(DoctorDiagnostic::info(
            "GRAPH_METRICS",
            format!("{packages_count} packages, {symbols_count} symbols, {refs_count} references, {edges_count} edges"),
        ));

        // Import resolution rate
        let import_stats: Result<(i64, i64, i64), _> = conn.query_row(
            "SELECT COUNT(*), \
                    SUM(CASE WHEN resolved_dir IS NOT NULL OR resolved_file IS NOT NULL THEN 1 ELSE 0 END), \
                    SUM(CASE WHEN unresolved_reason IS NOT NULL THEN 1 ELSE 0 END) \
             FROM imports",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        );

        if let Ok((total_imports, resolved_imports, unresolved_imports)) = import_stats {
            if total_imports > 0 {
                #[allow(clippy::cast_precision_loss)]
                let rate = (resolved_imports as f64 / total_imports as f64) * 100.0;
                diags.push(DoctorDiagnostic::info(
                    "IMPORT_RESOLUTION_RATE",
                    format!("{rate:.1}% of imports resolved ({resolved_imports}/{total_imports}; {unresolved_imports} unresolved)"),
                ));
            }
        }

        // Reference binding rate
        let bound_refs: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT ref_id) FROM binding_targets",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let unbound_refs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM bindings WHERE unbound_reason IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        if refs_count > 0 {
            #[allow(clippy::cast_precision_loss)]
            let rate = (bound_refs as f64 / refs_count as f64) * 100.0;
            diags.push(DoctorDiagnostic::info(
                "REF_BINDING_RATE",
                format!("{rate:.1}% of references bound ({bound_refs}/{refs_count}; {unbound_refs} explicit unbounds)"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{ingest_facts, DEFAULT_CONFIG_FINGERPRINT, DEFAULT_FACT_BATCH_SIZE};
    use crate::IndexDatabase;
    use cs_scanner::{scan, ScanConfig};

    #[test]
    fn doctor_reports_clean_on_fresh_empty_database() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("index.db");
        let db = IndexDatabase::open_or_create(&db_path).expect("open");

        let diags = Doctor::check(db.connection());
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .collect();
        assert!(
            errors.is_empty(),
            "Fresh database should have zero errors: {errors:?}"
        );

        let has_integrity = diags.iter().any(|d| d.code == "SQLITE_INTEGRITY");
        let has_fks = diags.iter().any(|d| d.code == "FOREIGN_KEYS");
        let has_empty_state = diags.iter().any(|d| d.code == "INDEX_STATE_EMPTY");
        assert!(has_integrity);
        assert!(has_fks);
        assert!(has_empty_state);
    }

    #[test]
    fn doctor_reports_full_health_on_indexed_fixture() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).expect("open");

        let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("fixtures/go-resolve");

        let scanned = scan(&fixture_root, &ScanConfig::default()).expect("scan");
        db.begin_build(DEFAULT_CONFIG_FINGERPRINT)
            .expect("begin_build");
        ingest_facts(&mut db, &fixture_root, &scanned, DEFAULT_FACT_BATCH_SIZE).expect("ingest");
        db.resolve_facts().expect("resolve_facts");

        let diags = Doctor::check(db.connection());
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .collect();
        assert!(
            errors.is_empty(),
            "Indexed fixture must have zero errors: {errors:?}"
        );

        assert!(diags.iter().any(|d| d.code == "INDEX_STATE"));
        assert!(diags.iter().any(|d| d.code == "SNAPSHOT_ID"));
        assert!(diags.iter().any(|d| d.code == "FILE_METRICS"));
        assert!(diags.iter().any(|d| d.code == "GRAPH_METRICS"));
        assert!(diags.iter().any(|d| d.code == "IMPORT_RESOLUTION_RATE"));
        assert!(diags.iter().any(|d| d.code == "REF_BINDING_RATE"));
    }

    #[test]
    fn doctor_detects_interrupted_building_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).expect("open");
        db.begin_build(DEFAULT_CONFIG_FINGERPRINT)
            .expect("begin_build");

        let diags = Doctor::check(db.connection());
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.code == "INDEX_STATE_BUILDING")
            .collect();
        assert_eq!(
            warnings.len(),
            1,
            "Interrupted building state must produce warning"
        );
    }

    #[test]
    fn doctor_detects_foreign_key_violation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("index.db");
        let db = IndexDatabase::open_or_create(&db_path).expect("open");

        // Insert illegal row with FK temporarily disabled
        db.connection()
            .execute_batch("PRAGMA foreign_keys = OFF;")
            .unwrap();
        db.connection()
            .execute(
                "INSERT INTO files (id, path, lang, package_id, hash, skip, size, mtime, parse_status) \
                 VALUES (1, 'bad.go', 'go', 9999, X'01', NULL, 10, 100, 'ok')",
                [],
            )
            .unwrap();
        db.connection()
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();

        let diags = Doctor::check(db.connection());
        let fk_errors: Vec<_> = diags
            .iter()
            .filter(|d| d.code == "FOREIGN_KEYS" && d.severity == DiagnosticSeverity::Error)
            .collect();
        assert_eq!(
            fk_errors.len(),
            1,
            "Missing parent package must trigger foreign key error"
        );
    }

    #[test]
    fn doctor_repair_resets_building_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).expect("open");
        db.begin_build(DEFAULT_CONFIG_FINGERPRINT)
            .expect("begin_build");

        Doctor::repair(db.connection_mut(), RepairPolicy::ResetToEmpty).expect("repair");

        let diags = Doctor::check(db.connection());
        assert!(diags.iter().any(|d| d.code == "INDEX_STATE_EMPTY"));
        assert!(!diags.iter().any(|d| d.code == "INDEX_STATE_BUILDING"));
    }

    #[test]
    fn doctor_repair_rebuild_drops_and_recreates_schema() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).expect("open");

        // Corrupt table structure
        db.connection()
            .execute_batch("DROP TABLE edges; DROP TABLE symbols;")
            .unwrap();

        let diags_before = Doctor::check(db.connection());
        assert!(diags_before
            .iter()
            .any(|d| d.code == "SCHEMA_TABLES_MISSING"));

        Doctor::repair(db.connection_mut(), RepairPolicy::Rebuild).expect("repair rebuild");

        let diags_after = Doctor::check(db.connection());
        assert!(diags_after.iter().any(|d| d.code == "SCHEMA_TABLES"));
        assert!(!diags_after
            .iter()
            .any(|d| d.code == "SCHEMA_TABLES_MISSING"));
    }

    #[test]
    fn doctor_check_file_handles_missing_file() {
        let missing = Path::new("/nonexistent/path/to/index.db");
        let diags = Doctor::check_file(missing).expect("check_file");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "FILE_NOT_FOUND");
    }
}
