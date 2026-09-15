//! Extraction throughput on a real repository (MASTER_PLAN §15 step 3:
//! "measure on a real Go repository, not only tiny fixtures").
//!
//! Usage:
//!
//! ```text
//! CS_PERF_REPO=/tmp/gin cargo test --release -p cs-extract --test perf -- --ignored --nocapture
//! ```
//!
//! The hard assertion is a floor an order of magnitude below the
//! ARCHITECTURE.md §8 budget (~10 MB/s/core), so it catches catastrophic
//! regressions without making CI machine-speed-dependent; the printed table
//! is what gets compared against the budget.

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use cs_extract::extract;
use cs_scanner::Language;

/// Slowest tolerable whole-pipeline throughput. ARCHITECTURE.md §8 budgets
/// ~10 MB/s/core for parsing; anything under 1 MB/s single-threaded means a
/// pathological regression, not a slow machine.
const FLOOR_MB_PER_SEC: f64 = 1.0;

#[test]
#[ignore = "needs a real repository clone; set CS_PERF_REPO"]
fn real_repo_throughput() {
    let root = PathBuf::from(
        std::env::var("CS_PERF_REPO").expect("set CS_PERF_REPO to a Go repository checkout"),
    );

    #[allow(clippy::cast_possible_truncation)] // 32-bit hosts: repo corpora stay far below 4 GiB
    let mut total_bytes = 0usize;
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path = entry.expect("readable").path();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name == ".git" || name == "vendor" || name == "node_modules" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if name.to_ascii_lowercase().ends_with(".go") {
                files.push(path);
            }
        }
    }
    files.sort();
    assert!(!files.is_empty(), "no .go files under {}", root.display());

    for f in &files {
        total_bytes +=
            usize::try_from(fs::metadata(f).expect("stat").len()).expect("file size fits in usize");
    }

    let mut slowest: Option<(f64, String)> = None;
    let started = Instant::now();
    let mut extracted_defs = 0usize;
    let mut partial = 0usize;
    for f in &files {
        let source = fs::read_to_string(f).expect("read");
        let file_start = Instant::now();
        let extracted =
            extract(&source, Language::Go).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        let took = file_start.elapsed().as_secs_f64();
        if slowest.as_ref().is_none_or(|(s, _)| took > *s) {
            slowest = Some((took, f.display().to_string()));
        }
        extracted_defs += extracted.defs.len();
        if extracted.status == cs_extract::ParseStatus::Partial {
            partial += 1;
        }
    }
    let elapsed = started.elapsed().as_secs_f64();

    let mb = total_bytes as f64 / (1024.0 * 1024.0);
    println!("repo:            {}", root.display());
    println!("files:           {}", files.len());
    println!("size:            {mb:.1} MiB");
    println!("wall:            {elapsed:.2} s (single-threaded)");
    println!(
        "throughput:      {:.2} MiB/s, {:.0} files/s",
        mb / elapsed,
        files.len() as f64 / elapsed
    );
    println!("defs extracted:  {extracted_defs}");
    println!("partial files:   {partial}");
    if let Some((secs, file)) = slowest {
        println!("slowest file:    {secs:.3} s — {file}");
    }

    assert!(
        mb / elapsed > FLOOR_MB_PER_SEC,
        "extraction throughput {mb:.2}/{elapsed:.2} = {:.2} MiB/s is below the {FLOOR_MB_PER_SEC} MiB/s floor",
        mb / elapsed
    );
}
