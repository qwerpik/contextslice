//! The cross-cutting gauntlet (ADR-021 D3/D5/D7): one scripted edit history
//! over a realistic multi-package tree, one incremental update after every
//! edit, and a final byte-for-byte semantic comparison against a fresh cold
//! build of the final tree. Any observable difference between "index built
//! incrementally" and "index built cold" is a defect, whatever the edit
//! sequence that produced it.
//!
//! The edit kinds are the ones the fix stages landed regression tests for —
//! add, delete, modify, package rename, blank import, unreadable, too large,
//! parse-cap crossing — composed so that no later step re-resolves the
//! packages an earlier step may have left stale: a sequence masks defects
//! that any single-step test cannot see.

#![forbid(unsafe_code)]

use std::fs;
use std::path::Path;

use cs_index::{IndexDatabase, IndexState};
use cs_scanner::{scan, ScanConfig, SkipReason};
use tempfile::{tempdir, TempDir};

fn write_file(root: &Path, rel: &str, content: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

/// Grow (or create) `rel` as a sparse file of `len` bytes: no data is
/// written, so the tree stays fast to build while the file genuinely sits
/// over the scanner's caps.
fn sparse_grow(root: &Path, rel: &str, len: u64) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    file.set_len(len).unwrap();
}

fn open_index() -> (TempDir, IndexDatabase) {
    let dir = tempdir().unwrap();
    let db = IndexDatabase::open_or_create(&dir.path().join("index.db")).unwrap();
    (dir, db)
}

/// Scan `root` and run one incremental update; the returned label only names
/// the step in panics.
fn run_step(step: &str, db: &mut IndexDatabase, root: &Path) -> cs_index::IncrementalStats {
    let files = scan(root, &ScanConfig::default()).unwrap();
    db.update_incremental(root, &files)
        .unwrap_or_else(|e| panic!("step {step}: incremental update failed: {e}"))
}

/// Every table of the index as text rows keyed by semantic identity (paths,
/// names, spans, ordinals), never by row ids — ids legitimately differ
/// between two databases built by different histories. NULLs are rendered
/// explicitly so a comparison cannot pass vacuously.
fn semantic_dump(db: &IndexDatabase) -> Vec<String> {
    const QUERIES: &[&str] = &[
        "SELECT 'meta', key, value FROM meta ORDER BY key",
        "SELECT 'packages', dir, name, lang FROM packages ORDER BY dir, name, lang",
        "SELECT 'files', f.path, f.lang, COALESCE(p.dir || '#' || p.name, 'NOPKG'), \
         COALESCE(hex(f.hash), 'NULL'), COALESCE(f.skip, 'NULL'), CAST(f.size AS TEXT), \
         CAST(f.mtime AS TEXT), f.parse_status, COALESCE(CAST(f.tokens_est AS TEXT), 'NULL') \
         FROM files f LEFT JOIN packages p ON p.id = f.package_id ORDER BY f.path",
        "SELECT 'symbols', f.path, s.name, s.qual_name, s.kind, CAST(s.exported AS TEXT), \
         CAST(s.line AS TEXT), CAST(s.end_line AS TEXT), CAST(s.start_byte AS TEXT), \
         CAST(s.end_byte AS TEXT), COALESCE(s.signature, 'NULL'), COALESCE(s.container, 'NULL'), \
         COALESCE(s.doc, 'NULL') \
         FROM symbols s JOIN files f ON f.id = s.file_id \
         ORDER BY f.path, s.start_byte, s.name, s.qual_name",
        "SELECT 'refs', f.path, r.name, r.kind, COALESCE(r.qualifier, 'NULL'), \
         CAST(r.line AS TEXT), CAST(r.start_byte AS TEXT), CAST(r.end_byte AS TEXT), \
         COALESCE(r.container, 'NULL') \
         FROM refs r JOIN files f ON f.id = r.file_id \
         ORDER BY f.path, r.start_byte, r.name",
        "SELECT 'imports', f.path, i.raw, COALESCE(i.alias, 'NULL'), i.kind, \
         CAST(i.ordinal AS TEXT), COALESCE(i.resolved_dir, 'NULL'), \
         COALESCE(i.resolved_file, 'NULL'), COALESCE(i.unresolved_reason, 'NULL') \
         FROM imports i JOIN files f ON f.id = i.file_id \
         ORDER BY f.path, i.ordinal, i.raw",
        "SELECT 'manifests', path, content FROM manifests ORDER BY path",
        "SELECT 'bindings', f.path, r.name, CAST(r.start_byte AS TEXT), \
         COALESCE(b.unbound_reason, 'NULL') \
         FROM bindings b JOIN refs r ON r.id = b.ref_id JOIN files f ON f.id = r.file_id \
         ORDER BY f.path, r.start_byte, r.name",
        "SELECT 'binding_targets', f1.path, r.name, CAST(r.start_byte AS TEXT), f2.path, \
         bt.qual_name, bt.kind \
         FROM binding_targets bt JOIN refs r ON r.id = bt.ref_id \
         JOIN files f1 ON f1.id = r.file_id JOIN files f2 ON f2.id = bt.file_id \
         ORDER BY f1.path, r.start_byte, r.name, f2.path, bt.qual_name",
        "SELECT 'edges', f1.path, f2.path, e.kind, CAST(e.weight AS TEXT) \
         FROM edges e JOIN files f1 ON f1.id = e.src JOIN files f2 ON f2.id = e.dst \
         ORDER BY f1.path, f2.path, e.kind",
    ];

    let conn = db.connection();
    let mut out = Vec::new();
    for sql in QUERIES {
        let mut stmt = conn.prepare(sql).unwrap();
        let ncols = stmt.column_count();
        let rows = stmt
            .query_map([], |row| {
                let mut cells = Vec::with_capacity(ncols);
                for i in 0..ncols {
                    cells.push(
                        row.get::<_, Option<String>>(i)?
                            .unwrap_or_else(|| "NULL".into()),
                    );
                }
                Ok(cells.join(" | "))
            })
            .unwrap();
        for row in rows {
            out.push(row.unwrap());
        }
    }
    out
}

/// A fresh cold build of `root` — the parity oracle.
fn cold_build(root: &Path) -> (TempDir, IndexDatabase) {
    let files = scan(root, &ScanConfig::default()).unwrap();
    let (dir, mut db) = open_index();
    db.ingest_facts(root, &files).unwrap();
    db.resolve_facts().unwrap();
    (dir, db)
}

#[cfg(unix)]
fn chmod_denies_read() -> bool {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempdir().unwrap();
    let canary = tmp.path().join("canary");
    fs::write(&canary, b"x").unwrap();
    let mut perms = fs::metadata(&canary).unwrap().permissions();
    perms.set_mode(0o0);
    fs::set_permissions(&canary, perms).unwrap();
    fs::read(&canary).is_err()
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(mode);
    fs::set_permissions(path, perms).unwrap();
}

/// The scripted edit sequence: every required edit kind, no fewer than ten
/// incremental runs, ending in full parity against a cold build.
#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn scripted_edit_history_parities_with_cold_build() {
    if !chmod_denies_read() {
        eprintln!("skipping: chmod 000 does not deny reads for this user");
        return;
    }

    let tree = tempdir().unwrap();
    let root = tree.path();

    // A realistic multi-package module: three imported packages plus main.
    write_file(root, "go.mod", b"module example.com/app\n\ngo 1.22\n");
    write_file(
        root,
        "auth/auth.go",
        b"package auth\n\nfunc Login() {}\n\nfunc Logout() {}\n",
    );
    write_file(
        root,
        "auth/session.go",
        b"package auth\n\nfunc Session() {}\n",
    );
    write_file(
        root,
        "web/handler.go",
        b"package web\n\nimport \"example.com/app/auth\"\n\nfunc Handler() { auth.Login() }\n",
    );
    write_file(
        root,
        "storage/store.go",
        b"package storage\n\nfunc Get() {}\n\nfunc Put() {}\n",
    );
    write_file(root, "util/util.go", b"package util\n\nfunc Helper() {}\n");
    write_file(
        root,
        "main.go",
        b"package main\n\nimport \"example.com/app/util\"\n\nfunc main() { util.Helper() }\n",
    );

    let (_db_dir, mut db) = cold_build(root);
    assert_eq!(db.state().unwrap(), IndexState::Ready);

    // 1. modify: a new export in auth.
    write_file(
        root,
        "auth/auth.go",
        b"package auth\n\nfunc Login() {}\n\nfunc Logout() {}\n\nfunc Ping() {}\n",
    );
    let s = run_step("1 modify auth/auth.go", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (1, 0, 0)
    );

    // 2. add file: a second file in the existing util package.
    write_file(root, "util/strings.go", b"package util\n\nfunc Join() {}\n");
    let s = run_step("2 add util/strings.go", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (0, 1, 0)
    );

    // 3. modify: the web caller starts using the new auth export.
    write_file(
        root,
        "web/handler.go",
        b"package web\n\nimport \"example.com/app/auth\"\n\nfunc Handler() { auth.Login(); auth.Ping() }\n",
    );
    let s = run_step("3 modify web/handler.go", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (1, 0, 0)
    );

    // 4. delete file: auth shrinks to its remaining file.
    fs::remove_file(root.join("auth/session.go")).unwrap();
    let s = run_step("4 delete auth/session.go", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (0, 0, 1)
    );

    // 5. add blank import: no bindings, only import_out edges.
    write_file(
        root,
        "web/blank.go",
        b"package web\n\nimport _ \"example.com/app/storage\"\n",
    );
    let s = run_step("5 add web/blank.go", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (0, 1, 0)
    );

    // 6. rename package: util becomes helpers in place; the caller follows.
    write_file(
        root,
        "util/util.go",
        b"package helpers\n\nfunc Helper() {}\n",
    );
    write_file(
        root,
        "util/strings.go",
        b"package helpers\n\nfunc Join() {}\n",
    );
    write_file(
        root,
        "main.go",
        b"package main\n\nimport \"example.com/app/util\"\n\nfunc main() { helpers.Helper() }\n",
    );
    let s = run_step("6 rename util to helpers", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (3, 0, 0)
    );
    let ghost: i64 = db
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM packages WHERE dir = 'util' AND name = 'util'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ghost, 0, "the renamed-away package must be collected");

    // 7. delete package: storage disappears; the blank import goes unresolved.
    fs::remove_file(root.join("storage/store.go")).unwrap();
    fs::remove_dir(root.join("storage")).unwrap();
    let s = run_step("7 delete storage package", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (0, 0, 1)
    );
    let ghost: i64 = db
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM packages WHERE dir = 'storage'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ghost, 0, "the deleted package must be collected");

    // 8. modify: main starts calling Join (a binding into util/strings.go).
    write_file(
        root,
        "main.go",
        b"package main\n\nimport \"example.com/app/util\"\n\nfunc main() {\n\thelpers.Helper()\n\thelpers.Join()\n}\n",
    );
    let s = run_step("8 modify main.go", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (1, 0, 0)
    );

    // 9. unreadable: auth's only file goes chmod 000; the package empties and
    // is collected, web's import re-classifies unresolved.
    let auth = root.join("auth/auth.go");
    set_mode(&auth, 0o0);
    let files = scan(root, &ScanConfig::default()).unwrap();
    assert_eq!(
        files
            .iter()
            .find(|f| f.path == "auth/auth.go")
            .unwrap()
            .skip,
        Some(SkipReason::Unreadable)
    );
    let s = run_step("9 chmod 000 auth/auth.go", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (1, 0, 0)
    );
    let ghost: i64 = db
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM packages WHERE dir = 'auth'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ghost, 0, "the emptied auth package must be collected");

    // 10. readable again: the package is recreated from scratch. While it was
    // gone web's import row said resolved_dir IS NULL, so nothing names web
    // for fan-out — the sequence must still rebind it exactly as a cold build.
    set_mode(&auth, 0o644);
    let s = run_step("10 restore auth/auth.go", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (1, 0, 0)
    );

    // 11. too large: strings.go grows past the 50 MiB read cap as a sparse
    // file; never read, hashed never, its facts gone.
    sparse_grow(root, "util/strings.go", 60 * 1024 * 1024);
    let files = scan(root, &ScanConfig::default()).unwrap();
    assert_eq!(
        files
            .iter()
            .find(|f| f.path == "util/strings.go")
            .unwrap()
            .skip,
        Some(SkipReason::TooLarge)
    );
    let s = run_step("11 sparse 60MiB util/strings.go", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (1, 0, 0)
    );

    // 12. parse cap: a file is BORN over the 1 MiB parse cap (hashed, never
    // parsed) — the add-side of the parse-skip transition.
    sparse_grow(root, "notes/notes.go", 2 * 1024 * 1024);
    let files = scan(root, &ScanConfig::default()).unwrap();
    assert_eq!(
        files
            .iter()
            .find(|f| f.path == "notes/notes.go")
            .unwrap()
            .skip,
        Some(SkipReason::ParseSkipped)
    );
    let s = run_step("12 sparse 2MiB notes/notes.go", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (0, 1, 0)
    );

    // 13. modify: main again (twice Join — the ref_def weight dampener sees a
    // repeat count), touching only main's package.
    write_file(
        root,
        "main.go",
        b"package main\n\nimport \"example.com/app/util\"\n\nfunc main() {\n\thelpers.Join()\n\thelpers.Join()\n\thelpers.Helper()\n}\n",
    );
    let s = run_step("13 modify main.go again", &mut db, root);
    assert_eq!(
        (s.files_modified, s.files_added, s.files_deleted),
        (1, 0, 0)
    );

    // The gauntlet: thirteen incremental runs later, the index must be
    // byte-for-byte what a fresh cold build of this final tree produces.
    assert_eq!(db.state().unwrap(), IndexState::Ready);
    assert!(
        db.meta("update_in_progress").unwrap().is_none(),
        "a completed run must clear the torn-update marker"
    );

    let (_fresh_dir, fresh) = cold_build(root);
    assert_eq!(db.state().unwrap(), IndexState::Ready);

    let incremental_snapshot = db.meta("snapshot_id").unwrap();
    let fresh_snapshot = fresh.meta("snapshot_id").unwrap();
    assert_eq!(
        incremental_snapshot, fresh_snapshot,
        "the incrementally maintained index must name the same content"
    );

    let got = semantic_dump(&db);
    let want = semantic_dump(&fresh);
    if got != want {
        use std::fmt::Write as _;
        let mut diff = String::from("incremental != cold build; differing rows:\n");
        for line in &want {
            if !got.contains(line) {
                let _ = writeln!(diff, "  cold only: {line}");
            }
        }
        for line in &got {
            if !want.contains(line) {
                let _ = writeln!(diff, "  incr only: {line}");
            }
        }
        panic!("{diff}");
    }
}
