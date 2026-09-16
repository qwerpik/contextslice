# ContextSlice Independent Adversarial Code Review & Audit

| | |
|---|---|
| **Auditor** | Independent Adversarial Review Team (Gemini 3.8 Flash High) |
| **Commit Target** | `3d178c0` (HEAD of `master`) |
| **Date** | 2026-09-15 |
| **Status** | Complete & Frozen — Read-Only Audit |
| **Scope** | Complete workspace (`crates/`, `docs/`, `fixtures/`, `.github/`, benchmarks, ADRs 001–018) |

---

## 1. Executive Summary

ContextSlice aims to build a **local-first, deterministic context-selection engine for AI coding agents**. At commit `3d178c0`, the project has completed Phase 0 (workspace bootstrap) and Phase 1 steps 3 and 4 (Go extraction and Go resolution). The codebase comprises three functioning pipeline crates (`cs-scanner`, `cs-extract`, `cs-resolve`), a CLI harness skeleton (`cs-cli`), and six future stage crates (`cs-index`, `cs-git`, `cs-select`, `cs-render`, `cs-mcp`, `cs-bench`).

This independent, multi-agent adversarial audit evaluated the repository from first principles without trusting claims in previous agent progress logs. Across six parallel review teams (Scanner, Extraction, Resolver, Architecture/CLI, Testing/Benchmark, Security/Performance), we identified **7 Critical (P0)** issues, **19 High-Severity (P1)** issues, **24 Medium-Severity (P2)** issues, and **17 Low-Severity / Hygiene (P3)** issues.

### Key Audit Discoveries
1. **Critical Secret & Privacy Leakage (P0 in `cs-scanner`):** Setting `.hidden(false)` in `ignore::WalkBuilder` causes the scanner to walk the entire `.git/` directory. On this repository, **332 of 474 scanned files (70%)** are internal `.git/` files, exposing `.git/config` (tokens, credentials), reflogs, commit messages, and loose objects.
2. **Silent Failure of the Universe Predeclared Identifier Filter (P0 in `cs-extract`):** `UNIVERSE_NAMES` is an unsorted slice upon which `.binary_search()` is called. 25 of 46 entries fail binary search. Every occurrence of Go's builtins (`len`, `make`, `append`, `panic`, `close`, `new`, etc.) leaks into the references table, corrupting goldens and deflating resolution metrics across every Go codebase. Furthermore, Rust types `f32` and `f64` were erroneously included.
3. **Catastrophic Short-Variable Declaration RHS Reference Dropping (P0 in `cs-extract`):** `collect_left_side_identifiers` visits all `expression_list` children of `short_var_declaration` without stopping after the left side. As a result, bare variable references on the RHS of `:=` (`x := y`, `a, b := c, d`) are marked as declarations and silently dropped from references.
4. **Fragile Byte-Distance Heuristic for Package Qualification (P0 in `cs-resolve`):** `preceding_qualifier` infers `pkg.Symbol` by checking if the byte distance between successive references is $\le 2$ without verifying a `.` separator or AST parentage. Any comma-separated argument/parameter list like `(auth, Session)` is falsely bound as `auth.Session`.
5. **False Benchmark Claim on `t.Run` and Universe Methods (P1 in `cs-resolve`):** `docs/benchmarks/resolver-gin-chi.md#L56` claims that `t.Run` false-positive bindings were eliminated by the universe-method filter. But `Run` was never added to `UNIVERSE_METHOD_NAMES`! In Gin, every `t.Run` in test files binds to `Engine.Run`.
6. **Unimplemented Security Safeguards (P0 in `cs-extract`):** The parse timeout (250 ms) and node-count cap promised in `docs/SECURITY.md` §6 and `docs/ARCHITECTURE.md` §4.2 do not exist in code; `PARSE_TIMEOUT_MS` is an unused constant.
7. **Major-Version External Import Failure (P1 in `cs-resolve`):** Imports like `github.com/go-chi/chi/v5` extract `"v5"` as the qualifier instead of `"chi"`, causing external calls like `chi.NewRouter()` to fail qualifier lookup and become bare method calls on the local package.
8. **Algorithmic $O(N^2)$ Bottlenecks and Monolithic Memory Exceeding 1 GB RSS (P1 in `cs-resolve`):** `non_test_package` performs linear scans over all repository packages on every import, creating an $O(N^2)$ bottleneck that turns 10 ms into ~100 s on a 10k-file repo. Furthermore, `ResolveSnapshot` holds all AST facts in memory, requiring 2.5–4.2 GB RAM on 50k repositories.

Despite these findings, the core design principles—deterministic sorting, zero-network enforcement, clean downward crate boundaries, and an honest approximation philosophy—are structurally sound. The issues identified are concrete implementation and algorithmic bugs that must be corrected before progressing to `cs-index` (Phase 1, Step 5).

---

## 2. P0 Findings (Correctness, Security, Data Loss, Crash)

### F-01: Scanner Traversal Leaks Entire `.git/` Directory, Exposing Secrets and Git Internals
- **Severity:** P0
- **File:** `crates/cs-scanner/src/lib.rs:196-203`
- **Function:** `discover()`
- **What is wrong:**  
  `discover()` configures `ignore::WalkBuilder::new(root).hidden(false)` to include dotfiles (e.g. `.github/`, `.env.example`). In the `ignore` crate, skipping `.git/` is implemented through hidden file filtering (git does not put `.git` in `.gitignore`). Because `hidden(false)` disables hidden file skipping, the walker traverses the entire `.git/` directory.
- **Why it matters:**  
  1. **Secret & Credential Exposure:** `.git/config` frequently stores personal access tokens, HTTP Basic Auth credentials, or private repository URLs. Commit edit messages (`.git/COMMIT_EDITMSG`) and reflogs can store sensitive data.
  2. **Security Specification Violation:** Violates `docs/SECURITY.md` §3 ("Logs never contain file contents"), §7 ("The index contains no source text, no file contents, no secrets beyond masked signatures"), and §8 ("no credential access (we never touch remotes — local objects only)").
  3. **Index Pollution:** In ContextSlice itself, **332 out of 474 scanned files (70%)** were internal `.git` files!
- **Concrete Evidence:**  
  Calling `cs_scanner::scan(Path::new("."), &ScanConfig::default())` returns `.git/config`, `.git/HEAD`, `.git/COMMIT_EDITMSG`, and hundreds of loose object files.
- **Confidence:** 100% (Empirically verified).
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Add an explicit filter entry on `WalkBuilder`:
  ```rust
  builder.filter_entry(|entry| {
      let name = entry.file_name();
      name != ".git" && name != ".contextslice"
  });
  ```

---

### F-02: Unsorted `UNIVERSE_NAMES` Causes `binary_search` to Fail for 25 Predeclared Identifiers; Rust Types `f32`/`f64` Present
- **Severity:** P0
- **File:** `crates/cs-extract/src/go/mod.rs:89-144`
- **Function:** `is_universe()`, `const UNIVERSE_NAMES`
- **What is wrong:**  
  `is_universe` performs `UNIVERSE_NAMES.binary_search(&name).is_ok()`. However, `UNIVERSE_NAMES` is partitioned into three independently sorted blocks ("types", "builtins", "constants") concatenated together:
  - "types" ends with `"uintptr"` (line 114)
  - "builtins" begins with `"append"` (line 116). But `"append" < "uintptr"`!
  - "constants" has `["nil", "true", "false", "iota"]` which is not even sorted internally (`"true" > "false"`).
  Because `binary_search` requires a strictly sorted slice, calling it on this slice fails for **25 of the 46 entries**:
  `complex64`, `int8`, `uint8`, `append`, `cap`, `clear`, `close`, `complex`, `copy`, `delete`, `imag`, `len`, `make`, `max`, `min`, `new`, `panic`, `print`, `println`, `real`, `recover`, `nil`, `true`, `false`, `iota`.
  Additionally, the list includes `"f32"` and `"f64"` (lines 98-99), which are Rust types, not Go types (Go has `float32` and `float64`).
- **Why it matters:**  
  Violates ADR-017 §8 and `LANGUAGES.md` §6.1. Every call to Go builtins (`make`, `len`, `append`, `close`, `panic`, etc.) leaks into the references table. In `fixtures/go/golden/12_generics.go.json`, `make` (line 137), `len` (line 159), and `append` (line 203) leaked into the golden. In real repos (Gin and Chi), thousands of builtin calls leaked and were recorded as `no_candidate` in `docs/benchmarks/resolver-gin-chi.md` (Gin: 14,492 / Chi: 7,941).
- **Concrete Evidence:**  
  `UNIVERSE_NAMES.binary_search(&"len").is_ok()` returns `false`.  
  `UNIVERSE_NAMES.binary_search(&"make").is_ok()` returns `false`.  
  `UNIVERSE_NAMES.binary_search(&"append").is_ok()` returns `false`.
- **Confidence:** 100% (Empirically verified).
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Sort `UNIVERSE_NAMES` alphabetically into a single slice, remove `f32` and `f64`, and add a unit test asserting `UNIVERSE_NAMES.windows(2).all(|w| w[0] < w[1])`.

---

### F-03: `collect_left_side_identifiers` Collects the RHS of `:=`, Dropping Valid References
- **Severity:** P0
- **File:** `crates/cs-extract/src/go/mod.rs:717-726, 783`
- **Function:** `collect_left_side_identifiers()`
- **What is wrong:**  
  In tree-sitter-go, `short_var_declaration` has two named `expression_list` children: `left` and `right`. `collect_left_side_identifiers` loops over all children of kind `expression_list` without stopping after the first child. It visits BOTH `left` and `right`. Any bare identifier on the RHS (`x := y`, `a, b := c, d`, `for i := start; ...`) is added into `declaration_spans`. In `collect_refs`, any identifier whose span matches `declaration_spans` is skipped.
- **Why it matters:**  
  Causes silent false-negative reference loss across all Go files. Any short variable declaration assigning from a variable reference (`val := existingVar`) completely drops `existingVar` from references.
- **Concrete Evidence:**  
  For `val := target`, CST has `left: (expression_list (identifier "val"))` and `right: (expression_list (identifier "target"))`. Both `val` and `target` spans are added to `declaration_spans`. `target` is filtered out in `collect_refs`.
- **Confidence:** 100% (Empirically verified).
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Inspect only child `left`:
  ```rust
  fn collect_left_side_identifiers(node: Node, spans: &mut HashSet<(usize, usize)>) {
      if let Some(left) = node.child_by_field_name("left") {
          collect_direct_identifiers(left, spans);
      }
  }
  ```

---

### F-04: Fragile 2-Byte Distance Adjacency Heuristic in `preceding_qualifier` Causes False Package Qualification and Drops Shadowed Variables
- **Severity:** P0
- **File:** `crates/cs-resolve/src/go.rs:621-638, 394-398`
- **Function:** `preceding_qualifier()`, `resolve_file()`
- **What is wrong:**  
  To determine whether reference $i$ is qualified by reference $i-1$ (as in `pkg.Foo`), `cs-resolve` checks:
  ```rust
  let adjacent = prev.kind == RefKind::NameRef
      && refs[i].span.start_byte >= prev.span.end_byte
      && refs[i].span.start_byte - prev.span.end_byte <= 2;
  ```
  It does NOT check if the intermediate byte is a dot `.`! Furthermore, line 395 unconditionally drops any reference whose name matches an imported package qualifier.
  If code contains a variable with the same name as an imported package, followed by a comma and space (2 bytes) and another identifier (e.g. `func(auth, Session)` or `var auth, Login string`):
  1. `auth` is dropped from refs.
  2. `Session` is measured as `start_byte - prev.end_byte == 2 <= 2`.
  3. `preceding_qualifier` returns `Some(auth)`.
  4. `Session` is falsely resolved as `auth.Session`!
  Conversely, formatted selectors like `auth . Login` or `auth.  Login` have distance > 2, failing qualification and becoming bare calls in the local package!
- **Why it matters:**  
  Corrupts the reference graph. False positive cross-package edges are created for common parameter/variable names, while formatted selectors fail to bind.
- **Concrete Evidence:**  
  In a file importing `auth`, `func check(auth int, Session int) {}`: `Session` starts 2 bytes after `auth` ends. `preceding_qualifier` returns `Some(Package("auth"))`, binding parameter `Session` to `auth/session.go`.
- **Confidence:** 100% (Empirically verified).
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Do not reconstruct AST parentage via raw byte distance in `cs-resolve`. `cs-extract` already inspects `selector_expression` and `qualified_type`; it should record the qualifier directly on the `Ref` struct (`pub qualifier: Option<String>`).

---

### F-05: Tree-sitter Parse Timeout & Node Budget Are Unimplemented (Denial-of-Service)
- **Severity:** P0
- **File:** `crates/cs-extract/src/lib.rs:31-36, 334-356`
- **Symbol:** `PARSE_TIMEOUT_MS`, `parse_source()`
- **What is wrong:**  
  `docs/SECURITY.md` §6 promises: *"Per-file parse timeout (250 ms) and node budget prevent pathological-grammar DoS; files > 1 MiB are not parsed at all (ARCHITECTURE §4.2)."*
  In code, `cs-extract/src/lib.rs:36` declares `pub const PARSE_TIMEOUT_MS: u64 = 250;` with a comment claiming tree-sitter exposes no timeout. `PARSE_TIMEOUT_MS` is never used. No timeout or watchdog is implemented, and no node budget check exists. Furthermore, `tree-sitter 0.27.0` does provide `Parser::parse_with_options` with `ParseOptions { progress_callback }` returning `ControlFlow::Break(())` to abort parsing cooperatively.
- **Why it matters:**  
  Tree-sitter grammars can exhibit exponential backtracking or extreme memory usage on adversarial inputs within the 1 MiB limit. A single malicious 900 KB file can hang the process indefinitely.
- **Concrete Evidence:**  
  `git grep PARSE_TIMEOUT_MS` matches only line 36 of `crates/cs-extract/src/lib.rs`. Line 349 calls `parser.parse(source, None)` unconditionally.
- **Confidence:** 100%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Use `parser.parse_with_options` with a progress callback checking elapsed `Instant` against `Duration::from_millis(PARSE_TIMEOUT_MS)` and node evaluation counts.

---

### F-06: CLI Contract Breakage on Documented MCP Flag & Primary Command
- **Severity:** P0
- **File:** `crates/cs-cli/src/main.rs:226-238, 408-424`
- **Symbol:** `Command::Mcp`, `main()`
- **What is wrong:**  
  1. `MASTER_PLAN.md` §6.1 specifies `contextslice mcp [--stdio]`. In `main.rs`, `stdio` is configured with `action = clap::ArgAction::Set`. Running `contextslice mcp --stdio` fails with Clap Usage Error 2: `error: a value is required for '--stdio <STDIO>' but none was supplied`.
  2. `contextslice "fix the auth timeout bug"` (the primary user workflow across all docs) fails with Clap Usage Error 2 because Clap requires an explicit subcommand (`slice`).
- **Why it matters:**  
  The two primary CLI entry points documented in README and MASTER_PLAN fail immediately upon invocation.
- **Concrete Evidence:**  
  Running `target/debug/contextslice mcp --stdio` returns exit code 2 with `error: a value is required for '--stdio <STDIO>'`.
- **Confidence:** 100% (Empirically verified).
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Configure `--stdio` as a flag with `num_args(0..=1)`, and route default positional arguments to the `slice` subcommand.

---

### F-07: Pipeline Poisoning: "stdout is sacred" Broken on Failure
- **Severity:** P0
- **File:** `crates/cs-cli/src/main.rs:353-379`
- **Function:** `run_slice()`
- **What is wrong:**  
  In `run_slice`, placeholder output is written to stdout or `--out` via `write_artifact` *before* returning `Err(CliError::NotImplemented)`. When the command fails with exit code 4, stdout has already received dummy placeholder content.
- **Why it matters:**  
  Violates `MASTER_PLAN.md` §6.1 ("stdout is sacred... only the slice artifact goes to stdout"). Piped consumers (`contextslice ... | pbcopy / agent`) receive corrupt placeholder content instead of clean empty stdout on error.
- **Concrete Evidence:**  
  `contextslice slice "task"` writes `<!-- contextslice bootstrap ... -->` to stdout while exiting with code 4.
- **Confidence:** 100% (Empirically verified).
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Never write to stdout or target file until the entire slice plan has been successfully constructed and rendered.

---

## 3. P1 Findings (Likely Real Bug or Major Architectural Flaw)

### F-08: Missing `Run` in `UNIVERSE_METHOD_NAMES` Contradicts Published Benchmark Claim and Causes False `t.Run` Bindings
- **Severity:** P1
- **File:** `crates/cs-resolve/src/go.rs:785-805`
- **Symbol:** `UNIVERSE_METHOD_NAMES`
- **What is wrong:**  
  `docs/benchmarks/resolver-gin-chi.md#L56` explicitly claims:  
  > *"Bare calls on external receivers with stdlib method names (14/312 at the second pass: t.Run, ts.Close, next.ServeHTTP, ctx.Value) — fixed by the universe-method filter: those names never bind bare calls."*  
  However, `Run` is **not** in `UNIVERSE_METHOD_NAMES`.
- **Why it matters:**  
  In Gin, `Engine.Run` (`gin.go:540`) is the only method named `Run`. In all test files, `t.Run(...)` is extracted as a bare `CallRef`. Because `Run` is not filtered, every single `t.Run` call binds to `Engine.Run`, generating false-positive `RefDef` edges from test files to `gin.go`.
- **Concrete Evidence:**  
  Running the resolver on Gin shows `t.Run` in `context_test.go:83` binding to `Engine.Run` in `gin.go:540`.
- **Confidence:** 100% (Empirically verified).
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Add `"Run"` to `UNIVERSE_METHOD_NAMES` in alphabetical order between `"RoundTrip"` and `"Scan"`.

---

### F-09: External Major-Version Imports (`/v2`, `/v3`, `/v5`) Extract Suffix as Qualifier, Breaking External Selector Binding
- **Severity:** P1
- **File:** `crates/cs-resolve/src/go.rs:353-358`
- **Function:** `qualifiers_for_file()`
- **What is wrong:**  
  For external imports (`Resolution::External`), line 357 falls back to `last_segment(&import.raw)`. For `import "github.com/go-chi/chi/v5"`, the qualifier is recorded as `"v5"`. In Go, code writes `chi.NewRouter()`, not `v5.NewRouter()`.
- **Why it matters:**  
  1. `"chi"` is missing from the qualifier table, so `chi` is treated as an unbound `NameRef` (`NoCandidate`).  
  2. `NewRouter()` has `prev_qualifier == None` and is resolved as a bare method call on the file's *own* package!  
  3. `UnboundReason::ExternalScope` is never emitted for these calls.
- **Concrete Evidence:**  
  Any Go file importing a v2+ package without an explicit alias fails to bind its selectors.
- **Confidence:** 100%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  If `last_segment` matches `v[0-9]+`, fall back to the preceding path segment as the default qualifier name.

---

### F-10: `parse_module_path` Breaks on Inline Comments and Tabs in `go.mod`
- **Severity:** P1
- **File:** `crates/cs-resolve/src/go.rs:111-122`
- **Function:** `parse_module_path()`
- **What is wrong:**  
  `line.strip_prefix("module ")` fails if tabs separate the keyword (`module\texample.com/m`), and fails to strip trailing comments (`module example.com/m // comment`).
- **Why it matters:**  
  If `go.mod` contains an inline comment, `own_module.path` becomes `"example.com/m // comment"`. All in-repo imports fail to match `strip_prefix(&prefix)` and are categorized as `External` or `NotFound`, breaking 100% of in-repo imports.
- **Concrete Evidence:**  
  `parse_module_path("module example.com/m // comment")` returns `Some("example.com/m // comment")`.
- **Confidence:** 100% (Empirically verified).
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Strip `//` comments before parsing, and split tokens using `split_whitespace()`.

---

### F-11: Chained Selectors on External/Returned Objects Falsely Bind to Local Package Methods
- **Severity:** P1
- **File:** `crates/cs-resolve/src/go.rs:621-638, 540-555`
- **Function:** `preceding_qualifier()`, `bind_one_ref()`
- **What is wrong:**  
  In chained calls like `w.Header().Add("Allow", ...)`, `Header()` is a `CallRef`. Line 630 requires `prev.kind == RefKind::NameRef`, so `preceding_qualifier` returns `None`. The resolver treats `Add` as an unqualified bare method call on the local package.
- **Why it matters:**  
  In Chi, `mux.go:535`: `w.Header().Add("Allow", reverseMethodMap[m])` bound to `context.go:RouteParams.Add`! A call to stdlib `http.Header.Add` was bound to an internal route parameter method.
- **Concrete Evidence:**  
  Verified in `tests/real_repo.rs` audit output on Chi.
- **Confidence:** 100% (Empirically verified).
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Add `"Add"`, `"Set"`, `"Get"`, `"Write"` to `UNIVERSE_METHOD_NAMES`, or do not treat chained selector calls as bare method calls when preceded by another selector or call.

---

### F-12: `package main` Files in Library Directories Hijack `non_test_package`
- **Severity:** P1
- **File:** `crates/cs-resolve/src/go.rs:655-663`
- **Function:** `non_test_package()`
- **What is wrong:**  
  `non_test_package` filters `_test` and `<<no-package>>`, then takes `.min()`. It does not filter out `"main"`. If a library directory contains an example or generator with `package main`, and the library package name is alphabetically after `"main"` (e.g. `router`, `server`, `util`), `.min()` selects `"main"`.
- **Why it matters:**  
  In Go, `package main` can never be imported. When `non_test_package` selects `"main"`, external importers bind to the `main` package file instead of the library files, causing all library references to fail.
- **Concrete Evidence:**  
  A directory with `package util` and `package main` selects `("dir", "main")` because `"main" < "util"`.
- **Confidence:** 100%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Filter out `name != "main"` in `non_test_package` when other candidate packages exist in the directory.

---

### F-13: Resolver Package Lookup `non_test_package` and Test Affinity Yield $O(N^2)$ Complexity
- **Severity:** P1
- **File:** `crates/cs-resolve/src/go.rs:655-663, 427-438`
- **Function:** `non_test_package()`, `resolve_file()`
- **What is wrong:**  
  `non_test_package(dir)` executes a linear scan over all packages in the entire repository:
  ```rust
  self.packages.keys().filter(|(d, name)| d == dir && ...).map(...).min()
  ```
  This is invoked up to 4 times per import. For $N$ files with $I$ imports in a repo of $P$ packages ($P \propto N$), this is $O(N \cdot I \cdot P) = O(N^2)$. Furthermore, lines 427-438 iterate over all packages in the repo for every test file.
- **Why it matters:**  
  On gin (99 files), $N^2$ takes ~10 ms. On a 10,000-file repo with 2,000 packages ($100\times$ files), iteration count grows by $10,000\times$, scaling resolution time to **~100 seconds**, failing the `< 1 s @ 10k files` performance budget.
- **Concrete Evidence:**  
  10k files $\times$ 10 imports $\times$ 4 lookups $\times$ 2,000 packages = **800,000,000 iterations** in `non_test_package`.
- **Confidence:** 100%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Index packages by directory (`dir_packages: BTreeMap<String, Vec<String>>`) during `prepare()`.

---

### F-14: Monolithic, Non-Streaming `ResolveSnapshot` & `ResolvedRepo` Exceed 1 GB RSS on 50k Repos
- **Severity:** P1
- **File:** `crates/cs-resolve/src/lib.rs:186-205, 294-302`
- **Symbols:** `ResolveSnapshot`, `ResolvedRepo`
- **What is wrong:**  
  `docs/ARCHITECTURE.md` §8 promises: *"Memory (indexing 50k files): < 1 GB RSS"*. However:
  1. `ResolveSnapshot` holds all extracted files in memory simultaneously: `pub files: Vec<(FilePath, cs_extract::ExtractedFile)>`.
  2. `Package` duplicates `DefLoc` (`file: path.clone()`, `qual_name: def.qual_name.clone()`) across `defs`, `exported`, and `methods`.
  3. `ResolvedRepo` stores in-memory `pub files: BTreeMap<FilePath, FileResolution>` where every reference holds `SymbolBinding { targets: Vec<DefLoc> }`.
- **Why it matters:**  
  On a 50,000-file repository with ~1.5M defs and ~5M refs, total heap objects exceed 30 million, consuming **2.5 GB to 4.2 GB of heap memory**, drastically breaching the 1 GB ceiling.
- **Confidence:** 95%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Intern file paths using integer file IDs (`FileId`), and stream extraction directly into SQLite batches of 1,000 files as planned in ARCHITECTURE §6.

---

### F-15: `.gitignore` Silently Ignored When `.git` Is Absent
- **Severity:** P1
- **File:** `crates/cs-scanner/src/lib.rs:195-203`
- **Function:** `discover()`
- **What is wrong:**  
  `ignore::WalkBuilder` defaults to `require_git(true)`, meaning it only honors `.gitignore` files if a `.git` folder exists in the directory or a parent. `discover()` never sets `builder.require_git(false)`.
- **Why it matters:**  
  When scanning an extracted release tarball, a clean CI archive, or a subdirectory where `.git` is omitted, **all `.gitignore` rules are completely ignored**. Build outputs, `node_modules/`, and secrets that rely on `.gitignore` are indexed.  
  Notably, `tests::honors_gitignore` (lines 460-463) worked around this by creating a fake `.git` folder rather than configuring `require_git(false)` in `discover()`.
- **Concrete Evidence:**  
  In a directory with `.gitignore` containing `ignored.go` and no `.git/` folder, `scan()` returns `ignored.go` as an indexed Go file.
- **Confidence:** 100% (Empirically verified).
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Explicitly configure `builder.require_git(false);` in `discover()`.

---

### F-16: Unbounded Memory Allocation & Traversal Before Cap Checks (DoS / OOM)
- **Severity:** P1
- **File:** `crates/cs-scanner/src/lib.rs:150-170, 209-258`
- **Function:** `discover()`, `scan()`
- **What is wrong:**  
  `discover()` pushes every encountered path into `relative: Vec<PathBuf>` and sums `total_bytes` during the full walk. The caps (`max_files`, `max_total_bytes`) are only checked in `scan()` *after* `discover()` finishes walking the entire tree.
- **Why it matters:**  
  Directly violates `docs/SECURITY.md` §6. An adversarial repository with 20 million files or 500 GB of content will force `discover()` to allocate millions of heap `PathBuf` objects and stat hundreds of gigabytes before tripping the cap, causing OOM or thrashing.
- **Concrete Evidence:**  
  In `discover()`, `relative.push(rel)` runs unconditionally without checking `relative.len() >= config.max_files`.
- **Confidence:** 100%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Check caps inside the `discover` loop on every entry and terminate early.

---

### F-17: Conflation of Parse Cap (1 MiB) with Read Cap (50 MiB) Breaks Incremental Indexing
- **Severity:** P1
- **File:** `crates/cs-scanner/src/lib.rs:308-320`
- **Symbol:** `inspect()`, `DEFAULT_PARSE_CAP`
- **What is wrong:**  
  `docs/ARCHITECTURE.md` §4.1 specifies: *"files > 1 MiB (parse-skip, still listed at L1 with size), files > 50 MiB (read-skip)."*  
  In `cs-scanner`, `parse_cap` (1 MiB) is used to skip reading and hashing altogether (`hash: None`, `skip: Some(SkipReason::TooLarge)`). No 50 MiB read cap exists.
- **Why it matters:**  
  Files between 1 MiB and 50 MiB are never hashed (`hash: None`). `docs/ARCHITECTURE.md` §4.4 states incrementality is keyed on BLAKE3 content hashes. Without a hash, `cs-index` cannot track whether files between 1 MiB and 50 MiB have changed, and `files.hash BLOB NOT NULL` in SQLite schema §5 is violated.
- **Concrete Evidence:**  
  `oversize_file_is_listed_but_not_hashed` test proves that any file exceeding 1 MiB receives `hash: None`.
- **Confidence:** 100%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Add `read_cap: u64` (default 50 MiB) to `ScanConfig`. Hash all files up to `read_cap`.

---

### F-18: `mtime` Missing from `ScannedFile` Record (Schema Contract Violation)
- **Severity:** P1
- **File:** `crates/cs-scanner/src/lib.rs:60-72`
- **Symbol:** `struct ScannedFile`
- **What is wrong:**  
  `docs/ARCHITECTURE.md` §4.1 specifies: *"Outputs: ordered stream of `ScannedFile { path, lang, size, mtime, blake3 }`"*, and SQLite schema §5 specifies `mtime INTEGER NOT NULL`. However, `ScannedFile` contains only `path`, `lang`, `size`, `hash`, `skip`. `mtime` is absent.
- **Why it matters:**  
  `cs-index` cannot populate the `NOT NULL` `mtime` column in SQLite without performing a redundant second `stat` call per file.
- **Confidence:** 100%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Add `pub mtime: i64` to `ScannedFile`, extracted from `metadata.modified()` in `inspect()`.

---

### F-19: Target Types in Bare Type Aliases and Type Specs Are Silently Dropped from `refs`
- **Severity:** P1
- **File:** `crates/cs-extract/src/go/mod.rs:662-670`
- **Function:** `collect_refs()`
- **What is wrong:**  
  `collect_refs` checks:
  ```rust
  RefKind::TypeRef => {
      if is_universe(name)
          || node
              .parent()
              .is_some_and(|p| p.kind() == "type_spec" || p.kind() == "type_alias")
      {
          continue;
      }
  }
  ```
  In tree-sitter-go, for `type MyAlias = TargetType` and `type NewType BaseType`, both the name and the target type are direct children of `type_alias` and `type_spec`. Because the code checks `node.parent() == type_spec || type_alias`, it drops NOT ONLY the defined name, but ALSO the referenced target type!
- **Why it matters:**  
  Any named type definition or alias based on another type (`type ID = UUID`) never records a reference to the underlying type, missing critical type alias edges.
- **Concrete Evidence:**  
  In `type A = B`, `B` is a child of `type_alias`. It is skipped by line 665.
- **Confidence:** 100%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Check `p.child_by_field_name("name") == Some(node)`.

---

### F-20: Group Doc Comments Poison Individual Spec Docs in `const` and `type` Blocks
- **Severity:** P1
- **File:** `crates/cs-extract/src/go/queries/defs.scm:35-66`, `crates/cs-extract/src/go/mod.rs:275-288`
- **Function:** `collect_defs()`
- **What is wrong:**  
  In `defs.scm`, pattern 1 matches grouped `const_declaration` and `type_declaration` blocks, attaching `@def.doc` to all inner specs. In `collect_defs`, pattern 1 runs first and sets `entry.doc_node = Some(group_comment)`. Later, pattern 3 matches the inner spec comment, but is rejected because `entry.doc_node.is_none()` is false. Then `doc_comment` checks line-adjacency between the group comment and the spec, which fails because `const (` sits between them, resulting in `doc: null`.
- **Why it matters:**  
  If a grouped `const (...)` or `type (...)` block has a comment on the block header, all individual doc comments inside the block are lost.
- **Concrete Evidence:**  
  In `fixtures/go/04_consts_vars.go`, `ModeFast` has comment `// ModeFast skips validation.` In `golden/04_consts_vars.go.json` line 32, `ModeFast` has `"doc": null`.
- **Confidence:** 100% (Empirically verified).
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Allow inner spec doc comments to overwrite group doc comments, or remove group doc captures from individual specs.

---

### F-21: Untyped Nested Composite Literals Leak Struct Field Names as References
- **Severity:** P1
- **File:** `crates/cs-extract/src/go/mod.rs:763-782`
- **Function:** `declaration_spans()`
- **What is wrong:**  
  In Go, nested struct literals omit their type: e.g. `Config{ Database: { Host: "localhost" } }`. When inspecting `Host: "localhost"`, the loop in `declaration_spans` reaches the inner `composite_literal`, sees `type: None`, and executes `break;`. Because `literal_type` is `None`, `Host` is not added to `declaration_spans` and leaks as a `NameRef`.
- **Why it matters:**  
  Field names in untyped nested composite literals leak as `NameRef` references, undermining ADR-018 §7.
- **Concrete Evidence:**  
  In `Config{ Database: { Host: "localhost" } }`, `child_by_field_name("type")` on `{ Host: "localhost" }` is `None`, breaking the search loop early.
- **Confidence:** 100%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  In the `while` loop, only `break` if `current.child_by_field_name("type").is_some()`. If `type` is `None`, continue walking up to the enclosing `composite_literal`.

---

### F-22: Mathematically Inverted and Flawed McNemar Power Calculations in `BENCHMARK.md` §4.2
- **Severity:** P1
- **File:** `docs/BENCHMARK.md:136-155, 192`
- **Section:** §4.2, §6
- **What is wrong:**  
  1. §4.2 asserts that for a true 12-point effect at $\alpha = 0.05$, power at $n = 50$ is 2% (15% discordant), 4% (30% discordant), and 6% (50% discordant). For any unbiased two-sided hypothesis test at $\alpha = 0.05$, power against a non-zero effect cannot be lower than $\alpha = 5\%$.
  2. The table displays power increasing as the discordant rate increases. In McNemar's paired test, for a fixed absolute marginal difference $\delta$, the test statistic is $\chi^2 = (n \delta^2) / p_d$. Higher discordant rate $p_d$ represents higher variance, which DECREASES power! The table is inverted.
  3. The error arose because the calculation treated a "12-point effect" as a 12% conditional win rate difference among discordant pairs ($p = 0.56$ vs $0.50$) rather than a 12% marginal task difference. With a true 12-point task difference at 15% discordant pairs, the win ratio is 90% vs 10% ($p = 0.90$), which yields over 30% power at $n=50$, not 2%.
- **Why it matters:**  
  ContextSlice explicitly grounds its credibility in "reproducible, mathematically sound benchmarking" and dismisses competitor benchmarks (such as Graft's $n=50$ SWE-bench run) based on this calculation. Publishing mathematically impossible power figures (2% power at 5% alpha) discredits the benchmark claims.
- **Concrete Evidence:**  
  Exact binomial test under $H_0: p=0.5, \alpha=0.05$: for $n=50, p_d=0.15 \implies N_d \approx 7$. Rejection region is $b \in \{0, 7\}$. Under true marginal effect $\delta=0.12 \implies p=0.90$. Power is $P(b=7 \mid p=0.9) = 0.9^7 = 47.8\% \gg 2\%$.
- **Confidence:** 99%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Recompute the power table using standard non-central $\chi^2$ or exact binomial power for marginal difference $\delta=0.12$. Update the prose in §4.2 and §6.

---

### F-23: Total Absence of Mandatory Property Tests
- **Severity:** P1
- **File:** `Cargo.toml:82`, `docs/MASTER_PLAN.md:§8.1`, `docs/ALGORITHM.md:§8`
- **Section:** Master Plan Testing Standards
- **What is wrong:**  
  `MASTER_PLAN.md` §8.1 and `ALGORITHM.md` §8 mandate property tests for: (1) token budget never exceeded, and (2) determinism across arbitrary inputs. `proptest = "1.11.0"` is declared in `Cargo.toml`, but not a single crate includes `proptest` in its dependencies, and zero property tests exist anywhere in the repository.
- **Why it matters:**  
  Core architectural invariants (budget integrity under adversarial unicode/signatures, determinism under permutations) are completely unverified by property testing.
- **Concrete Evidence:**  
  `git grep proptest` returns only the entry in `Cargo.toml:82`.
- **Confidence:** 100%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Add `proptest` to dev-dependencies in `cs-scanner`, `cs-extract`, and `cs-resolve`. Implement property tests for parser fuzzing, span non-overlapping, directory permutation invariance, and DAG cycle resistance.

---

### F-24: Schema & Resolution Conflation (Directory Packages vs TS/Python File Modules)
- **Severity:** P1
- **File:** `crates/cs-resolve/src/lib.rs:51-53`, `docs/ARCHITECTURE.md:§5`
- **Symbol:** `Resolution::Resolved(FilePath)`, `CREATE TABLE imports`
- **What is wrong:**  
  In `cs-resolve`, `Resolution::Resolved(FilePath)` returns directory paths for Go packages (`"internal/auth"`). In SQLite schema §5, `CREATE TABLE imports` has `resolved_dir TEXT`. However, TypeScript and Python import *files*, not directories (`./auth.ts`, `auth.py`). The schema and enum conflate file paths and package directories.
- **Why it matters:**  
  TypeScript and Python adapters cannot be integrated cleanly into `cs-index` without altering the schema or using awkward hacks (e.g. putting file paths into `resolved_dir`).
- **Confidence:** 100%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Distinguish `ResolvedDir(String)` and `ResolvedFile(FilePath)` in `Resolution`, and update `CREATE TABLE imports` to carry both `resolved_dir` and `resolved_file`.

---

### F-25: Inability to Support Incremental Indexing (`cs-resolve` & `cs-index`)
- **Severity:** P1
- **File:** `crates/cs-resolve/src/lib.rs:169-180`, `docs/ARCHITECTURE.md:§4.4`
- **Symbol:** `trait LanguageResolver`
- **What is wrong:**  
  `cs-resolve` provides no per-file resolution API (`resolve(&snapshot) -> ResolvedRepo`). Its two-phase design (`prepare(&snapshot)` followed by `resolve(&snapshot)`) requires the entire repository snapshot, contradicting the claim in `ARCHITECTURE.md` §4.4 that a changed file only rewrites its own outgoing edges. Furthermore, reverse edges (`ref_def`) remain stale when a destination file alters its definitions.
- **Why it matters:**  
  `cs-index` cannot perform incremental indexing in `< 2 s` without reloading all extracted definitions from SQLite on every run.
- **Confidence:** 95%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Provide an incremental resolution API: `resolve_file(&self, path: &str, file: &ExtractedFile) -> FileResolution`.

---

### F-26: Non-Object-Safe `LanguageResolver` Prevents Dynamic Language Dispatch
- **Severity:** P1
- **File:** `crates/cs-resolve/src/lib.rs:169-180`
- **Symbol:** `trait LanguageResolver`
- **What is wrong:**  
  `LanguageResolver: Sized` with methods returning `Self` and taking `&ResolveSnapshot` cannot be used as a trait object (`dyn LanguageResolver`).
- **Why it matters:**  
  Prevents polyglot repository resolution and dynamic language adapter dispatch across Go, TypeScript, and Python without hardcoded enum matches.
- **Confidence:** 100%.
- **Classification:** Genuinely new finding.
- **Recommended Fix:**  
  Separate `LanguageResolverFactory` from an object-safe `LanguageResolver` instance trait.

---

## 4. P2 Findings (Meaningful Quality, Performance, Maintainability Problems)

| ID | File & Line | Summary | Impact |
|---|---|---|---|
| **F-27** | `cs-scanner/src/lib.rs:155, 303` | `PathBuf::cmp` sort != string sort (`"a-b/c.go"` vs `"a/b/c.go"`) | Determinism failure: output order does not match SQLite path ordering |
| **F-28** | `cs-scanner/src/lib.rs:149, 173` | Traversal & hashing are sequential despite docs claiming parallelism | Performance bottleneck on 10k+ file repositories |
| **F-29** | `cs-scanner/src/lang.rs:1, 74` | Shebang detection completely unimplemented | Extensionless scripts always misclassified as `Unknown` |
| **F-30** | `cs-scanner/src/lang.rs:81-88` | Config/manifest files (`go.mod`, `tsconfig.json`) typed as code languages | Tree-sitter attempts to parse TOML/JSON as Go/TS code |
| **F-31** | `cs-scanner/src/lang.rs:77` vs `cs-resolve/src/go.rs:208` | Extension case-sensitivity contradiction (`.GO` vs `.go`) | `.GO` scanned and parsed, then discarded by resolver |
| **F-32** | `cs-scanner/src/lib.rs:382` | Non-UTF-8 paths lossily decoded with `\u{FFFD}` without labeling | Dangling DB references, potential SQLite `UNIQUE` collisions |
| **F-33** | `cs-extract/src/go/mod.rs:641-654` | Generic calls (`pkg.Func[T]()`) and parenthesized calls (`(x.Foo)()`) misclassified as `FieldRef` | Calls fail to bind in `cs-resolve` and remain unbound |
| **F-34** | `cs-extract/src/go/queries/imports.scm:15` | Raw string literal imports (``import `fmt` ``) completely ignored | Backtick imports silently dropped, breaking dependencies |
| **F-35** | `cs-resolve/src/lib.rs:230` | `UnboundReason::NoScope` is dead code and never emitted | Unmatched selector operands silently treated as bare calls |
| **F-36** | `cs-resolve/src/go.rs:753-757` | Premature ambiguity check in `bind_or_reason` for test-file defs | Non-test files falsely get `AmbiguousDotImport` from test helpers |
| **F-37** | `cs-resolve/src/go.rs:765-769` | `ResolutionStats.names_skipped` undercounts unqualified names | Exceeded candidates on unqualified names not tracked in stats |
| **F-38** | `cs-resolve/src/go.rs:174` | Overly permissive `go.mod` matching (`path.ends_with("go.mod")`) | Matches `backup_go.mod`, `test_go.mod` |
| **F-39** | `cs-resolve/src/go.rs:785-805` | Missing pervasive stdlib methods in `UNIVERSE_METHOD_NAMES` (`Lock`, `Done`, `Do`, `Add`, `Wait`) | Common stdlib calls falsely bind to local methods with those names |
| **F-40** | `cs-resolve/src/go.rs:95, 400-405` | Multi-threading data race on `self.dampened` Atomic in `GoResolver` | Non-deterministic `names_skipped` count under Rayon parallelism |
| **F-41** | `docs/benchmarks/resolver-gin-chi.md` | 97.2% precision claim only applies to the 14-20% of references that bound | Over 80% of references unbound; Wilson CI lower bound is ~94.7% |
| **F-42** | `cs-resolve/tests/real_repo.rs:25` | Ignored real-repo tests hard-panic on missing env vars; omitted from CI | `cargo test -- --ignored` fails; validation gates not checked in CI |
| **F-43** | `cs-resolve/src/go.rs:500-502` | Qualified type conversions (`types.ID(s)`) rejected by `call_kind` | Silently dropped as `UnboundReason::NoCandidate` |
| **F-44** | `cs-resolve/src/go.rs:541-555` | Platform build-tag method variants falsely flagged as `MethodAmbiguous` | Methods defined across OS tags dropped as ambiguous |
| **F-45** | `cs-extract/src/go/mod.rs:364-368` | `debug_assert!` formats raw file source code into panic messages | Violates privacy policy: user source code leaked into panic logs |
| **F-46** | `cs-extract/src/go/mod.rs:416, 452` | Unchecked UTF-8 byte boundary string slicing causes panics on malformed input | Truncated multibyte runes cause `byte index is not a char boundary` |
| **F-47** | `Cargo.toml:43-77` | Dependency manifest lacks exact `=` pinning syntax (`globset` drifted) | Violates ADR-011 exact pinning rule |
| **F-48** | `cs-extract/src/go/mod.rs:275, 833` | Quadratic def/import lookups and per-node heap vector allocations | Allocation churn and $O(N^2)$ AST search on large files |
| **F-49** | `cs-cli/src/main.rs:148, 161` | Redundant CLI argument declarations (`--no-git`, `--index` in both structs) | Argument parsing confusion and maintenance overhead |
| **F-50** | `cs-extract/src/go/mod.rs:796-804` | `is_struct_shaped` mistreats `[]int` as struct-shaped | Drops index constant expressions in primitive slice literals |

---

## 5. P3 Findings (Polish, Dead Code, Minor Inconsistencies)

| ID | File & Line | Summary | Impact |
|---|---|---|---|
| **F-51** | `cs-extract/tests/scratch_cst.rs:1-2` | Unremoved temporary scratch file committed to master | Violates comment "Deleted before the milestone commit" |
| **F-52** | `cs-resolve/tests/fixture_matrix.rs:255` | Dead loop in fixture test (`for r in &["Name", "Name"] { let _ = r; }`) | Code hygiene |
| **F-53** | `docs/BENCHMARK.md:78-84` | `recall_strong` formula includes `gold.config` in denominator but not numerator | Capped below 100% on PRs editing configuration |
| **F-54** | `docs/SECURITY.md:39, 111` | Documented CI security promises (strace, libgit2 hook checks, fuzzing) missing | Governance / CI gap |
| **F-55** | `cs-scanner/src/lib.rs:35` | Duplicate parse cap constants (`DEFAULT_PARSE_CAP` vs `PARSE_SIZE_CAP`) | Code hygiene |
| **F-56** | `cs-scanner/src/lib.rs:195` | Missing built-in default ignores (vendor/media/binaries) | Large non-code files scanned if `.gitignore` missing |
| **F-57** | `cs-scanner/src/lib.rs:202` | Blanket skip of all symlinks vs documented loop detection | Symlinked code in monorepos silently omitted |
| **F-58** | `cs-scanner/Cargo.toml:15` | Unused `globset` dependency in `crates/cs-scanner/Cargo.toml` | Manifest bloat |
| **F-59** | `cs-scanner/src/lib.rs:248, 306` | Redundant double-stat per file in scanner | Unnecessary I/O system calls |
| **F-60** | `cs-extract/src/go/mod.rs:514` | Block comments with ` *` lines fail first-paragraph extraction | Minor doc formatting defect |
| **F-61** | `cs-extract/src/go/mod.rs:469` | Compiler pragmas (`//go:noinline`) extracted as doc comments | Pollutes symbol docs |
| **F-62** | `cs-extract/src/go/queries/defs.scm:78` | Inconsistent pattern ordering for grouped vars in `defs.scm` | Query style drift |
| **F-63** | `cs-resolve/src/lib.rs:46, 138` | Unused constants `REEXPORT_DEPTH_CAP` and `W_IMPORT_IN` | Dead code |
| **F-64** | `cs-cli/src/main.rs:273` | Missing `tracing_subscriber` initialization in `cs-cli` | `--verbose` / logging produces no output |
| **F-65** | `cs-extract/tests/golden.rs:134` | Determinism test reverses output vector rather than re-running concurrently | Tautological test |
| **F-66** | `cs-extract/src/go/mod.rs:434` | Large composite literals in var/const signatures produce massive strings | L2/L3 signature bloat |
| **F-67** | `cs-resolve/src/lib.rs:309` | Schema doc drift on `ResolutionStats.resolved` | Documentation precision |

---

## 6. Findings by Crate

### `crates/cs-scanner`
- **P0:** F-01 (traversal into `.git/` directory leaks credentials).
- **P1:** F-15 (`.gitignore` ignored without `.git`), F-16 (unbounded memory/traversal before cap checks), F-17 (parse cap vs read cap conflation), F-18 (`mtime` missing from `ScannedFile`).
- **P2:** F-27 (PathBuf vs string sorting), F-28 (sequential traversal/hashing), F-29 (shebangs unimplemented), F-30 (manifests typed as code), F-31 (case-sensitivity contradiction), F-32 (non-UTF-8 path lossy decode).
- **P3:** F-55 (duplicate cap constants), F-56 (missing default ignores), F-57 (blanket symlink skip), F-58 (unused `globset`), F-59 (redundant double-stat).

### `crates/cs-extract`
- **P0:** F-02 (unsorted `UNIVERSE_NAMES` breaking `binary_search`), F-03 (`:=` RHS bare identifier dropping), F-05 (unimplemented parse timeout and node budget).
- **P1:** F-19 (bare type alias target dropping), F-20 (group doc comment poisoning), F-21 (untyped nested composite literal leaking field names).
- **P2:** F-33 (generic/parenthesized calls misclassified as `FieldRef`), F-34 (raw string literal imports ignored), F-45 (`debug_assert!` leaking code), F-46 (unchecked UTF-8 byte slicing), F-48 (quadratic def lookup & vector churn), F-50 (`is_struct_shaped` on primitive slices).
- **P3:** F-51 (scratch CST file), F-60 (block comment ` *` handling), F-61 (compiler pragmas as docs), F-62 (query pattern ordering), F-65 (tautological determinism test), F-66 (composite literal signature bloat).

### `crates/cs-resolve`
- **P0:** F-04 (fragile 2-byte distance heuristic in `preceding_qualifier`).
- **P1:** F-08 (`Run` missing from universe methods, breaking benchmark claim), F-09 (major version `/v5` qualifier failure), F-10 (`go.mod` inline comment & tab breakage), F-11 (chained selector false positives), F-12 (`package main` hijacking `non_test_package`), F-13 ($O(N^2)$ scaling in `non_test_package`), F-14 (monolithic memory exceeding 1 GB RSS), F-24 (directory vs file import schema conflation), F-25 (lack of per-file incremental API), F-26 (non-object-safe `LanguageResolver`).
- **P2:** F-35 (`NoScope` dead code), F-36 (premature dot-import ambiguity check), F-37 (`names_skipped` undercount), F-38 (permissive `go.mod` matching), F-39 (missing stdlib universe methods), F-40 (`AtomicU64` race under parallelism), F-41 (precision claim caveats), F-42 (real-repo tests panic), F-43 (type conversions rejected), F-44 (build-tag method ambiguity).
- **P3:** F-52 (dead loop in fixture test), F-63 (unused constants), F-67 (stats doc drift).

### `crates/cs-cli`
- **P0:** F-06 (`mcp --stdio` Clap crash; primary command `contextslice "task"` fails).
- **P0:** F-07 (stdout pipeline poisoning on error).
- **P2:** F-49 (redundant CLI argument declarations).
- **P3:** F-64 (missing tracing subscriber initialization).

---

## 7. Cross-Cutting Architectural Concerns

### 1. Inability to Support Incremental Indexing (`cs-resolve` & `cs-index`)
`docs/ARCHITECTURE.md` §4.4 states:
> *"Incrementality keyed on blake3 content hash: unchanged hash => skip parse; changed => re-extract that file and re-resolve its edges (resolution is per-file-pair, so a changed file only rewrites edges where it is `src`)."*

In reality, `LanguageResolver` requires the entire `ResolveSnapshot` (`prepare(&snapshot)` followed by `resolve(&snapshot)`). There is no per-file resolution API. If `cs-index` only re-extracts dirty files, it cannot resolve them without loading every file in the repository back into memory. Furthermore, if a destination file removes or modifies a definition, its incoming `ref_def` edges from unchanged source files become stale and will never be invalidated without a complete graph rebuild.

### 2. File vs Directory Import Conflation Across Language Boundaries
Go resolves imports to *directories* of files (`(dir, package_name)`), while TypeScript and Python resolve imports to *individual files* (`./auth.ts`). `cs-resolve/src/lib.rs` returns `Resolution::Resolved(FilePath)`, but for Go, `FilePath` actually holds a directory path. `CREATE TABLE imports` only has `resolved_dir TEXT`. This conflation must be cleanly partitioned into `ResolvedDir` and `ResolvedFile` before TypeScript and Python adapters are built.

### 3. Separation of Concerns: Syntax Extraction vs AST Qualification
`cs-extract` throws away AST parentage for `selector_expression` and `qualified_type`, emitting flat, unlinked references. `cs-resolve` then attempts to reconstruct token adjacency using a raw byte offset heuristic (`start_byte - end_byte <= 2`). This boundary is flawed: `cs-extract` has the full AST in hand and should directly emit `Ref { qualifier: Option<String>, ... }`.

---

## 8. Documentation vs Implementation Mismatches

| Document | Stated Claim | Actual Implementation |
|---|---|---|
| `docs/SECURITY.md` §6 | "Per-file parse timeout (250 ms) and node budget prevent pathological-grammar DoS" | Completely missing; `PARSE_TIMEOUT_MS` is an unused constant; no timeout or node cap. |
| `docs/benchmarks/resolver-gin-chi.md` | "Bare calls on external receivers with stdlib method names (14/312 at second pass: t.Run...) — fixed by universe-method filter" | Factual error: `Run` is not in `UNIVERSE_METHOD_NAMES`. `t.Run` binds to `Engine.Run` in Gin. |
| `docs/ARCHITECTURE.md` §4.1 | "blake3 hashing... hashing is the largest single cost and is parallelized." | Traversal and hashing are strictly sequential; `rayon` is not even in `cs-scanner/Cargo.toml`. |
| `docs/ARCHITECTURE.md` §4.1 | "extension+shebang→language map" | Shebang parsing is completely unimplemented; `detect_language` takes only `&str`. |
| `docs/ARCHITECTURE.md` §4.1 | "files > 1 MiB (parse-skip, listed at L1), files > 50 MiB (read-skip)" | Any file > 1 MiB skips hashing (`hash: None`); no 50 MiB read cap exists. |
| `docs/ARCHITECTURE.md` §5 | `CREATE TABLE files ... mtime INTEGER NOT NULL` | `ScannedFile` has no `mtime` field. |
| `docs/MASTER_PLAN.md` §6.1 | `contextslice "task text"` | Exits with Clap Usage Error 2; requires explicit `contextslice slice "task text"`. |
| `docs/MASTER_PLAN.md` §6.1 | `contextslice mcp [--stdio]` | Exits with Clap Usage Error 2 due to misconfigured `ArgAction::Set`. |
| `docs/MASTER_PLAN.md` §6.1 | "stdout is sacred... only the slice artifact goes to stdout" | Writes dummy placeholder to stdout on failure before exiting code 4. |
| `docs/BENCHMARK.md` §4.2 | McNemar power at $n=50, \alpha=0.05$ is 2%–6% and increases with discordant rate | Mathematically flawed and inverted: power cannot be below $\alpha=5\%$; higher discordant rate decreases power. |
| `docs/LANGUAGES.md` §4 | "Markdown/configs (not languages — handled as data files by path heuristics)" | `go.mod`, `tsconfig.json`, `pyproject.toml` are typed as code languages in `lang.rs`. |
| `docs/adr/ADR-011.md` | Dependencies are pinned to exact versions | Cargo manifest uses `0.4.18` (not `=0.4.18`); `globset` drifted to `0.4.20`. |

---

## 9. Testing Gaps

1. **Zero Property Tests:** Despite explicit requirements in `MASTER_PLAN.md` §8.1 and `ALGORITHM.md` §8, zero property tests (`proptest` / `quickcheck`) exist. Fuzzing of AST byte spans, directory ordering invariance, and parser resilience is missing.
2. **Real-Repository Tests Are Omitted from CI:** `tests/real_repo.rs` and `tests/perf.rs` are marked `#[ignore]` and hard-panic when run (`cargo test -- --ignored`) because `CS_PERF_REPO` and `CS_RESOLVE_REPO` are unhandled `expect()` calls. Neither test runs in GitHub Actions CI.
3. **Goldens Masking Implementation Bugs:** In `fixtures/go/golden/12_generics.go.json`, `make`, `len`, and `append` are recorded as active references because `is_universe` failed; the test passed because the golden was generated with the bug. In `04_consts_vars.go.json`, `ModeFast` has `"doc": null` due to group doc poisoning, and the test asserted `null`.
4. **Tautological Determinism Tests:** `extraction_is_deterministic_across_runs_and_orderings` simply reverses an in-memory vector of results from a single-threaded function. It does not test concurrent batching, multi-threaded parsing, or thread interleavings.
5. **Dead Code in Fixture Tests:** `fixture_matrix.rs:255` contains dead code: `for r in &["Name", "Name"] { let _ = r; }`.

---

## 10. Benchmark Concerns

1. **Flawed McNemar Power Calculations:** §4.2 of `docs/BENCHMARK.md` miscalculated paired McNemar power, claiming 2%–6% power for an $n=50$ run. Power cannot be lower than the significance level $\alpha = 0.05$. This flawed math was used to dismiss Graft's SWE-bench Verified results.
2. **Precision Metric Denominator Scope:** The reported 97.2% precision on Gin and Chi applies only to the **14%–20%** of references that were actually bound. Over 80% of references remain unbound (`no_candidate`, `external_scope`, `needs_type_info`).
3. **Missed False Positives in Manual Audit:** The manual audit sample missed severe false positives:
   - `mux.go:535 w.Header().Add(...)` bound to `context.go:RouteParams.Add` in Chi.
   - `t.Run` bound to `Engine.Run` across test files in Gin.
4. **Confidence Interval Below Gate:** The Wilson 95% CI lower bound for combined precision is **94.7%**, which falls below the project's own $\ge 95\%$ acceptance threshold.
5. **Config Bias in Intrinsic Recall:** `recall_strong` includes `gold.config` in the denominator, but config files are capped at L3 (ALGORITHM §7). Any PR modifying configuration files is mathematically barred from reaching 100% `recall_strong`.

---

## 11. Security Concerns

1. **Secret & Credential Exposure via `.git/` Traversal:** As detailed in F-01, scanning a repository indexes `.git/config`, hooks, logs, and commit messages. Where users pipe slices into LLMs, personal access tokens or private URLs stored in `.git/config` will be leaked.
2. **Denial-of-Service via Tree-sitter Parser Hanging:** As detailed in F-05, the absence of parse timeouts and node budgets allows an untrusted repo with a pathological grammar construct to hang the indexer indefinitely.
3. **Denial-of-Service via Unbounded Traversal:** Malicious repositories with 10M empty files will exhaust heap memory in `discover()` before any file cap is checked.
4. **Confidential Source Code Leaked into Panic Backtraces:** `debug_assert!` in `cs-extract/src/go/mod.rs:364-368` formats raw file source code into panic messages, violating `SECURITY.md` §7.
5. **Unverified CI Security Promises:** `SECURITY.md` claims CI runs sandboxed `strace` tests for zero network syscalls, nightly `cargo fuzz` on grammars, and libgit2 hook execution prevention tests. None of these exist in `.github/workflows/ci.yml`.

---

## 12. Performance Concerns

1. **Resolver Scaling Bottleneck ($O(N^2)$ in `non_test_package`):** Scanning all repository packages up to 4 times per import will cause resolution time to explode on 10k+ file monorepos (~100 seconds vs < 1 s budget).
2. **Monolithic In-Memory AST Footprint:** Holding all extracted files, AST symbols, and resolved bindings in RAM will consume 2.5–4.2 GB RSS on 50k-file repositories, violating the `< 1 GB RSS` target.
3. **Completely Sequential Scanner:** Traversal and BLAKE3 hashing are single-threaded, leaving multi-core throughput on the table.
4. **Excessive Ephemeral Heap Allocations in `cs-extract`:** `visit_descendants` calls `.children(&mut cursor).collect::<Vec<_>>()` for every single AST node, creating hundreds of thousands of heap vectors during cold indexing.

---

## 13. Things That Are Already Done Very Well

1. **Strict Zero-Network Hygiene:** Verified across all dependencies and code paths. Zero sockets, zero HTTP clients, zero telemetry, zero update checkers. Offline token counting and local SQLite index.
2. **Determinism Architecture:** Snapshot design with path-sorted keys, BTree structures, and strictly defined tie-breakers guarantees bit-for-bit reproducible runs when bugs are fixed.
3. **Honest Approximation Philosophy:** Unbound references are categorized into a clear, documented taxonomy (`UnboundReason`) rather than hidden or silently dropped.
4. **Test Package Isolation:** Clean separation between internal and external test packages (`foo` vs `foo_test`) in `cs-resolve`. Production code never binds to test helpers.
5. **Crate Boundary Structure:** Strict downward dependency hierarchy without cyclic dependencies.
6. **Codebase Cleanliness & Discipline:** `#![forbid(unsafe_code)]` across all crates, strict Clippy pedantic rules enforced, and zero compilation warnings.

---

## 14. Top 5 Things That Should Be Fixed Before Next Milestone

1. **Fix `.git/` Traversal in `cs-scanner` (F-01):** Add an explicit `filter_entry` to skip `.git` and `.contextslice` to prevent secret leakage and index pollution.
2. **Fix `UNIVERSE_NAMES` Binary Search & `:=` RHS Dropping in `cs-extract` (F-02, F-03):** Sort `UNIVERSE_NAMES` alphabetically, remove `f32`/`f64`, and restrict short-var declaration collection strictly to the `left` child. Regenerate goldens.
3. **Fix Byte-Distance Heuristic & Major Version Suffixes in `cs-resolve` (F-04, F-09):** Record qualifiers directly during extraction, and handle `/v2`, `/v3`, `/v5` external import paths cleanly. Add `Run` to `UNIVERSE_METHOD_NAMES`.
4. **Implement Parse Timeout & Early Cap Termination (F-05, F-16):** Use cooperative parse timeouts in Tree-sitter, and terminate traversal immediately upon exceeding file/byte caps.
5. **Fix CLI Contract and Pipeline Poisoning in `cs-cli` (F-06, F-07):** Fix `--stdio` argument parsing, route default positional arguments to `slice`, and never write dummy artifacts to stdout on error.

---

## 15. Things That Should NOT Be Changed (Intentional Design Decisions)

1. **Approximate Reference Graph Without Type Resolution (ADR-003):** Do not attempt full semantic type checking, compiler integration (`go/packages`), or heavyweight LSP sidecars. The approximate name/container matching is fast, robust, and intentional.
2. **Bounded 2-Hop Graph Walk Over Personalized PageRank (ADR-004):** Do not replace the bounded walk with PageRank before Phase 3 extrinsic evaluation proves a need. The 2-hop walk is deterministic, fast, and explainable.
3. **Owned `.scm` Tree-sitter Queries (ADR-012):** Do not revert to `tree-sitter-tags` or external query crates. The owned query model is critical for cross-platform stability.
4. **SQLite as the Sole Persistence Store (ADR-002):** Do not introduce custom binary formats or secondary caches. SQLite WAL mode provides the right balance of inspectability, crash resilience, and performance.
5. **No Telemetry Under Any Circumstances (ADR-010):** Do not add opt-in crash reporting or phone-home telemetry.

---

## 16. Confidence & Uncertainty for Major Findings

- **High Confidence (100% Verified by Code & Execution):**
  - F-01 (`.git` traversal empirical leakage)
  - F-02 (`UNIVERSE_NAMES` binary search failure for 25 builtins)
  - F-03 (`:=` RHS reference dropping)
  - F-04 (`preceding_qualifier` 2-byte distance misattribution)
  - F-06 (`mcp --stdio` Clap crash & `contextslice "task"` failure)
  - F-07 (stdout pollution on failure)
  - F-08 (`Run` missing from universe methods; `t.Run` binding to `Engine.Run`)
  - F-09 (`/v5` external import qualifier failure)
  - F-10 (`go.mod` inline comment and tab failure)
  - F-15 (`.gitignore` ignored without `.git`)
- **Medium Confidence (90%–95% Analytical / Extrapolated):**
  - F-13 ($O(N^2)$ scaling bottleneck on 10k+ repos: verified mathematically by loop iteration counts; requires a synthetic 10k-repo benchmark to measure exact wall time).
  - F-14 (Monolithic memory exceeding 1 GB RSS on 50k repos: estimated from heap object counts; depends on allocator fragmentation).
  - F-22 (McNemar power inversion: verified by binomial distribution math; requires empirical SWE-bench pilot data for exact discordant rate).

---

## Finding Classifications

### 1. Genuinely New Findings
All P0 findings (F-01 through F-07), P1 findings (F-08 through F-26), and P2 findings (F-27 through F-50) are genuinely new discoveries from this adversarial audit that were not documented in previous agent reports.

### 2. Findings Already Covered by Tests or ADRs
- The trade-off of named map types (`type H map[...]`) remaining `name_ref` is acknowledged in ADR-018.
- The distinction between `.ts` and `.tsx` grammars is documented in ADR-008 and tested in `cs-extract`.
- Local variable shadowing of package qualifiers is acknowledged as an unavoidable ~2% FP class in ADR-018 and `resolver-gin-chi.md`.

### 3. Reviewer Opinions & Potential Trade-offs
- **Test Affinity Edge Density:** Full Cartesian product between test files and production files in the same directory generates >70% of all edges in Gin and Chi. Limiting affinity to matching basenames (`foo_test.go` $\leftrightarrow$ `foo.go`) is an architectural trade-off for Phase 3 tuning.
- **Buffer Allocation in Scanner:** Allocating a 1 MiB buffer per file in `hash_file` vs a thread-local reusable buffer.

### 4. Findings Requiring Empirical Validation Before Changing Code
- Re-measuring Gin and Chi resolution precision and unbound reason histograms after fixing `UNIVERSE_NAMES` and `t.Run`.
- Profiling actual heap RSS on a synthetic 50k-file repository before restructuring `ResolveSnapshot`.

---

## Project Health Assessment

```
┌─────────────────────────────────────────────────────────┐
│                  PROJECT HEALTH SCORE                   │
├───────────────────────────────┬─────────────────────────┤
│ Correctness                   │  6 / 10                 │
│ Architecture                  │  7 / 10                 │
│ Test Quality                  │  6 / 10                 │
│ Security                      │  5 / 10                 │
│ Performance                   │  5 / 10                 │
│ Maintainability               │  8 / 10                 │
│ Readiness for cs-index        │  4 / 10                 │
└───────────────────────────────┴─────────────────────────┘
```

- **Correctness (6/10):** Foundation is clean, but critical logic bugs (`UNIVERSE_NAMES` binary search, `:=` RHS dropping, 2-byte distance qualifier heuristic, `/v5` import failure) corrupt extraction and resolution facts.
- **Architecture (7/10):** Strong crate boundaries and downward dependencies. Weakened by file vs directory import conflation and lack of a per-file incremental resolution API.
- **Test Quality (6/10):** Extensive golden fixtures, but goldens silently enshrined implementation bugs without detection. Zero property tests; real-repo tests panic when run and are omitted from CI.
- **Security (5/10):** Zero-network discipline is outstanding. Severely degraded by unconstrained `.git/` traversal leaking credentials, lack of parser DoS timeouts, and code leakage in panic messages.
- **Performance (5/10):** Fast on small repositories (<100 files), but exhibits $O(N^2)$ scaling bottlenecks and monolithic memory structures that will fail on 10k–50k file repositories. Completely sequential scanner.
- **Maintainability (8/10):** Excellent documentation, clear module structure, strict `#![forbid(unsafe_code)]`, and readable idiomatic Rust.
- **Readiness for cs-index (4/10):** NOT ready to proceed to `cs-index` until the extraction and resolution data contracts are corrected. Persisting broken facts into SQLite will compound architectural debt.

---

## Top 5 Actions Before Next Milestone

1. **Patch Scanner Traversal & Caps:** Exclude `.git/` and `.contextslice` from the walker, set `require_git(false)`, and abort traversal early when caps are reached.
2. **Correct Extraction Fact Generation:** Sort `UNIVERSE_NAMES` alphabetically, remove `f32`/`f64`, fix `:=` RHS dropping, and ensure type alias targets and nested composite literals are extracted accurately. Regenerate and re-audit goldens.
3. **Upgrade Qualifier Attribution & Resolver Robustness:** Record qualifiers directly during extraction (`Ref.qualifier`), strip comments/tabs in `go.mod`, handle `/v2+` imports, and add `Run` and pervasive stdlib methods to `UNIVERSE_METHOD_NAMES`.
4. **Fix CLI Contract & Pipeline Cleanliness:** Configure `--stdio` as a flag, route default arguments to `slice`, and ensure errors never emit placeholder artifacts to stdout.
5. **Implement Mandatory Property Tests & Fix Real-Repo CI:** Add `proptest` suites for determinism and parser invariants, make real-repo tests self-calibrating, and run them against pinned SHAs in CI.
