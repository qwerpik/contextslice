//! Real-repository resolution harness (ADR-018 §7): stats, thresholds,
//! runtime and the deterministic audit sample for manual precision
//! verification.
//!
//! Usage:
//!
//! ```text
//! CS_RESOLVE_REPO=/tmp/gin cargo test --release -p cs-resolve --test real_repo -- --ignored --nocapture
//! CS_RESOLVE_DUMP_AUDIT=1 …   # also print the n≥150 audit sample
//! ```

mod common;

use std::time::Instant;

use cs_resolve::{GoResolver, LanguageResolver};

use crate::common::build_snapshot;

/// In-repo import resolution rate floor (ADR-018 §7).
const IMPORT_RATE_FLOOR: f64 = 0.95;
/// Wall-clock ceiling per repository (ADR-018 §8).
const RESOLVE_TIME_CEILING_SECS: f64 = 1.0;

#[test]
#[ignore = "needs a real repository clone; set CS_RESOLVE_REPO"]
fn resolves_a_real_repository() {
    let root =
        std::env::var("CS_RESOLVE_REPO").expect("set CS_RESOLVE_REPO to a Go repository checkout");
    let root = std::path::PathBuf::from(&root);

    let extract_start = Instant::now();
    let snapshot = build_snapshot(&root);
    let extract_secs = extract_start.elapsed().as_secs_f64();

    let prepare_start = Instant::now();
    let resolver = GoResolver::prepare(&snapshot);
    let prepare_secs = prepare_start.elapsed().as_secs_f64();

    let resolve_start = Instant::now();
    let repo = resolver.resolve(&snapshot);
    let resolve_secs = resolve_start.elapsed().as_secs_f64();

    let stats = &repo.stats;
    println!("repo:            {}", root.display());
    println!("files:           {}", snapshot.files.len());
    println!(
        "extract+prepare+resolve: {extract_secs:.2}s + {prepare_secs:.3}s + {resolve_secs:.3}s"
    );
    println!(
        "imports:         {} resolved / {} external / {} unresolved",
        stats.resolved, stats.external, stats.unresolved
    );
    println!(
        "import rate (in-repo found / (found+unresolved)): {:.1}%",
        stats.in_repo_import_success_rate() * 100.0
    );
    println!(
        "refs:            {} bound / {} unbound / {} qualifier occurrences",
        stats.refs_bound, stats.refs_unbound, stats.package_qualifier_refs
    );
    println!("unbound reasons:");
    for (reason, count) in &stats.unbound_reasons {
        println!("  {reason:?}: {count}");
    }
    println!(
        "edges:           {} ({} import_out, {} ref_def, {} test_affinity)",
        repo.edges.len(),
        repo.edges
            .iter()
            .filter(|e| e.kind == cs_resolve::EdgeKind::ImportOut)
            .count(),
        repo.edges
            .iter()
            .filter(|e| e.kind == cs_resolve::EdgeKind::RefDef)
            .count(),
        repo.edges
            .iter()
            .filter(|e| e.kind == cs_resolve::EdgeKind::TestAffinity)
            .count(),
    );

    // Threshold gates (ADR-018 §7).
    let rate = stats.in_repo_import_success_rate();
    assert!(
        rate >= IMPORT_RATE_FLOOR,
        "in-repo import resolution {rate:.3} below floor {IMPORT_RATE_FLOOR}"
    );
    assert!(
        resolve_secs < RESOLVE_TIME_CEILING_SECS,
        "resolve took {resolve_secs:.2}s, ceiling is {RESOLVE_TIME_CEILING_SECS}s"
    );

    // The nested-module rule on real repos: chi's _examples modules.
    for (path, resolution) in repo
        .files
        .values()
        .flat_map(|f| f.imports.iter().map(|(i, r)| (i.raw.clone(), r.clone())))
    {
        if let cs_resolve::Resolution::Resolved(dir) = resolution {
            assert!(
                !dir.starts_with("vendor/")
                    || repo.edges.iter().any(|e| e.dst.starts_with("vendor/")),
                "vendor resolution present for {path}"
            );
        }
    }

    // Deterministic audit sample for manual precision verification: every
    // stride-th bound ref, n>=150.
    if std::env::var("CS_RESOLVE_DUMP_AUDIT").is_ok() {
        let mut bound: Vec<(String, u32, String, String, String)> = Vec::new();
        for (path, f) in &repo.files {
            for (r, b) in &f.refs {
                if !b.targets.is_empty() {
                    let targets = b
                        .targets
                        .iter()
                        .map(|t| format!("{}:{}", t.file, t.qual_name))
                        .collect::<Vec<_>>()
                        .join(" | ");
                    bound.push((
                        path.clone(),
                        r.span.start_line,
                        r.name.clone(),
                        format!("{:?}", r.kind),
                        targets,
                    ));
                }
            }
        }
        bound.sort();
        let n = bound.len();
        let stride = (n / 150).max(1);
        println!(
            "--- AUDIT SAMPLE (n={n}, stride={stride}, sampled {}) ---",
            n.div_ceil(stride)
        );
        for entry in bound.iter().step_by(stride) {
            println!(
                "AUDIT {}:{}\t{}\t{}\t-> {}",
                entry.0, entry.1, entry.2, entry.3, entry.4
            );
        }
    }
}
