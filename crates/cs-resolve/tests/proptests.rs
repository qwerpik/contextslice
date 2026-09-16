//! Property tests for Go resolution order-invariance (MASTER_PLAN §8.1).
//!
//! Invariant: `GoResolver::prepare` + `resolve` are invariant under the
//! caller's file iteration order. The crate docs promise that candidate
//! sets are ordered by file path *before* any binding decision
//! (ALGORITHM §12), so feeding the same files in any permutation must
//! produce byte-identical `serde_json` of `ResolvedRepo::edges` and
//! `ResolvedRepo::stats` — the two outputs downstream stages persist.
//!
//! `ResolveSnapshot::new` sorts its inputs and would make the property
//! vacuous, so the snapshot is constructed **literally** from its pub
//! fields — bypassing `new()` — to hand `prepare`/`resolve` the raw
//! shuffled order.
//!
//! The synthetic repo is fixed, and its base order is deliberately not
//! the canonical sorted order (so the identity permutation is not the
//! sorted order either). It covers every resolution outcome and edge
//! kind at least once: four packages (plus a `b_test` external test
//! package), an in-repo import cycle (`d` → `a`), an external `fmt`
//! import, a dangling import (`example.com/prop/missing` → unresolved),
//! and internal `_test.go` files for `test_affinity` edges.

use proptest::prelude::*;

use cs_extract::{extract, ExtractedFile};
use cs_resolve::{FilePath, GoResolver, LanguageResolver, ResolveSnapshot};
use cs_scanner::Language;

/// Number of files in the synthetic snapshot; kept in sync with
/// [`synthetic_files`] by an assertion in the property below.
const FILE_COUNT: usize = 7;

const GO_MOD_PATH: &str = "go.mod";
const GO_MOD_CONTENT: &str = "module example.com/prop\n";

/// The Go source of the synthetic snapshot, by repo-relative path.
fn source_of(path: &str) -> &'static str {
    match path {
        "a/a.go" => {
            "package a\n\nimport (\n\t\"example.com/prop/b\"\n\t\"fmt\"\n)\n\n\
             // Helper calls into b and prints.\nfunc Helper() {\n\tb.Thing()\n\tfmt.Println(\"hi\")\n}\n"
        }
        "a/a_test.go" => {
            "package a\n\nimport \"example.com/prop/c\"\n\n\
             func TestHelper() {\n\tc.Work()\n}\n"
        }
        "b/b1.go" => {
            "package b\n\n// Thing is exported.\nfunc Thing() {}\n\n\
             // Widget is an exported type.\ntype Widget struct {\n\tID int\n}\n"
        }
        "b/b2.go" => {
            "package b\n\nimport \"example.com/prop/d\"\n\n\
             // Other reaches into d.\nfunc Other() {\n\td.Deep()\n}\n"
        }
        "b/b1_test.go" => {
            "package b_test\n\nimport \"example.com/prop/b\"\n\n\
             func TestThing() {\n\tb.Thing()\n}\n"
        }
        "c/c.go" => {
            "package c\n\nimport (\n\t\"example.com/prop/missing\"\n\t\"example.com/prop/d\"\n)\n\n\
             // Work does the thing.\nfunc Work() {\n\td.Deep()\n\tmissing.Nothing()\n}\n"
        }
        "d/d.go" => {
            "package d\n\nimport \"example.com/prop/a\"\n\n\
             // Deep closes the import cycle back into a.\nfunc Deep() {\n\ta.Helper()\n}\n"
        }
        other => unreachable!("fixture paths are exhaustive, got {other}"),
    }
}

/// The synthetic snapshot's files in a deliberately unsorted base order
/// (sorted would make the identity permutation coincide with the
/// canonical order and weaken the property).
fn synthetic_files() -> Vec<(FilePath, ExtractedFile)> {
    let paths = [
        "d/d.go",
        "a/a_test.go",
        "b/b2.go",
        "a/a.go",
        "c/c.go",
        "b/b1_test.go",
        "b/b1.go",
    ];
    paths
        .iter()
        .map(|path| {
            let extracted = extract(source_of(path), Language::Go)
                .unwrap_or_else(|e| panic!("{path}: go extraction never fails: {e}"));
            assert!(
                extracted.status == cs_extract::ParseStatus::Ok,
                "{path}: fixture must parse cleanly, got {:?}",
                extracted.status
            );
            ((*path).to_string(), extracted)
        })
        .collect()
}

/// Resolve the given files *in exactly this order*: a literal
/// `ResolveSnapshot` construction (pub fields, no `new()`) so the
/// resolver sees the raw caller order, then serialize the two outputs
/// that must not depend on it.
fn resolve_in_given_order(
    files: &[(FilePath, ExtractedFile)],
    manifests: &[(FilePath, String)],
) -> String {
    let snapshot = ResolveSnapshot {
        files: files.to_vec(),
        manifests: manifests.to_vec(),
    };
    let resolver = GoResolver::prepare(&snapshot);
    let repo = resolver.resolve(&snapshot);
    let edges = serde_json::to_string(&repo.edges).expect("edges serialize");
    let stats = serde_json::to_string(&repo.stats).expect("stats serialize");
    format!("{edges}\n{stats}")
}

proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 256,
        ..proptest::test_runner::Config::default()
    })]

    #[test]
    fn edges_and_stats_are_identical_under_any_input_file_permutation(
        keys in proptest::collection::vec(any::<u64>(), FILE_COUNT),
    ) {
        let files = synthetic_files();
        assert_eq!(files.len(), FILE_COUNT, "fixture count drifted");
        let manifests = vec![(
            GO_MOD_PATH.to_string(),
            GO_MOD_CONTENT.to_string(),
        )];
        let baseline = resolve_in_given_order(&files, &manifests);

        // Non-vacuity guard: the synthetic repo must genuinely exercise
        // the machinery — every edge kind and all three import outcomes
        // (resolved / external / unresolved) must appear in the
        // serialized output, or comparing permutations proves nothing.
        prop_assert!(
            baseline.contains("\"import_out\"")
                && baseline.contains("\"ref_def\"")
                && baseline.contains("\"test_affinity\""),
            "synthetic repo must produce every edge kind, got: {baseline}"
        );
        prop_assert!(
            baseline.contains("\"external\":1") && baseline.contains("\"unresolved\":1"),
            "synthetic repo must include one external and one unresolved import, got: {baseline}"
        );

        // A genuine permutation: sort indices by a generated key (ties
        // break by original index, so the permutation is total and
        // well-defined). Every reachable ordering of the input vec is
        // exercised across cases.
        let mut order: Vec<usize> = (0..files.len()).collect();
        order.sort_by_key(|&i| (keys[i], i));
        let shuffled: Vec<(FilePath, ExtractedFile)> =
            order.iter().map(|&i| files[i].clone()).collect();

        // Sanity: the shuffle really is a permutation of the same paths.
        let mut before: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
        let mut after: Vec<&str> = shuffled.iter().map(|(path, _)| path.as_str()).collect();
        before.sort_unstable();
        after.sort_unstable();
        prop_assert_eq!(before, after);

        let permuted = resolve_in_given_order(&shuffled, &manifests);
        prop_assert_eq!(baseline, permuted);
    }
}
