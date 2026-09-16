# Resolver scaling at synthetic GIN-density repositories

The audit made two unverified scaling claims against the resolver: "~100 s at
10k files" (attributed to O(N²) package lookups, since fixed by the
directory→packages `BTreeMap` index in `cs-resolve/src/go.rs`) and "2.5–4.2 GB
RSS at 50k files". Both were impressions, not measurements. This report
replaces them with measured numbers, taken to ground the cs-index design
(ARCHITECTURE §8: cold 10k < 60 s, RSS < 1 GB @50k).

Method: a scratch harness (kept outside the repository, under `/tmp/scaling`)
generates synthetic Go repositories at GIN density — the def/ref mix a real
framework repository carries per file — and drives the real library APIs
(`cs_extract::extract`, `cs_resolve::ResolveSnapshot::new`,
`GoResolver::prepare`, `GoResolver::resolve`) in release mode. Nothing in the
repository was modified for this run; only the numbers below are committed.
Runs of 2026-09-16, rustc 1.96.0, release profile (`lto = "thin"`,
`codegen-units = 1`), single-threaded pipeline, on an AMD Ryzen 5 5600X
(6 cores / 12 threads), 31.3 GiB RAM, Linux 7.1.9-artix1-2.

## Methodology

**Generator shape.** `N` files in `N/10` packages of 10 files each, one
module (`example.com/big`, single root `go.mod`). Every file declares
`package pkgNNNN` and carries:

- 26 definitions: 1 exported struct + 1 unexported struct + 1 interface +
  1 named slice type, 10 package-level functions (5 exported, 5 unexported,
  including the file's `Exercise` driver), 7 methods (pointer receivers on
  both structs), 2 package-level vars (1 exported), 2 consts (1 exported).
- ~61 references in a deliberate mix: plain calls of package-level functions,
  method calls on local variables, bare field accesses, own-package type
  usages (composite literals), qualified calls `pkgNNNN.HelperM(seed)` and a
  qualified type `var peer pkgNNNN.ConfigM` against 2–4 imported generated
  packages, and `fmt` calls. Cross-package refs target the first file of the
  imported package, so every import resolves and every qualified ref has a
  real target.
- 3–5 imports per file (2–4 generated packages + `fmt`), chosen
  deterministically, never self.

Every file must extract with `ParseStatus::Ok` — the harness panics on
`Partial`, and the generator was tuned until all runs were clean
(50,000/50,000 `ok` at the largest scale). Density is verified from the
harness output, not asserted; the actual per-file counts (identical at every
scale because files are parameterized only by a global index):

| Measure | Per file | Mix (per-file averages) |
|---|---|---|
| Definitions | **26.0** (min = max = 26) | 11 func, 7 method, 2 struct, 2 const, 2 var, 1 interface, 1 type |
| References | **61.0** (min 59, max 63) | 26 name_ref, 22 type_ref, 8 call_ref, 5 field_ref |
| Imports | 4.0 average (3–5) | fmt + 2–4 generated packages |
| Source size | ≈ 2.9 KiB | |

(The earlier small-density measurement pinned only an average file *size*
(~17 KB/file) and no facts density; the counts above are the density the
numbers below actually exercise. Note the local identifiers — `total`,
`seed`, `cfg` — count as `name_ref`s, exactly as in real code; gin's census
showed locals are the dominant reference class there too. At ~2.9 KiB and
87 facts per file, these synthetic files are smaller but denser than the
17 KB/file average, which keeps the *per-file* cost conservative and the
*per-byte* cost representative.)

**Why /tmp harness + committed numbers.** The generator and the stage driver
are measurement scaffolding, not product code; keeping them out of the
repository avoids a synthetic-corpus asset and a second workspace in-tree
while the numbers themselves (and the full generator source, embedded below)
are committed for reproducibility.

**Staged RSS.** `VmHWM` (peak RSS, from `/proc/self/status`) is a cumulative
high-water mark, so each stage endpoint was measured in its own process:
`snapshot` = generate + extract + `ResolveSnapshot::new`; `prepare` = the
same plus `GoResolver::prepare`; `resolve` = the same plus
`GoResolver::resolve`. Sources are streamed one file at a time (generate →
extract → drop), so the peak carries at most one ~3 kB source string beyond
the extracted snapshot — the same shape production streaming would use.

## Results

Wall times below are from the `resolve`-stage process of each row (all three
stages timed in one run); the extract column re-measured across the three
stage processes varied by <5% (e.g. 9,255 / 9,203 / 9,117 ms at 10k), so
single-run wall times carry roughly that uncertainty.

| Files | Snapshot RSS | Prepare RSS | Resolve RSS (peak) | Extract ms | Prepare ms | Resolve ms | Edges | Refs bound |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1,000 | 23.6 MiB | 42.5 MiB | 71.2 MiB | 945.6 | 15.5 | 35.9 | 32,989 | 32,999 / 60,998 (54.1%) |
| 10,000 | 200.9 MiB | 390.9 MiB | 678.5 MiB | 9,116.6 | 163.3 | 436.8 | 329,989 | 329,999 / 609,998 (54.1%) |
| 50,000 | 991.3 MiB | 1.90 GiB | **3.30 GiB** | 47,173.4 | 825.4 | 2,382.6 | 1,649,989 | 1,649,999 / 3,049,998 (54.1%) |

Every column scales linearly in file count (peak ≈ 69 kB/file; extract
≈ 0.92 ms/file ≈ 3.2 MB of source per second single-threaded; edges exactly
10× per 10× files). Import resolution: 100% in-repo success at every scale
(3,000 / 30,000 / 150,000 resolved; the `fmt` imports correctly `External`;
zero unresolved). Edge composition at 50k: 1,499,990 `import_out`, 149,999
`ref_def`, 0 `test_affinity` (the generator makes no `_test.go` files).
Unbound refs carry documented reasons: ≈1.0M `no_candidate` (locals), 250k
`needs_type_info` (bare field accesses), 150k `external_scope` (`fmt.`
selectors).

**Cold 10k total.** Extract + prepare + resolve = **9.72 s** at 10k files
(1k: 1.0 s; 50k: 50.4 s). Against the ARCHITECTURE §8 budget of "cold 10k
< 60 s" this leaves ~6× headroom — and this measurement excludes the
directory scan, Git signals and SQLite persistence that the real budget also
covers, but it is also single-threaded where the budget assumes rayon-parallel
parsing.

## 50k RSS verdict

**Exceeded.** Peak RSS at 50k files is **3.30 GiB** against the ARCHITECTURE
§8 target of "< 1 GB RSS @50k" — 3.3× over. The breach is structural, not
noise: the extracted snapshot alone reaches 991 MiB (≈ the whole budget), the
package/def index adds ~0.95 GiB (≈2 `DefLoc` clones per definition on
average: the by-name `defs` map plus the `exported`/`methods` maps), and the
materialized `ResolvedRepo` — which
clones every reference plus per-binding `DefLoc` targets — adds another
~1.40 GiB. At ≈ 69 kB of peak per file, the current whole-repo-in-memory
pipeline stays under 1 GB only up to roughly **15k files**. For calibration
against the audit: the "2.5–4.2 GB at 50k" RSS claim is the right order of
magnitude for this shape, but it is the *in-memory pipeline* footprint, not
the resolver index alone; and the "~100 s at 10k" time claim is refuted —
prepare + resolve together measure 0.60 s at 10k on the post-fix
(`package_dirs`-indexed) resolver.

## Parse headroom at the 1 MiB cap

The cap test generates a single valid Go file padded with small functions to
1,040,420 bytes (just under `PARSE_SIZE_CAP`), 8,864 definitions, and times
`cs_extract::parse_source` (parse only, under `PARSE_TIMEOUT_MS`) and
`cs_extract::extract` (parse + query extraction) separately; three runs each,
after a warm-up parse:

| File size | Defs | Parse ms | Extract ms |
|---:|---:|---:|---:|
| 254,068 B | 2,200 | 16.3 – 22.5 | 781 – 816 |
| 516,146 B | 4,421 | 32.4 – 36.0 | 2,976 – 2,981 |
| 1,040,420 B (at cap) | 8,864 | **66.3 – 74.1** | **11,336 – 11,687** |

Two findings:

1. **Parse headroom has a ~3–4× margin.** The tree-sitter parse
   of a max-cap file takes ~66–81 ms across all runs against the 250 ms
   `PARSE_TIMEOUT_MS` budget. The doc assumption "a legitimate file at the cap
   finishes orders of magnitude under it" holds for the parse.
2. **Extraction at the cap busts the budget 46×, and the timeout cannot see
   it.** Full extraction of the same file takes ~11.5–11.7 s — and still
   reports `ParseStatus::Ok`, because the budget's progress callback guards
   only the parse, not the query-capture/def-building phase that follows.
   That phase also grows **quadratically** in def count (2× defs → ~3.8–3.9×
   time across the sweep above). One machine-generated or bundled 1 MiB Go
   file costs more extraction time than the *entire* 10k-file pipeline
   (9.7 s). The budget seam needs to cover extraction, and the quadratic
   per-def work in `cs-extract` needs a fix, before any "1 MiB worst case is
   fine" claim is true.

## Caveats

- **Synthetic ≠ real.** Uniform 10-file packages, one module, no `_test.go`
  (hence zero `test_affinity` edges), no `vendor/`, no nested modules, no dot
  or blank imports, no build-tag duplicate definitions, no generics. Names
  are globally unique, so unique-method binding always succeeds — real
  repositories also hit `method_ambiguous` and `universe_method` (gin/chi
  audit), which would lower the binding rate, and real import graphs are
  skewed, not the near-regular fan-out measured here. The *per-byte cost
  model* (≈ 69 kB peak/file, ≈ 0.92 ms extract/file) should transfer; the
  binding-rate mix should not be quoted for real repos.
- **RSS is a high-water mark per process.** Per-stage numbers come from
  separate processes at stage endpoints; `VmHWM` ≥ live bytes at the
  checkpoint (the allocator does not necessarily return freed memory), so
  per-stage "deltas" inferred across processes are approximate.
- **No parallelism.** The library APIs are single-threaded and were driven
  single-threaded; no rayon, no batching overlap. The 60 s cold budget
  assumes parallel parsing, so the 10k total here is conservative on time.
- **Single runs per cell** (three process repeats for extract variance, <5%);
  no pinning of CPU frequency; other workloads on the machine were possible.
- 50k was measured directly: generation + extraction took ~47 s, well under
  the 10-minute cutoff that would have forced a 40k substitute.

## Implication for cs-index

The measurements draw a sharp boundary for the indexer. At 10k files the
in-memory pipeline is comfortable — 9.7 s cold and 678 MiB peak, both well
inside budget — but peak RSS crosses the 1 GB target at roughly 15k files if
cs-index ever materializes the whole snapshot + `ResolvedRepo` in memory, and
at 50k it is 3.3× over. Indexing at 50k+ therefore cannot be a
"build-then-persist" pipeline: it must stream — extract and resolve
package-by-package (or batch), write symbols/edges into SQLite incrementally,
and drop in-memory results, keeping only the resolver's prepared index alive
across batches — which in turn wants a resolver API that emits a file's
import/edge results without materializing per-file `FileResolution` clones of
every ref. Two cheaper secondary fixes fall out of the same data: the
extraction cost of a single max-cap file (11.6 s, quadratic in def count,
outside the timeout's coverage) is large enough to starve the 60 s budget on
a handful of adversarial or machine-generated files, so the per-file budget
must extend over extraction and the O(defs²) work in `cs-extract` needs to
become near-linear; and the resolver index itself (~19 kB/file from ~2
`DefLoc` clones per definition) is worth slimming before 100k-file repos are
attempted.

## Reproducing: the generator

The harness is a single-binary cargo project at `/tmp/scaling` (path deps on
`cs-extract`, `cs-resolve`, `cs-scanner`; `[workspace]` isolation; release
profile mirroring the repo's). `scaling <N> snapshot|prepare|resolve` runs
one stage endpoint and prints density, per-stage wall times and `VmHWM`;
`scaling headroom` runs the cap sweep. The generator — the part that
determines the corpus — is embedded verbatim below (`{G}` is replaced by the
global file index; `FILES_PER_PKG = 10`; manifest
`("go.mod", "module example.com/big\n\ngo 1.24\n")`; paths
`pkgNNNN/file_GGGGG.go`):

```rust
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

/// The fixed (per-file, name-parameterized) part of a generated source file.
/// Ends mid-function: the exercise body's tail (qualified cross-package
/// calls) is appended dynamically by `gen_source`.
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
    let p = g / FILES_PER_PKG;
    let j = g % FILES_PER_PKG;
    let k = 2 + g % 3; // 2..4 generated imports, plus fmt
    let targets = import_targets(p, j, num_pkgs, k);

    let mut out = String::with_capacity(8_000);
    let _ = writeln!(out, "package pkg{p:04}");
    out.push_str("\nimport (\n\t\"fmt\"");
    for t in &targets {
        let _ = writeln!(out, "\n\n\t\"example.com/big/pkg{t:04}\"");
    }
    out.push_str("\n)\n");
    out.push_str(&FIXED.replace("{G}", &g.to_string()));

    // Cross-package qualified refs against the target package's first file
    // (global index t*FILES_PER_PKG), whose exported API is guaranteed to exist.
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
```

The stage driver generates each file, immediately extracts it (panicking on
any status other than `Ok`), accumulates `(path, ExtractedFile)` pairs, then
constructs the snapshot and, per stage, `GoResolver::prepare(&snapshot)` /
`resolver.resolve(&snapshot)` (with `cs_resolve::LanguageResolver` in
scope), timing each phase with `Instant` and reading `VmHWM` from
`/proc/self/status` at the stage endpoint.
