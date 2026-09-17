//! Scaling and performance benchmark harness for `cs-index` (M7).
//!
//! # Scale Gates (MASTER_PLAN §15 Step 5, ARCHITECTURE §8)
//!
//! 1. **Cold-Index Speed Gate**: 10k synthetic files cold index in < 60 s.
//! 2. **Incremental No-op Gate**: 10k files second run in < 2 s.
//! 3. **Memory RSS Gate**: 50k synthetic files peak RSS < 512 MiB.

#![forbid(unsafe_code)]

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use cs_index::{DiagnosticSeverity, IndexDatabase, DEFAULT_CONFIG_FINGERPRINT};
use cs_scanner::{scan, ScanConfig};

const FILES_PER_PKG: usize = 10;

/// Deterministic import targets: `k` distinct generated packages, never self.
fn import_targets(p: usize, j: usize, num_pkgs: usize, k: usize) -> Vec<usize> {
    let mut out = Vec::with_capacity(k);
    let span = (num_pkgs - 1).max(1);
    let mut i = 0usize;
    while out.len() < k {
        let cand = (p + 1 + (k * 7919 + j * 104_729 + i * 31) % span) % num_pkgs;
        if cand != p && !out.contains(&cand) {
            out.push(cand);
        }
        i += 1;
    }
    out
}

const FIXED: &str = r#"
// Config{G} carries per-file state.
type Config{G} struct {
	Count{G} int
	Label{G} string
	Ready{G} bool
}

// state{G} is the unexported companion.
type state{G} struct {
	Items{G} []string
	Seen{G}  map[string]int
}

// Handler{G} is the exported behavior surface.
type Handler{G} interface {
	Compute{G}(n int) int
	Observe{G}(s string) error
}

// list{G} is a small named slice type.
type list{G} []string

// Shared{G} is a package-level value.
var Shared{G} = Config{G}{Count{G}: 1}

// shared{G} is an unexported package-level value.
var shared{G} = list{G}{"a", "b"}

// Const{G} is an exported constant.
const Const{G} = {G}

// limit{G} is an unexported constant.
const limit{G} = {G} + 17

// Helper{G} doubles.
func Helper{G}(n int) int {
	return n * 2
}

// Build{G} constructs a config value.
func Build{G}(n int) Config{G} {
	return Config{G}{Count{G}: n}
}

// Wrap{G} formats a label.
func Wrap{G}(n int) string {
	return ""
}

// Load{G} warms the shared state.
func Load{G}(n int) *Config{G} {
	return &Shared{G}
}

// Draft{G} sketches a handler.
func Draft{G}(n int) Handler{G} {
	return nil
}

// probe{G} measures a string.
func probe{G}(s string) int {
	return len(s)
}

// helper{G} also measures a string.
func helper{G}(s string) int {
	return len(s) - 1
}

// build{G} constructs a state value.
func build{G}(s string, n int) *state{G} {
	return &state{G}{Items{G}: []string{s}}
}

// wrap{G} formats a number.
func wrap{G}(n int) string {
	return fmt.Sprint(n)
}

// load{G} compares against the limit.
func load{G}(n int) bool {
	return n > limit{G}
}

// Compute{G} reports a count.
func (c *Config{G}) Compute{G}(n int) int {
	return c.Count{G}
}

// Observe{G} accepts a label.
func (c *Config{G}) Observe{G}(s string) error {
	return fmt.Errorf("%s", s)
}

// Apply{G} merges another config.
func (c *Config{G}) Apply{G}(other Config{G}) Config{G} {
	return other
}

// resolve{G} classifies a label.
func (c *Config{G}) resolve{G}() int {
	return probe{G}(c.Label{G})
}

// Blend{G} folds items.
func (s *state{G}) Blend{G}(n int) int {
	return len(s.Items{G})
}

// Trace{G} records a label.
func (s *state{G}) Trace{G}(label string, n int) {
	s.Seen{G} = nil
}

// Merge{G} joins two states.
func (s *state{G}) Merge{G}(other *state{G}) *state{G} {
	return other
}

// Exercise{G} touches this file's reference mix.
func Exercise{G}(seed int) int {
	cfg := Build{G}(seed)
	total := cfg.Compute{G}(seed)
	_ = cfg.Observe{G}("d")
	total += cfg.resolve{G}()
	if load{G}(seed) {
		total += Const{G}
	}
	_ = Config{G}{Count{G}: seed}
	_ = list{G}{"z"}
	_ = fmt.Sprint(total)
"#;

fn gen_source(g: usize, num_pkgs: usize) -> String {
    use std::fmt::Write;
    let p = g / FILES_PER_PKG;
    let j = g % FILES_PER_PKG;
    let k = 2 + g % 3; // 2..4 generated imports, plus fmt
    let targets = import_targets(p, j, num_pkgs, k);

    let mut out = String::with_capacity(8_000);
    let _ = writeln!(out, "package pkg{p:04}");
    out.push_str("\nimport (\n\t\"fmt\"");
    for t in &targets {
        let _ = writeln!(out, "\n\t\"example.com/big/pkg{t:04}\"");
    }
    out.push_str("\n)\n");
    out.push_str(&FIXED.replace("{G}", &g.to_string()));

    for (i, t) in targets.iter().enumerate() {
        let tf = t * FILES_PER_PKG;
        if i == 0 {
            let _ = writeln!(out, "\tvar peer pkg{t:04}.Config{tf}");
            let _ = writeln!(out, "\t_ = peer.Label{tf}");
        } else {
            let _ = writeln!(out, "\t_ = pkg{t:04}.Helper{tf}(seed)");
        }
    }
    let _ = writeln!(out, "\treturn total\n}}");
    out
}

fn generate_repo(dir: &Path, num_files: usize) -> std::io::Result<()> {
    let num_pkgs = (num_files / FILES_PER_PKG).max(1);
    fs::write(dir.join("go.mod"), "module example.com/big\n\ngo 1.24\n")?;
    for g in 0..num_files {
        let p = g / FILES_PER_PKG;
        let pkg_dir = dir.join(format!("pkg{p:04}"));
        if g % FILES_PER_PKG == 0 {
            fs::create_dir_all(&pkg_dir)?;
        }
        let src = gen_source(g, num_pkgs);
        fs::write(pkg_dir.join(format!("file_{g:05}.go")), src)?;
    }
    Ok(())
}

/// Read peak resident set size (`VmHWM`) from `/proc/self/status` in bytes.
fn read_peak_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if line.starts_with("VmHWM:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let kb: u64 = parts[1].parse().ok()?;
                return Some(kb * 1024);
            }
        }
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let is_release = !cfg!(debug_assertions);
    let run_50k_only = args.iter().any(|a| a == "--50k");
    let run_all = args.iter().any(|a| a == "--all");
    let force_full = args.iter().any(|a| a == "--full" || a == "--10k") || is_release;

    let target_files = if force_full { 10_000 } else { 1_000 };

    println!("============================================================");
    println!("ContextSlice `cs-index` Scaling Benchmark (M7)");
    println!("Profile: {}", if is_release { "release" } else { "debug" });
    if run_50k_only {
        println!("Mode: 50k Memory Gate only");
    } else {
        println!("Target cold scale: {target_files} files");
    }
    println!("============================================================");

    if !run_50k_only {
        let temp = tempfile::tempdir().expect("create tempdir");
        let repo_root = temp.path().join("repo");
        fs::create_dir_all(&repo_root).expect("create repo dir");

        println!("\n[1/3] Generating synthetic repository with {target_files} files...");
        let gen_start = Instant::now();
        generate_repo(&repo_root, target_files).expect("generate repo");
        let gen_dur = gen_start.elapsed();
        println!("Generated in {gen_dur:?}");

        // --- GATE 1: Cold Index Speed Gate ---
        println!("\n[2/3] Running Cold Index Speed Gate...");
        let db_path = temp.path().join("index.db");
        let mut db = IndexDatabase::open_or_create(&db_path).expect("open index db");

        let scan_start = Instant::now();
        let scanned = scan(&repo_root, &ScanConfig::default()).expect("scan files");
        let scan_dur = scan_start.elapsed();
        println!("Scanned {} files in {scan_dur:?}", scanned.len());

        let cold_start = Instant::now();
        db.begin_build(DEFAULT_CONFIG_FINGERPRINT)
            .expect("begin build");
        let ingest_stats = db.ingest_facts(&repo_root, &scanned).expect("ingest facts");
        let resolve_stats = db.resolve_facts().expect("resolve facts");
        let cold_dur = cold_start.elapsed();

        println!(
        "Cold index finished in {cold_dur:?} (ingested {} files, resolved {} imports, {} bindings)",
        ingest_stats.files_inserted,
        resolve_stats.resolved,
        resolve_stats.refs_bound
    );

        if target_files == 10_000 {
            assert!(
                cold_dur < Duration::from_secs(60),
                "Gate 1 FAIL: Cold 10k index took {cold_dur:?} which exceeds the 60 s budget!"
            );
            println!(">>> GATE 1 PASS: Cold 10k index took {cold_dur:?} (< 60 s)");
        } else {
            println!(">>> Smoke Cold index ({target_files} files) took {cold_dur:?}");
        }

        // --- GATE 2: Incremental No-op Gate ---
        println!("\n[3/3] Running Incremental No-op Gate...");
        let noop_scan_start = Instant::now();
        let rescanned = scan(&repo_root, &ScanConfig::default()).expect("rescan");
        let noop_scan_dur = noop_scan_start.elapsed();

        let inc_start = Instant::now();
        let inc_stats = db
            .update_incremental(&repo_root, &rescanned)
            .expect("incremental update");
        let inc_dur = inc_start.elapsed();

        println!(
        "Incremental no-op completed in {inc_dur:?} (scan: {noop_scan_dur:?}; unchanged: {}, modified: {})",
        inc_stats.files_unchanged,
        inc_stats.files_modified
    );
        assert_eq!(inc_stats.files_unchanged, scanned.len());
        assert_eq!(inc_stats.files_modified, 0);
        assert_eq!(inc_stats.files_added, 0);
        assert_eq!(inc_stats.files_deleted, 0);

        if target_files == 10_000 {
            assert!(
                inc_dur < Duration::from_secs(2),
                "Gate 2 FAIL: Incremental no-op took {inc_dur:?} which exceeds the 2 s budget!"
            );
            println!(">>> GATE 2 PASS: Incremental no-op took {inc_dur:?} (< 2 s)");
        } else {
            println!(">>> Smoke Incremental no-op took {inc_dur:?}");
        }

        // Verify index health with Doctor
        let diags = db.doctor_check();
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .collect();
        assert!(errors.is_empty(), "Doctor found index errors: {errors:?}");
        println!(">>> Doctor verified index integrity: zero errors");

        if let Some(peak_rss) = read_peak_rss_bytes() {
            let mib = peak_rss as f64 / (1024.0 * 1024.0);
            println!("Peak RSS at {target_files} files: {mib:.2} MiB");
        }
    }

    // --- GATE 3: 50k Memory RSS Gate (optional / on-demand) ---
    if run_50k_only || run_all {
        println!("\n============================================================");
        println!("Running 50k Memory RSS Gate (< 512 MiB)...");
        println!("============================================================");

        let temp_50k = tempfile::tempdir().expect("create tempdir 50k");
        let repo_50k = temp_50k.path().join("repo");
        fs::create_dir_all(&repo_50k).expect("create repo 50k");

        println!("Generating 50k files...");
        generate_repo(&repo_50k, 50_000).expect("generate 50k repo");

        let db_50k_path = temp_50k.path().join("index.db");
        let mut db_50k = IndexDatabase::open_or_create(&db_50k_path).expect("open 50k index");

        let scanned_50k = scan(&repo_50k, &ScanConfig::default()).expect("scan 50k");
        let start_50k = Instant::now();
        db_50k
            .begin_build(DEFAULT_CONFIG_FINGERPRINT)
            .expect("begin 50k");
        db_50k
            .ingest_facts(&repo_50k, &scanned_50k)
            .expect("ingest 50k");
        db_50k.resolve_facts().expect("resolve 50k");
        let dur_50k = start_50k.elapsed();

        if let Some(peak_rss) = read_peak_rss_bytes() {
            let mib = peak_rss as f64 / (1024.0 * 1024.0);
            println!("Cold 50k index finished in {dur_50k:?}; Peak RSS: {mib:.2} MiB");
            let max_allowed_bytes = 512 * 1024 * 1024;
            assert!(
                peak_rss < max_allowed_bytes,
                "Gate 3 FAIL: Peak RSS {mib:.2} MiB exceeded 512 MiB budget!"
            );
            println!(">>> GATE 3 PASS: Peak RSS {mib:.2} MiB (< 512 MiB)");
        }
    }

    println!("\nAll scaling benchmarks and assertions completed successfully!");
}
