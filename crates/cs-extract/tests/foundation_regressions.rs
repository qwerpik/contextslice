//! Foundation-regression integration test for the scanner→extract seam:
//! a directory that contains a real `.git/` tree must scan down to source
//! files only, and every scanned Go file must feed extraction cleanly.
//! Each crate's unit tests pin the behaviors in isolation; this pins the
//! composition the pipeline actually performs — credential-bearing `.git`
//! internals never reach the scan output, so they can never reach
//! extraction, the index, or a rendered slice.

use cs_scanner::{scan, Language, ScanConfig};

/// Write `contents` to `root/rel`, creating parent directories.
fn write(root: &std::path::Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, contents).expect("write fixture");
}

/// A tree containing `.git/config` with an embedded credential scans down
/// to source files only: no `.git` path, and the `.gitignore`-governed
/// directory is pruned even though the walker includes dotfiles on purpose.
#[test]
fn scan_over_a_git_bearing_dir_yields_source_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        ".git/config",
        "[remote \"origin\"]\n\turl = https://token@example.com/x/y\n",
    );
    write(dir.path(), ".git/HEAD", "ref: refs/heads/main\n");
    write(dir.path(), ".git/objects/ab/cdef0123", "loose object bytes");
    write(dir.path(), ".gitignore", "generated/\n");
    write(dir.path(), "generated/stub.go", "package generated\n");
    write(
        dir.path(),
        "keep.go",
        "package keep\n\n// Keep is documented.\nfunc Keep() {}\n",
    );
    write(
        dir.path(),
        "cmd/main.go",
        "package main\n\nfunc main() {}\n",
    );

    let files = scan(dir.path(), &ScanConfig::default()).expect("scan");
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![".gitignore", "cmd/main.go", "keep.go"],
        "source files plus the ignore manifest itself — no .git internals, no ignored dir"
    );
}

/// The exact handoff the index stage performs: every scanned file carries
/// its detected language, and every scanned Go file extracts with its
/// package clause, definitions and docs intact. Whatever the scanner
/// excluded (`.git/`, `generated/`) contributes nothing here by
/// construction — it is simply absent from the input list.
#[test]
fn scanned_files_feed_extraction_with_facts_intact() {
    let dir = tempfile::tempdir().expect("tempdir");
    write(
        dir.path(),
        ".git/config",
        "[remote \"origin\"]\n\turl = token\n",
    );
    write(
        dir.path(),
        "keep.go",
        "package keep\n\n// Keep is documented.\nfunc Keep() {}\n",
    );
    write(
        dir.path(),
        "cmd/main.go",
        "package main\n\nfunc main() {}\n",
    );
    write(dir.path(), "notes.txt", "not source\n");

    let files = scan(dir.path(), &ScanConfig::default()).expect("scan");

    let mut extracted_packages: Vec<(String, String)> = Vec::new();
    for file in &files {
        let source =
            std::fs::read_to_string(dir.path().join(&file.path)).expect("scanned file readable");
        match file.lang {
            Language::Go => {
                let extracted =
                    cs_extract::extract(&source, Language::Go).expect("go extraction never fails");
                assert_eq!(
                    extracted.status,
                    cs_extract::ParseStatus::Ok,
                    "scanned Go must parse clean: {}",
                    file.path
                );
                let package = extracted
                    .package_name
                    .expect("scanned Go has a package clause");
                extracted_packages.push((file.path.clone(), package));
            }
            Language::Unknown => {} // data files ride along, unparsed
            other => panic!("unexpected language for {}: {other:?}", file.path),
        }
    }

    // The scanner's order feeds the extractor, so the packages come out in
    // the same normalized path order the index persists.
    assert_eq!(
        extracted_packages,
        vec![
            ("cmd/main.go".to_owned(), "main".to_owned()),
            ("keep.go".to_owned(), "keep".to_owned()),
        ]
    );

    // And the extracted facts are the real ones, not empty shells: the
    // documented definition survives the pipeline seam with its doc.
    let keep_source = std::fs::read_to_string(dir.path().join("keep.go")).expect("readable");
    let keep = cs_extract::extract(&keep_source, Language::Go).expect("go extraction");
    assert_eq!(keep.defs.len(), 1, "exactly Keep");
    assert_eq!(keep.defs[0].doc.as_deref(), Some("Keep is documented."));
}
