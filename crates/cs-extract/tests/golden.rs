//! Golden fixtures for Go extraction (LANGUAGES.md §8.3: goldens assert
//! exact extracted defs/refs/imports).
//!
//! Regenerate with `UPDATE_GOLDENS=1 cargo test -p cs-extract --test golden`.
//! A regenerated golden is a *review artifact*: read it against its fixture
//! before committing. The harness makes incorrect extraction obvious; the
//! reviewer makes it correct. Goldens are pretty-printed JSON of the whole
//! [`cs_extract::ExtractedFile`], compared byte-for-byte, so any drift in
//! defs, refs, imports, docs, signatures, spans or statuses fails loudly.

use std::fs;
use std::path::{Path, PathBuf};

use cs_extract::{extract, ExtractedFile, ParseStatus};
use cs_scanner::Language;

const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/go");

fn fixtures() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(FIXTURES_DIR)
        .unwrap_or_else(|e| panic!("fixture dir {FIXTURES_DIR} must exist: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "go"))
        .collect();
    files.sort();
    assert!(
        files.len() >= 20,
        "the Go fixture corpus must stay representative (≥20 files), got {}",
        files.len()
    );
    files
}

fn golden_path(fixture: &Path) -> PathBuf {
    let name = fixture
        .file_name()
        .and_then(|n| n.to_str())
        .expect("utf8 name");
    Path::new(FIXTURES_DIR)
        .join("golden")
        .join(format!("{name}.json"))
}

fn extract_fixture(fixture: &Path) -> String {
    let source = fs::read_to_string(fixture)
        .unwrap_or_else(|e| panic!("{} must be valid UTF-8: {e}", fixture.display()));
    let extracted = extract(&source, Language::Go)
        .unwrap_or_else(|e| panic!("{} must extract: {e}", fixture.display()));
    serde_json::to_string_pretty(&extracted).expect("serialization is total")
}

#[test]
fn go_goldens_match_exactly() {
    let update = std::env::var("UPDATE_GOLDENS").is_ok();
    let mut failures: Vec<String> = Vec::new();

    for fixture in fixtures() {
        let actual = extract_fixture(&fixture);
        let golden_file = golden_path(&fixture);

        if update {
            fs::create_dir_all(golden_file.parent().expect("parent exists"))
                .expect("create golden dir");
            fs::write(&golden_file, format!("{actual}\n")).expect("write golden");
            continue;
        }

        let Ok(expected) = fs::read_to_string(&golden_file) else {
            failures.push(format!(
                "{}: no golden at {} — regenerate with UPDATE_GOLDENS=1 and review it",
                fixture.display(),
                golden_file.display()
            ));
            continue;
        };

        if expected.trim() != actual {
            // First divergent line keeps the failure readable.
            let exp_line = expected.lines().zip(actual.lines()).find(|(a, b)| a != b);
            let where_ = match exp_line {
                Some((a, _)) => format!("first divergence near golden line: {a}"),
                None => "length mismatch".to_owned(),
            };
            failures.push(format!(
                "{}: extraction drifted ({where_}); re-run with UPDATE_GOLDENS=1 and review",
                fixture.display()
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "golden mismatches:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn no_orphan_goldens() {
    let golden_dir = Path::new(FIXTURES_DIR).join("golden");
    let mut orphans = Vec::new();
    for entry in fs::read_dir(&golden_dir).expect("golden dir exists after first run") {
        let path = entry.expect("readable").path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let fixture = Path::new(FIXTURES_DIR).join(name.trim_end_matches(".json"));
        if !fixture.exists() {
            orphans.push(name.to_owned());
        }
    }
    assert!(
        orphans.is_empty(),
        "goldens without fixtures (stale, must be deleted): {orphans:?}"
    );
}

#[test]
fn extraction_is_deterministic_across_runs_and_orderings() {
    // Same input, second run: identical bytes.
    for fixture in fixtures() {
        let first = extract_fixture(&fixture);
        let second = extract_fixture(&fixture);
        assert_eq!(
            first,
            second,
            "{}: two extractions of the same bytes differ",
            fixture.display()
        );
    }

    // Batch in reverse order: per-file output unaffected by processing
    // order (the property the parallel index stage depends on).
    let forward: Vec<String> = fixtures().iter().map(|f| extract_fixture(f)).collect();
    let mut reversed: Vec<String> = fixtures()
        .iter()
        .rev()
        .map(|f| extract_fixture(f))
        .collect();
    reversed.reverse();
    assert_eq!(
        forward, reversed,
        "processing order must not leak into output"
    );
}

#[test]
fn degraded_files_have_defined_behavior() {
    let read = |name: &str| {
        let path = Path::new(FIXTURES_DIR).join(name);
        let source = fs::read_to_string(&path).expect("fixture exists");
        extract(&source, Language::Go).expect("go extraction")
    };

    // Missing closing brace: complete def kept, broken one dropped.
    let f = read("20_broken_brace.go");
    assert_eq!(f.status, ParseStatus::Partial);
    let names: Vec<&str> = f.defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["Works"], "only the clean declaration survives");

    // Garbage tokens mid-file: whatever recovery allows, cleanly.
    let f = read("21_broken_mid.go");
    assert_eq!(f.status, ParseStatus::Partial);

    // Truncated mid-signature: no half definitions.
    let f = read("22_truncated.go");
    assert_eq!(f.status, ParseStatus::Partial);
    assert!(f.defs.is_empty(), "half a signature is not a def");

    // Python in a .go file: honest label, no defs, no package. Refs may
    // survive where tree-sitter recovered a clean subtree (the golden pins
    // exactly what: the `print("hi")` call).
    let f = read("25_garbage.go");
    assert_eq!(f.status, ParseStatus::Partial);
    assert!(f.defs.is_empty() && f.imports.is_empty());
    assert!(f.package_name.is_none());
    let names: Vec<&str> = f.refs.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["print"]);

    // Empty and package-only files are Ok, not Partial.
    let f = read("23_empty.go");
    assert_eq!(f.status, ParseStatus::Ok);
    let f: ExtractedFile = f;
    assert!(f.package_name.is_none());
    let f = read("24_package_only.go");
    assert_eq!(f.status, ParseStatus::Ok);
    assert_eq!(f.package_name.as_deref(), Some("only"));
}

#[test]
fn build_tagged_files_extract_independently() {
    // Two files, same package, same function name, different build tags:
    // extraction is per-file and must not collapse or deduplicate them.
    for name in ["10_buildtag_linux.go", "10_buildtag_windows.go"] {
        let f = extract_fixture(&Path::new(FIXTURES_DIR).join(name));
        let parsed: ExtractedFile = serde_json::from_str(&f).expect("golden parses back");
        assert_eq!(parsed.package_name.as_deref(), Some("buildtag"));
        assert_eq!(parsed.defs.len(), 1);
        assert_eq!(parsed.defs[0].name, "Syscall");
    }
}
