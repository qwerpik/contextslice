//! Incremental indexing engine.
//!
//! Implements Milestone M5 of the streaming-first architecture (ADR-021, ARCHITECTURE §4.4).
//!
//! Key principles:
//! - Content hashing (`blake3`) is always computed; `mtime` is recorded for diagnostics and never trusted as a skip shortcut.
//! - Unchanged files: zero parsing, zero re-extraction, fast no-op return.
//! - Modified/added file in package P: re-extract its facts, re-resolve package P.
//! - Reverse invalidation is package-scoped (ADR-021 D5): a change to any file
//!   of package P re-resolves P itself, every package holding bindings into
//!   P's files, and every package importing P's directory (blank imports
//!   included — they bind nothing but still import).
//! - Deleted file: delete row (cascades to facts) + reverse-invalidate consumers.
//! - Packages left with no files (deletions, in-place renames) are garbage-collected.
//! - State invariant: incremental runs operate on and remain in [`IndexState::Ready`].
//!
//! # Crash coherence
//!
//! A run is phased: classification (read-only), one fact transaction, per
//! package derived transactions, one finalize transaction. The fact
//! transaction also plants the `update_in_progress` meta key; only the
//! finalize transaction — which commits the new `snapshot_id` — clears it.
//! A crash between the two leaves a `ready` index whose facts are new but
//! whose derived rows may be stale or missing; the marker makes that state
//! visible (doctor reports it) and repairable (the next run skips the
//! no-op fast path and rebuilds the derived projection from the
//! hash-consistent facts).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::Duration;

use cs_extract::{extract_with_timeout, ExtractError, PARSE_TIMEOUT_MS};
use cs_scanner::ScannedFile;
use rusqlite::{params, Connection};

use crate::facts::{dir_of, import_kind_str, parse_status_str, skip_label, to_i64};
use crate::index_pass::compute_snapshot_id;
use crate::resolve_pass::DeriveEngine;
use crate::{IndexDatabase, IndexError, IndexState};

/// The `meta` key that marks a torn incremental update: planted inside the
/// fact transaction, cleared only by the finalize transaction that commits
/// the new `snapshot_id`. Present on a `ready` index it means facts and
/// derived rows may disagree until the next update repairs them.
pub(crate) const UPDATE_IN_PROGRESS_KEY: &str = "update_in_progress";

/// The `(dir, name)` identity of a package id, or `None` if no such row.
fn package_key(
    conn: &Connection,
    db_path: &Path,
    package_id: i64,
) -> Result<Option<(String, String)>, IndexError> {
    conn.query_row(
        "SELECT dir, name FROM packages WHERE id = ?1",
        params![package_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .map(Some)
    .or_else(|err| match err {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(IndexDatabase::classify(db_path, other)),
    })
}

/// Package identities of files whose imports resolved into `dir`
/// (`idx_imports_resolved_dir` covers the lookup). Blank imports are
/// included: their resolution is recorded even though they bind nothing.
fn importer_packages(
    conn: &Connection,
    db_path: &Path,
    dir: &str,
) -> Result<BTreeSet<(String, String)>, IndexError> {
    let mut out = BTreeSet::new();
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT p.dir, p.name FROM imports i \
             JOIN files f ON f.id = i.file_id \
             JOIN packages p ON p.id = f.package_id \
             WHERE i.resolved_dir = ?1",
        )
        .map_err(|e| IndexDatabase::classify(db_path, e))?;
    let mut rows = stmt
        .query(params![dir])
        .map_err(|e| IndexDatabase::classify(db_path, e))?;
    while let Some(row) = rows
        .next()
        .map_err(|e| IndexDatabase::classify(db_path, e))?
    {
        let key = (
            row.get::<_, String>(0)
                .map_err(|e| IndexDatabase::classify(db_path, e))?,
            row.get::<_, String>(1)
                .map_err(|e| IndexDatabase::classify(db_path, e))?,
        );
        out.insert(key);
    }
    Ok(out)
}

fn to_i64_usize(val: usize) -> i64 {
    i64::try_from(val).unwrap_or(i64::MAX)
}

/// Statistics reported from an incremental update run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncrementalStats {
    /// Files whose hash matched existing database rows (zero work).
    pub files_unchanged: usize,
    /// New files added to the index.
    pub files_added: usize,
    /// Modified files whose facts were re-extracted.
    pub files_modified: usize,
    /// Files removed from the index.
    pub files_deleted: usize,
    /// Packages that were re-resolved (directly affected + reverse-invalidated).
    pub packages_resolved: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ExistingFile {
    pub(crate) id: i64,
    pub(crate) path: String,
    pub(crate) hash: Option<Vec<u8>>,
}

/// The planned and in-flight state of one incremental update, phased so a
/// crash between phases is representable in tests without timers or process
/// tricks (`pub(crate)`): plan (read-only) → facts → derived → finalize.
pub(crate) struct IncrementalRun<'a> {
    /// New files the scanner reports that the index does not have.
    pub(crate) added: Vec<&'a ScannedFile>,
    /// Files whose hash changed, with their existing rows.
    pub(crate) modified: Vec<(&'a ScannedFile, ExistingFile)>,
    /// Rows whose paths the scanner no longer reports.
    pub(crate) deleted: Vec<ExistingFile>,
    /// Files whose hash matched (zero work).
    pub(crate) files_unchanged: usize,
    /// Whether a `go.mod` is added, changed, or deleted (module config moved).
    pub(crate) manifest_changed: bool,
    /// Invalidation set as of the pre-transaction state, kept separately so
    /// the post-commit fan-out can diff packages discovered during extraction.
    pub(crate) pre_transaction_keys: BTreeSet<(String, String)>,
    /// Every package this run must re-resolve; grows through the phases.
    pub(crate) packages_to_resolve: BTreeSet<(String, String)>,
    /// Whether the fact transaction created or garbage-collected a package.
    /// Importer fan-out keys on `imports.resolved_dir`, which is NULL for
    /// every importer whose target was missing, so a changed package set
    /// means the invalidation set cannot be trusted to name the stale
    /// importers — the derived phase must re-resolve everything.
    pub(crate) package_set_changed: bool,
}

impl IncrementalRun<'_> {
    /// The fast-path question: is there any fact work at all?
    pub(crate) fn is_noop(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

/// Phases 1-2: classify the scan against the index and select the packages to
/// re-resolve. Read-only; runs before the fact transaction so reverse
/// invalidation still sees binding targets into files that are about to be
/// deleted.
///
/// # Errors
///
/// Propagates SQLite failures.
pub(crate) fn plan_incremental<'a>(
    db: &IndexDatabase,
    scanned_files: &'a [ScannedFile],
) -> Result<IncrementalRun<'a>, IndexError> {
    let db_path = db.path.clone();

    // 1. Load existing files from SQLite
    let mut existing_files: BTreeMap<String, ExistingFile> = BTreeMap::new();
    {
        let mut stmt = db
            .conn
            .prepare("SELECT id, path, hash FROM files")
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;

        while let Some(row) = rows
            .next()
            .map_err(|e| IndexDatabase::classify(&db_path, e))?
        {
            let id: i64 = row
                .get(0)
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            let path: String = row
                .get(1)
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            let hash: Option<Vec<u8>> = row
                .get(2)
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;

            existing_files.insert(path.clone(), ExistingFile { id, path, hash });
        }
    }

    // 2. Classify scanned files against existing files
    let mut run = IncrementalRun {
        added: Vec::new(),
        modified: Vec::new(),
        deleted: Vec::new(),
        files_unchanged: 0,
        manifest_changed: false,
        pre_transaction_keys: BTreeSet::new(),
        packages_to_resolve: BTreeSet::new(),
        package_set_changed: false,
    };
    let mut seen_paths: BTreeSet<String> = BTreeSet::new();

    for file in scanned_files {
        seen_paths.insert(file.path.clone());
        if let Some(existing) = existing_files.get(&file.path) {
            let hash_matches = match (&file.hash, &existing.hash) {
                (Some(h1), Some(h2)) => h1.as_slice() == h2.as_slice(),
                (None, None) => true,
                _ => false,
            };

            if hash_matches {
                run.files_unchanged += 1;
            } else {
                run.modified.push((file, existing.clone()));
            }
        } else {
            run.added.push(file);
        }
    }

    for (path, existing) in existing_files {
        if !seen_paths.contains(&path) {
            run.deleted.push(existing);
        }
    }

    for (file, _) in &run.modified {
        if file.path == "go.mod" || file.path.ends_with("/go.mod") {
            run.manifest_changed = true;
        }
    }
    for existing in &run.deleted {
        if existing.path == "go.mod" || existing.path.ends_with("/go.mod") {
            run.manifest_changed = true;
        }
    }

    // 3. Package-scoped invalidation (ADR-021 D5): a change to any file of
    //    package P re-resolves P itself, every package holding bindings into
    //    P's files, and every package importing P's directory.
    //
    // Packages of every changed file as they stand now (pre-update): a file
    // that moves between packages invalidates its old one here and its new
    // one during extraction below.
    let mut affected_pkg_ids: BTreeSet<i64> = BTreeSet::new();
    for existing in run
        .modified
        .iter()
        .map(|(_, existing)| existing)
        .chain(run.deleted.iter())
    {
        let pkg_id: Option<i64> = db
            .conn
            .query_row(
                "SELECT package_id FROM files WHERE id = ?1",
                params![existing.id],
                |r| r.get(0),
            )
            .unwrap_or(None);
        if let Some(pid) = pkg_id {
            affected_pkg_ids.insert(pid);
        }
    }

    for pid in affected_pkg_ids {
        let Some(key) = package_key(&db.conn, &db_path, pid)? else {
            continue;
        };
        run.packages_to_resolve.insert(key.clone());

        // Packages holding bindings into any file of P.
        let mut stmt_binders = db
            .conn
            .prepare(
                "SELECT DISTINCT f.package_id FROM binding_targets bt \
                 JOIN refs r ON r.id = bt.ref_id \
                 JOIN files f ON f.id = r.file_id \
                 WHERE bt.file_id IN (SELECT id FROM files WHERE package_id = ?1) \
                 AND f.package_id IS NOT NULL",
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        let mut binder_ids: Vec<i64> = Vec::new();
        let mut rows = stmt_binders
            .query(params![pid])
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        while let Some(row) = rows
            .next()
            .map_err(|e| IndexDatabase::classify(&db_path, e))?
        {
            binder_ids.push(
                row.get(0)
                    .map_err(|e| IndexDatabase::classify(&db_path, e))?,
            );
        }
        drop(rows);
        for binder in binder_ids {
            if let Some(binder_key) = package_key(&db.conn, &db_path, binder)? {
                run.packages_to_resolve.insert(binder_key);
            }
        }

        // Packages importing P's directory — the only signal that covers
        // blank imports, which produce no bindings at all.
        for importer in importer_packages(&db.conn, &db_path, &key.0)? {
            run.packages_to_resolve.insert(importer);
        }
    }

    run.pre_transaction_keys
        .clone_from(&run.packages_to_resolve);
    Ok(run)
}

/// Phase 3: apply the fact changes in one transaction. The torn-update
/// marker is planted inside this transaction, so it exists exactly when the
/// new facts do — a rollback leaves neither. Packages discovered during
/// extraction are added to `run.packages_to_resolve`.
///
/// # Errors
///
/// Propagates I/O and SQLite storage failures.
#[allow(clippy::too_many_lines, clippy::similar_names)]
pub(crate) fn apply_fact_changes(
    db: &mut IndexDatabase,
    root: &Path,
    run: &mut IncrementalRun<'_>,
) -> Result<(), IndexError> {
    let db_path = db.path.clone();

    // 4. Update Facts in a single transaction
    {
        let txn = db
            .conn
            .transaction()
            .map_err(|e| IndexDatabase::lock(&db_path, e))?;

        // The marker rides with the facts: after this commit, derived rows
        // lag until the finalize transaction clears it.
        txn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, '1') \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![UPDATE_IN_PROGRESS_KEY],
        )
        .map_err(|e| IndexDatabase::classify(&db_path, e))?;

        // A. Handle deleted files
        for existing in &run.deleted {
            txn.execute(
                "DELETE FROM binding_targets WHERE file_id = ?1",
                params![existing.id],
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            txn.execute("DELETE FROM files WHERE id = ?1", params![existing.id])
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            if existing.path == "go.mod" || existing.path.ends_with("/go.mod") {
                txn.execute(
                    "DELETE FROM manifests WHERE path = ?1",
                    params![existing.path],
                )
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            }
        }

        // Statements for fact insertion
        let mut stmt_insert_package = txn
            .prepare(
                "INSERT INTO packages (dir, name, lang) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(dir, name, lang) DO NOTHING",
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        let mut stmt_select_package = txn
            .prepare("SELECT id FROM packages WHERE dir = ?1 AND name = ?2 AND lang = ?3")
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        let mut stmt_insert_file = txn
            .prepare(
                "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 RETURNING id",
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        let mut stmt_update_file = txn
            .prepare(
                "UPDATE files SET lang = ?1, package_id = ?2, hash = ?3, skip = ?4, \
                 size = ?5, mtime = ?6, parse_status = ?7 WHERE id = ?8",
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        let mut stmt_insert_manifest = txn
            .prepare(
                "INSERT INTO manifests (path, content) VALUES (?1, ?2) \
                 ON CONFLICT(path) DO UPDATE SET content = excluded.content",
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        let mut stmt_insert_symbol = txn
            .prepare(
                "INSERT INTO symbols (file_id, name, qual_name, kind, exported, line, end_line, \
                 start_byte, end_byte, signature, container, doc) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        let mut stmt_insert_ref = txn
            .prepare(
                "INSERT INTO refs (file_id, name, kind, qualifier, line, start_byte, end_byte, container) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        let mut stmt_insert_import = txn
            .prepare(
                "INSERT INTO imports (file_id, raw, alias, kind, ordinal) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;

        // Helper closures. The insert reports whether it created the row
        // (`DO NOTHING` changes nothing on conflict): a package born in this
        // transaction is one half of the package-set-change signal.
        let mut get_or_create_pkg = |dir: &str, name: &str| -> Result<(i64, bool), IndexError> {
            let created = stmt_insert_package
                .execute(params![dir, name, "go"])
                .map_err(|e| IndexDatabase::classify(&db_path, e))?
                > 0;
            let id = stmt_select_package
                .query_row(params![dir, name, "go"], |r| r.get(0))
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            Ok((id, created))
        };

        // B. Handle modified files
        for (file, existing) in &run.modified {
            let file_id = existing.id;

            // Delete previous facts and edges for this file
            txn.execute("DELETE FROM symbols WHERE file_id = ?1", params![file_id])
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            txn.execute("DELETE FROM refs WHERE file_id = ?1", params![file_id])
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            txn.execute("DELETE FROM imports WHERE file_id = ?1", params![file_id])
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            txn.execute(
                "DELETE FROM edges WHERE src = ?1 OR dst = ?1",
                params![file_id],
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;

            let full_path = root.join(&file.path);

            // Trust the scanner's skip label: a file it did not read (over
            // the read cap, unreadable) or did not parse is not read here
            // either — the read cap holds in incremental updates too.
            if let Some((skip_str, status_str)) = skip_label(file) {
                if file.path == "go.mod" || file.path.ends_with("/go.mod") {
                    // A fresh build of this tree records no manifest for a
                    // file the scanner never read; drop the stale copy and
                    // re-resolve everything under the changed module config.
                    txn.execute("DELETE FROM manifests WHERE path = ?1", params![file.path])
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    run.manifest_changed = true;
                }
                stmt_update_file
                    .execute(params![
                        file.lang.as_str(),
                        None::<i64>,
                        file.hash.as_ref().map(<[u8; 32]>::as_slice),
                        skip_str,
                        to_i64(file.size),
                        file.mtime,
                        status_str,
                        file_id,
                    ])
                    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                continue;
            }

            let Ok(content_bytes) = fs::read(&full_path) else {
                // Unreadable here and now: persist the only truthful row —
                // never read, no hash, nothing parsed. The next run's hash
                // diff re-extracts the file if it becomes readable again.
                stmt_update_file
                    .execute(params![
                        file.lang.as_str(),
                        None::<i64>,
                        None::<&[u8]>,
                        "unreadable",
                        to_i64(file.size),
                        file.mtime,
                        "skipped",
                        file_id,
                    ])
                    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                continue;
            };

            let source = String::from_utf8_lossy(&content_bytes);
            if file.path == "go.mod" || file.path.ends_with("/go.mod") {
                stmt_insert_manifest
                    .execute(params![file.path, source.as_ref()])
                    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            }

            let extracted =
                extract_with_timeout(&source, file.lang, Duration::from_millis(PARSE_TIMEOUT_MS));
            let (package_id, status_str, extracted_file) = match extracted {
                Ok(ext) => {
                    let pkg_id = if let Some(ref pkg_name) = ext.package_name {
                        let dir = dir_of(&file.path);
                        let (id, created) = get_or_create_pkg(dir, pkg_name)?;
                        run.package_set_changed |= created;
                        run.packages_to_resolve
                            .insert((dir.to_owned(), pkg_name.clone()));
                        Some(id)
                    } else {
                        None
                    };
                    (pkg_id, parse_status_str(ext.status), Some(ext))
                }
                Err(
                    ExtractError::IncompatibleGrammar { .. } | ExtractError::NotImplemented { .. },
                ) => (None, "unsupported", None),
                Err(ExtractError::NoTree) => (None, "partial", None),
            };

            stmt_update_file
                .execute(params![
                    file.lang.as_str(),
                    package_id,
                    file.hash.as_ref().map(<[u8; 32]>::as_slice),
                    None::<&str>,
                    to_i64(file.size),
                    file.mtime,
                    status_str,
                    file_id,
                ])
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;

            if let Some(ext) = extracted_file {
                for def in ext.defs {
                    stmt_insert_symbol
                        .execute(params![
                            file_id,
                            def.name,
                            def.qual_name,
                            def.kind.as_str(),
                            i32::from(def.exported),
                            def.span.start_line,
                            def.span.end_line,
                            def.span.start_byte,
                            def.span.end_byte,
                            def.signature,
                            def.container,
                            def.doc,
                        ])
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                }
                for r in ext.refs {
                    stmt_insert_ref
                        .execute(params![
                            file_id,
                            r.name,
                            r.kind.as_str(),
                            r.qualifier,
                            r.span.start_line,
                            r.span.start_byte,
                            r.span.end_byte,
                            r.container,
                        ])
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                }
                for (ordinal, imp) in ext.imports.into_iter().enumerate() {
                    stmt_insert_import
                        .execute(params![
                            file_id,
                            imp.raw,
                            imp.alias,
                            import_kind_str(imp.kind),
                            to_i64_usize(ordinal),
                        ])
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                }
            }
        }

        // C. Handle added files
        for file in &run.added {
            let hash_bytes = file.hash.as_ref().map(<[u8; 32]>::as_slice);

            // Same read-or-skip contract as the cold path (see facts.rs).
            if let Some((skip_str, status_str)) = skip_label(file) {
                stmt_insert_file
                    .query_row(
                        params![
                            file.path,
                            file.lang.as_str(),
                            None::<i64>,
                            hash_bytes,
                            skip_str,
                            to_i64(file.size),
                            file.mtime,
                            status_str,
                        ],
                        |r| r.get::<_, i64>(0),
                    )
                    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                continue;
            }

            let full_path = root.join(&file.path);
            let Ok(content_bytes) = fs::read(&full_path) else {
                stmt_insert_file
                    .query_row(
                        params![
                            file.path,
                            file.lang.as_str(),
                            None::<i64>,
                            None::<&[u8]>,
                            "unreadable",
                            to_i64(file.size),
                            file.mtime,
                            "skipped",
                        ],
                        |r| r.get::<_, i64>(0),
                    )
                    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                continue;
            };

            let source = String::from_utf8_lossy(&content_bytes);
            if file.path == "go.mod" || file.path.ends_with("/go.mod") {
                run.manifest_changed = true;
                stmt_insert_manifest
                    .execute(params![file.path, source.as_ref()])
                    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            }

            let extracted =
                extract_with_timeout(&source, file.lang, Duration::from_millis(PARSE_TIMEOUT_MS));
            let (package_id, status_str, extracted_file) = match extracted {
                Ok(ext) => {
                    let pkg_id = if let Some(ref pkg_name) = ext.package_name {
                        let dir = dir_of(&file.path);
                        let (id, created) = get_or_create_pkg(dir, pkg_name)?;
                        run.package_set_changed |= created;
                        run.packages_to_resolve
                            .insert((dir.to_owned(), pkg_name.clone()));
                        Some(id)
                    } else {
                        None
                    };
                    (pkg_id, parse_status_str(ext.status), Some(ext))
                }
                Err(
                    ExtractError::IncompatibleGrammar { .. } | ExtractError::NotImplemented { .. },
                ) => (None, "unsupported", None),
                Err(ExtractError::NoTree) => (None, "partial", None),
            };

            let file_id: i64 = stmt_insert_file
                .query_row(
                    params![
                        file.path,
                        file.lang.as_str(),
                        package_id,
                        hash_bytes,
                        None::<&str>,
                        to_i64(file.size),
                        file.mtime,
                        status_str,
                    ],
                    |r| r.get(0),
                )
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;

            if let Some(ext) = extracted_file {
                for def in ext.defs {
                    stmt_insert_symbol
                        .execute(params![
                            file_id,
                            def.name,
                            def.qual_name,
                            def.kind.as_str(),
                            i32::from(def.exported),
                            def.span.start_line,
                            def.span.end_line,
                            def.span.start_byte,
                            def.span.end_byte,
                            def.signature,
                            def.container,
                            def.doc,
                        ])
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                }
                for r in ext.refs {
                    stmt_insert_ref
                        .execute(params![
                            file_id,
                            r.name,
                            r.kind.as_str(),
                            r.qualifier,
                            r.span.start_line,
                            r.span.start_byte,
                            r.span.end_byte,
                            r.container,
                        ])
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                }
                for (ordinal, imp) in ext.imports.into_iter().enumerate() {
                    stmt_insert_import
                        .execute(params![
                            file_id,
                            imp.raw,
                            imp.alias,
                            import_kind_str(imp.kind),
                            to_i64_usize(ordinal),
                        ])
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                }
            }
        }

        drop(stmt_insert_package);
        drop(stmt_select_package);
        drop(stmt_insert_file);
        drop(stmt_update_file);
        drop(stmt_insert_manifest);
        drop(stmt_insert_symbol);
        drop(stmt_insert_ref);
        drop(stmt_insert_import);

        // Garbage-collect packages left with no files (deletions, in-place
        // renames). An empty ghost would otherwise win `non_test_package`
        // for its directory and shadow the real package in every import
        // resolution — deterministically wrong, so it must not survive.
        // A collected package is the other half of the package-set-change
        // signal: its importers' `resolved_dir` values die with it.
        let collected = txn
            .execute(
                "DELETE FROM packages WHERE id NOT IN \
                 (SELECT DISTINCT package_id FROM files WHERE package_id IS NOT NULL)",
                [],
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        run.package_set_changed |= collected > 0;

        txn.commit()
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
    }

    Ok(())
}

/// Phases 4.5-7: importer fan-out for packages discovered during extraction,
/// then resolve the affected packages and rewrite their derived rows. With
/// `rebuild_all` (module config moved, a torn earlier run left the projection
/// untrustworthy, or the fact transaction created/collected a package) every
/// package is re-resolved. Returns how many packages were resolved.
///
/// # Errors
///
/// Propagates I/O and SQLite storage failures.
pub(crate) fn apply_derived_changes(
    db: &mut IndexDatabase,
    run: &mut IncrementalRun<'_>,
    rebuild_all: bool,
) -> Result<usize, IndexError> {
    let db_path = db.path.clone();

    // 4.5 Importer fan-out for packages discovered during extraction: a
    // package that gained a file must also invalidate every importer of its
    // directory, whose import_out edges fan out over the package's full file
    // set. Runs after the fact transaction (the new packages only exist
    // then); blank imports are covered because their resolution is recorded
    // even though they bind nothing. A full rebuild needs no fan-out: every
    // package is already in the set.
    if !rebuild_all {
        let new_dirs: BTreeSet<String> = run
            .packages_to_resolve
            .iter()
            .filter(|key| !run.pre_transaction_keys.contains(*key))
            .map(|(dir, _)| dir.clone())
            .collect();
        for dir in new_dirs {
            for importer in importer_packages(&db.conn, &db_path, &dir)? {
                run.packages_to_resolve.insert(importer);
            }
        }
    }

    // 5-6. Rebuild the exported index and the resolver it feeds.
    let mut sink = cs_resolve::ResolutionStats::default();
    let mut engine = DeriveEngine::build(&db.conn, &db_path)?;

    // A moved module config (or a torn earlier run) re-resolves everything:
    // the invalidation set cannot be trusted to name the stale packages.
    let keys: Vec<(String, String)> = if rebuild_all || run.manifest_changed {
        engine.package_keys()
    } else {
        run.packages_to_resolve.iter().cloned().collect()
    };

    // 7. Resolve the affected packages, rewriting their derived rows.
    let mut resolved = 0;
    for key in &keys {
        if engine.resolve_package(db, key, &mut sink)? {
            resolved += 1;
        }
    }
    Ok(resolved)
}

/// Phase 8: commit the new `snapshot_id` and clear the torn-update marker in
/// one transaction, so the index never claims a finished update whose
/// projection is still marked torn.
///
/// # Errors
///
/// Propagates SQLite storage failures.
pub(crate) fn finalize_incremental(db: &mut IndexDatabase) -> Result<(), IndexError> {
    let db_path = db.path.clone();
    let new_snapshot_id = compute_snapshot_id(&db.conn, &db_path)?;
    let txn = db
        .conn
        .transaction()
        .map_err(|e| IndexDatabase::lock(&db_path, e))?;
    txn.execute(
        "INSERT INTO meta (key, value) VALUES ('snapshot_id', ?1) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![new_snapshot_id],
    )
    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
    txn.execute(
        "DELETE FROM meta WHERE key = ?1",
        params![UPDATE_IN_PROGRESS_KEY],
    )
    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
    txn.commit()
        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
    Ok(())
}

/// Run an incremental update against an existing [`IndexState::Ready`] database.
///
/// # Errors
///
/// Returns [`IndexError::StateConflict`] if the index is not in [`IndexState::Ready`].
/// Propagates I/O and SQLite storage failures.
pub fn update_incremental(
    db: &mut IndexDatabase,
    root: &Path,
    scanned_files: &[ScannedFile],
) -> Result<IncrementalStats, IndexError> {
    let lifecycle = db.state()?;
    if lifecycle != IndexState::Ready {
        return Err(IndexError::StateConflict {
            found: lifecycle,
            expected: "ready",
        });
    }

    // A marker left by a torn previous run means the derived projection is
    // not trustworthy even when every hash now matches: skip the fast path
    // and rebuild the projection from the (hash-consistent) facts.
    let torn = db.meta(UPDATE_IN_PROGRESS_KEY)?.is_some();

    let mut run = plan_incremental(db, scanned_files)?;

    let mut stats = IncrementalStats {
        files_unchanged: run.files_unchanged,
        files_added: run.added.len(),
        files_modified: run.modified.len(),
        files_deleted: run.deleted.len(),
        packages_resolved: 0,
    };

    // Fast-path no-op — only when nothing changed AND no torn run awaits repair.
    if run.is_noop() && !torn {
        return Ok(stats);
    }

    apply_fact_changes(db, root, &mut run)?;
    // A created or collected package invalidates the `resolved_dir` fan-out
    // below: importers of a package that was missing carry NULL resolutions,
    // so no scoped set can name them. Re-resolve everything (facts stay
    // hash-consistent; only the derived projection is rebuilt).
    let rebuild_all = torn || run.package_set_changed;
    stats.packages_resolved = apply_derived_changes(db, &mut run, rebuild_all)?;
    finalize_incremental(db)?;

    Ok(stats)
}

/// Sorted semantic dump of every derived projection — import
/// classification, bindings, binding targets, edges — keyed by paths, names,
/// and spans, never row ids (ids differ between independently built
/// databases). The parity oracle for repair and idempotence tests.
#[cfg(test)]
pub(crate) fn derived_projection(db: &IndexDatabase) -> Vec<String> {
    let conn = db.connection();
    let mut out = Vec::new();

    {
        let mut stmt = conn
            .prepare(
                "SELECT f.path, i.raw, i.resolved_dir, i.resolved_file, i.unresolved_reason \
                 FROM imports i JOIN files f ON f.id = i.file_id \
                 ORDER BY f.path ASC, i.ordinal ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok(format!(
                    "import {} {} dir={:?} file={:?} reason={:?}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })
            .unwrap();
        for row in rows {
            out.push(row.unwrap());
        }
    }

    {
        let mut stmt = conn
            .prepare(
                "SELECT f.path, r.name, r.start_byte, b.unbound_reason \
                 FROM bindings b JOIN refs r ON r.id = b.ref_id \
                 JOIN files f ON f.id = r.file_id \
                 ORDER BY f.path ASC, r.start_byte ASC, r.name ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok(format!(
                    "binding {} {} @{} reason={:?}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .unwrap();
        for row in rows {
            out.push(row.unwrap());
        }
    }

    {
        let mut stmt = conn
            .prepare(
                "SELECT f1.path, r.name, r.start_byte, f2.path, bt.qual_name, bt.kind \
                 FROM binding_targets bt JOIN refs r ON r.id = bt.ref_id \
                 JOIN files f1 ON f1.id = r.file_id JOIN files f2 ON f2.id = bt.file_id \
                 ORDER BY f1.path ASC, r.start_byte ASC, r.name ASC, f2.path ASC, \
                 bt.qual_name ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok(format!(
                    "target {} {} @{} -> {} {} {}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .unwrap();
        for row in rows {
            out.push(row.unwrap());
        }
    }

    {
        let mut stmt = conn
            .prepare(
                "SELECT f1.path, f2.path, e.kind, e.weight FROM edges e \
                 JOIN files f1 ON f1.id = e.src JOIN files f2 ON f2.id = e.dst \
                 ORDER BY f1.path ASC, f2.path ASC, e.kind ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok(format!(
                    "edge {} {} {} weight={:?}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, f64>(3)?,
                ))
            })
            .unwrap();
        for row in rows {
            out.push(row.unwrap());
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::ingest_facts;
    use crate::resolve_pass::resolve_facts;
    use crate::IndexDatabase;
    use cs_scanner::{scan, ScanConfig, SkipReason};
    use tempfile::{tempdir, TempDir};

    fn write_file(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    /// Build a complete (Ready) index over `root`, with the database kept
    /// outside the scanned tree so it never becomes a scanned file itself.
    fn build_index(root: &Path) -> (TempDir, IndexDatabase) {
        let files = scan(root, &ScanConfig::default()).unwrap();
        let db_dir = tempdir().unwrap();
        let mut db = IndexDatabase::open_or_create(&db_dir.path().join("index.db")).unwrap();
        ingest_facts(&mut db, root, &files, 100).unwrap();
        resolve_facts(&mut db).unwrap();
        (db_dir, db)
    }

    /// The full edge set as (src path, dst path, kind), sorted.
    fn edge_triples(db: &IndexDatabase) -> Vec<(String, String, String)> {
        let conn = db.connection();
        let mut stmt = conn
            .prepare(
                "SELECT f1.path, f2.path, e.kind FROM edges e \
                 JOIN files f1 ON f1.id = e.src \
                 JOIN files f2 ON f2.id = e.dst \
                 ORDER BY f1.path ASC, f2.path ASC, e.kind ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    /// The `(hash, skip, parse_status)` triple stored for one file path.
    fn file_state(db: &IndexDatabase, path: &str) -> (Option<Vec<u8>>, Option<String>, String) {
        db.connection()
            .query_row(
                "SELECT hash, skip, parse_status FROM files WHERE path = ?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    /// Runtime canary: does `chmod 000` actually deny reads for this user?
    /// Root bypasses file modes, so permission-based tests must skip honestly.
    #[cfg(unix)]
    fn chmod_denies_read() -> bool {
        let tmp = tempdir().unwrap();
        let canary = tmp.path().join("canary");
        fs::write(&canary, b"x").unwrap();
        set_file_mode(&canary, 0o0);
        fs::read(&canary).is_err()
    }

    #[cfg(unix)]
    fn set_file_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(mode);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn incremental_noop_does_zero_work() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(tmp.path(), "a.go", b"package test\nfunc Alpha() {}\n");

        let files = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let db_path = tmp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).unwrap();
        ingest_facts(&mut db, tmp.path(), &files, 100).unwrap();
        resolve_facts(&mut db).unwrap();

        let snap_before = db.meta("snapshot_id").unwrap().unwrap();

        // Run incremental with unchanged files
        let stats = update_incremental(&mut db, tmp.path(), &files).unwrap();
        assert_eq!(stats.files_unchanged, 2);
        assert_eq!(stats.files_added, 0);
        assert_eq!(stats.files_modified, 0);
        assert_eq!(stats.files_deleted, 0);
        assert_eq!(stats.packages_resolved, 0);

        let snap_after = db.meta("snapshot_id").unwrap().unwrap();
        assert_eq!(snap_before, snap_after);
    }

    #[test]
    fn incremental_repairs_torn_update_after_fact_commit() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(
            tmp.path(),
            "auth/auth.go",
            b"package auth\nfunc Login() {}\n",
        );
        write_file(
            tmp.path(),
            "web/web.go",
            b"package web\nimport \"example.com/test/auth\"\nfunc Handler() { auth.Login() }\n",
        );

        let (_db_dir, mut db) = build_index(tmp.path());
        let before = derived_projection(&db);
        assert!(
            before.iter().any(|r| r.starts_with("edge")),
            "fixture must produce derived rows"
        );

        // Change the tree, then run the update phased and stop after the
        // fact transaction commits — the exact residue of a crash between
        // the facts commit and the derived phase. No timers, no processes.
        write_file(
            tmp.path(),
            "web/web.go",
            b"package web\nimport \"example.com/test/auth\"\nfunc Handler() { auth.Logout() }\n",
        );
        let files_after = scan(tmp.path(), &ScanConfig::default()).unwrap();

        let mut run = plan_incremental(&db, &files_after).unwrap();
        assert_eq!(run.modified.len(), 1);
        apply_fact_changes(&mut db, tmp.path(), &mut run).unwrap();
        // CRASH: apply_derived_changes and finalize_incremental never run.

        // The torn state: still Ready, hashes already reflect the new
        // content, and the marker names the torn run.
        assert_eq!(db.state().unwrap(), IndexState::Ready);
        assert!(
            db.meta(UPDATE_IN_PROGRESS_KEY).unwrap().is_some(),
            "the facts commit must have planted the torn-update marker"
        );

        // Every hash matches now, so only the marker separates this run
        // from a no-op: it must trigger a repair, not the fast path.
        let stats = update_incremental(&mut db, tmp.path(), &files_after).unwrap();
        assert_eq!(stats.files_unchanged, 3);
        assert_eq!(stats.files_added, 0);
        assert_eq!(stats.files_modified, 0);
        assert_eq!(stats.files_deleted, 0);
        assert!(
            stats.packages_resolved > 0,
            "a torn index must be re-resolved, not skipped: {stats:?}"
        );
        assert!(
            db.meta(UPDATE_IN_PROGRESS_KEY).unwrap().is_none(),
            "a completed run must clear the torn-update marker"
        );

        // Full semantic parity with a fresh build of the same tree.
        let (_fresh_dir, fresh) = build_index(tmp.path());
        assert_eq!(
            derived_projection(&db),
            derived_projection(&fresh),
            "recovered index must match a fresh build on every derived table"
        );
    }

    #[test]
    fn incremental_repairs_crash_mid_derivation() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(
            tmp.path(),
            "auth/auth.go",
            b"package auth\nfunc Login() {}\n",
        );
        write_file(
            tmp.path(),
            "web/web.go",
            b"package web\nimport \"example.com/test/auth\"\nfunc Handler() { auth.Login() }\n",
        );

        let (_db_dir, mut db) = build_index(tmp.path());

        write_file(
            tmp.path(),
            "auth/auth.go",
            b"package auth\nfunc Logout() {}\n",
        );
        let files_after = scan(tmp.path(), &ScanConfig::default()).unwrap();

        // Crash after the facts commit but partway through derivation: one
        // package of the affected set resolved and committed, the rest not.
        let mut run = plan_incremental(&db, &files_after).unwrap();
        assert!(run.packages_to_resolve.len() >= 2);
        apply_fact_changes(&mut db, tmp.path(), &mut run).unwrap();
        let mut engine =
            crate::resolve_pass::DeriveEngine::build(db.connection(), db.path()).unwrap();
        let mut sink = cs_resolve::ResolutionStats::default();
        let first_key = run.packages_to_resolve.iter().next().cloned().unwrap();
        engine
            .resolve_package(&mut db, &first_key, &mut sink)
            .unwrap();
        assert!(
            db.meta(UPDATE_IN_PROGRESS_KEY).unwrap().is_some(),
            "an unfinished derivation keeps the marker set"
        );

        update_incremental(&mut db, tmp.path(), &files_after).unwrap();
        assert!(db.meta(UPDATE_IN_PROGRESS_KEY).unwrap().is_none());

        let (_fresh_dir, fresh) = build_index(tmp.path());
        assert_eq!(
            derived_projection(&db),
            derived_projection(&fresh),
            "a partially derived index must still repair to fresh-build parity"
        );
    }

    #[test]
    fn incremental_modified_file_updates_facts_and_resolves_package() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(tmp.path(), "a.go", b"package test\nfunc Alpha() {}\n");
        write_file(
            tmp.path(),
            "b.go",
            b"package test\nfunc Beta() { Alpha() }\n",
        );

        let files = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let db_path = tmp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).unwrap();
        ingest_facts(&mut db, tmp.path(), &files, 100).unwrap();
        resolve_facts(&mut db).unwrap();

        let edge_count_before: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE kind = 'ref_def'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(edge_count_before, 1, "b.go -> a.go ref_def edge must exist");

        // Now modify a.go: rename Alpha to Omega
        write_file(tmp.path(), "a.go", b"package test\nfunc Omega() {}\n");

        let files_after = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let stats = update_incremental(&mut db, tmp.path(), &files_after).unwrap();
        assert_eq!(stats.files_modified, 1);
        assert_eq!(stats.packages_resolved, 1);

        // Verify symbol Omega is in symbols table and Alpha is gone
        let omega_count: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE name = 'Omega'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(omega_count, 1);
        let alpha_count: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE name = 'Alpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alpha_count, 0);

        // Verify ref_def edge is gone because Alpha is now unbound
        let edge_count_after: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE kind = 'ref_def'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(edge_count_after, 0);

        // Verify binding unbound reason for Alpha is no_candidate
        let unbound_reason: String = db
            .connection()
            .query_row(
                "SELECT b.unbound_reason FROM bindings b JOIN refs r ON r.id = b.ref_id WHERE r.name = 'Alpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unbound_reason, "no_candidate");
    }

    #[test]
    fn incremental_reverse_invalidation_updates_external_callers() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(
            tmp.path(),
            "auth/auth.go",
            b"package auth\nfunc Login() {}\n",
        );
        write_file(
            tmp.path(),
            "web/web.go",
            b"package web\nimport \"example.com/test/auth\"\nfunc Handler() { auth.Login() }\n",
        );

        let files = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let db_path = tmp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).unwrap();
        ingest_facts(&mut db, tmp.path(), &files, 100).unwrap();
        resolve_facts(&mut db).unwrap();

        let edge_count_before: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE kind = 'ref_def'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            edge_count_before, 1,
            "web -> auth ref_def edge must initially exist"
        );

        // Modify auth/auth.go: remove Login()
        write_file(
            tmp.path(),
            "auth/auth.go",
            b"package auth\nfunc Logout() {}\n",
        );

        let files_after = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let stats = update_incremental(&mut db, tmp.path(), &files_after).unwrap();
        assert_eq!(stats.files_modified, 1);
        // Both auth (directly modified) and web (reverse-invalidated caller) must be re-resolved
        assert_eq!(stats.packages_resolved, 2);

        let edge_count_after: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE kind = 'ref_def'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            edge_count_after, 0,
            "web -> auth ref_def edge must be gone after Login is deleted"
        );
    }

    #[test]
    fn incremental_delete_file_cleans_up_and_invalidates_callers() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(
            tmp.path(),
            "auth/login.go",
            b"package auth\nfunc Login() {}\n",
        );
        write_file(
            tmp.path(),
            "web/web.go",
            b"package web\nimport \"example.com/test/auth\"\nfunc Handler() { auth.Login() }\n",
        );

        let files = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let db_path = tmp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).unwrap();
        ingest_facts(&mut db, tmp.path(), &files, 100).unwrap();
        resolve_facts(&mut db).unwrap();

        // Delete auth/login.go from disk
        fs::remove_file(tmp.path().join("auth/login.go")).unwrap();

        let files_after = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let stats = update_incremental(&mut db, tmp.path(), &files_after).unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(stats.packages_resolved, 1); // web is reverse-invalidated

        // Check that auth/login.go is deleted from files table
        let deleted_count: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path = 'auth/login.go'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(deleted_count, 0);

        // Check that binding in web is now unbound exactly as a fresh build
        // of this tree reports it: the emptied auth package is gone, so the
        // auth qualifier lives in unresolved-import scope.
        let reason: Option<String> = db
            .connection()
            .query_row(
                "SELECT b.unbound_reason FROM bindings b JOIN refs r ON r.id = b.ref_id WHERE r.name = 'Login'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(reason.is_some(), "the Login ref must be unbound");
        let (_fresh_dir, fresh) = build_index(tmp.path());
        let fresh_reason: Option<String> = fresh
            .connection()
            .query_row(
                "SELECT b.unbound_reason FROM bindings b JOIN refs r ON r.id = b.ref_id WHERE r.name = 'Login'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reason, fresh_reason);
    }

    #[test]
    fn incremental_sibling_edit_parities_with_fresh_build() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(tmp.path(), "auth/a.go", b"package auth\nfunc Login() {}\n");
        write_file(tmp.path(), "auth/b.go", b"package auth\nfunc Logout() {}\n");
        write_file(
            tmp.path(),
            "web/app.go",
            b"package web\nimport \"example.com/test/auth\"\nfunc Handler() { auth.Login() }\n",
        );

        let (_db_dir, mut db) = build_index(tmp.path());

        // Edit the SIBLING b.go: web binds only into a.go, so file-scoped
        // invalidation sees no caller — yet web's import_out edge fans out
        // over every file of package auth (ADR-021 D5).
        write_file(
            tmp.path(),
            "auth/b.go",
            b"package auth\nfunc Logout() { Login() }\n",
        );

        let files_after = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let stats = update_incremental(&mut db, tmp.path(), &files_after).unwrap();
        assert_eq!(stats.files_modified, 1);
        // auth itself plus the reverse-invalidated importer web.
        assert_eq!(stats.packages_resolved, 2);

        let (_fresh_dir, fresh) = build_index(tmp.path());
        assert_eq!(
            edge_triples(&db),
            edge_triples(&fresh),
            "incremental edges must match a fresh build of the same tree"
        );
    }

    #[test]
    fn incremental_blank_import_sibling_edit_parities_with_fresh_build() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(tmp.path(), "auth/a.go", b"package auth\nfunc Login() {}\n");
        write_file(tmp.path(), "auth/b.go", b"package auth\nfunc Logout() {}\n");
        // A blank import produces no bindings at all — only its import_out
        // edges tie it to the imported package's files.
        write_file(
            tmp.path(),
            "web/blank.go",
            b"package web\nimport _ \"example.com/test/auth\"\n",
        );

        let (_db_dir, mut db) = build_index(tmp.path());

        write_file(
            tmp.path(),
            "auth/b.go",
            b"package auth\nfunc Logout() { Login() }\n",
        );

        let files_after = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let stats = update_incremental(&mut db, tmp.path(), &files_after).unwrap();
        assert_eq!(stats.files_modified, 1);
        assert_eq!(stats.packages_resolved, 2);

        let (_fresh_dir, fresh) = build_index(tmp.path());
        assert_eq!(
            edge_triples(&db),
            edge_triples(&fresh),
            "blank-import edges must survive a sibling edit exactly as in a fresh build"
        );
    }

    #[test]
    fn incremental_package_rename_garbage_collects_ghost() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(tmp.path(), "pkg/x.go", b"package alpha\nfunc A() {}\n");
        write_file(
            tmp.path(),
            "web/web.go",
            b"package web\nimport \"example.com/test/pkg\"\nfunc H() { alpha.A() }\n",
        );

        let (_db_dir, mut db) = build_index(tmp.path());

        // Rename the package in place; the caller follows the rename.
        write_file(tmp.path(), "pkg/x.go", b"package beta\nfunc A() {}\n");
        write_file(
            tmp.path(),
            "web/web.go",
            b"package web\nimport \"example.com/test/pkg\"\nfunc H() { beta.A() }\n",
        );

        let files_after = scan(tmp.path(), &ScanConfig::default()).unwrap();
        update_incremental(&mut db, tmp.path(), &files_after).unwrap();

        // The renamed-away package must not linger as an empty ghost that
        // `non_test_package` would prefer over the real one.
        let ghost: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM packages WHERE dir = 'pkg' AND name = 'alpha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ghost, 0, "renamed-away package must be garbage-collected");

        let (_fresh_dir, fresh) = build_index(tmp.path());
        assert_eq!(
            edge_triples(&db),
            edge_triples(&fresh),
            "incremental edges must match a fresh build of the same tree"
        );
    }

    #[cfg(unix)]
    #[test]
    fn incremental_trusts_scanner_unreadable_skip() {
        if !chmod_denies_read() {
            eprintln!("skipping: chmod 000 does not deny reads for this user");
            return;
        }
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(tmp.path(), "a.go", b"package p\nfunc A() {}\n");
        write_file(tmp.path(), "locked.go", b"package p\nfunc Locked() {}\n");

        let (_db_dir, mut db) = build_index(tmp.path());

        // The scanner sees locked.go as Unreadable...
        let locked = tmp.path().join("locked.go");
        set_file_mode(&locked, 0o0);

        let files_after = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let scanned = files_after.iter().find(|f| f.path == "locked.go").unwrap();
        assert_eq!(scanned.skip, Some(SkipReason::Unreadable));

        // ...and it is readable again by the time the incremental runs. The
        // scanner's label must win: never read what the scanner skipped.
        set_file_mode(&locked, 0o644);

        let stats = update_incremental(&mut db, tmp.path(), &files_after).unwrap();
        assert_eq!(stats.files_modified, 1);
        let (stored_hash, stored_skip, stored_status) = file_state(&db, "locked.go");
        assert_eq!(stored_hash, None);
        assert_eq!(stored_skip.as_deref(), Some("unreadable"));
        assert_eq!(stored_status, "skipped");

        // Self-heal: the next run sees the readable file and re-ingests it.
        let files_healed = scan(tmp.path(), &ScanConfig::default()).unwrap();
        update_incremental(&mut db, tmp.path(), &files_healed).unwrap();
        let (stored_hash, stored_skip, stored_status) = file_state(&db, "locked.go");
        assert!(stored_hash.is_some(), "healed file must be hashed again");
        assert_eq!(stored_skip, None);
        assert_eq!(stored_status, "ok");
    }

    #[cfg(unix)]
    #[test]
    fn incremental_read_failure_writes_unreadable_row() {
        if !chmod_denies_read() {
            eprintln!("skipping: chmod 000 does not deny reads for this user");
            return;
        }
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(tmp.path(), "a.go", b"package p\nfunc A() {}\n");

        let (_db_dir, mut db) = build_index(tmp.path());

        // Change the content, scan it while readable (so it classifies as
        // modified with a fresh hash), then make the read fail mid-update.
        write_file(tmp.path(), "a.go", b"package p\nfunc B() {}\n");
        let files_after = scan(tmp.path(), &ScanConfig::default()).unwrap();

        let a = tmp.path().join("a.go");
        set_file_mode(&a, 0o0);

        let stats = update_incremental(&mut db, tmp.path(), &files_after).unwrap();
        assert_eq!(stats.files_modified, 1);
        let (stored_hash, stored_skip, stored_status) = file_state(&db, "a.go");
        assert_eq!(stored_hash, None, "an unread row must not carry a hash");
        assert_eq!(stored_skip.as_deref(), Some("unreadable"));
        assert_eq!(stored_status, "skipped");

        set_file_mode(&a, 0o644);
    }

    #[test]
    fn incremental_parse_cap_transition_parities_with_fresh_build() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(tmp.path(), "pkg/a.go", b"package pkg\nfunc A() {}\n");
        write_file(tmp.path(), "pkg/big.go", b"package pkg\nfunc Big() {}\n");
        write_file(
            tmp.path(),
            "web/w.go",
            b"package web\nimport \"example.com/test/pkg\"\nfunc H() { pkg.Big() }\n",
        );

        let (_db_dir, mut db) = build_index(tmp.path());

        // big.go crosses the 1 MiB parse cap between runs (still under the
        // read cap, so the scanner hashes it but nothing parses it).
        let big = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tmp.path().join("pkg/big.go"))
            .unwrap();
        big.set_len(2 * 1024 * 1024).unwrap();
        drop(big);

        let files_after = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let scanned = files_after.iter().find(|f| f.path == "pkg/big.go").unwrap();
        assert_eq!(scanned.skip, Some(SkipReason::ParseSkipped));
        assert!(scanned.hash.is_some(), "parse-skipped files stay hashed");

        let step_stats = update_incremental(&mut db, tmp.path(), &files_after).unwrap();
        assert_eq!(step_stats.files_modified, 1);
        let (hash, skip, status) = file_state(&db, "pkg/big.go");
        assert_eq!(
            hash.as_deref(),
            scanned.hash.as_ref().map(<[u8; 32]>::as_slice),
            "the parse-skip band keeps the scanner's hash"
        );
        assert_eq!(skip.as_deref(), Some("parse_skipped"));
        assert_eq!(status, "skipped");

        // The whole stored state must equal a fresh cold build of this tree:
        // the files row (including the NULLed package), the absence of stale
        // facts, and every derived row.
        let (_fresh_dir, fresh) = build_index(tmp.path());
        for probe in [&db, &fresh] {
            let row: (String, Option<i64>, Option<String>, String) = probe
                .connection()
                .query_row(
                    "SELECT lang, package_id, skip, parse_status FROM files \
                     WHERE path = 'pkg/big.go'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            assert_eq!(row.0, "go");
            assert_eq!(
                row.1, None,
                "a parse-skipped file belongs to no package (facts row)"
            );
            assert_eq!(row.2.as_deref(), Some("parse_skipped"));
            assert_eq!(row.3, "skipped");
            let facts: i64 = probe
                .connection()
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM symbols WHERE file_id = \
                     (SELECT id FROM files WHERE path = 'pkg/big.go')) + \
                     (SELECT COUNT(*) FROM refs WHERE file_id = \
                     (SELECT id FROM files WHERE path = 'pkg/big.go')) + \
                     (SELECT COUNT(*) FROM imports WHERE file_id = \
                     (SELECT id FROM files WHERE path = 'pkg/big.go'))",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(facts, 0, "a parse-skipped file carries no facts");
        }
        assert_eq!(
            derived_projection(&db),
            derived_projection(&fresh),
            "crossing the parse cap must leave the index exactly as a fresh build"
        );

        // And the way back under the cap: re-parsed, still at fresh-build parity.
        write_file(tmp.path(), "pkg/big.go", b"package pkg\nfunc Big2() {}\n");
        let files_healed = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let step_stats = update_incremental(&mut db, tmp.path(), &files_healed).unwrap();
        assert_eq!(step_stats.files_modified, 1);
        let (hash, skip, status) = file_state(&db, "pkg/big.go");
        assert!(hash.is_some(), "a re-parsed file is hashed again");
        assert_eq!(skip, None);
        assert_eq!(status, "ok");

        let (_fresh2_dir, fresh2) = build_index(tmp.path());
        assert_eq!(
            derived_projection(&db),
            derived_projection(&fresh2),
            "dropping back under the parse cap must also parity with a fresh build"
        );
    }

    #[cfg(unix)]
    #[test]
    fn recreated_package_rebinds_its_importers() {
        if !chmod_denies_read() {
            eprintln!("skipping: chmod 000 does not deny reads for this user");
            return;
        }
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(
            tmp.path(),
            "auth/auth.go",
            b"package auth\nfunc Login() {}\n",
        );
        write_file(
            tmp.path(),
            "web/w.go",
            b"package web\nimport \"example.com/test/auth\"\nfunc H() { auth.Login() }\n",
        );

        let (_db_dir, mut db) = build_index(tmp.path());

        // auth's only file becomes unreadable: the package is garbage-collected
        // and web's import is re-classified unresolved (resolved_dir NULL).
        let auth = tmp.path().join("auth/auth.go");
        set_file_mode(&auth, 0o0);
        let files_gone = scan(tmp.path(), &ScanConfig::default()).unwrap();
        update_incremental(&mut db, tmp.path(), &files_gone).unwrap();
        let auth_pkg: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM packages WHERE dir = 'auth'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(auth_pkg, 0, "the emptied auth package must be collected");
        let (resolved, reason): (Option<String>, Option<String>) = db
            .connection()
            .query_row(
                "SELECT i.resolved_dir, i.unresolved_reason FROM imports i \
                 JOIN files f ON f.id = i.file_id WHERE f.path = 'web/w.go'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(resolved, None);
        assert!(
            reason.is_some(),
            "the import must carry a reason, not nothing"
        );

        // The file comes back readable: the package is recreated. While it was
        // gone, web's import row said resolved_dir IS NULL, so fan-out keyed on
        // resolved_dir cannot name web — yet web must rebind exactly as a fresh
        // build of the restored tree.
        set_file_mode(&auth, 0o644);
        let files_back = scan(tmp.path(), &ScanConfig::default()).unwrap();
        update_incremental(&mut db, tmp.path(), &files_back).unwrap();

        let (_fresh_dir, fresh) = build_index(tmp.path());
        assert_eq!(
            derived_projection(&db),
            derived_projection(&fresh),
            "a recreated package must rebind its importers to fresh-build rows"
        );
    }

    #[test]
    fn incremental_too_large_modified_file_is_never_read() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(tmp.path(), "a.go", b"package p\nfunc A() {}\n");
        write_file(tmp.path(), "big.go", b"package p\nfunc Big() {}\n");

        let (_db_dir, mut db) = build_index(tmp.path());

        // Grow big.go past the 50 MiB read cap as a sparse file: the scanner
        // lists it with skip=TooLarge and no hash; the incremental must not
        // read it back.
        let big = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(tmp.path().join("big.go"))
            .unwrap();
        big.set_len(60 * 1024 * 1024).unwrap();
        drop(big);

        let files_after = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let scanned = files_after.iter().find(|f| f.path == "big.go").unwrap();
        assert_eq!(scanned.skip, Some(SkipReason::TooLarge));

        let stats = update_incremental(&mut db, tmp.path(), &files_after).unwrap();
        assert_eq!(stats.files_modified, 1);
        let (stored_hash, stored_skip, stored_status) = file_state(&db, "big.go");
        assert_eq!(stored_hash, None);
        assert_eq!(stored_skip.as_deref(), Some("too_large"));
        assert_eq!(stored_status, "skipped");

        // Its pre-change facts are gone, as in a fresh build of this tree.
        let symbols: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE file_id = \
                 (SELECT id FROM files WHERE path = 'big.go')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(symbols, 0);
    }
}
