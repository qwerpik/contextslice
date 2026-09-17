//! Incremental indexing engine.
//!
//! Implements Milestone M5 of the streaming-first architecture (ADR-021, ARCHITECTURE §4.4).
//!
//! Key principles:
//! - Content hashing (`blake3`) is always computed; `mtime` is recorded for diagnostics and never trusted as a skip shortcut.
//! - Unchanged files: zero parsing, zero re-extraction, fast no-op return.
//! - Modified/added file in package P: re-extract its facts, re-resolve package P.
//! - Reverse invalidation: callers whose `binding_targets` pointed into P are re-resolved in their own package contexts.
//! - Deleted file: delete row (cascades to facts) + reverse-invalidate consumers.
//! - State invariant: incremental runs operate on and remain in [`IndexState::Ready`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::Duration;

use cs_extract::{
    extract_with_timeout, DefKind, ExtractError, ImportKind, ParseStatus, RefKind, Span,
    PARSE_TIMEOUT_MS,
};
use cs_resolve::{
    go::{GoResolver, ModuleInfo, Package as GoPackage},
    ref_def_weight, DefLoc, FilePath, Resolution, UnboundReason, W_IMPORT_OUT, W_TEST_AFFINITY,
};
use cs_scanner::{ScannedFile, SkipReason};
use rusqlite::params;

use crate::facts::{dir_of, import_kind_str, parse_status_str, to_i64};
use crate::index_pass::{build_exported_index, compute_snapshot_id, parse_def_kind};
use crate::{IndexDatabase, IndexError, IndexState};

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
struct ExistingFile {
    id: i64,
    path: String,
    hash: Option<Vec<u8>>,
}

/// Helper to convert `i64` to `u32` safely.
fn to_u32(val: i64) -> u32 {
    u32::try_from(val).unwrap_or(0)
}

fn parse_import_kind(kind: &str) -> ImportKind {
    match kind {
        "export-from" => ImportKind::ExportFrom,
        "require" => ImportKind::Require,
        "dynamic" => ImportKind::Dynamic,
        _ => ImportKind::Import,
    }
}

fn parse_ref_kind(kind: &str) -> RefKind {
    match kind {
        "field_ref" => RefKind::FieldRef,
        "call_ref" => RefKind::CallRef,
        "type_ref" => RefKind::TypeRef,
        _ => RefKind::NameRef,
    }
}

/// Run an incremental update against an existing [`IndexState::Ready`] database.
///
/// # Errors
///
/// Returns [`IndexError::StateConflict`] if the index is not in [`IndexState::Ready`].
/// Propagates I/O and SQLite storage failures.
#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::case_sensitive_file_extension_comparisons
)]
pub fn update_incremental(
    db: &mut IndexDatabase,
    root: &Path,
    scanned_files: &[ScannedFile],
) -> Result<IncrementalStats, IndexError> {
    let state = db.state()?;
    if state != IndexState::Ready {
        return Err(IndexError::StateConflict {
            found: state,
            expected: "ready",
        });
    }

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
    let mut stats = IncrementalStats::default();
    let mut added: Vec<&ScannedFile> = Vec::new();
    let mut modified: Vec<(&ScannedFile, ExistingFile)> = Vec::new();
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
                stats.files_unchanged += 1;
            } else {
                stats.files_modified += 1;
                modified.push((file, existing.clone()));
            }
        } else {
            stats.files_added += 1;
            added.push(file);
        }
    }

    let mut deleted: Vec<ExistingFile> = Vec::new();
    for (path, existing) in existing_files {
        if !seen_paths.contains(&path) {
            stats.files_deleted += 1;
            deleted.push(existing);
        }
    }

    // Fast-path no-op
    if added.is_empty() && modified.is_empty() && deleted.is_empty() {
        return Ok(stats);
    }

    // 3. Collect directly affected packages & reverse-invalidated callers
    let mut packages_to_resolve: BTreeSet<(String, String)> = BTreeSet::new();
    let mut reverse_file_ids: BTreeSet<i64> = BTreeSet::new();
    let mut manifest_changed = false;

    for (file, existing) in &modified {
        if file.path == "go.mod" || file.path.ends_with("/go.mod") {
            manifest_changed = true;
        }
        let file_id = existing.id;
        let mut stmt = db
            .conn
            .prepare(
                "SELECT DISTINCT r.file_id FROM binding_targets bt \
                 JOIN refs r ON r.id = bt.ref_id \
                 WHERE bt.file_id = ?1",
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        let mut rows = stmt
            .query(params![file_id])
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        while let Some(row) = rows
            .next()
            .map_err(|e| IndexDatabase::classify(&db_path, e))?
        {
            let caller_file_id: i64 = row
                .get(0)
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            reverse_file_ids.insert(caller_file_id);
        }
    }

    for existing in &deleted {
        if existing.path == "go.mod" || existing.path.ends_with("/go.mod") {
            manifest_changed = true;
        }
        let file_id = existing.id;
        let mut stmt = db
            .conn
            .prepare(
                "SELECT DISTINCT r.file_id FROM binding_targets bt \
                 JOIN refs r ON r.id = bt.ref_id \
                 WHERE bt.file_id = ?1",
            )
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        let mut rows = stmt
            .query(params![file_id])
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
        while let Some(row) = rows
            .next()
            .map_err(|e| IndexDatabase::classify(&db_path, e))?
        {
            let caller_file_id: i64 = row
                .get(0)
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            reverse_file_ids.insert(caller_file_id);
        }
    }

    // Add reverse-invalidated packages
    for fid in reverse_file_ids {
        let pkg_info: Option<(String, String)> = db
            .conn
            .query_row(
                "SELECT p.dir, p.name FROM files f JOIN packages p ON p.id = f.package_id WHERE f.id = ?1",
                params![fid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        if let Some(pkg) = pkg_info {
            packages_to_resolve.insert(pkg);
        }
    }

    // 4. Update Facts in a single transaction
    {
        let txn = db
            .conn
            .transaction()
            .map_err(|e| IndexDatabase::lock(&db_path, e))?;

        // A. Handle deleted files
        for existing in &deleted {
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

        // Helper closures
        let mut get_or_create_pkg = |dir: &str, name: &str| -> Result<i64, IndexError> {
            stmt_insert_package
                .execute(params![dir, name, "go"])
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            stmt_select_package
                .query_row(params![dir, name, "go"], |r| r.get(0))
                .map_err(|e| IndexDatabase::classify(&db_path, e))
        };

        // B. Handle modified files
        for (file, existing) in &modified {
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
            let Ok(content_bytes) = fs::read(&full_path) else {
                stmt_update_file
                    .execute(params![
                        file.lang.as_str(),
                        None::<i64>,
                        file.hash.as_ref().map(<[u8; 32]>::as_slice),
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
                        let id = get_or_create_pkg(dir, pkg_name)?;
                        packages_to_resolve.insert((dir.to_owned(), pkg_name.clone()));
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
        for file in &added {
            let hash_bytes = file.hash.as_ref().map(<[u8; 32]>::as_slice);
            if file.skip == Some(SkipReason::TooLarge) {
                stmt_insert_file
                    .query_row(
                        params![
                            file.path,
                            file.lang.as_str(),
                            None::<i64>,
                            None::<&[u8]>,
                            "too_large",
                            to_i64(file.size),
                            file.mtime,
                            "skipped",
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
                            hash_bytes,
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
                manifest_changed = true;
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
                        let id = get_or_create_pkg(dir, pkg_name)?;
                        packages_to_resolve.insert((dir.to_owned(), pkg_name.clone()));
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

        txn.commit()
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
    }

    // 5. Rebuild ExportedIndex (Pass 2)
    let mut exported_index = build_exported_index(&db.conn, &db_path)?;

    if manifest_changed {
        // Module configuration changed: re-resolve all packages in the repo
        for key in exported_index.packages.keys() {
            packages_to_resolve.insert(key.clone());
        }
    }

    // 6. Build GoResolver from updated ExportedIndex
    let mut resolver = GoResolver::new_empty(
        exported_index
            .modules
            .iter()
            .map(|m| ModuleInfo::new(m.dir.clone(), m.path.clone()))
            .collect(),
        exported_index.package_dirs.clone(),
        exported_index.has_vendor,
    );

    for ((dir, name), pkg) in &mut exported_index.packages {
        let files: Vec<FilePath> = pkg.files.iter().map(|(_, path)| path.clone()).collect();
        let mut exported: BTreeMap<String, Vec<DefLoc>> = BTreeMap::new();
        let raw_exported = std::mem::take(&mut pkg.exported);
        for (sym_name, syms) in raw_exported {
            let locs = syms
                .into_iter()
                .map(|s| DefLoc {
                    file: s.file_path,
                    qual_name: s.qual_name,
                    kind: s.kind,
                })
                .collect();
            exported.insert(sym_name, locs);
        }
        resolver.add_package(
            (dir.clone(), name.clone()),
            GoPackage {
                files,
                defs: BTreeMap::new(),
                exported,
                methods: BTreeMap::new(),
            },
        );
    }

    let mut dummy_stats = cs_resolve::ResolutionStats::default();

    // 7. Resolve affected packages
    for key in &packages_to_resolve {
        let Some(pkg_exported) = exported_index.packages.get(key) else {
            continue;
        };
        if pkg_exported.files.is_empty() {
            continue;
        }

        stats.packages_resolved += 1;
        let txn = db
            .conn
            .transaction()
            .map_err(|e| IndexDatabase::lock(&db_path, e))?;

        {
            let mut stmt_load_symbols = txn
                .prepare("SELECT file_id, name, qual_name, kind FROM symbols WHERE file_id = ?1")
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            let mut stmt_load_imports = txn
                .prepare(
                    "SELECT id, raw, alias, kind, ordinal FROM imports \
                     WHERE file_id = ?1 ORDER BY ordinal ASC",
                )
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            let mut stmt_load_refs = txn
                .prepare(
                    "SELECT id, name, kind, qualifier, line, start_byte, end_byte, container \
                     FROM refs WHERE file_id = ?1 ORDER BY id ASC",
                )
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;

            let mut stmt_update_import = txn
                .prepare(
                    "UPDATE imports SET resolved_dir = ?1, resolved_file = ?2, \
                     unresolved_reason = ?3 WHERE id = ?4",
                )
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            let mut stmt_insert_binding = txn
                .prepare(
                    "INSERT INTO bindings (ref_id, unbound_reason) VALUES (?1, ?2) \
                     ON CONFLICT(ref_id) DO UPDATE SET unbound_reason = excluded.unbound_reason",
                )
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            let mut stmt_insert_binding_target = txn
                .prepare(
                    "INSERT INTO binding_targets (ref_id, file_id, qual_name, kind) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
                )
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            let mut stmt_insert_edge = txn
                .prepare(
                    "INSERT INTO edges (src, dst, kind, weight) VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT(src, dst, kind) DO UPDATE SET weight = excluded.weight",
                )
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;

            // Clear old derived rows for files in this package
            for &(file_id, _) in &pkg_exported.files {
                txn.execute(
                    "DELETE FROM bindings WHERE ref_id IN (SELECT id FROM refs WHERE file_id = ?1)",
                    params![file_id],
                )
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                txn.execute(
                    "DELETE FROM binding_targets WHERE ref_id IN (SELECT id FROM refs WHERE file_id = ?1)",
                    params![file_id],
                )
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                txn.execute("DELETE FROM edges WHERE src = ?1", params![file_id])
                    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
            }

            // Populate defs and methods
            let mut defs: BTreeMap<String, Vec<DefLoc>> = BTreeMap::new();
            let mut methods: BTreeMap<String, Vec<DefLoc>> = BTreeMap::new();
            for &(file_id, ref file_path) in &pkg_exported.files {
                let mut rows = stmt_load_symbols
                    .query(params![file_id])
                    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                while let Some(row) = rows
                    .next()
                    .map_err(|e| IndexDatabase::classify(&db_path, e))?
                {
                    let sym_name: String = row
                        .get(1)
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    let qual_name: String = row
                        .get(2)
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    let kind_str: String = row
                        .get(3)
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    let kind = parse_def_kind(&kind_str);
                    let loc = DefLoc {
                        file: file_path.clone(),
                        qual_name,
                        kind,
                    };
                    defs.entry(sym_name.clone()).or_default().push(loc.clone());
                    if kind == DefKind::Method {
                        methods.entry(sym_name).or_default().push(loc);
                    }
                }
            }

            resolver.set_package_defs(key, defs, methods);

            // Resolve each file in package
            for &(file_id, ref file_path) in &pkg_exported.files {
                if !file_path.ends_with(".go") {
                    continue;
                }

                let mut imports = Vec::new();
                let mut import_ids = Vec::new();
                {
                    let mut imp_rows = stmt_load_imports
                        .query(params![file_id])
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    while let Some(row) = imp_rows
                        .next()
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?
                    {
                        let imp_id: i64 = row
                            .get(0)
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        let raw: String = row
                            .get(1)
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        let alias: Option<String> = row
                            .get(2)
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        let kind_str: String = row
                            .get(3)
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;

                        import_ids.push(imp_id);
                        imports.push(cs_extract::Import {
                            raw,
                            alias,
                            kind: parse_import_kind(&kind_str),
                            span: Span {
                                start_line: 0,
                                end_line: 0,
                                start_byte: 0,
                                end_byte: 0,
                            },
                        });
                    }
                }

                let mut refs = Vec::new();
                let mut ref_ids = Vec::new();
                {
                    let mut ref_rows = stmt_load_refs
                        .query(params![file_id])
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    while let Some(row) = ref_rows
                        .next()
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?
                    {
                        let r_id: i64 = row
                            .get(0)
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        let r_name: String = row
                            .get(1)
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        let kind_str: String = row
                            .get(2)
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        let qualifier: Option<String> = row
                            .get(3)
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        let line: i64 = row
                            .get(4)
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        let start_byte: i64 = row
                            .get(5)
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        let end_byte: i64 = row
                            .get(6)
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        let container: Option<String> = row
                            .get(7)
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;

                        ref_ids.push(r_id);
                        refs.push(cs_extract::Ref {
                            name: r_name,
                            kind: parse_ref_kind(&kind_str),
                            qualifier,
                            span: Span {
                                start_line: to_u32(line),
                                end_line: to_u32(line),
                                start_byte: to_u32(start_byte),
                                end_byte: to_u32(end_byte),
                            },
                            container,
                        });
                    }
                }

                let extracted = cs_extract::ExtractedFile {
                    package_name: Some(key.1.clone()),
                    status: ParseStatus::Ok,
                    defs: Vec::new(),
                    refs,
                    imports,
                };

                let outcome =
                    resolver.resolve_file_outcome(file_path, &extracted, &mut dummy_stats);

                // Write derived imports
                for (i, (_imp, resolution)) in outcome.resolution.imports.iter().enumerate() {
                    let imp_id = import_ids[i];
                    match resolution {
                        Resolution::Resolved(resolved_dir) => {
                            stmt_update_import
                                .execute(params![resolved_dir, None::<&str>, None::<&str>, imp_id])
                                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        }
                        Resolution::External { .. } => {
                            stmt_update_import
                                .execute(params![None::<&str>, None::<&str>, "external", imp_id])
                                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        }
                        Resolution::Unresolved { reason, .. } => {
                            stmt_update_import
                                .execute(params![
                                    None::<&str>,
                                    None::<&str>,
                                    reason.as_str(),
                                    imp_id
                                ])
                                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        }
                    }
                }

                // Write bindings & binding_targets
                for (j, (_r, binding)) in outcome.resolution.refs.iter().enumerate() {
                    let ref_id = ref_ids[j];
                    let reason_str = binding.unbound_reason.map(UnboundReason::as_str);
                    stmt_insert_binding
                        .execute(params![ref_id, reason_str])
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;

                    for target in &binding.targets {
                        if let Some(&tgt_file_id) = exported_index.file_by_path.get(&target.file) {
                            stmt_insert_binding_target
                                .execute(params![
                                    ref_id,
                                    tgt_file_id,
                                    target.qual_name,
                                    target.kind.as_str(),
                                ])
                                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        }
                    }
                }

                // Write edges: import_out
                for dst_path in outcome.import_targets {
                    if let Some(&dst_id) = exported_index.file_by_path.get(&dst_path) {
                        stmt_insert_edge
                            .execute(params![file_id, dst_id, "import_out", W_IMPORT_OUT])
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    }
                }

                // Write edges: test_affinity
                for dst_path in outcome.affinity_targets {
                    if let Some(&dst_id) = exported_index.file_by_path.get(&dst_path) {
                        stmt_insert_edge
                            .execute(params![file_id, dst_id, "test_affinity", W_TEST_AFFINITY])
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                        stmt_insert_edge
                            .execute(params![dst_id, file_id, "test_affinity", W_TEST_AFFINITY])
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    }
                }

                // Write edges: ref_def
                let mut ref_counts: BTreeMap<&str, u64> = BTreeMap::new();
                for dst_path in &outcome.ref_targets {
                    *ref_counts.entry(dst_path.as_str()).or_default() += 1;
                }
                for (dst_path, count) in ref_counts {
                    if let Some(&dst_id) = exported_index.file_by_path.get(dst_path) {
                        let weight = ref_def_weight(count);
                        stmt_insert_edge
                            .execute(params![file_id, dst_id, "ref_def", weight])
                            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    }
                }
            }
        }

        txn.commit()
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;

        resolver.clear_package_defs(key);
    }

    // 8. Update snapshot_id
    let new_snapshot_id = compute_snapshot_id(&db.conn, &db_path)?;
    db.conn
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = 'snapshot_id'",
            params![new_snapshot_id],
        )
        .map_err(|e| IndexDatabase::classify(&db_path, e))?;

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::ingest_facts;
    use crate::resolve_pass::resolve_facts;
    use crate::IndexDatabase;
    use cs_scanner::{scan, ScanConfig};
    use tempfile::tempdir;

    fn write_file(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
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

        // Check that binding in web is now unbound
        let reason: String = db
            .connection()
            .query_row(
                "SELECT b.unbound_reason FROM bindings b JOIN refs r ON r.id = b.ref_id WHERE r.name = 'Login'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reason, "no_candidate");
    }
}
