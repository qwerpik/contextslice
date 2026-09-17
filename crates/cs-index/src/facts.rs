//! Fact Ingestion Pass (ARCHITECTURE §4.4, ADR-021 D1/D2).
//!
//! Streams scanner output into SQLite in batched transactions of ≤1,000 files.
//! SQLite becomes the source of truth for raw extracted facts (`files`,
//! `symbols`, `refs`, `imports`, `manifests`). At most one file's AST exists
//! in memory at a time.
//!
//! # Lifecycle (ADR-021 D3)
//!
//! If the index is in [`IndexState::Empty`], `ingest_facts` transitions it to
//! [`IndexState::Building`] before the first batch. If already [`IndexState::Building`],
//! it resumes: files already present in the database are skipped without re-parsing.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use cs_extract::{extract_with_timeout, ExtractError, ImportKind, ParseStatus, PARSE_TIMEOUT_MS};
use cs_scanner::{Language, ScannedFile, SkipReason};
use rusqlite::params;

use crate::{IndexDatabase, IndexError, IndexState};

/// Default batch size for fact ingestion transactions (ADR-021 D1).
pub const DEFAULT_FACT_BATCH_SIZE: usize = 1000;

/// Default config fingerprint written into `meta` at the start of a build.
pub const DEFAULT_CONFIG_FINGERPRINT: &str = "contextslice:v1:defaults";

/// Aggregate statistics from a fact ingestion pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestStats {
    /// Total number of scanned files passed into ingestion.
    pub files_scanned: usize,
    /// Number of files newly inserted into the database.
    pub files_inserted: usize,
    /// Number of files skipped because they were already present (resume).
    pub files_skipped_already_present: usize,
    /// Total definitions inserted into the `symbols` table.
    pub symbols_inserted: usize,
    /// Total references inserted into the `refs` table.
    pub refs_inserted: usize,
    /// Total imports inserted into the `imports` table.
    pub imports_inserted: usize,
    /// Total manifests (`go.mod`) inserted into the `manifests` table.
    pub manifests_inserted: usize,
    /// Total distinct packages inserted into the `packages` table.
    pub packages_inserted: usize,
}

/// Directory part of a `/`-separated repo-relative path (`""` for root).
fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

fn import_kind_str(kind: ImportKind) -> &'static str {
    match kind {
        ImportKind::Import => "import",
        ImportKind::ExportFrom => "export-from",
        ImportKind::Require => "require",
        ImportKind::Dynamic => "dynamic",
    }
}

fn parse_status_str(status: ParseStatus) -> &'static str {
    match status {
        ParseStatus::Ok => "ok",
        ParseStatus::Partial => "partial",
        ParseStatus::Timeout => "timeout",
        ParseStatus::Skipped => "skipped",
        ParseStatus::Unsupported => "unsupported",
    }
}

fn to_i64(val: u64) -> i64 {
    i64::try_from(val).unwrap_or(i64::MAX)
}

/// Ingest raw facts from `files` into `db` in transactions of `batch_size`.
///
/// If `db` is [`IndexState::Empty`], begins build with [`DEFAULT_CONFIG_FINGERPRINT`].
/// If `db` is [`IndexState::Building`], resumes build by skipping already recorded files.
///
/// # Errors
///
/// Returns [`IndexError::StateConflict`] if the index is in [`IndexState::Ready`].
/// Propagates I/O and SQLite storage failures.
#[allow(clippy::too_many_lines, clippy::similar_names)]
pub fn ingest_facts(
    db: &mut IndexDatabase,
    root: &Path,
    files: &[ScannedFile],
    batch_size: usize,
) -> Result<IngestStats, IndexError> {
    let state = db.state()?;
    match state {
        IndexState::Empty => {
            db.begin_build(DEFAULT_CONFIG_FINGERPRINT)?;
        }
        IndexState::Building => {
            // Resume mode: continue populating facts without modifying index_state.
        }
        IndexState::Ready => {
            return Err(IndexError::StateConflict {
                found: IndexState::Ready,
                expected: "empty or building",
            });
        }
    }

    let mut stats = IngestStats {
        files_scanned: files.len(),
        ..Default::default()
    };

    if files.is_empty() {
        return Ok(stats);
    }

    let batch_size = batch_size.max(1);

    // If resuming, query files already persisted so we skip them.
    let mut existing_paths: HashSet<String> = HashSet::new();
    {
        let conn = db.connection();
        let mut stmt = conn
            .prepare("SELECT path FROM files")
            .map_err(|e| IndexDatabase::classify(db.path(), e))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| IndexDatabase::classify(db.path(), e))?;
        for r in rows {
            let p = r.map_err(|e| IndexDatabase::classify(db.path(), e))?;
            existing_paths.insert(p);
        }
    }

    // In-memory package cache: (dir, name) -> package_id
    let mut packages_cache: HashMap<(String, String), i64> = HashMap::new();
    {
        let conn = db.connection();
        let mut stmt = conn
            .prepare("SELECT id, dir, name FROM packages")
            .map_err(|e| IndexDatabase::classify(db.path(), e))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| IndexDatabase::classify(db.path(), e))?;
        for r in rows {
            let (id, dir, name) = r.map_err(|e| IndexDatabase::classify(db.path(), e))?;
            packages_cache.insert((dir, name), id);
        }
    }

    let mut pending_files = Vec::new();
    for file in files {
        if existing_paths.contains(&file.path) {
            stats.files_skipped_already_present += 1;
        } else {
            pending_files.push(file);
        }
    }

    let db_path = db.path().to_path_buf();

    for chunk in pending_files.chunks(batch_size) {
        let txn = db.transaction()?;
        {
            let mut stmt_insert_package = txn
                .prepare("INSERT OR IGNORE INTO packages (dir, name, lang) VALUES (?1, ?2, ?3)")
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;

            let mut stmt_select_package = txn
                .prepare("SELECT id FROM packages WHERE dir = ?1 AND name = ?2 AND lang = ?3")
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;

            let mut stmt_insert_file = txn
                .prepare(
                    "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status, tokens_est) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id",
                )
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;

            let mut stmt_insert_manifest = txn
                .prepare("INSERT OR REPLACE INTO manifests (path, content) VALUES (?1, ?2)")
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;

            let mut stmt_insert_symbol = txn
                .prepare(
                    "INSERT INTO symbols (file_id, name, qual_name, kind, exported, line, end_line, start_byte, end_byte, signature, container, doc) \
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
                    "INSERT INTO imports (file_id, raw, alias, kind, ordinal, resolved_dir, resolved_file, unresolved_reason) \
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL)",
                )
                .map_err(|e| IndexDatabase::classify(&db_path, e))?;

            for file in chunk {
                let is_manifest = file.path == "go.mod" || file.path.ends_with("/go.mod");

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
                                None::<i64>,
                            ],
                            |r| r.get::<_, i64>(0),
                        )
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    stats.files_inserted += 1;
                    continue;
                }

                if file.skip == Some(SkipReason::ParseSkipped) {
                    let hash_bytes = file.hash.as_ref().map(<[u8; 32]>::as_slice);
                    stmt_insert_file
                        .query_row(
                            params![
                                file.path,
                                file.lang.as_str(),
                                None::<i64>,
                                hash_bytes,
                                "parse_skipped",
                                to_i64(file.size),
                                file.mtime,
                                "skipped",
                                None::<i64>,
                            ],
                            |r| r.get::<_, i64>(0),
                        )
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    stats.files_inserted += 1;
                    continue;
                }

                let abs_path = root.join(&file.path);
                let Ok(content_bytes) = std::fs::read(&abs_path) else {
                    let (hash_bytes, skip_str) = if let Some(ref h) = file.hash {
                        (Some(h.as_slice()), "unreadable")
                    } else {
                        (None, "too_large")
                    };
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
                                "skipped",
                                None::<i64>,
                            ],
                            |r| r.get::<_, i64>(0),
                        )
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    stats.files_inserted += 1;
                    continue;
                };

                let hash_bytes = file.hash.as_ref().map(<[u8; 32]>::as_slice);

                if is_manifest {
                    let content_str = String::from_utf8_lossy(&content_bytes);
                    stmt_insert_manifest
                        .execute(params![file.path, content_str])
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    stats.manifests_inserted += 1;

                    stmt_insert_file
                        .query_row(
                            params![
                                file.path,
                                file.lang.as_str(),
                                None::<i64>,
                                hash_bytes,
                                None::<&str>,
                                to_i64(file.size),
                                file.mtime,
                                "skipped",
                                None::<i64>,
                            ],
                            |r| r.get::<_, i64>(0),
                        )
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    stats.files_inserted += 1;
                    continue;
                }

                if file.lang == Language::Unsupported || file.lang == Language::Unknown {
                    stmt_insert_file
                        .query_row(
                            params![
                                file.path,
                                file.lang.as_str(),
                                None::<i64>,
                                hash_bytes,
                                None::<&str>,
                                to_i64(file.size),
                                file.mtime,
                                "unsupported",
                                None::<i64>,
                            ],
                            |r| r.get::<_, i64>(0),
                        )
                        .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                    stats.files_inserted += 1;
                    continue;
                }

                let source = String::from_utf8_lossy(&content_bytes);
                let extracted = extract_with_timeout(
                    &source,
                    file.lang,
                    Duration::from_millis(PARSE_TIMEOUT_MS),
                );

                let (package_id, status_str, extracted_file) = match extracted {
                    Ok(ext) => {
                        let pkg_id = if let Some(ref pkg_name) = ext.package_name {
                            let dir = dir_of(&file.path);
                            let key = (dir.to_owned(), pkg_name.clone());
                            if let Some(&id) = packages_cache.get(&key) {
                                Some(id)
                            } else {
                                stmt_insert_package
                                    .execute(params![dir, pkg_name, "go"])
                                    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                                let id: i64 = stmt_select_package
                                    .query_row(params![dir, pkg_name, "go"], |r| r.get(0))
                                    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                                packages_cache.insert(key, id);
                                stats.packages_inserted += 1;
                                Some(id)
                            }
                        } else {
                            None
                        };
                        (pkg_id, parse_status_str(ext.status), Some(ext))
                    }
                    Err(
                        ExtractError::IncompatibleGrammar { .. }
                        | ExtractError::NotImplemented { .. },
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
                            None::<i64>,
                        ],
                        |r| r.get(0),
                    )
                    .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                stats.files_inserted += 1;

                if let Some(ext) = extracted_file {
                    if ext.status != ParseStatus::Timeout
                        && ext.status != ParseStatus::Skipped
                        && ext.status != ParseStatus::Unsupported
                    {
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
                            stats.symbols_inserted += 1;
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
                            stats.refs_inserted += 1;
                        }

                        for (ord, imp) in ext.imports.into_iter().enumerate() {
                            stmt_insert_import
                                .execute(params![
                                    file_id,
                                    imp.raw,
                                    imp.alias,
                                    import_kind_str(imp.kind),
                                    i64::try_from(ord).unwrap_or(i64::MAX),
                                ])
                                .map_err(|e| IndexDatabase::classify(&db_path, e))?;
                            stats.imports_inserted += 1;
                        }
                    }
                }
            }
        }
        txn.commit()
            .map_err(|e| IndexDatabase::classify(&db_path, e))?;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cs_scanner::{scan, ScanConfig};
    use tempfile::tempdir;

    fn write_file(dir: &Path, rel: &str, content: &[u8]) {
        let abs = dir.join(rel);
        if let Some(p) = abs.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(abs, content).unwrap();
    }

    #[test]
    fn ingest_fixtures_go_resolve_matches_schema_and_facts() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("fixtures/go-resolve");

        let files = scan(&repo_root, &ScanConfig::default()).expect("scan fixtures");
        assert!(!files.is_empty(), "fixtures must contain files");

        let db_dir = tempdir().unwrap();
        let db_path = db_dir.path().join("index.db");

        let mut db = IndexDatabase::open_or_create(&db_path).expect("open_or_create");
        assert_eq!(db.state().unwrap(), IndexState::Empty);

        let stats = ingest_facts(&mut db, &repo_root, &files, 1000).expect("ingest_facts");
        assert_eq!(db.state().unwrap(), IndexState::Building);
        assert_eq!(stats.files_inserted, files.len());
        assert_eq!(stats.files_skipped_already_present, 0);
        assert!(stats.symbols_inserted > 0);
        assert!(stats.refs_inserted > 0);
        assert!(stats.imports_inserted > 0);
        assert_eq!(
            stats.manifests_inserted, 2,
            "both go.mod and nested/go.mod should be inserted"
        );
        assert!(stats.packages_inserted > 0);

        // Verify count matches in DB
        let conn = db.connection();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(usize::try_from(count).unwrap(), files.len());

        let manifest_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM manifests", [], |r| r.get(0))
            .unwrap();
        assert_eq!(manifest_count, 2);

        let sym_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
            .unwrap();
        assert_eq!(usize::try_from(sym_count).unwrap(), stats.symbols_inserted);
    }

    #[test]
    fn ingest_resumes_cleanly_when_building() {
        let tmp = tempdir().unwrap();
        write_file(
            tmp.path(),
            "go.mod",
            b"module example.com/test\n\ngo 1.22\n",
        );
        write_file(tmp.path(), "a.go", b"package test\n\nfunc Alpha() {}\n");
        write_file(
            tmp.path(),
            "b.go",
            b"package test\n\nfunc Beta() { Alpha() }\n",
        );

        let files = scan(tmp.path(), &ScanConfig::default()).expect("scan");
        assert_eq!(files.len(), 3);

        let db_dir = tempdir().unwrap();
        let db_path = db_dir.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).expect("open");

        // Ingest first 2 files with batch_size 1
        let stats1 = ingest_facts(&mut db, tmp.path(), &files[..2], 1).expect("ingest partial");
        assert_eq!(stats1.files_inserted, 2);
        assert_eq!(stats1.files_skipped_already_present, 0);
        assert_eq!(db.state().unwrap(), IndexState::Building);

        // Now resume with all 3 files
        let stats2 = ingest_facts(&mut db, tmp.path(), &files, 1).expect("ingest resume");
        assert_eq!(
            stats2.files_inserted, 1,
            "only 1 new file should be inserted"
        );
        assert_eq!(stats2.files_skipped_already_present, 2);
        assert_eq!(db.state().unwrap(), IndexState::Building);

        let conn = db.connection();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn ingest_handles_large_and_timeout_files() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "normal.go", b"package p\n\nfunc Normal() {}\n");

        let files = vec![
            ScannedFile {
                path: "normal.go".to_string(),
                lang: Language::Go,
                size: 25,
                mtime: 100,
                hash: Some([1; 32]),
                skip: None,
            },
            ScannedFile {
                path: "too_large.bin".to_string(),
                lang: Language::Unknown,
                size: 60 * 1024 * 1024,
                mtime: 200,
                hash: None,
                skip: Some(SkipReason::TooLarge),
            },
            ScannedFile {
                path: "parse_skipped.go".to_string(),
                lang: Language::Go,
                size: 2 * 1024 * 1024,
                mtime: 300,
                hash: Some([2; 32]),
                skip: Some(SkipReason::ParseSkipped),
            },
        ];

        let db_dir = tempdir().unwrap();
        let db_path = db_dir.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).expect("open");

        let stats = ingest_facts(&mut db, tmp.path(), &files, 10).expect("ingest");
        assert_eq!(stats.files_inserted, 3);

        let conn = db.connection();
        let too_large_status: (Option<Vec<u8>>, Option<String>, String) = conn
            .query_row(
                "SELECT hash, skip, parse_status FROM files WHERE path = 'too_large.bin'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(too_large_status.0, None);
        assert_eq!(too_large_status.1.as_deref(), Some("too_large"));
        assert_eq!(too_large_status.2, "skipped");

        let parse_skipped_status: (Option<Vec<u8>>, Option<String>, String) = conn
            .query_row(
                "SELECT hash, skip, parse_status FROM files WHERE path = 'parse_skipped.go'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(parse_skipped_status.0, Some(vec![2; 32]));
        assert_eq!(parse_skipped_status.1.as_deref(), Some("parse_skipped"));
        assert_eq!(parse_skipped_status.2, "skipped");
    }

    #[test]
    fn small_batch_size_chunks_correctly() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "1.go", b"package p\n\nfunc F1() {}\n");
        write_file(tmp.path(), "2.go", b"package p\n\nfunc F2() {}\n");
        write_file(tmp.path(), "3.go", b"package p\n\nfunc F3() {}\n");
        write_file(tmp.path(), "4.go", b"package p\n\nfunc F4() {}\n");
        write_file(tmp.path(), "5.go", b"package p\n\nfunc F5() {}\n");

        let files = scan(tmp.path(), &ScanConfig::default()).expect("scan");
        assert_eq!(files.len(), 5);

        let db_dir = tempdir().unwrap();
        let db_path = db_dir.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).expect("open");

        // Batch size 2 across 5 files -> 3 batches (2 + 2 + 1)
        let stats = ingest_facts(&mut db, tmp.path(), &files, 2).expect("ingest batch 2");
        assert_eq!(stats.files_inserted, 5);
        assert_eq!(stats.symbols_inserted, 5);

        let conn = db.connection();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 5);
    }
}
