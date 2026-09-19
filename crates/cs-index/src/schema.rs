//! The frozen index schema (ARCHITECTURE §5, ADR-021).
//!
//! Every table and index lives here as one canonical DDL string, applied in
//! order inside the open transaction by [`initialize`]. The schema is
//! versioned through `meta.schema_version` (mirrored in `PRAGMA
//! user_version`); a version mismatch on open is a rebuild, not a migration
//! — there is no migration machinery until a released on-disk format exists
//! (ADR-021 D6).
//!
//! The `files` CHECK constraints bake the scanner's hash/skip/status
//! consistency rules into the database itself: a row that violates them
//! fails at insert time rather than surfacing later as a doctor finding.

#![forbid(unsafe_code)]

use rusqlite::Transaction;

/// The schema version this build writes and requires.
pub const SCHEMA_VERSION: i64 = 2;

/// The frozen DDL, in dependency order (ARCHITECTURE §5).
///
/// `meta` is deliberately first: a database whose *first* table is not ours
/// is a foreign or corrupt file, and open detection keys on it.
pub const SCHEMA_SQL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS meta (
        key TEXT PRIMARY KEY NOT NULL,
        value TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS packages (
        id INTEGER PRIMARY KEY,
        dir TEXT NOT NULL,
        name TEXT NOT NULL,
        lang TEXT NOT NULL DEFAULT 'go',
        UNIQUE (dir, name, lang)
    )",
    // Consistency rules (ADR-021 D6), frozen as the scanner's truth table:
    // `hash IS NULL` exactly when the file was never read — `skip` is then
    // 'too_large' (over the read cap) or 'unreadable' (I/O failure) and
    // nothing was parsed; a hashed file was read, so it is either clean
    // (`skip IS NULL`) or in the parse-skip band (`skip = 'parse_skipped'`).
    // Written so the OR-chain is never NULL: SQLite CHECK treats NULL as
    // satisfied, and `skip` is nullable, so it is anchored with COALESCE —
    // a bare `skip IN (...)` would let (hash NULL, skip NULL) through.
    "CREATE TABLE IF NOT EXISTS files (
        id INTEGER PRIMARY KEY,
        path TEXT NOT NULL UNIQUE,
        lang TEXT NOT NULL,
        package_id INTEGER REFERENCES packages(id) ON DELETE SET NULL,
        hash BLOB,
        skip TEXT,
        size INTEGER NOT NULL,
        mtime INTEGER NOT NULL,
        parse_status TEXT NOT NULL,
        tokens_est INTEGER,
        CHECK (
            (hash IS NULL
                AND COALESCE(skip, '') IN ('too_large', 'unreadable')
                AND parse_status = 'skipped')
            OR (hash IS NOT NULL AND (skip IS NULL OR skip = 'parse_skipped'))
        ),
        CHECK (parse_status IN ('ok', 'partial', 'timeout', 'skipped', 'unsupported'))
    )",
    "CREATE TABLE IF NOT EXISTS symbols (
        id INTEGER PRIMARY KEY,
        file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        qual_name TEXT NOT NULL,
        kind TEXT NOT NULL,
        exported INTEGER NOT NULL,
        line INTEGER NOT NULL,
        end_line INTEGER NOT NULL,
        start_byte INTEGER NOT NULL,
        end_byte INTEGER NOT NULL,
        signature TEXT,
        container TEXT,
        doc TEXT
    )",
    "CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id)",
    "CREATE INDEX IF NOT EXISTS idx_symbols_exported
        ON symbols(name, file_id) WHERE exported = 1",
    "CREATE TABLE IF NOT EXISTS refs (
        id INTEGER PRIMARY KEY,
        file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        kind TEXT NOT NULL,
        qualifier TEXT,
        line INTEGER NOT NULL,
        start_byte INTEGER NOT NULL,
        end_byte INTEGER NOT NULL,
        container TEXT
    )",
    "CREATE INDEX IF NOT EXISTS idx_refs_file ON refs(file_id)",
    "CREATE TABLE IF NOT EXISTS imports (
        id INTEGER PRIMARY KEY,
        file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
        raw TEXT NOT NULL,
        alias TEXT,
        kind TEXT NOT NULL,
        ordinal INTEGER NOT NULL,
        resolved_dir TEXT,
        resolved_file TEXT,
        unresolved_reason TEXT
    )",
    "CREATE INDEX IF NOT EXISTS idx_imports_file ON imports(file_id)",
    // Package-scoped invalidation looks up imports by resolved package dir;
    // the index ships with the schema so a later-stage query cannot silently
    // degrade into a full scan.
    "CREATE INDEX IF NOT EXISTS idx_imports_resolved_dir ON imports(resolved_dir)",
    "CREATE TABLE IF NOT EXISTS manifests (
        path TEXT PRIMARY KEY NOT NULL,
        content TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS bindings (
        ref_id INTEGER PRIMARY KEY REFERENCES refs(id) ON DELETE CASCADE,
        unbound_reason TEXT
    )",
    "CREATE TABLE IF NOT EXISTS binding_targets (
        ref_id INTEGER NOT NULL REFERENCES refs(id) ON DELETE CASCADE,
        file_id INTEGER NOT NULL REFERENCES files(id),
        qual_name TEXT NOT NULL,
        kind TEXT NOT NULL,
        PRIMARY KEY (ref_id, file_id, qual_name)
    )",
    "CREATE INDEX IF NOT EXISTS idx_btargets_file ON binding_targets(file_id)",
    "CREATE TABLE IF NOT EXISTS edges (
        src INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
        dst INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
        kind TEXT NOT NULL,
        weight REAL NOT NULL,
        PRIMARY KEY (src, dst, kind)
    )",
    "CREATE INDEX IF NOT EXISTS idx_edges_dst ON edges(dst)",
];

/// Apply the frozen schema and seed the lifecycle meta keys.
///
/// Runs inside the caller's transaction so a fresh database is created
/// atomically: either the whole schema lands or none of it does. Every
/// statement is `IF NOT EXISTS` and the meta writes are `INSERT OR IGNORE`,
/// so re-running against an up-to-date database is a no-op — the version
/// gate in [`crate::IndexDatabase::open_or_create`] decides *whether* this
/// should run, not this function.
///
/// # Errors
///
/// Propagates SQLite failures from DDL execution or the `meta`/`user_version`
/// writes.
pub fn initialize(txn: &Transaction<'_>) -> Result<(), rusqlite::Error> {
    for statement in SCHEMA_SQL {
        txn.execute_batch(statement)?;
    }
    txn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', ?1)",
        [SCHEMA_VERSION.to_string()],
    )?;
    txn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES ('index_state', 'empty')",
        [],
    )?;
    txn.execute(&format!("PRAGMA user_version = {SCHEMA_VERSION}"), [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("memory db");
        conn.execute_batch("PRAGMA foreign_keys = ON").expect("fk");
        conn
    }

    #[test]
    fn schema_applies_atomically_and_is_idempotent() {
        let mut conn = memory_db();
        let txn = conn.transaction().expect("txn");
        initialize(&txn).expect("initialize");
        txn.commit().expect("commit");

        // Idempotent: a second full application must be a no-op, not an
        // error (reopening an up-to-date database re-runs nothing, but the
        // property is what makes resume-safe code simple).
        let txn = conn.transaction().expect("txn");
        initialize(&txn).expect("re-initialize");
        txn.commit().expect("commit");

        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .expect("schema_version");
        assert_eq!(version, SCHEMA_VERSION.to_string());
        let state: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'index_state'",
                [],
                |r| r.get(0),
            )
            .expect("index_state");
        assert_eq!(state, "empty");
        let user_version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("user_version");
        assert_eq!(user_version, SCHEMA_VERSION);
    }

    #[test]
    fn files_check_admits_every_scanner_state() {
        let mut conn = memory_db();
        let txn = conn.transaction().expect("txn");
        initialize(&txn).expect("initialize");
        txn.commit().expect("commit");

        conn.execute("INSERT INTO packages (dir, name) VALUES ('', 'm')", [])
            .expect("package");
        let package_id: i64 = conn.last_insert_rowid();

        // Every (hash, skip, parse_status) triple the scanner plus extraction
        // can legitimately produce must be insertable (cs-scanner `inspect`):
        // read-and-parsed, parse-skip band, over-read-cap, and unreadable.
        conn.execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('a.go', 'go', ?1, x'00', NULL, 3, 0, 'ok')",
            [package_id],
        )
        .expect("clean row");
        conn.execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('mid.go', 'go', ?1, x'01', 'parse_skipped', 50, 0, 'skipped')",
            [package_id],
        )
        .expect("parse-skipped band row");
        conn.execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('big.go', 'go', ?1, NULL, 'too_large', 99, 0, 'skipped')",
            [package_id],
        )
        .expect("too-large row");
        // The scanner's Unreadable state: listed, never read, therefore no
        // hash and nothing parsed. This is the row CF-02 froze out.
        conn.execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('locked.go', 'go', ?1, NULL, 'unreadable', 5, 0, 'skipped')",
            [package_id],
        )
        .expect("unreadable row");
    }

    #[test]
    fn schema_v2_furniture_is_present() {
        let mut conn = memory_db();
        let txn = conn.transaction().expect("txn");
        initialize(&txn).expect("initialize");
        txn.commit().expect("commit");

        assert_eq!(SCHEMA_VERSION, 2);

        // Package-scoped invalidation queries imports by resolved_dir; the
        // index is part of the frozen schema, not an afterthought.
        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'idx_imports_resolved_dir'",
                [],
                |r| r.get(0),
            )
            .expect("index lookup");
        assert_eq!(idx, 1, "idx_imports_resolved_dir must exist");

        // TEXT PRIMARY KEY on a rowid table does not imply NOT NULL; a NULL
        // key would be unaddressable by the (key = ?) meta reads.
        let err = conn.execute("INSERT INTO meta (key, value) VALUES (NULL, 'x')", []);
        assert!(err.is_err(), "meta.key must be NOT NULL");

        let err = conn.execute(
            "INSERT INTO manifests (path, content) VALUES (NULL, 'module m')",
            [],
        );
        assert!(err.is_err(), "manifests.path must be NOT NULL");
    }

    #[test]
    fn files_consistency_checks_reject_impossible_rows() {
        let mut conn = memory_db();
        let txn = conn.transaction().expect("txn");
        initialize(&txn).expect("initialize");
        txn.commit().expect("commit");

        conn.execute("INSERT INTO packages (dir, name) VALUES ('', 'm')", [])
            .expect("package");
        let package_id: i64 = conn.last_insert_rowid();

        // Seed one legal row so rejections cannot pass vacuously.
        conn.execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('a.go', 'go', ?1, x'00', NULL, 3, 0, 'ok')",
            [package_id],
        )
        .expect("clean row");

        // VIOLATION: no hash and no skip reason at all.
        let err = conn.execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('bad.go', 'go', ?1, NULL, NULL, 5, 0, 'skipped')",
            [package_id],
        );
        assert!(err.is_err(), "NULL hash requires a skip reason");

        // VIOLATION: the parse-skip band is hashed by definition.
        let err = conn.execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('bad0.go', 'go', ?1, NULL, 'parse_skipped', 5, 0, 'skipped')",
            [package_id],
        );
        assert!(err.is_err(), "skip='parse_skipped' requires a hash");

        // VIOLATION: no hash but claimed parsed.
        let err = conn.execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('bad2.go', 'go', ?1, NULL, 'too_large', 5, 0, 'ok')",
            [package_id],
        );
        assert!(err.is_err(), "NULL hash forces parse_status='skipped'");

        // VIOLATION: hashed file must not claim skip='too_large'.
        let err = conn.execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('bad3.go', 'go', ?1, x'02', 'too_large', 5, 0, 'ok')",
            [package_id],
        );
        assert!(err.is_err(), "skip='too_large' requires hash IS NULL");

        // VIOLATION: a hashed file was read by definition; 'unreadable' on a
        // hashed row is a label the scanner can never produce.
        let err = conn.execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('bad4.go', 'go', ?1, x'03', 'unreadable', 5, 0, 'skipped')",
            [package_id],
        );
        assert!(err.is_err(), "skip='unreadable' requires hash IS NULL");

        // VIOLATION: parse_status outside the extraction vocabulary.
        let err = conn.execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('bad5.go', 'go', ?1, x'04', NULL, 5, 0, 'bogus')",
            [package_id],
        );
        assert!(err.is_err(), "parse_status must be an extraction status");
    }

    #[test]
    fn foreign_keys_reject_unknown_package() {
        let mut conn = memory_db();
        let txn = conn.transaction().expect("txn");
        initialize(&txn).expect("initialize");
        txn.commit().expect("commit");

        // The test connection enables foreign_keys explicitly (the real
        // enforcement lives on IndexDatabase's connection pragma).
        let err = conn.execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('x.go', 'go', 9999, x'00', NULL, 3, 0, 'ok')",
            [],
        );
        assert!(err.is_err(), "FK to a missing package must be rejected");
    }
}
