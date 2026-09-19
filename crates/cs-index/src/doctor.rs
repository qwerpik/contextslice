//! Corruption detection and diagnostic doctor engine for `cs-index`.
//!
//! # Responsibilities
//!
//! - **Integrity verification**: runs SQLite `PRAGMA quick_check` and `PRAGMA foreign_key_check`.
//! - **Schema and state validation**: checks `meta` keys (`schema_version`, `index_state`, `snapshot_id`).
//! - **Referential consistency**: detects dangling references in `binding_targets`, `edges`, `bindings`, `symbols`, `refs`, and `imports`.
//! - **Projection completeness**: detects references without bindings rows and unclassified imports (a torn derived write), and surfaces a lingering incremental-update marker.
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

/// Up to ten sample paths for a diagnostic's details (deterministic: sorted).
fn sample_paths(conn: &Connection, sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(sql) {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(row)) = rows.next() {
                if let Ok(path) = row.get::<_, String>(0) {
                    out.push(path);
                }
                if out.len() == 10 {
                    break;
                }
            }
        }
    }
    out
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

            // 6. Derived Projection Invariants
            Self::check_projection_completeness(conn, &mut diagnostics);

            // 7. Statistics & Health Metrics
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

        // A read-only diagnostic handle cannot re-journal the file it is
        // diagnosing, so only the fail-fast half of the connection contract
        // applies here; without it rusqlite's default 5-second busy timeout
        // turns a locked index into a stall before the first diagnostic.
        if let Err(e) = conn.execute_batch("PRAGMA busy_timeout = 0") {
            return Ok(vec![DoctorDiagnostic::error(
                "CANNOT_OPEN",
                format!(
                    "Failed to configure read-only connection at {}: {e}",
                    path.display()
                ),
                Vec::new(),
            )]);
        }

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
                     DELETE FROM meta WHERE key = 'update_in_progress';
                     PRAGMA foreign_keys = ON;
                     VACUUM;",
                )?;
            }
        }
        Ok(())
    }

    /// Open an index file and apply the given repair policy.
    ///
    /// The connection carries the same contract as
    /// [`IndexDatabase::open_or_create`] (WAL, `synchronous=NORMAL`,
    /// `foreign_keys=ON`, no busy timeout), and its failures are classified
    /// the same way.
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

        IndexDatabase::apply_connection_contract(&conn, path)?;

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

        // Torn incremental update: an update's fact transaction committed but
        // its finalize (snapshot_id + marker clear) never ran.
        Self::check_torn_update_marker(conn, diags);
    }

    /// Surface a lingering incremental-update marker: facts are new while
    /// derived rows may lag or be missing; the next incremental run repairs
    /// it.
    fn check_torn_update_marker(conn: &Connection, diags: &mut Vec<DoctorDiagnostic>) {
        let marker: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![crate::incremental::UPDATE_IN_PROGRESS_KEY],
                |r| r.get(0),
            )
            .ok();
        if marker.is_some() {
            diags.push(DoctorDiagnostic::warning(
                "UPDATE_INCOMPLETE",
                "A previous incremental update committed its facts but never finalized (meta.update_in_progress is set); derived rows and snapshot_id may be stale until the next update repairs them",
            ));
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
        // Files consistency check: the negation of the schema v2 files
        // CHECK (schema.rs, ADR-022 D1), mirrored here so doctor flags
        // exactly the rows the schema would have rejected — rows that can
        // only exist in a database written by an older schema or a foreign
        // tool, which is what this check exists to diagnose.
        let files_inconsistent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files \
                 WHERE NOT ( \
                    (hash IS NULL \
                        AND COALESCE(skip, '') IN ('too_large', 'unreadable') \
                        AND parse_status = 'skipped') \
                    OR (hash IS NOT NULL AND (skip IS NULL OR skip = 'parse_skipped')) \
                 )",
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

    /// The derived-projection invariants, derived from the Pass-3 write
    /// contract: the resolver emits one bindings row for every reference of
    /// every `.go` file in a package (unbound refs are recorded with an
    /// `unbound_reason` — being unbound is written, not absent), and
    /// classifies every import of such a file (`resolved_dir`/`resolved_file`
    /// set, or `unresolved_reason`). Files outside any package are facts
    /// only; the resolver never touches them, so they are excluded from both
    /// predicates. A violation means the projection does not match the facts
    /// — a torn or lost derived write.
    fn check_projection_completeness(conn: &Connection, diags: &mut Vec<DoctorDiagnostic>) {
        // References of package files with no bindings row at all.
        let missing_bindings: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM refs r JOIN files f ON f.id = r.file_id \
                 WHERE f.package_id IS NOT NULL AND f.path GLOB '*.go' \
                 AND NOT EXISTS (SELECT 1 FROM bindings b WHERE b.ref_id = r.id)",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        if missing_bindings > 0 {
            let mut details = sample_paths(
                conn,
                "SELECT DISTINCT f.path FROM refs r JOIN files f ON f.id = r.file_id \
                 WHERE f.package_id IS NOT NULL AND f.path GLOB '*.go' \
                 AND NOT EXISTS (SELECT 1 FROM bindings b WHERE b.ref_id = r.id)",
            );
            details.insert(
                0,
                format!("{missing_bindings} references have no bindings row at all"),
            );
            diags.push(DoctorDiagnostic::error(
                "PROJECTION_INCOMPLETE",
                "References of package files are missing their bindings rows",
                details,
            ));
        }

        // Imports of package files with no resolution and no reason.
        let unclassified_imports: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM imports i JOIN files f ON f.id = i.file_id \
                 WHERE f.package_id IS NOT NULL AND f.path GLOB '*.go' \
                 AND i.resolved_dir IS NULL AND i.resolved_file IS NULL \
                 AND i.unresolved_reason IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        if unclassified_imports > 0 {
            let mut details = sample_paths(
                conn,
                "SELECT DISTINCT f.path FROM imports i JOIN files f ON f.id = i.file_id \
                 WHERE f.package_id IS NOT NULL AND f.path GLOB '*.go' \
                 AND i.resolved_dir IS NULL AND i.resolved_file IS NULL \
                 AND i.unresolved_reason IS NULL",
            );
            details.insert(
                0,
                format!("{unclassified_imports} imports carry no resolution and no reason"),
            );
            diags.push(DoctorDiagnostic::error(
                "IMPORTS_UNCLASSIFIED",
                "Imports of package files were never classified by the resolver",
                details,
            ));
        }

        if missing_bindings == 0 && unclassified_imports == 0 {
            diags.push(DoctorDiagnostic::info(
                "DERIVED_PROJECTION",
                "Derived projection complete: every reference of a package file carries a binding row and every import is classified",
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
    use crate::incremental::{apply_fact_changes, plan_incremental, UPDATE_IN_PROGRESS_KEY};
    use crate::IndexDatabase;
    use cs_scanner::{scan, ScanConfig};

    fn write_file(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

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
    fn doctor_accepts_scanner_unreadable_rows() {
        // The schema v2 truth table admits (hash NULL, skip 'unreadable',
        // parse_status 'skipped'); doctor must not flag that row as
        // inconsistent. Runtime canary: root reads through chmod 000, in
        // which case the scanner would not report Unreadable at all and the
        // premise does not hold — skip honestly rather than pass vacuously.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let canary_dir = tempfile::tempdir().unwrap();
            let canary = canary_dir.path().join("canary");
            std::fs::write(&canary, b"x").unwrap();
            let mut perms = std::fs::metadata(&canary).unwrap().permissions();
            perms.set_mode(0o0);
            std::fs::set_permissions(&canary, perms).unwrap();
            if std::fs::read(&canary).is_ok() {
                eprintln!("skipping: chmod 000 does not deny reads for this user");
                return;
            }

            let temp = tempfile::tempdir().expect("tree");
            write_file(temp.path(), "go.mod", b"module example.com/test\n");
            write_file(temp.path(), "a.go", b"package p\n\nfunc A() {}\n");
            write_file(temp.path(), "locked.go", b"package p\n\nfunc Locked() {}\n");
            let locked = temp.path().join("locked.go");
            let mut perms = std::fs::metadata(&locked).unwrap().permissions();
            perms.set_mode(0o0);
            std::fs::set_permissions(&locked, perms).unwrap();

            let db_dir = tempfile::tempdir().expect("db dir");
            let mut db =
                IndexDatabase::open_or_create(&db_dir.path().join("index.db")).expect("open");
            let scanned = scan(temp.path(), &ScanConfig::default()).expect("scan");
            db.begin_build(DEFAULT_CONFIG_FINGERPRINT)
                .expect("begin_build");
            ingest_facts(&mut db, temp.path(), &scanned, DEFAULT_FACT_BATCH_SIZE).expect("ingest");
            db.resolve_facts().expect("resolve");

            let (skip, hash): (Option<String>, Option<Vec<u8>>) = db
                .connection()
                .query_row(
                    "SELECT skip, hash FROM files WHERE path = 'locked.go'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .expect("unreadable row");
            assert_eq!(skip.as_deref(), Some("unreadable"));
            assert!(hash.is_none());

            let diags = Doctor::check(db.connection());
            let errors: Vec<_> = diags
                .iter()
                .filter(|d| d.severity == DiagnosticSeverity::Error)
                .collect();
            assert!(
                errors.is_empty(),
                "an unreadable file is a legal v2 row: {errors:?}"
            );
        }
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
    fn doctor_detects_torn_update_projection_before_repair_and_clean_after() {
        let tmp = tempfile::tempdir().expect("tree");
        let root = tmp.path();
        write_file(root, "go.mod", b"module example.com/test\n");
        write_file(root, "auth/auth.go", b"package auth\nfunc Login() {}\n");
        write_file(
            root,
            "web/web.go",
            b"package web\nimport \"example.com/test/auth\"\nfunc Handler() { auth.Login() }\n",
        );

        let db_dir = tempfile::tempdir().expect("db dir");
        let mut db = IndexDatabase::open_or_create(&db_dir.path().join("index.db")).expect("open");
        let files = scan(root, &ScanConfig::default()).expect("scan");
        ingest_facts(&mut db, root, &files, DEFAULT_FACT_BATCH_SIZE).expect("ingest");
        db.resolve_facts().expect("resolve");

        // Tear the index: commit an update's fact transaction and stop. The
        // modified file's refs lost their bindings to the FK cascade and its
        // new import rows carry no resolution.
        write_file(
            root,
            "web/web.go",
            b"package web\nimport \"example.com/test/auth\"\nfunc Handler() { auth.Logout() }\n",
        );
        let files_after = scan(root, &ScanConfig::default()).expect("rescan");
        let mut run = plan_incremental(&db, &files_after).expect("plan");
        apply_fact_changes(&mut db, root, &mut run).expect("facts phase only");

        let diags = Doctor::check(db.connection());
        assert!(
            diags
                .iter()
                .any(|d| d.code == "PROJECTION_INCOMPLETE"
                    && d.severity == DiagnosticSeverity::Error),
            "refs with no bindings row at all must be flagged: {diags:#?}"
        );
        assert!(
            diags.iter().any(
                |d| d.code == "IMPORTS_UNCLASSIFIED" && d.severity == DiagnosticSeverity::Error
            ),
            "imports with all resolution columns NULL must be flagged: {diags:#?}"
        );
        assert!(
            diags.iter().any(|d| d.code == "UPDATE_INCOMPLETE"),
            "the lingering torn-update marker must be surfaced: {diags:#?}"
        );

        // Repair through the ordinary update path; doctor must come back clean.
        crate::incremental::update_incremental(&mut db, root, &files_after)
            .expect("repairing update");

        let diags = Doctor::check(db.connection());
        assert!(
            diags.iter().any(|d| d.code == "DERIVED_PROJECTION"),
            "a healthy projection must be reported: {diags:#?}"
        );
        for gone in [
            "UPDATE_INCOMPLETE",
            "PROJECTION_INCOMPLETE",
            "IMPORTS_UNCLASSIFIED",
        ] {
            assert!(
                !diags.iter().any(|d| d.code == gone),
                "{gone} must clear after repair: {diags:#?}"
            );
        }
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .collect();
        assert!(
            errors.is_empty(),
            "repaired index must be error-free: {errors:#?}"
        );
    }

    #[test]
    fn doctor_repair_reset_clears_torn_update_marker() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).expect("open");
        db.connection()
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, '1')",
                [UPDATE_IN_PROGRESS_KEY],
            )
            .expect("plant marker");

        Doctor::repair(db.connection_mut(), RepairPolicy::ResetToEmpty).expect("repair");
        assert!(
            db.meta(UPDATE_IN_PROGRESS_KEY).expect("meta").is_none(),
            "a reset index must not keep the torn-update marker"
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

    #[test]
    fn repair_file_against_locked_index_fails_fast_as_locked() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("index.db");
        drop(IndexDatabase::open_or_create(&db_path).expect("open"));

        let blocker = Connection::open(&db_path).expect("blocker");
        blocker
            .execute_batch("PRAGMA busy_timeout = 0")
            .expect("no timeout");
        blocker
            .execute("BEGIN IMMEDIATE", [])
            .expect("take write lock");

        // Doctor's own connection must carry the open_or_create contract:
        // without an explicit busy timeout, rusqlite's 5 s default turns a
        // locked index into a stall before the error.
        let started = std::time::Instant::now();
        let err = Doctor::repair_file(&db_path, RepairPolicy::ResetToEmpty).expect_err("blocked");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "must fail fast, stalled {:?}",
            started.elapsed()
        );
        assert!(matches!(err, IndexError::Locked), "got {err:?}");

        // Diagnosis under the same lock reports instead of stalling.
        let started = std::time::Instant::now();
        let diags = Doctor::check_file(&db_path).expect("check");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "must report fast, stalled {:?}",
            started.elapsed()
        );
        assert!(!diags.is_empty());

        blocker.execute("ROLLBACK", []).expect("release");
        Doctor::repair_file(&db_path, RepairPolicy::ResetToEmpty).expect("repair after release");
    }
}
