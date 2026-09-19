# ContextSlice — System Architecture

| | |
|---|---|
| **Status** | Pre-implementation blueprint (no code yet, by design) |
| **Version** | 1.0 — 2026-09-14 |
| **Depends on** | [MASTER_PLAN.md](MASTER_PLAN.md) (product, roadmap) · [ALGORITHM.md](ALGORITHM.md) (selection semantics) · [LANGUAGES.md](LANGUAGES.md) (adapter contracts) |

---

## 1. System overview

One static Rust binary, one SQLite index per repository, zero background processes.

```text
                 ┌────────────────────────────  contextslice (binary)  ───────────────────────────┐
                 │                                                                                     │
  Repository ──► │ cs-scanner ─► cs-extract ─► cs-resolve ─► cs-index (SQLite .contextslice/)      │
  (files+git)    │   walk,         tree-sitter     import→file      files/symbols/refs/edges/FTS5  │
                 │   ignore,       .scm queries    resolution,      content-hash incremental        │
                 │   lang map      defs/refs/      approx symbol    snapshot ids                    │
                 │   blake3        imports/sigs    binding                                          │
                 │                                  │                                                │
                 │   cs-git (recency, hotspots ────┘   optional, snapshot-pinned                    │
                 │                                                                                     │
                 │                     task + flags ─► cs-select ─► slice plan ─► cs-render          │
                 │                                    (seed/propagate/        (L0–L5, md/xml/json,  │
                 │                                     levels/budget)          anchors, elision)      │
                 │                                                                                     │
                 │   consumers:  stdout pipe ──► any agent / human                                     │
                 │               cs-mcp (stdio MCP server: slice/map/get_file/search_symbols)        │
                 └───────────────────────────────────────────────────────────────────────────────── ┘
```

The pipeline has two distinct runtime modes sharing the same index:

- **Index mode** (`contextslice index`, or automatic on first slice): heavy, parallel,
  incremental. Writes SQLite.
- **Slice mode** (`contextslice "task"`): light, <1 s warm. Reads SQLite, runs selection,
  renders. Never mutates the index.

---

## 2. Design principles

1. **Determinism first.** Same index snapshot + task + flags ⇒ byte-identical output.
   Parallelism is allowed internally only where results are order-normalized afterwards.
2. **Local-first, zero network.** The core never opens a socket. MCP runs over stdio.
   No telemetry, no update checks (SECURITY.md).
3. **Degrade, don't fail.** Every subsystem has a defined fallback (§11): unknown
   language → heuristics; unresolved import → marked unresolved; no git → skip signals;
   unparseable file → skipped with a warning row.
4. **Honest approximation.** Reference edges are approximate by construction (ADR-003).
   Nothing in the UI or docs implies IDE-grade precision. Unresolved things are labeled.
5. **Narrow seams.** Two extension surfaces only: `LanguageAdapter` (per-language
   extraction + resolution) and scorer signals (per-signal seed weights). Everything
   else is closed until Phase 6.
6. **One SQLite file.** No server, no custom storage, no second database, ever (ADR-002).

---

## 3. Workspace layout (crate map)

```text
contextslice/
├── crates/
│   ├── cs-scanner/    # repository traversal, language detection, hashing
│   ├── cs-extract/    # tree-sitter parsing, .scm query engine, symbol/ref extraction
│   ├── cs-resolve/    # per-language import resolution, approx symbol binding, edges
│   ├── cs-index/      # SQLite storage, incremental updates, snapshots, FTS5
│   ├── cs-git/        # git signals (recency now; co-change later)
│   ├── cs-select/     # seeding, propagation, levels, budget fitting (pure functions)
│   ├── cs-render/     # L0–L5 rendering, markdown/xml/json writers
│   ├── cs-cli/        # clap binary `contextslice`
│   ├── cs-mcp/        # MCP stdio server (thin over select/render)
│   └── cs-bench/      # intrinsic benchmark harness (extrinsic harness lives in bench/)
├── fixtures/          # golden corpora per language (go/, ts/, python/, pathological/)
├── bench/             # extrinsic harness + corpus manifests (separate from unit CI)
├── docs/              # these documents + adr/
└── .github/workflows/ # ci.yml, release.yml, bench.yml
```

Dependency direction is strictly downward: `cli`/`mcp` → `select` → `index` →
`resolve` → `extract` → `scanner`; `git` feeds `select` only; `render` consumes
`select` output; `bench` consumes everything. No cycles. `cs-select` contains **no I/O**
beyond reading the index snapshot — this is what makes determinism testable.

---

## 4. Subsystem specifications

### 4.1 cs-scanner

- **Purpose:** enumerate the files that belong to the index.
- **Inputs:** repo root, ignore rules (`.gitignore`, `.ignore`, `.contextsliceignore`,
  built-in defaults: vendored dirs, lockfiles, binaries, media), size caps.
- **Outputs (as built, 2026-09-16):** ordered stream of
  `ScannedFile { path, lang, size, mtime, blake3 }`, sorted by the normalized
  `/`-separated path **string** — the same ordering SQLite's BINARY collation
  produces, so document order and database order can never disagree (a
  `PathBuf` component-wise sort orders `"a-b/c.go"` and `"a/b/c.go"`
  differently; the string sort is the contract).
- **Algorithms/data (as built):** `ignore`-crate walker, **sequential** — one
  stat per file during discovery, then hashing in sorted order (streaming,
  1 MiB chunks). Language = extension map plus shebang patterns for
  extensionless scripts. `.git/` is never descended (credential-bearing
  internals; SECURITY §7/§8) and ignore semantics apply whether or not
  `.git` exists (`require_git(false)` — extracted archives get the same
  rules). Caps are enforced *during* the walk: an adversarial tree stops at
  the first trip of the file or byte cap, not after full discovery. Two
  caps, both as built: the 1 MiB **parse cap** (hashed for change detection,
  never parsed) and the 50 MiB **read cap** (listed with size, never read,
  no hash). `mtime` is captured per file (seconds since epoch; `0` when the
  filesystem does not report one).
- **Storage:** none (feeds cs-index).
- **Performance:** traversal+hash dominated; measured single-threaded as part
  of the 10k cold pipeline (9.72 s end-to-end through extraction and
  resolution, `docs/benchmarks/scaling_report.md`). **Rayon parallelism is a
  cs-index cold-path *option*, not an as-built fact** — it is gated on
  measurement: the single-threaded pipeline already leaves ~6× headroom
  against the 60 s cold budget, so parallelizing before cs-index exists would
  be tuning without a number. Any future parallelism reuses the
  sort-before-persist rule (§7).
- **Failure modes:** symlink loops (never followed), unreadable files (skip +
  labeled reason), files > 1 MiB (parse-skip, still listed and hashed at L1),
  files > 50 MiB (read-skip, no hash), cap trips (labeled `CapExceeded`).
- **MVP:** yes.

### 4.2 cs-extract

- **Purpose:** turn source files into structured facts.
- **Inputs:** file bytes + language.
- **Outputs (as built for Go, 2026-09-16):** `ExtractedFile { package_name,
  defs: Vec<Def>, refs: Vec<Ref>, imports: Vec<Import>, status }` where
  `Def { name, qual_name, kind, span, container, exported, signature, doc }`,
  `Ref { name, kind, qualifier, span, container }` (kind: `name_ref`/
  `field_ref`/`call_ref`/`type_ref` — the resolver's binding rule selector;
  qualifier: the selector operand's identifier text per ADR-020, `None` when
  there is no selector or the operand is computed — operands themselves are
  scope structure and are not emitted as refs), and
  `Import { raw, alias, kind, span }`. Signatures and docs are extracted
  strings, not spans (spans alone cannot render L2/L3 without the source at
  hand, and goldens pin the exact strings).
- **Algorithms/data:** tree-sitter 0.27.x (pinned), **owned `.scm` query sets per
  language** (`crates/cs-extract/src/go/queries/` for Go), compiled once per
  process. Queries capture `@def.name/@def.node/@def.doc`,
  `@ref.name/@ref.field/@ref.type`, `@import.path/@import.alias`; Rust
  post-processing applies the rules queries cannot express (declaration-
  position filtering, doc adjacency, signature construction — LANGUAGES §6.1).
  **Parse timeout as built (2026-09-16):** the 250 ms per-file budget is
  enforced *inside* tree-sitter via the `ParseOptions` progress callback — an
  over-budget parse is aborted cooperatively and the file is labeled
  `timeout` (`ParseStatus::Timeout`) with no extracted facts; there is no
  node-count cap, which was unimplementable to enforce. The budget covers the
  **parse**; post-parse fact collection is a separate phase that is linear in
  the 1 MiB input cap, with a measured worst case of ~0.34 s at the cap
  (ADR-019 §D records the decision to keep the deadline on the parse only,
  and the `only_directive_lines` quadratic that made collection super-linear
  until it was fixed to O(gap)).
- **Why owned queries (ADR-012):** the upstream `tree-sitter-tags` convention relies on
  `#strip!` / `#set-adjacent!` predicates that the core Rust binding does not implement.
  Core parsing *accepts* unknown predicates and then ignores them, so a tags-style query
  compiles cleanly and silently yields un-stripped doc comments and no adjacency — a
  failure with no diagnostic. Owned queries avoid the tags convention entirely and use
  the supported anchor operator for adjacency. Aider's `.scm` files are reference
  material for node names, not a drop-in.
- **Grammar selection:** `.ts` uses `LANGUAGE_TYPESCRIPT`, `.tsx`/`.js`/`.jsx` use
  `LANGUAGE_TSX`. The two grammars are not interchangeable and neither is a superset
  (measured; ADR-008). A file whose content disagrees with its extension parses partially
  and is labeled `partial` rather than guessed at.
- **Storage:** none (feeds cs-resolve/cs-index).
- **Performance:** tree-sitter parses at roughly Babel-class speed; the
  measured max-cap (1 MiB) parse is 66–74 ms against the 250 ms budget, and
  the 10k-file cold pipeline (extract + resolve) runs 9.72 s single-threaded
  (`docs/benchmarks/scaling_report.md`). Parallel-by-file (rayon) remains the
  cs-index cold-path option (§4.1).
- **Failure modes:** grammar errors (tree-sitter error nodes — extract what
  resolved, labeled `partial`); timeout (parse aborted under the budget,
  labeled `timeout`, no facts — SECURITY §6); unknown language (no adapter →
  heuristic path: no defs, content terms only).
- **MVP:** yes (Go first, then TS, Python).

### 4.3 cs-resolve

- **Purpose:** convert per-file facts into cross-file structure.
- **Inputs (as built for Go, ADR-018):** a `ResolveSnapshot` — the extracted
  files plus module-manifest *contents* (`go.mod` text; the resolver performs
  zero I/O) — both path-sorted at construction.
- **Outputs:** per-file `FileResolution` (package identity, import
  `Resolution`s, per-ref `SymbolBinding`s with documented unbound reasons);
  file `edges(src, dst, kind, weight)` where kind ∈ `import_out`,
  `ref_def` (aggregated per file pair, sqrt-damped weight), `test_affinity`
  (`import_in` remains derived at CSR build time); `ResolutionStats` with an
  unbound-reason histogram.
- **API shape:** two-phase — `LanguageResolver::prepare(&snapshot)` builds a
  repo-level package index once; `resolve(&snapshot)` consumes it. The
  original one-import-per-call trait could not express directory-packages
  or repo-wide binding.
- **Algorithms/data:** per-language (Go as built in LANGUAGES.md §6.1 /
  ADR-018): nearest-`go.mod` module mapping; package identity
  `(dir, package_name)`; qualifier scope from aliases and package clauses
  (never path tails); bind-all for build-tag variants; unique-only bare
  method binding with a universe-method filter; bare field accesses never
  bound.
- **Storage:** none (feeds cs-index).
- **Performance:** in-memory BTree maps; measured gin (99 files) prepare
  1 ms + resolve 10 ms, chi (84 files) similar — budget < 1 s @ 10k files
  is untouched. Names with >256 candidates skipped (aider's dampener).
- **Failure modes:** every degradation is a labeled outcome, not an error:
  `External` (stdlib/third-party/nested module — correct), `NotFound`,
  `EscapesRoot`, `Internal` (Go's `internal/` visibility rule module-relative
  — the code could not compile, so the dependency does not exist for this
  importer), and per-ref unbound reasons (`no_candidate`,
  `external_scope`, `no_scope` (computed selector operands, ADR-020),
  `method_ambiguous`, `ambiguous_dot_import`, `needs_type_info`,
  `universe_method`) surfaced as a histogram.
- **MVP:** yes (Go; TS/Python follow the same trait shape).

### 4.4 cs-index

- **Purpose:** persistent, incremental store of everything above.
- **Inputs:** scanner/extract/resolve outputs.
- **Outputs:** query APIs: FTS5 symbol search (step 6), file/symbol lookup, edge CSR, snapshot ids.
- **Algorithms/data (ADR-021, streaming-first):** three passes over
  SQLite-as-fact-store. *Pass 1 (facts):* scan → per-file extract → persist
  raw facts (`files`, `symbols`, `refs`, `imports`, `manifests`) in
  transactions of ≤1k files, one `ExtractedFile` in memory at a time.
  *Pass 2 (index):* streaming SELECTs build the only repo-proportional
  memory — exported definitions as integer tuples, the package table, the
  module list — plus `snapshot_id` (blake3 over sorted `(path, hash)`).
  *Pass 3 (resolve):* packages in sorted `(dir, name)` order; per package,
  load facts, resolve via the `cs-resolve` seam, and write derived rows
  (`imports.resolved_*`, `bindings`, `binding_targets`, `edges`) through in
  one transaction. Raw facts are the durable truth; derived rows are a
  re-derivable projection (ADR-021 D2). WAL mode, `synchronous=NORMAL`,
  `foreign_keys=ON`; fresh builds run under `meta.index_state='building'`
  (slices refuse; indexing resumes) and flip to `'ready'` atomically.
  Incremental runs stay `'ready'`; the fact transaction plants a transient
  `update_in_progress` marker that only the finalizing transaction (new
  `snapshot_id`) clears, so a crash mid-update leaves a *marked* torn
  state, never a falsely finished one (ADR-022).
- **Incrementality (ADR-021 D5 as built, ADR-022):** keyed on blake3
  content hash — always computed; `mtime` is recorded, never trusted.
  Unchanged hash ⇒ zero parsing. Changed file ⇒ re-extract it, re-resolve
  its whole package P, then reverse-invalidate package-scoped: every
  package holding bindings into any file of P, plus every package
  importing P's directory (blank imports included; indexed lookups over
  `binding_targets(file_id)` and `imports.resolved_dir`), re-resolves in
  its own package context. Deletions cascade, clean both edge directions
  in the same transaction, and garbage-collect packages left with no
  files — an empty ghost would shadow the real package in import
  resolution.
- **Storage:** `.contextslice/index.db` at repo root (self-contained, easy to inspect
  with the sqlite3 CLI — a feature, not an accident). Schema in §5.
- **Performance:** 10k files cold < 60 s (measured pipeline floor 9.7 s,
  extraction-dominated); incremental < 2 s for typical dirty sets @10k;
  RSS < 512 MiB @50k (the in-memory pipeline measured 3.30 GiB — ADR-021
  exists to retire that number); index size 5–15 MB/10k files.
- **Failure modes:** crash mid-fresh-build (`building` state; resume is
  hash-driven and free — the CLI resumes a `building` index, and
  `--rebuild` is the only reset), crash mid-incremental (facts commit
  atomically; the derived projection may lag until the next run — the
  `update_in_progress` marker names the torn state, `doctor` reports it,
  and the next update repairs by re-deriving from the hash-consistent
  facts), corruption (`PRAGMA quick_check` at open in doctor mode;
  remediation = rebuild), schema/config mismatch (rebuild; no migration
  machinery until a released format exists), concurrent writers (fail
  fast as a `Locked` error in milliseconds; no busy timeout).
- **MVP:** yes.

### 4.5 cs-git

- **Purpose:** cheap behavioral priors.
- **Inputs:** repo root, snapshot-pinned HEAD.
- **Outputs:** per-file recency score (log-scaled days since last touch), hotspot rank
  (commit touch count), later: co-change coupling matrix.
- **Algorithms/data:** `git2` (libgit2) log walk, path-filtered; results cached in
  `meta` keyed by HEAD sha so repeated slices are free.
- **Performance:** one log walk per HEAD; ~1 s on medium repos; skipped entirely with
  `--no-git` or outside a repo.
- **Failure modes:** not a repo (skip signals); huge histories (cap at last 5,000
  commits; document the cap).
- **MVP:** recency only. Co-change is Phase 5–6.

### 4.6 cs-select

- **Purpose:** the brain — task + snapshot ⇒ slice plan. **Pure, I/O-free** given the
  snapshot data.
- **Inputs:** task text, flags (budget, includes, format), snapshot (files, symbols FTS,
  edges CSR, git priors).
- **Outputs:** `SlicePlan { header, tree, entries: Vec<{path, level, symbols, score,
  reasons}> , elision_summary }`.
- **Algorithms:** specified normatively in ALGORITHM.md (seeding → bounded propagation →
  level assignment → greedy budget fitting with demotion ladder).
- **Performance:** in-memory; <100 ms at 50k files (FTS5 + 2-hop walk over CSR with
  K=64 frontier cap).
- **Failure modes:** empty seeds (map-mode fallback), budget smaller than seed skeletons
  (exit 3 with guidance), adversarial huge frontiers (caps).
- **MVP:** yes. This crate carries the determinism property tests.

### 4.7 cs-render

- **Purpose:** slice plan → bytes.
- **Inputs:** `SlicePlan`, source files (for L4/L5 bodies and L2/L3 skeletons),
  format flags.
- **Outputs:** markdown (default), JSON (machine), XML (experimental).
- **Algorithms/data:** skeleton rendering from `sig_spans`/`doc_spans` + elision markers
  (`⋯ N lines elided (M symbols) ⋯`); every symbol anchored `path:line`; header contains
  task, budget, measured tokens, index snapshot id, level legend, elision totals, and a
  "how to ask for more" hint. Token measurement via `tiktoken-rs` (o200k default,
  configurable) — the same estimator used by budget fitting, so "measured ≤ budget" is
  a real invariant, not an approximation mismatch.
- **Performance:** linear in output size; sub-100 ms typical.
- **Failure modes:** file changed between index and render (detect hash mismatch →
  warn + render from disk with a `possibly-stale` marker); invalid UTF-8 (lossy decode,
  labeled).
- **MVP:** markdown + JSON; XML experimental.

### 4.8 cs-cli / 4.9 cs-mcp

- **CLI:** clap; commands and exit codes specified in MASTER_PLAN §6.1. stdout is
  sacred (slice only), stderr for progress/diagnostics/`--explain`.
- **MCP:** stdio server, thin over `cs-select`/`cs-render`. **Transport and SDK are not
  yet chosen** — see ADR-013, which is `Proposed` and settles it at Phase 2 step 14.
  Verified 2026-09-15: the current MCP revision (`2026-07-28`) **removed** the
  `initialize` handshake in favour of mandatory per-request `_meta` plus a mandatory
  `server/discover` RPC, and `rmcp` cannot serve stdio without an async runtime (tokio is
  a non-optional dependency; there is no synchronous transport). Both facts change the
  Phase 2 design, so the decision is deferred rather than made against a skeleton engine.
  Tools:
  - `contextslice_slice(task, budget?, format?)` → rendered slice;
  - `contextslice_map(budget?)` → task-less repo map;
  - `contextslice_get_file(path, level?)` → file at requested level (agents use this to
    drill into elided files);
  - `contextslice_search_symbols(query, kind?, limit?)` → symbol table rows;
  - `contextslice_index_status()` → freshness, file count, snapshot id, staleness hint.
  Every tool description carries the untrusted-content warning (SECURITY §4). The server
  never auto-indexes on `slice` unless explicitly configured — it returns a staleness
  hint instead, so agents cannot silently trigger multi-minute builds.
- **MVP:** CLI yes; MCP yes (Phase 2 step 14).

### 4.10 cs-bench

See BENCHMARK.md. Ships in-repo so every release (and every algorithm-changing PR) can
attach numbers.

---

## 5. Data model (SQLite schema v2, frozen — ADR-021; v2 amendments in ADR-022)

```sql
-- meta: key/value; normative keys (ADR-021 D6): 'schema_version', 'index_state'
-- ('empty'|'building'|'ready'), 'snapshot_id', 'config_fingerprint', 'git_head'
-- (step 5c), plus the transient 'update_in_progress' torn-update marker
-- (ADR-022). No 'created_at': meta must be a function of the indexed input
-- (byte-identical semantic dumps).
CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
-- PRAGMA user_version mirrors meta.schema_version for sqlite3 CLI inspection.

CREATE TABLE packages (
  id INTEGER PRIMARY KEY,
  dir TEXT NOT NULL,                  -- repo-relative, '' at root (ADR-018 identity)
  name TEXT NOT NULL,                 -- package clause name (never a path tail)
  lang TEXT NOT NULL DEFAULT 'go',    -- language-tagged now, free for adapters later
  UNIQUE (dir, name, lang)
);

CREATE TABLE files (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,          -- repo-relative, '/'-separated, normalized
  lang TEXT NOT NULL,                 -- 'go','ts','tsx','python',... 'unknown'
  package_id INTEGER REFERENCES packages(id) ON DELETE SET NULL,  -- NULL = no clause / pseudo
  hash BLOB,                          -- blake3; NULL iff the file was never read (ADR-022)
  skip TEXT,                          -- 'too_large'|'unreadable' (never read, hash NULL) |
                                      -- 'parse_skipped' (hashed, over parse cap); NULL = hashed clean
  size INTEGER NOT NULL,
  mtime INTEGER NOT NULL,             -- recorded for diagnostics; NEVER a skip shortcut (ADR-021 D5)
  parse_status TEXT NOT NULL,         -- ok | partial | timeout | skipped | unsupported
  tokens_est INTEGER                  -- whole-file estimate, for map mode (unpopulated in step 5)
);
-- consistency (schema-enforced by CHECK; doctor re-verifies; ADR-022 truth table):
-- hash IS NULL => COALESCE(skip,'') IN ('too_large','unreadable') AND parse_status='skipped';
-- hash IS NOT NULL => (skip IS NULL OR skip='parse_skipped');
-- parse_status IN ('ok','partial','timeout','skipped','unsupported').
-- The COALESCE anchors the nullable skip so the OR-chain is total: a bare
-- `skip IN (...)` evaluates NULL, and SQLite CHECK treats NULL as satisfied.

CREATE TABLE symbols (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  qual_name TEXT NOT NULL,            -- file-local qualified name (Session.Validate)
  kind TEXT NOT NULL,                 -- func|method|class|struct|interface|type|const|var|enum...
  exported INTEGER NOT NULL,          -- language's export rule, decided at extraction
  line INTEGER NOT NULL, end_line INTEGER NOT NULL,
  start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,  -- body slicing for L4/L5
  signature TEXT,                     -- rendered one-liner for L2/L3
  container TEXT,                     -- enclosing symbol's qual_name (ADR-021 D6: TEXT, not id)
  doc TEXT                            -- first doc-comment paragraph
);
CREATE INDEX idx_symbols_file ON symbols(file_id);
CREATE INDEX idx_symbols_exported ON symbols(name, file_id) WHERE exported = 1;  -- binding hot path

CREATE TABLE refs (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  kind TEXT NOT NULL,                 -- name_ref | field_ref | call_ref | type_ref
  qualifier TEXT,                     -- selector operand identifier (ADR-020); NULL = none/computed
  line INTEGER NOT NULL,
  start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,  -- span (re-derivation, audit sampling)
  container TEXT                      -- enclosing symbol's qual_name; NULL at file scope
);
CREATE INDEX idx_refs_file ON refs(file_id);
-- Bindings deliberately do NOT live here (ADR-021 D4): def rowids churn on
-- re-extraction; binding targets are (file_id, qual_name, kind) below, which
-- survive churn and index the reverse-invalidation lookup.

CREATE TABLE imports (
  id INTEGER PRIMARY KEY,             -- resolution needs a stable per-row key
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  raw TEXT NOT NULL,                  -- specifier as written
  alias TEXT,                         -- named alias; '.' dot import; '_' blank import; NULL plain
  kind TEXT NOT NULL,                 -- import|export-from|require|dynamic...
  ordinal INTEGER NOT NULL,           -- source order (deterministic iteration)
  resolved_dir TEXT,                  -- directory-packages (Go, ADR-018): package dir
  resolved_file TEXT,                 -- file-module languages only (TS/Python): exactly ONE of
                                      -- resolved_dir/resolved_file is non-NULL when status is resolved
  unresolved_reason TEXT              -- escaperoot|notfound|internal|alias|... (NULL when resolved)
);
CREATE INDEX idx_imports_file ON imports(file_id);
CREATE INDEX idx_imports_resolved_dir ON imports(resolved_dir);
-- ^ package-scoped reverse invalidation looks up imports by resolved package dir,
--   blank imports included (ADR-022); ships with the schema so the query cannot
--   degrade into a full scan.
-- resolved_dir/resolved_file/unresolved_reason are DERIVED (resolve pass UPDATEs);
-- truth = (imports.raw, packages, manifests). Doctor may re-derive and compare.

CREATE TABLE manifests (
  path TEXT PRIMARY KEY NOT NULL,     -- repo-relative ('go.mod' today; language-generic name)
  content TEXT NOT NULL               -- verbatim; re-resolution needs modules without the FS
);

CREATE TABLE bindings (               -- DERIVED (resolve pass): one per ref
  ref_id INTEGER PRIMARY KEY REFERENCES refs(id) ON DELETE CASCADE,
  unbound_reason TEXT                  -- no_candidate|external_scope|no_scope|method_ambiguous|
                                       -- ambiguous_dot_import|needs_type_info|universe_method; NULL = bound
);

CREATE TABLE binding_targets (        -- DERIVED: 0..n per binding (bind-all for tag variants)
  ref_id INTEGER NOT NULL REFERENCES refs(id) ON DELETE CASCADE,
  file_id INTEGER NOT NULL REFERENCES files(id),   -- deletes of targets go through invalidation (D5), not cascade
  qual_name TEXT NOT NULL,
  kind TEXT NOT NULL,
  PRIMARY KEY (ref_id, file_id, qual_name)
);
CREATE INDEX idx_btargets_file ON binding_targets(file_id);  -- reverse-invalidation hot path

CREATE TABLE edges (                  -- DERIVED projection of imports+bindings+files (rebuildable)
  src INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  dst INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,                 -- import_out|ref_def|test_affinity
  weight REAL NOT NULL,
  PRIMARY KEY (src, dst, kind)
);
CREATE INDEX idx_edges_dst ON edges(dst);
-- FTS5 (symbols_fts) lands with cs-select (step 6, ADR-021 D10): the raw
-- text and spans above are everything it will need.
```

In-memory (built at slice time, never persisted): CSR adjacency over `edges` — 100k
edges materialize in <50 ms. `import_in` edges are derived (reverse of `import_out`) at
CSR build time rather than stored, to halve writes.

Determinism note (ADR-021 D7): batches are contiguous slices of the scanner's
sorted path list and packages resolve in `(dir, name)` order, so rowids assign in
sorted-path order — a fresh full index of an identical tree yields a byte-identical
ordered semantic dump (`SELECT * ORDER BY` per table) and the same `snapshot_id`.
Physical `.db` bytes are not promised (WAL checkpointing, freelist layout).

---

## 6. Index lifecycle

- **Create:** `contextslice index` (or first slice). Walk → parse → resolve → one
  transaction per 1k files → snapshot id computed (blake3 over sorted `(path, hash)`
  pairs) → stored in `meta`.
- **Update:** re-walk; for changed hashes, re-extract those files, re-resolve their
  packages plus the reverse-invalidated binders and importers (package-scoped,
  ADR-021 D5 as built / ADR-022), garbage-collect emptied packages, and recompute
  snapshot id. Typical cost ∝ dirty set.
- **Invalidate:** `--rebuild` drops and rebuilds; `doctor` detects corruption/version
  mismatch and offers it; major grammar-query upgrades bump schema_version and force a
  one-time rebuild (announced in CHANGELOG).
- **Staleness contract:** every slice header carries the snapshot id and a staleness
  line (`17 files changed since index`). Slicing never mutates the index, so a stale
  index produces a *labeled* slice, not a wrong-but-silent one.
- **Removal from git's view:** `contextslice init` (Phase 2) appends `.contextslice/`
  to `.git/info/exclude` — local ignore without touching the tracked `.gitignore`
  (SECURITY §7).

---

## 7. Concurrency and process model

- Single-shot process per invocation; no daemon, no file watcher (Phase 6 decision).
- Parallelism: rayon pools inside index mode (parse, hash, resolve). Parallel results are
  collected and **sorted by path before any persisting decision**, which is what makes
  determinism compatible with parallelism.
- Slice mode is single-threaded and fast (<1 s); the only I/O is SQLite reads plus
  opening the L4/L5 source files at render time.
- Concurrency between processes: one writer at a time (SQLite lock; fail-fast message
  naming the holder's pid). Multiple concurrent readers are fine (WAL).

---

## 8. Performance engineering

Targets (restated from MASTER_PLAN §9) and the techniques that buy them:

| Target | Technique |
|---|---|
| Cold 10k < 60 s | measured single-threaded pipeline: 9.72 s at 10k (extract + prepare + resolve, `docs/benchmarks/scaling_report.md`); rayon parse remains an option if a real index run breaches the budget; skip vendored/lockfiles by default |
| Cold 100k < 12 min | same + 1 MiB parse cap, candidate-list caps in resolver |
| Incremental < 2 s | content-hash diffing; only dirty files re-parsed/edge-rewritten |
| Warm slice < 1 s | snapshot-pinned reads; CSR built once per invocation; FTS5 with a prepared query |
| Startup < 50 ms | lazy grammar loading (only languages present); clap minimal; no runtime config parsing beyond one TOML |
| RSS < 1 GB @50k | **requires the streaming design — mandatory in cs-index.** The current materializing API (whole-repo snapshot + resolver index + `ResolvedRepo` in memory) measures **3.30 GiB peak at 50k** — a 3.3× breach; the 1 GB wall sits near 15k files — `docs/benchmarks/scaling_report.md`. cs-index must stream: extract and resolve package-by-package (or batch), write incrementally, drop in-memory results |
| Index ≤ ~15 MB/10k | no source text stored (spans + signatures only); refs deduplicated by (name,line) |

Load-bearing rule: **source text is never persisted.** The index stores spans and
signatures; bodies are read from the working tree at render time (with the staleness
hash check from §4.7). This keeps the index small, avoids duplicating user code, and
sidesteps secret-retention concerns (SECURITY §5–7).

---

## 9. Output formats

**Markdown (default)** — header block (task, budget, measured tokens, snapshot id,
staleness line, level legend), repo tree with per-file level markers, then sections in
descending level order (L5 first), each file under a `### path` header with fenced
content, `path:line` anchors on symbols, elision markers inline, and a footer with
elision totals plus the ask-for-more hint.

**JSON** — the `SlicePlan` serialized: machine-readable for harnesses and agent tooling;
stable schema (`schema: 1`), documented in-renderer.

**XML (experimental)** — Repomix-style `<file path=...>` envelope for consumers that
already parse that convention.

Byte-stability of all formats is golden-tested (MASTER_PLAN §8.1).

---

## 10. MCP surface

Five tools (§4.9), all read-only, all carrying untrusted-content warnings, none able to
trigger an index build silently. Rationale for this surface: `slice` is the product;
`map` covers task-less orientation; `get_file` closes the loop when an agent needs
elided content (this is the complement-not-competitor posture toward agentic search —
the agent still explores, but starts oriented and drills down cheaply);
`search_symbols` exposes the index to agents that want precision;
`index_status` makes staleness inspectable so the *agent* can decide to ask the user to
re-index.

---

## 11. Degradation and failure matrix

| Condition | Behavior | User-visible |
|---|---|---|
| Language without adapter | content/path heuristics only (no defs/edges) | header notes "heuristic mode for 412 files" |
| Unresolved TS alias | ref left unresolved; edge absent | resolution-rate % in `doctor` |
| Parse timeout / error nodes | timeout: parse aborted, no facts, `parse_status=timeout`; error nodes: partial extraction, `parse_status=partial` | warning row in `doctor`, marker in header |
| File > 1 MiB | listed (L1 eligible), not parsed | header note |
| Not a git repo / `--no-git` | git priors = 0 | silent (documented) |
| Empty seed set | map-mode fallback (aider-style skeleton map) | explicit "no seeds; repo map" header |
| Budget < seed skeletons | exit 3 | message suggests `--tokens` value that fits |
| Index older than working tree | render from disk, hash-checked | staleness line in header |
| Corrupt index | fail slice with exit 3 | `contextslice doctor` offers `--rebuild` |
| Second concurrent indexer | fail fast | message names lock holder |

---

## 12. Extension points (post-MVP, in priority order)

1. **Deep modes (Phase 6):** optional per-language precise edge providers behind an
   `EdgeProvider` trait — `scip-*` consumption (SCIP has Go+Rust crates and active
   indexers for TS/Python/Java/C#; rust-analyzer emits it natively), gopls sidecar.
   Deterministic heuristics remain the always-on baseline.
2. **Semantic layer (Phase 5):** opt-in embeddings + local LLM (Ollama) for query
   expansion and re-ranking, strictly additive behind `--semantic`; must beat the
   deterministic baseline on the intrinsic benchmark to ship enabled (roadmap gate).
3. **Watch mode (Phase 6):** file-watcher maintaining the index; trigger = demonstrated
   agent-loop usage of `get_file`/`index_status` complaining about staleness.
4. **Co-change coupling (Phase 5–6):** git co-commit matrix as an additional seed prior.
5. **Plugin API (Phase 6):** out-of-tree `LanguageAdapter`s and scorer signals; decided
   only after three in-tree adapters prove the seam is stable.

---

## 13. Packaging and distribution

- Static binaries: x86_64/aarch64 Linux (musl), macOS universal, Windows x86_64 — via
  GitHub Releases, built in CI.
- `cargo install contextslice` always works.
- Homebrew tap (Phase 4).
- npm wrapper `@contextslice/cli` (Phase 4): platform-specific optional dependencies
  carrying the binary (esbuild/Biome model) so `npx contextslice` works with no Rust
  toolchain — this recovers the distribution reach Repomix enjoys.
- No Docker image (pointless for a local tool); documented objection in ADR folder.

---

## 14. Technology choices, one line each (full ADRs once code starts)

**The manifest is the single source of dependency truth.** Exact versions and features
are declared once in the root `Cargo.toml`, with per-dependency justification in the ADRs
(ADR-011). This section deliberately no longer restates versions: two documents asserting
them independently is how the `rmcp` mismatch happened, and the CI `docs-drift` job checks
that any version quoted here also exists in the manifest.

The *choices* and the reasoning behind each:

| Choice | Why | Record |
|---|---|---|
| tree-sitter | only credible multi-language parser with Rust-first bindings | ADR-011 |
| Owned `.scm` queries | the tags convention's predicates are silently ignored by core | ADR-012 |
| `rusqlite` + FTS5 | zero-ops, inspectable with the stock CLI, niche-standard | ADR-002 |
| `blake3` | fast content keys for incremental re-parse | ADR-011 |
| `rayon` | data parallelism, order-normalized before it can affect output | ADR-006 |
| `clap` | CLI parsing with derive | ADR-011 |
| `tiktoken-rs` | budget estimation and measurement share one code path | ADR-014 |
| `git2` | no shell-out for git signals | ADR-011 |
| `ignore` / `globset` | ripgrep-grade traversal and ignore semantics | ADR-011 |
| ~~`rmcp`~~ | **not adopted**; no synchronous stdio transport | ADR-013 |
