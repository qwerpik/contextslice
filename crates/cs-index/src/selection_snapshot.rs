//! The ADR-023 D2 selection read-side seam: one read-only load of everything
//! selection consumes, as plain owned data.
//!
//! The snapshot is the whole contract between the index and `cs-select`:
//! files, symbols, and edges joined into path-keyed vectors plus the
//! `snapshot_id` a completed build commits. Loading is straight `SELECT`s in
//! deterministic order and nothing else — no lifecycle gate (accepting only
//! [`crate::IndexState::Ready`] is cs-select's policy) and no FTS5 (ADR-023
//! D3 places the per-selection in-memory FTS table on the select side, so
//! this database is never written here).

use std::path::Path;

use rusqlite::{Connection, Row};

use crate::IndexDatabase;
use crate::IndexError;

/// v1 defensive bound (not in ALGORITHM.md): the most rows the snapshot
/// seam materializes in one load, summed across `files`, `symbols`, and
/// `edges`. Fixture-scale snapshots are thousands of rows; symbols carry
/// `signature`/`doc` strings, so a million rows already means hundreds of
/// megabytes. Past this bound the loader refuses loudly instead of `OOM`ing
/// selection — streaming arrives with the seed stage.
pub const MAX_SNAPSHOT_ROWS: u64 = 1_000_000;

/// A package identity as stored in `packages`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPackage {
    /// Repo-relative package directory (`""` at the repo root).
    pub dir: String,
    /// Package clause name.
    pub name: String,
}

/// One indexed file: identity, classification, and its package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFile {
    /// Repo-relative normalized path (the snapshot's join key).
    pub path: String,
    /// Language tag (`"go"`).
    pub lang: String,
    /// File size in bytes as indexed.
    pub size: i64,
    /// The file's package, or `None` when the row carries no package
    /// (`files.package_id` is nullable).
    pub package: Option<SnapshotPackage>,
}

/// One indexed symbol, joined to its file by path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotSymbol {
    /// Repo-relative path of the containing file.
    pub file: String,
    /// Bare symbol name.
    pub name: String,
    /// Stored definition kind (`"func"`, `"method"`, `"struct"`, ...).
    pub kind: String,
    /// Whether the symbol is exported.
    pub exported: bool,
    /// One-based declaration line.
    pub line: i64,
    /// Enclosing receiver type for methods, `None` for free symbols.
    pub container: Option<String>,
    /// One-line signature when extraction captured one. Stored index facts
    /// re-exposed, not source text: the storage contract is spans plus
    /// signatures, never file contents (ARCHITECTURE.md §8) — ruling
    /// (2026-09-20): carrying these fields through the snapshot complies.
    pub signature: Option<String>,
    /// Doc comment when extraction captured one (same contract note as
    /// `signature` above).
    pub doc: Option<String>,
}

/// One stored directed edge, joined to file paths on both ends.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotEdge {
    /// Repo-relative path of the edge tail (`edges.src`).
    pub src: String,
    /// Repo-relative path of the edge head (`edges.dst`).
    pub dst: String,
    /// Stored edge kind (`"import_out"`, `"ref_def"`, `"test_affinity"`).
    pub kind: String,
    /// Stored traversal weight (ALGORITHM.md §6, via the cs-resolve
    /// constants at write time).
    pub weight: f64,
}

/// Everything selection needs from the index, loaded read-only in one call
/// (ADR-023 D2).
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionSnapshot {
    /// All indexed files, ascending by path.
    pub files: Vec<SnapshotFile>,
    /// All indexed symbols, ordered by (file path, line, name).
    pub symbols: Vec<SnapshotSymbol>,
    /// All stored edges, ordered by (src, dst, kind). The order is total:
    /// `edges` carries `PRIMARY KEY (src, dst, kind)` and file paths are
    /// unique, so no two rows share a triple.
    pub edges: Vec<SnapshotEdge>,
    /// The `meta.snapshot_id` of the completed build, or the empty string
    /// when the index carries none (fresh/`empty` state). Whether an empty
    /// id disqualifies selection is cs-select's policy, not this loader's.
    pub snapshot_id: String,
    /// Whether `meta.update_in_progress` is set: a previous incremental
    /// update committed facts but never finalized, so derived rows and the
    /// id above may be stale until the next update repairs them. The loader
    /// reports this; refusing torn snapshots is cs-select's policy.
    pub update_in_progress: bool,
}

/// Run one read-only query and collect its rows through `to_row`.
fn collect<T>(
    conn: &Connection,
    db_path: &Path,
    sql: &str,
    to_row: impl Fn(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, IndexError> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| IndexDatabase::classify(db_path, e))?;
    let rows = stmt
        .query_map([], to_row)
        .map_err(|e| IndexDatabase::classify(db_path, e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| IndexDatabase::classify(db_path, e))
}

/// Count the rows the snapshot would materialize, per table.
///
/// Split out so the bound check reads as one line at the call site; the
/// counts also let tests assert the guard sits far above fixture scale.
fn snapshot_row_counts(conn: &Connection, db_path: &Path) -> Result<(u64, u64, u64), IndexError> {
    let count = |table: &str| -> Result<u64, IndexError> {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .map_err(|e| IndexDatabase::classify(db_path, e))?;
        u64::try_from(n).map_err(|_| IndexError::Corrupt {
            path: db_path.to_owned(),
            detail: format!("negative COUNT(*) on {table}"),
        })
    };
    Ok((count("files")?, count("symbols")?, count("edges")?))
}

/// `true` when the three table counts fit the in-memory snapshot bound.
///
/// Saturating: no table combination, however adversarial, wraps the total.
#[must_use]
fn within_row_limit(files: u64, symbols: u64, edges: u64) -> bool {
    files.saturating_add(symbols).saturating_add(edges) <= MAX_SNAPSHOT_ROWS
}

/// Load a [`SelectionSnapshot`] from an open index connection.
///
/// Straight SELECTs over `files`, `symbols`, `edges`, and `meta`; no
/// lifecycle state is enforced and nothing is written. On a fresh or empty
/// index this returns empty vectors with the empty `snapshot_id` — a missing
/// or partial index is not a loader error; the accept/reject decision
/// belongs to the selection stage.
///
/// Before loading, the per-table row counts are checked against
/// [`MAX_SNAPSHOT_ROWS`]: an index past the bound is refused with
/// [`IndexError::TooLarge`] instead of materializing gigabytes.
///
/// # Errors
///
/// Propagates SQLite failures as [`IndexError`], and refuses over-bound
/// indexes with [`IndexError::TooLarge`].
pub fn load_selection_snapshot(
    conn: &Connection,
    db_path: &Path,
) -> Result<SelectionSnapshot, IndexError> {
    let (files_n, symbols_n, edges_n) = snapshot_row_counts(conn, db_path)?;
    let rows = files_n.saturating_add(symbols_n).saturating_add(edges_n);
    if !within_row_limit(files_n, symbols_n, edges_n) {
        return Err(IndexError::TooLarge {
            path: db_path.to_owned(),
            rows,
            limit: MAX_SNAPSHOT_ROWS,
        });
    }
    let snapshot_id = IndexDatabase::meta_value(conn, db_path, "snapshot_id")?.unwrap_or_default();
    let update_in_progress =
        IndexDatabase::meta_value(conn, db_path, crate::incremental::UPDATE_IN_PROGRESS_KEY)?
            .is_some();

    let files = collect(
        conn,
        db_path,
        "SELECT f.path, f.lang, f.size, p.dir, p.name
         FROM files f
         LEFT JOIN packages p ON p.id = f.package_id
         ORDER BY f.path ASC",
        |row| {
            let path: String = row.get(0)?;
            let lang: String = row.get(1)?;
            let size: i64 = row.get(2)?;
            let dir: Option<String> = row.get(3)?;
            let name: Option<String> = row.get(4)?;
            let package = match (dir, name) {
                (Some(dir), Some(name)) => Some(SnapshotPackage { dir, name }),
                _ => None,
            };
            Ok(SnapshotFile {
                path,
                lang,
                size,
                package,
            })
        },
    )?;

    // `s.id` is the final tie-break: extraction never emits two symbols with
    // one (path, line, name), but a total order costs nothing and the
    // rowids are themselves insertion-ordered deterministically.
    let symbols = collect(
        conn,
        db_path,
        "SELECT f.path, s.name, s.kind, s.exported, s.line, s.container, s.signature, s.doc
         FROM symbols s
         JOIN files f ON f.id = s.file_id
         ORDER BY f.path ASC, s.line ASC, s.name ASC, s.id ASC",
        |row| {
            let file: String = row.get(0)?;
            let name: String = row.get(1)?;
            let kind: String = row.get(2)?;
            let exported: i64 = row.get(3)?;
            Ok(SnapshotSymbol {
                file,
                name,
                kind,
                exported: exported != 0,
                line: row.get(4)?,
                container: row.get(5)?,
                signature: row.get(6)?,
                doc: row.get(7)?,
            })
        },
    )?;

    let edges = collect(
        conn,
        db_path,
        "SELECT sf.path, df.path, e.kind, e.weight
         FROM edges e
         JOIN files sf ON sf.id = e.src
         JOIN files df ON df.id = e.dst
         ORDER BY sf.path ASC, df.path ASC, e.kind ASC",
        |row| {
            Ok(SnapshotEdge {
                src: row.get(0)?,
                dst: row.get(1)?,
                kind: row.get(2)?,
                weight: row.get(3)?,
            })
        },
    )?;

    Ok(SelectionSnapshot {
        files,
        symbols,
        edges,
        snapshot_id,
        update_in_progress,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{ingest_facts, DEFAULT_CONFIG_FINGERPRINT, DEFAULT_FACT_BATCH_SIZE};
    use crate::IndexState;
    use cs_resolve::{W_IMPORT_OUT, W_TEST_AFFINITY};
    use cs_scanner::{scan, ScanConfig};
    use std::collections::BTreeSet;

    /// The `fixtures/go-resolve` repository indexed end to end — the same
    /// idiom the doctor tests use.
    fn indexed_fixture() -> (tempfile::TempDir, IndexDatabase) {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).expect("open");
        let fixture_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates dir")
            .parent()
            .expect("workspace")
            .join("fixtures/go-resolve");
        let scanned = scan(&fixture_root, &ScanConfig::default()).expect("scan");
        db.begin_build(DEFAULT_CONFIG_FINGERPRINT)
            .expect("begin_build");
        ingest_facts(&mut db, &fixture_root, &scanned, DEFAULT_FACT_BATCH_SIZE).expect("ingest");
        db.resolve_facts().expect("resolve_facts");
        (temp, db)
    }

    #[test]
    fn snapshot_counts_and_snapshot_id_match_the_index() {
        let (_dir, db) = indexed_fixture();
        assert_eq!(db.state().expect("state"), IndexState::Ready);

        let snap = db.load_selection_snapshot().expect("load");
        let count = |table: &str| -> i64 {
            db.connection()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .expect("count")
        };
        assert_eq!(
            snap.files.len(),
            usize::try_from(count("files")).unwrap(),
            "every indexed file is in the snapshot"
        );
        assert_eq!(
            snap.symbols.len(),
            usize::try_from(count("symbols")).unwrap(),
            "every indexed symbol is in the snapshot"
        );
        assert_eq!(
            snap.edges.len(),
            usize::try_from(count("edges")).unwrap(),
            "every stored edge is in the snapshot"
        );
        assert!(!snap.files.is_empty(), "the fixture is non-trivial");
        assert_eq!(
            db.meta("snapshot_id").expect("meta").as_deref(),
            Some(snap.snapshot_id.as_str()),
            "snapshot_id comes from meta"
        );
    }

    #[test]
    fn snapshot_rows_load_in_deterministic_order() {
        let (_dir, db) = indexed_fixture();
        let snap = db.load_selection_snapshot().expect("load");

        // paths are UNIQUE, so the file order is strictly ascending.
        assert!(
            snap.files.windows(2).all(|w| w[0].path < w[1].path),
            "files must be strictly path-sorted"
        );
        assert!(
            snap.symbols
                .windows(2)
                .all(|w| (w[0].file.as_str(), w[0].line, w[0].name.as_str())
                    <= (w[1].file.as_str(), w[1].line, w[1].name.as_str())),
            "symbols must be ordered by (file, line, name)"
        );
        assert!(
            snap.edges.windows(2).all(|w| (
                w[0].src.as_str(),
                w[0].dst.as_str(),
                w[0].kind.as_str()
            ) < (
                w[1].src.as_str(),
                w[1].dst.as_str(),
                w[1].kind.as_str()
            )),
            "edges must be strictly (src, dst, kind)-sorted"
        );
    }

    #[test]
    fn repeated_loads_are_identical() {
        let (_dir, db) = indexed_fixture();
        let first = db.load_selection_snapshot().expect("load");
        let second = db.load_selection_snapshot().expect("reload");
        assert_eq!(first, second, "the load must be a pure read");
    }

    #[test]
    fn snapshot_files_carry_their_package_identity() {
        let (_dir, db) = indexed_fixture();
        let snap = db.load_selection_snapshot().expect("load");

        let session = snap
            .files
            .iter()
            .find(|f| f.path == "auth/session.go")
            .expect("auth/session.go is indexed");
        assert_eq!(session.lang, "go");
        assert!(session.size > 0);
        let package = session.package.as_ref().expect("session.go has a package");
        assert_eq!(package.dir, "auth");
        assert_eq!(package.name, "auth");
    }

    #[test]
    fn snapshot_symbols_carry_extracted_fields() {
        let (_dir, db) = indexed_fixture();
        let snap = db.load_selection_snapshot().expect("load");

        let login = snap
            .symbols
            .iter()
            .find(|s| s.file == "auth/session.go" && s.name == "Login")
            .expect("Login is indexed");
        assert!(login.exported, "Login is exported");
        assert_eq!(login.kind, "func");
        assert!(login.container.is_none(), "Login is a free function");
        let signature = login.signature.as_deref().expect("Login has a signature");
        assert!(signature.contains("Login"), "got {signature}");
        let doc = login.doc.as_deref().expect("Login has a doc comment");
        assert!(doc.contains("validates a session"), "got {doc}");

        let token = snap
            .symbols
            .iter()
            .find(|s| s.file == "auth/session.go" && s.name == "token")
            .expect("token is indexed");
        assert!(!token.exported, "token is unexported");

        let validate = snap
            .symbols
            .iter()
            .find(|s| s.file == "auth/session.go" && s.name == "Validate")
            .expect("Validate is indexed");
        assert_eq!(
            validate.container.as_deref(),
            Some("Session"),
            "Validate is a method on Session"
        );
    }

    #[test]
    fn snapshot_edges_carry_the_stored_kinds_and_weights() {
        let (_dir, db) = indexed_fixture();
        let snap = db.load_selection_snapshot().expect("load");

        let kinds: BTreeSet<&str> = snap.edges.iter().map(|e| e.kind.as_str()).collect();
        for expected in ["import_out", "ref_def", "test_affinity"] {
            assert!(
                kinds.contains(expected),
                "missing {expected}, got {kinds:?}"
            );
        }

        for edge in &snap.edges {
            match edge.kind.as_str() {
                "import_out" => {
                    assert!((edge.weight - W_IMPORT_OUT).abs() < 1e-9);
                }
                "test_affinity" => {
                    assert!((edge.weight - W_TEST_AFFINITY).abs() < 1e-9);
                }
                "ref_def" => {
                    // 0.5·√(count/(count+8)): positive, strictly below its 0.5 asymptote.
                    assert!(edge.weight > 0.0, "got {}", edge.weight);
                    assert!(edge.weight < 0.5, "got {}", edge.weight);
                }
                other => panic!("unexpected edge kind {other}"),
            }
        }
    }

    #[test]
    fn snapshot_test_affinity_edges_are_stored_bidirectionally() {
        let (_dir, db) = indexed_fixture();
        let snap = db.load_selection_snapshot().expect("load");

        let affinity: BTreeSet<(&str, &str)> = snap
            .edges
            .iter()
            .filter(|e| e.kind == "test_affinity")
            .map(|e| (e.src.as_str(), e.dst.as_str()))
            .collect();
        assert!(!affinity.is_empty(), "the fixture has _test.go files");
        let mirrored: BTreeSet<(&str, &str)> =
            affinity.iter().map(|(src, dst)| (*dst, *src)).collect();
        assert_eq!(affinity, mirrored, "every affinity edge needs its reverse");
    }

    #[test]
    fn snapshot_file_without_package_loads_as_none() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).expect("open");
        db.begin_build(DEFAULT_CONFIG_FINGERPRINT)
            .expect("begin_build");
        db.connection()
            .execute(
                "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
                 VALUES ('orphan.go', 'go', NULL, x'00', NULL, 10, 0, 'ok')",
                [],
            )
            .expect("insert orphan file");
        db.resolve_facts().expect("resolve with no packages");

        let snap = db.load_selection_snapshot().expect("load");
        assert_eq!(snap.files.len(), 1);
        assert!(snap.files[0].package.is_none(), "NULL package_id → None");
        assert!(snap.symbols.is_empty());
        assert!(snap.edges.is_empty());
    }

    #[test]
    fn empty_index_loads_an_empty_snapshot_without_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("index.db");
        let db = IndexDatabase::open_or_create(&db_path).expect("open");
        assert_eq!(db.state().expect("state"), IndexState::Empty);

        let snap = db
            .load_selection_snapshot()
            .expect("an empty index is not a loader error");
        assert!(snap.files.is_empty());
        assert!(snap.symbols.is_empty());
        assert!(snap.edges.is_empty());
        assert_eq!(
            snap.snapshot_id, "",
            "no meta.snapshot_id exists yet; policy is cs-select's"
        );
    }

    #[test]
    fn row_guard_counts_match_the_tables_and_sits_far_above_fixtures() {
        let (_dir, db) = indexed_fixture();
        let (files, symbols, edges) =
            snapshot_row_counts(db.connection(), db.path()).expect("counts");
        let snap = db.load_selection_snapshot().expect("load");
        assert_eq!(
            usize::try_from(files).expect("fixture fits in memory"),
            snap.files.len()
        );
        assert_eq!(
            usize::try_from(symbols).expect("fixture fits in memory"),
            snap.symbols.len()
        );
        assert_eq!(
            usize::try_from(edges).expect("fixture fits in memory"),
            snap.edges.len()
        );
        let total = files.saturating_add(symbols).saturating_add(edges);
        assert!(
            total * 1000 < MAX_SNAPSHOT_ROWS,
            "the bound must leave three orders of magnitude of headroom \
             over fixture scale, else normal indexes trip it: total {total}"
        );
        assert!(within_row_limit(files, symbols, edges));
    }

    #[test]
    fn row_limit_holds_at_the_boundary_and_saturates() {
        assert!(within_row_limit(0, 0, 0));
        assert!(within_row_limit(1, 2, 3));
        assert!(
            within_row_limit(MAX_SNAPSHOT_ROWS - 2, 1, 1),
            "exactly at the bound still loads"
        );
        assert!(!within_row_limit(MAX_SNAPSHOT_ROWS, 1, 0));
        assert!(
            !within_row_limit(u64::MAX, u64::MAX, u64::MAX),
            "adversarial counts must saturate, never wrap past the bound"
        );
    }

    #[test]
    fn loaded_edge_triples_are_unique_so_the_order_is_total() {
        // PRIMARY KEY (src, dst, kind) plus unique file paths: no two loaded
        // rows can share a triple, which is what makes the loader's
        // ORDER BY a total order. This test pins that schema guarantee
        // against future migrations.
        let (_dir, db) = indexed_fixture();
        let snap = db.load_selection_snapshot().expect("load");
        let triples: BTreeSet<(&str, &str, &str)> = snap
            .edges
            .iter()
            .map(|e| (e.src.as_str(), e.dst.as_str(), e.kind.as_str()))
            .collect();
        assert_eq!(
            triples.len(),
            snap.edges.len(),
            "duplicate (src, dst, kind) triple would break totality"
        );
    }
}
