//! Pass 3: Package-by-package resolution and derived row write-through.
//!
//! Implements Milestone M4 of the streaming-first architecture (ADR-021, ARCHITECTURE §4.4).
//!
//! Pass 3 iterates through packages in sorted `(dir, name)` order:
//! 1. Loads package facts from SQLite (`symbols`, `refs`, `imports`).
//! 2. Resolves imports and binds references via `cs-resolve`.
//! 3. Writes derived rows (`imports.resolved_*`, `bindings`, `binding_targets`, `edges`)
//!    directly to SQLite within a single package transaction.
//! 4. Clears package-local definitions from memory, keeping peak RSS bounded.
//! 5. Commits `snapshot_id` and flips `index_state` to `'ready'` atomically.

use std::collections::BTreeMap;

use cs_extract::{DefKind, ImportKind, ParseStatus, RefKind, Span};
use cs_resolve::{
    go::{GoResolver, ModuleInfo, Package as GoPackage},
    ref_def_weight, DefLoc, FilePath, Resolution, ResolutionStats, W_IMPORT_OUT, W_TEST_AFFINITY,
};
use rusqlite::params;

use crate::index_pass::{build_exported_index, parse_def_kind};
use crate::{IndexDatabase, IndexError, IndexState};

/// Helper to convert `i64` to `u32` safely.
fn to_u32(val: i64) -> u32 {
    u32::try_from(val).unwrap_or(0)
}

/// Parse import kind string from SQLite into [`ImportKind`].
fn parse_import_kind(kind: &str) -> ImportKind {
    match kind {
        "export-from" => ImportKind::ExportFrom,
        "require" => ImportKind::Require,
        "dynamic" => ImportKind::Dynamic,
        _ => ImportKind::Import,
    }
}

/// Parse reference kind string from SQLite into [`RefKind`].
fn parse_ref_kind(kind: &str) -> RefKind {
    match kind {
        "field_ref" => RefKind::FieldRef,
        "call_ref" => RefKind::CallRef,
        "type_ref" => RefKind::TypeRef,
        _ => RefKind::NameRef,
    }
}

/// Run Pass 3: Resolve all packages in `(dir, name)` order and write derived rows.
///
/// # Errors
///
/// Returns [`IndexError::StateConflict`] unless the index is in [`IndexState::Building`].
/// Propagates SQLite errors.
#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::case_sensitive_file_extension_comparisons
)]
pub fn resolve_facts(db: &mut IndexDatabase) -> Result<ResolutionStats, IndexError> {
    let state = db.state()?;
    if state != IndexState::Building {
        return Err(IndexError::StateConflict {
            found: state,
            expected: "building",
        });
    }

    let db_path = db.path.clone();
    let exported_index = build_exported_index(&db.conn, &db_path)?;

    let mut resolver = GoResolver::new_empty(
        exported_index
            .modules
            .iter()
            .map(|m| ModuleInfo::new(m.dir.clone(), m.path.clone()))
            .collect(),
        exported_index.package_dirs.clone(),
        exported_index.has_vendor,
    );

    for ((dir, name), pkg) in &exported_index.packages {
        let files: Vec<FilePath> = pkg.files.iter().map(|(_, path)| path.clone()).collect();
        let mut exported: BTreeMap<String, Vec<DefLoc>> = BTreeMap::new();
        for (sym_name, syms) in &pkg.exported {
            let locs = syms
                .iter()
                .map(|s| DefLoc {
                    file: s.file_path.clone(),
                    qual_name: s.qual_name.clone(),
                    kind: s.kind,
                })
                .collect();
            exported.insert(sym_name.clone(), locs);
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

    let mut stats = ResolutionStats::default();

    // Resolve package by package in sorted order
    for ((dir, name), pkg_exported) in &exported_index.packages {
        if pkg_exported.files.is_empty() {
            continue;
        }

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

            // 1. Load package-local definitions and methods
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

            resolver.set_package_defs(&(dir.clone(), name.clone()), defs, methods);

            // 2. Resolve each file in the package
            for &(file_id, ref file_path) in &pkg_exported.files {
                if !file_path.ends_with(".go") {
                    continue;
                }

                // Load imports
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

                // Load refs
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
                    package_name: Some(name.clone()),
                    status: ParseStatus::Ok,
                    defs: Vec::new(),
                    refs,
                    imports,
                };

                let outcome = resolver.resolve_file_outcome(file_path, &extracted, &mut stats);

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

                // Write derived bindings & binding_targets
                for (j, (_r, binding)) in outcome.resolution.refs.iter().enumerate() {
                    let ref_id = ref_ids[j];
                    let reason_str = binding
                        .unbound_reason
                        .map(cs_resolve::UnboundReason::as_str);
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

                // Write edges: test_affinity (bidirectional)
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

                // Write edges: ref_def (with sqrt dampener weight)
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

        // 3. Clear package definitions from resolver to keep memory O(1 package)
        resolver.clear_package_defs(&(dir.clone(), name.clone()));
    }

    // Now finalize: mark index ready and write snapshot_id
    db.mark_ready(&exported_index.snapshot_id)?;

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::ingest_facts;
    use crate::IndexDatabase;
    use cs_resolve::{LanguageResolver, ResolveSnapshot};
    use cs_scanner::{scan, ScanConfig};
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    fn resolve_facts_on_fixtures_go_resolve_matches_in_memory_resolver() {
        let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("fixtures/go-resolve");

        let files = scan(&fixture_path, &ScanConfig::default()).expect("scan fixtures");
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).expect("open index");
        ingest_facts(&mut db, &fixture_path, &files, 100).expect("ingest");

        assert_eq!(db.state().unwrap(), IndexState::Building);

        // Run Pass 3: package-by-package streaming resolve
        let streaming_stats = resolve_facts(&mut db).expect("resolve facts");
        assert_eq!(db.state().unwrap(), IndexState::Ready);

        // Run in-memory resolver on the exact same fixtures to compare
        let mut extracted_files = Vec::new();
        let mut manifests = Vec::new();
        for file in &files {
            if file.path.ends_with(".go") {
                let content = fs::read_to_string(fixture_path.join(&file.path)).unwrap();
                let extracted = cs_extract::extract(&content, cs_scanner::Language::Go).unwrap();
                extracted_files.push((file.path.clone(), extracted));
            } else if file.path == "go.mod" || file.path.ends_with("/go.mod") {
                let content = fs::read_to_string(fixture_path.join(&file.path)).unwrap();
                manifests.push((file.path.clone(), content));
            }
        }
        let snapshot = ResolveSnapshot::new(extracted_files, manifests);
        let in_memory_resolver = GoResolver::prepare(&snapshot);
        let in_memory_repo = in_memory_resolver.resolve(&snapshot);

        // 1. Check stats equality
        assert_eq!(streaming_stats.resolved, in_memory_repo.stats.resolved);
        assert_eq!(streaming_stats.external, in_memory_repo.stats.external);
        assert_eq!(streaming_stats.unresolved, in_memory_repo.stats.unresolved);
        assert_eq!(streaming_stats.refs_bound, in_memory_repo.stats.refs_bound);
        assert_eq!(
            streaming_stats.refs_unbound,
            in_memory_repo.stats.refs_unbound
        );
        assert_eq!(
            streaming_stats.names_skipped,
            in_memory_repo.stats.names_skipped
        );
        assert_eq!(
            streaming_stats.package_qualifier_refs,
            in_memory_repo.stats.package_qualifier_refs
        );
        assert_eq!(
            streaming_stats.unbound_reasons,
            in_memory_repo.stats.unbound_reasons
        );

        // 2. Check derived SQLite rows
        let conn = db.connection();

        let edge_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            usize::try_from(edge_count).unwrap_or(0),
            in_memory_repo.edges.len(),
            "edge count in SQLite must match in-memory resolver"
        );

        let binding_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM bindings", [], |r| r.get(0))
            .unwrap();
        let ref_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM refs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            binding_count, ref_count,
            "every reference must have a binding row"
        );

        // Check that no unresolved imports are left unclassified
        let unclassified_imports: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM imports WHERE resolved_dir IS NULL \
                 AND resolved_file IS NULL AND unresolved_reason IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unclassified_imports, 0, "all imports must be classified");
    }
}
