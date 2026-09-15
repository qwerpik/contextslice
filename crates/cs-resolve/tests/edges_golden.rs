//! Golden edge-set snapshot for the fixture matrix + determinism proof.
//!
//! The golden is the serialized `edges` + `stats` of resolving
//! `fixtures/go-resolve/`. Regenerate with `UPDATE_GOLDENS=1` and review —
//! it is the machine-checkable form of ADR-018's binding table.

mod common;

use cs_resolve::{GoResolver, LanguageResolver};

use crate::common::{build_snapshot, matrix_root};

fn resolve_matrix() -> (String, String) {
    let snapshot = build_snapshot(&matrix_root());
    let resolver = GoResolver::prepare(&snapshot);
    let repo = resolver.resolve(&snapshot);
    let edges = serde_json::to_string_pretty(&repo.edges).expect("ser");
    let stats = serde_json::to_string_pretty(&repo.stats).expect("ser");
    (edges, stats)
}

#[test]
fn edge_set_golden() {
    let (edges, stats) = resolve_matrix();
    let golden_path = matrix_root().join("../go-resolve-golden/edges.json");
    let golden_stats = matrix_root().join("../go-resolve-golden/stats.json");

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        std::fs::create_dir_all(golden_path.parent().expect("parent")).expect("dir");
        std::fs::write(&golden_path, format!("{edges}\n")).expect("write");
        std::fs::write(&golden_stats, format!("{stats}\n")).expect("write");
        return;
    }

    let expected = std::fs::read_to_string(&golden_path)
        .unwrap_or_else(|_| panic!("golden missing; run UPDATE_GOLDENS=1 and review"));
    assert_eq!(
        expected.trim(),
        edges,
        "edge set drifted; re-run with UPDATE_GOLDENS=1 and review the diff"
    );
    let expected_stats = std::fs::read_to_string(&golden_stats)
        .unwrap_or_else(|_| panic!("stats golden missing; run UPDATE_GOLDENS=1"));
    assert_eq!(expected_stats.trim(), stats, "stats drifted");
}

#[test]
fn resolution_is_deterministic_across_runs_and_input_order() {
    let (first_edges, first_stats) = resolve_matrix();

    // Second run: byte-identical.
    let (second_edges, second_stats) = resolve_matrix();
    assert_eq!(first_edges, second_edges);
    assert_eq!(first_stats, second_stats);

    // Shuffled input order: the snapshot constructor sorts, so output must
    // be unchanged (the property the parallel index stage depends on).
    let snapshot = build_snapshot(&matrix_root());
    let mut shuffled = cs_resolve::ResolveSnapshot {
        files: snapshot.files.clone(),
        manifests: snapshot.manifests.clone(),
    };
    shuffled.files.reverse();
    shuffled.manifests.reverse();
    let resolver = GoResolver::prepare(&shuffled);
    let repo = resolver.resolve(&shuffled);
    let edges = serde_json::to_string_pretty(&repo.edges).expect("ser");
    let stats = serde_json::to_string_pretty(&repo.stats).expect("ser");
    assert_eq!(edges, first_edges, "input order leaked into edges");
    assert_eq!(stats, first_stats, "input order leaked into stats");
}
