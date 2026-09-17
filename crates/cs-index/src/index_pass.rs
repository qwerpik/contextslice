//! Pass 2: In-memory exported definition index and `snapshot_id` computation.
//!
//! Implements Milestone M3 of the streaming-first architecture (ADR-021, ARCHITECTURE §4.4).
//!
//! Pass 2 streams lightweight queries over SQLite to build the only repo-proportional
//! in-memory structure:
//! - Module manifests (`go.mod` files parsed into module roots)
//! - Directory-to-package mappings and non-test package identities
//! - Exported definitions per package from non-test source files
//! - Deterministic `snapshot_id` (blake3 over sorted `(path, hash)` pairs)

use std::collections::BTreeMap;
use std::path::Path;

use cs_extract::DefKind;
use cs_resolve::parse_module_path;
use rusqlite::Connection;

use crate::{IndexDatabase, IndexError};

/// One module manifest: the directory it governs (`""` at the repo root) and
/// the declared module path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleInfo {
    /// Directory containing the manifest, repo-relative (`""` for root).
    pub dir: String,
    /// Declared module path (`module example.com/pkg`).
    pub path: String,
}

/// An exported definition indexed for cross-package reference binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedSymbol {
    /// Primary key in `symbols` table.
    pub symbol_id: i64,
    /// Primary key of containing file in `files` table.
    pub file_id: i64,
    /// Repo-relative normalized file path.
    pub file_path: String,
    /// File-local qualified name (`Session.Validate`).
    pub qual_name: String,
    /// Canonical symbol kind.
    pub kind: DefKind,
}

/// Exported definitions and file membership for a single package.
#[derive(Debug, Clone)]
pub struct PackageExported {
    /// Primary key in `packages` table.
    pub id: i64,
    /// Repo-relative directory (`""` for root).
    pub dir: String,
    /// Package clause name.
    pub name: String,
    /// Language string (`"go"`).
    pub lang: String,
    /// All files in this package as `(file_id, path)` pairs.
    pub files: Vec<(i64, String)>,
    /// Exported definitions by bare name, excluding test files (`_test.go`).
    pub exported: BTreeMap<String, Vec<ExportedSymbol>>,
}

/// The complete Pass 2 in-memory index.
#[derive(Debug, Clone)]
pub struct ExportedIndex {
    /// Blake3 content hash across all indexed files in sorted path order.
    pub snapshot_id: String,
    /// Sorted list of modules from longest dir to shortest (root last).
    pub modules: Vec<ModuleInfo>,
    /// Directory → sorted package names present in that directory.
    pub package_dirs: BTreeMap<String, Vec<String>>,
    /// Packages keyed by `(dir, name)` identity.
    pub packages: BTreeMap<(String, String), PackageExported>,
    /// Mapping from `packages.id` to `(dir, name)` identity.
    pub package_by_id: BTreeMap<i64, (String, String)>,
    /// Mapping from `files.id` to repo-relative path.
    pub file_by_id: BTreeMap<i64, String>,
    /// Mapping from repo-relative path to `files.id`.
    pub file_by_path: BTreeMap<String, i64>,
    /// Whether any indexed file lives under `vendor/`.
    pub has_vendor: bool,
}

impl ExportedIndex {
    /// Find the nearest enclosing module for `dir`.
    #[must_use]
    pub fn nearest_module(&self, dir: &str) -> &ModuleInfo {
        for m in &self.modules {
            if m.dir.is_empty() || dir == m.dir || dir.starts_with(&format!("{}/", m.dir)) {
                return m;
            }
        }
        // Fallback to the root module (always present in modules).
        self.modules
            .last()
            .expect("modules must contain at least one module")
    }

    /// The non-test package of a directory (Go's rule: an import binds the
    /// package, and `foo_test` is a different, test-only package).
    #[must_use]
    pub fn non_test_package(&self, dir: &str) -> Option<(String, String)> {
        let names = self.package_dirs.get(dir)?;
        let eligible: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|name| !name.ends_with("_test") && !name.contains("<<no-package>>"))
            .collect();
        let best = eligible
            .iter()
            .filter(|name| **name != "main")
            .min()
            .or_else(|| eligible.iter().min())?;
        Some((dir.to_owned(), (*best).to_owned()))
    }
}

/// Compute the deterministic `snapshot_id` across all files in sorted path order.
///
/// Format: `blake3` over `path\0<hash-bytes or '<too_large>'>\0`.
///
/// # Errors
///
/// Propagates SQLite query failures.
pub fn compute_snapshot_id(conn: &Connection, db_path: &Path) -> Result<String, IndexError> {
    let mut stmt = conn
        .prepare("SELECT path, hash FROM files ORDER BY path ASC")
        .map_err(|e| IndexDatabase::classify(db_path, e))?;

    let mut rows = stmt
        .query([])
        .map_err(|e| IndexDatabase::classify(db_path, e))?;

    let mut hasher = blake3::Hasher::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| IndexDatabase::classify(db_path, e))?
    {
        let path: String = row
            .get(0)
            .map_err(|e| IndexDatabase::classify(db_path, e))?;
        let hash: Option<Vec<u8>> = row
            .get(1)
            .map_err(|e| IndexDatabase::classify(db_path, e))?;

        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        if let Some(ref h) = hash {
            hasher.update(h);
        } else {
            hasher.update(b"<too_large>");
        }
        hasher.update(b"\0");
    }

    Ok(hasher.finalize().to_hex().to_string())
}

/// Parse symbol kind string from SQLite into [`DefKind`].
pub(crate) fn parse_def_kind(kind: &str) -> DefKind {
    match kind {
        "method" => DefKind::Method,
        "class" => DefKind::Class,
        "struct" => DefKind::Struct,
        "interface" => DefKind::Interface,
        "type" => DefKind::Type,
        "const" => DefKind::Const,
        "var" => DefKind::Var,
        "enum" => DefKind::Enum,
        "module" => DefKind::Module,
        _ => DefKind::Function,
    }
}

/// Directory part of a `/`-separated repo-relative path (`""` for root).
fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Run Pass 2: Stream lightweight queries to construct the [`ExportedIndex`].
///
/// # Errors
///
/// Propagates SQLite query failures.
#[allow(clippy::too_many_lines)]
pub fn build_exported_index(
    conn: &Connection,
    db_path: &Path,
) -> Result<ExportedIndex, IndexError> {
    let snapshot_id = compute_snapshot_id(conn, db_path)?;

    // 1. Files
    let mut file_by_id = BTreeMap::new();
    let mut file_by_path = BTreeMap::new();
    let mut file_pkg_ids = Vec::new();
    let mut has_vendor = false;

    {
        let mut stmt_files = conn
            .prepare("SELECT id, path, package_id FROM files ORDER BY id ASC")
            .map_err(|e| IndexDatabase::classify(db_path, e))?;

        let mut rows = stmt_files
            .query([])
            .map_err(|e| IndexDatabase::classify(db_path, e))?;

        while let Some(row) = rows
            .next()
            .map_err(|e| IndexDatabase::classify(db_path, e))?
        {
            let id: i64 = row
                .get(0)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;
            let path: String = row
                .get(1)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;
            let pkg_id: Option<i64> = row
                .get(2)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;

            if path.starts_with("vendor/") {
                has_vendor = true;
            }

            file_by_id.insert(id, path.clone());
            file_by_path.insert(path.clone(), id);
            file_pkg_ids.push((id, path, pkg_id));
        }
    }

    // 2. Packages
    let mut packages = BTreeMap::new();
    let mut package_by_id = BTreeMap::new();
    let mut package_dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();

    {
        let mut stmt_pkgs = conn
            .prepare("SELECT id, dir, name, lang FROM packages ORDER BY dir ASC, name ASC")
            .map_err(|e| IndexDatabase::classify(db_path, e))?;

        let mut rows = stmt_pkgs
            .query([])
            .map_err(|e| IndexDatabase::classify(db_path, e))?;

        while let Some(row) = rows
            .next()
            .map_err(|e| IndexDatabase::classify(db_path, e))?
        {
            let id: i64 = row
                .get(0)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;
            let dir: String = row
                .get(1)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;
            let name: String = row
                .get(2)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;
            let lang: String = row
                .get(3)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;

            package_by_id.insert(id, (dir.clone(), name.clone()));
            package_dirs
                .entry(dir.clone())
                .or_default()
                .push(name.clone());

            packages.insert(
                (dir.clone(), name.clone()),
                PackageExported {
                    id,
                    dir,
                    name,
                    lang,
                    files: Vec::new(),
                    exported: BTreeMap::new(),
                },
            );
        }
    }

    for names in package_dirs.values_mut() {
        names.sort();
    }

    // Assign files to packages
    for (file_id, path, pkg_id) in file_pkg_ids {
        if let Some(id) = pkg_id {
            if let Some(key) = package_by_id.get(&id) {
                if let Some(pkg) = packages.get_mut(key) {
                    pkg.files.push((file_id, path));
                }
            }
        }
    }

    // 3. Modules (manifests)
    let mut modules = Vec::new();
    {
        let mut stmt_manifests = conn
            .prepare("SELECT path, content FROM manifests ORDER BY path ASC")
            .map_err(|e| IndexDatabase::classify(db_path, e))?;

        let mut rows = stmt_manifests
            .query([])
            .map_err(|e| IndexDatabase::classify(db_path, e))?;

        while let Some(row) = rows
            .next()
            .map_err(|e| IndexDatabase::classify(db_path, e))?
        {
            let path: String = row
                .get(0)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;
            let content: String = row
                .get(1)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;

            if path.rsplit('/').next() == Some("go.mod") {
                if let Some(mod_path) = parse_module_path(&content) {
                    modules.push(ModuleInfo {
                        dir: dir_of(&path).to_owned(),
                        path: mod_path,
                    });
                }
            }
        }
    }

    if modules.is_empty() {
        modules.push(ModuleInfo {
            dir: String::new(),
            path: String::new(),
        });
    }
    // Sort modules by longest directory first, then ascending
    modules.sort_by(|a, b| b.dir.len().cmp(&a.dir.len()).then(a.dir.cmp(&b.dir)));

    // 4. Exported Symbols (excluding test files)
    {
        let mut stmt_syms = conn
            .prepare(
                "SELECT s.id, s.file_id, s.name, s.qual_name, s.kind, f.package_id, f.path \
                 FROM symbols s \
                 JOIN files f ON f.id = s.file_id \
                 WHERE s.exported = 1 \
                 ORDER BY s.name ASC, s.file_id ASC",
            )
            .map_err(|e| IndexDatabase::classify(db_path, e))?;

        let mut rows = stmt_syms
            .query([])
            .map_err(|e| IndexDatabase::classify(db_path, e))?;

        while let Some(row) = rows
            .next()
            .map_err(|e| IndexDatabase::classify(db_path, e))?
        {
            let sym_id: i64 = row
                .get(0)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;
            let file_id: i64 = row
                .get(1)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;
            let name: String = row
                .get(2)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;
            let qual_name: String = row
                .get(3)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;
            let kind_str: String = row
                .get(4)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;
            let pkg_id: Option<i64> = row
                .get(5)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;
            let file_path: String = row
                .get(6)
                .map_err(|e| IndexDatabase::classify(db_path, e))?;

            // Go build semantics: test files (_test.go) are invisible to importers
            if file_path.ends_with("_test.go") {
                continue;
            }

            if let Some(pid) = pkg_id {
                if let Some(key) = package_by_id.get(&pid) {
                    if let Some(pkg) = packages.get_mut(key) {
                        pkg.exported.entry(name).or_default().push(ExportedSymbol {
                            symbol_id: sym_id,
                            file_id,
                            file_path,
                            qual_name,
                            kind: parse_def_kind(&kind_str),
                        });
                    }
                }
            }
        }
    }

    Ok(ExportedIndex {
        snapshot_id,
        modules,
        package_dirs,
        packages,
        package_by_id,
        file_by_id,
        file_by_path,
        has_vendor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::ingest_facts;
    use crate::IndexDatabase;
    use cs_scanner::{scan, ScanConfig};
    use std::fs;
    use tempfile::tempdir;

    fn write_file(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn compute_snapshot_id_is_deterministic_and_changes_with_content() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(tmp.path(), "a.go", b"package test\nfunc A() {}\n");

        let files = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let db_path = tmp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).unwrap();
        ingest_facts(&mut db, tmp.path(), &files, 100).unwrap();

        let snap1 = db.compute_snapshot_id().unwrap();
        assert_eq!(snap1.len(), 64, "blake3 hex string length must be 64");

        // Recomputing produces the exact same hash
        let snap2 = db.compute_snapshot_id().unwrap();
        assert_eq!(snap1, snap2);

        // Different database with modified content
        let tmp2 = tempdir().unwrap();
        write_file(tmp2.path(), "go.mod", b"module example.com/test\n");
        write_file(
            tmp2.path(),
            "a.go",
            b"package test\nfunc A() { /* changed */ }\n",
        );

        let files2 = scan(tmp2.path(), &ScanConfig::default()).unwrap();
        let db_path2 = tmp2.path().join("index.db");
        let mut db2 = IndexDatabase::open_or_create(&db_path2).unwrap();
        ingest_facts(&mut db2, tmp2.path(), &files2, 100).unwrap();

        let snap3 = db2.compute_snapshot_id().unwrap();
        assert_ne!(
            snap1, snap3,
            "modified file content must yield different snapshot_id"
        );
    }

    #[test]
    fn build_exported_index_on_fixtures_go_resolve() {
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

        let index = db.build_exported_index().expect("build exported index");

        assert_eq!(index.snapshot_id.len(), 64);
        assert!(!index.modules.is_empty(), "must find go.mod module");
        assert!(index.modules.iter().any(|m| m.path == "example.com/m/v2"));
        assert!(index.modules.iter().any(|m| m.path == "nested.example/x"));

        // Check package dirs
        assert!(index.package_dirs.contains_key("nested"));
        assert!(index.package_dirs.contains_key(""));

        // Check non-test package lookup
        let non_test = index.non_test_package("nested");
        assert_eq!(non_test, Some(("nested".to_owned(), "nested".to_owned())));

        // Check that exported definitions are present and unexported definitions are omitted
        let pkg_key = (String::new(), "main".to_owned());
        if let Some(pkg) = index.packages.get(&pkg_key) {
            // main package has files
            assert!(!pkg.files.is_empty());
        }

        // Check exported defs in nested package
        let nested_key = ("nested".to_owned(), "nested".to_owned());
        let nested_pkg = index.packages.get(&nested_key).expect("nested package");
        assert!(
            nested_pkg.exported.contains_key("Cross"),
            "exported function Cross must be in exported index"
        );
    }

    #[test]
    fn exported_index_excludes_test_file_exports() {
        let tmp = tempdir().unwrap();
        write_file(tmp.path(), "go.mod", b"module example.com/test\n");
        write_file(
            tmp.path(),
            "lib.go",
            b"package lib\n\nfunc ExportedFunc() {}\nfunc internalFunc() {}\n",
        );
        write_file(
            tmp.path(),
            "lib_test.go",
            b"package lib\n\nfunc TestExportedHelper() {}\n",
        );

        let files = scan(tmp.path(), &ScanConfig::default()).unwrap();
        let db_path = tmp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).unwrap();
        ingest_facts(&mut db, tmp.path(), &files, 100).unwrap();

        let index = db.build_exported_index().unwrap();
        let pkg = index
            .packages
            .get(&(String::new(), "lib".to_owned()))
            .expect("lib package");

        assert!(
            pkg.exported.contains_key("ExportedFunc"),
            "ExportedFunc must be present in exported index"
        );
        assert!(
            !pkg.exported.contains_key("internalFunc"),
            "unexported internalFunc must NOT be in exported index"
        );
        assert!(
            !pkg.exported.contains_key("TestExportedHelper"),
            "exports from _test.go files must NOT be in exported index"
        );
    }
}
