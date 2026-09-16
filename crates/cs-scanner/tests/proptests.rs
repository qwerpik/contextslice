//! Property tests for scanner determinism (MASTER_PLAN §8.1).
//!
//! Input: arbitrary sets of 0..30 valid, traversal-free relative `.go`
//! paths (1–3 lowercase segments, optionally underscore-prefixed
//! directories), materialized in a fresh tempdir *in case-generation
//! order* — so the sequence of `create` calls is itself part of the
//! generated case.
//!
//! Invariants:
//!
//! - **Determinism.** Two `cs_scanner::scan` runs over the same tree
//!   produce identical `Vec<ScannedFile>`, field for field (path,
//!   language, size, mtime, hash, skip) via the derived `PartialEq`.
//! - **Canonical order.** Results are strictly ascending by the
//!   normalized path *string* (ARCHITECTURE §7: order-normalize before
//!   any persisting decision); strictness follows because the paths are
//!   a set, so no two entries can tie.
//! - **Completeness.** Every written file appears exactly once, labeled
//!   Go, sized correctly, hashed, and not skipped.
//! - **Creation-order invariance.** Because case generation varies the
//!   write order while the tree content stays the same, the properties
//!   above hold for every creation order: the scan output is a function
//!   of the tree, not of the order it was built in.

use std::collections::BTreeSet;

use proptest::prelude::*;

use cs_scanner::{scan, Language, ScanConfig};

/// Fixture content: tiny but a real Go file. Language detection keys off
/// the extension (any content works); hashing keys off the bytes, so the
/// size assertion below pins that the listed files are the written ones.
const CONTENT: &str = "package p\n\nfunc F() {}\n";

proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 256,
        ..proptest::test_runner::Config::default()
    })]

    #[test]
    fn scan_is_deterministic_strictly_sorted_and_complete(
        rel_paths in proptest::collection::vec("[a-z]{1,8}(/_?[a-z]{1,8}){0,2}\\.go", 0..30usize),
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        // Write in *generated* order, duplicates skipped: the case's
        // creation order is part of the generated input.
        let mut written: BTreeSet<String> = BTreeSet::new();
        for rel in &rel_paths {
            if written.insert(rel.clone()) {
                let abs = dir.path().join(rel);
                if let Some(parent) = abs.parent() {
                    std::fs::create_dir_all(parent).expect("create fixture directories");
                }
                std::fs::write(&abs, CONTENT).expect("write fixture");
            }
        }
        let expected_size = u64::try_from(CONTENT.len()).expect("fixture is tiny");

        let config = ScanConfig::default();
        let first = scan(dir.path(), &config).expect("scan succeeds");
        let second = scan(dir.path(), &config).expect("scan succeeds");

        // Determinism, field for field (mtime included: nothing touches
        // the fixtures between the two runs).
        prop_assert_eq!(&first, &second);

        // Strictly ascending by normalized path string.
        for pair in first.windows(2) {
            prop_assert!(
                pair[0].path < pair[1].path,
                "paths must be strictly ascending: {:?} then {:?}",
                pair[0].path,
                pair[1].path
            );
        }

        // Every input file appears exactly once, correctly labeled.
        let found: BTreeSet<&str> = first.iter().map(|file| file.path.as_str()).collect();
        prop_assert_eq!(
            found.len(),
            written.len(),
            "every fixture must be listed exactly once"
        );
        for rel in &written {
            prop_assert!(
                found.contains(rel.as_str()),
                "scanned output is missing fixture {rel}"
            );
        }
        for file in &first {
            prop_assert_eq!(file.lang, Language::Go);
            prop_assert_eq!(file.size, expected_size);
            prop_assert!(file.hash.is_some(), "{} must be hashed", file.path);
            prop_assert!(file.skip.is_none(), "{} must not be skipped", file.path);
        }
    }
}
