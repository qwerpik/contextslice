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
- **Outputs:** ordered stream of `ScannedFile { path, lang, size, mtime, blake3 }`.
- **Algorithms/data:** `ignore` crate walker (parallel), extension+shebang→language map,
  blake3 hashing (streaming, 1 MiB chunks).
- **Storage:** none (feeds cs-index).
- **Performance:** traversal+hash dominated; 10k files in seconds; hashing is the
  largest single cost and is parallelized.
- **Failure modes:** symlink loops (cycle detection), unreadable files (skip + record),
  files > 1 MiB (parse-skip, still listed at L1 with size), files > 50 MiB (read-skip).
- **MVP:** yes.

### 4.2 cs-extract

- **Purpose:** turn source files into structured facts.
- **Inputs:** file bytes + language.
- **Outputs:** `ExtractedFile { defs: Vec<Def>, refs: Vec<Ref>, imports: Vec<Import>,
  sig_spans, doc_spans }` where `Def { name, qual_name, kind, span, container, signature,
  doc }` and `Ref { name, span, container }`.
- **Algorithms/data:** tree-sitter 0.27.x (pinned), **owned `.scm` query sets per
  language**. Queries capture:
  `@def.name/@def.kind/@def.sig/@def.doc`, `@ref.name`, `@import.module/@import.names`.
  Parse timeout (default 250 ms/file) and node-count cap guard against pathological
  grammars.
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
- **Performance:** tree-sitter parses at roughly Babel-class speed; parallel by file
  (rayon). This is the cold-index bottleneck; budget ~10 MB/s/core.
- **Failure modes:** grammar errors (tree-sitter error nodes — extract what resolved);
  timeout (mark `parse_partial`); unknown language (no adapter → heuristic path: no defs,
  content terms only).
- **MVP:** yes (Go first, then TS, Python).

### 4.3 cs-resolve

- **Purpose:** convert per-file facts into cross-file structure.
- **Inputs:** `ExtractedFile` stream + repo layout facts (go.mod, tsconfig.json,
  package.json workspaces, pyproject/setup).
- **Outputs:** file `edges(src, dst, kind, weight)` where kind ∈ `import_out`,
  `import_in` (derived reverse), `ref_def` (aggregated per file pair, sqrt-damped
  weight), `test_affinity`; plus per-ref resolved def id or `unresolved`.
- **Algorithms/data:** per-language resolvers (LANGUAGES.md §6): Go module-relative
  paths; TS `paths`/`baseUrl`/`node_modules`/workspace walking with re-export chain
  following (depth-capped, cycle-safe); Python pyright-style static rules. Approx symbol
  binding = name match restricted to defs exported/importable from resolved files,
  scoped by module + container class.
- **Storage:** none (feeds cs-index).
- **Performance:** in-memory maps; O(refs × candidate-defs-per-name) worst case, capped
  by per-name candidate lists (names with >256 candidates are skipped as uninformative —
  same insight as aider's ">5 files" dampener).
- **Failure modes:** unresolved alias (marked, counted, published as resolution-rate %);
  ambiguous name (container scoping, else unresolved); re-export cycles (depth cap 8).
- **MVP:** yes.

### 4.4 cs-index

- **Purpose:** persistent, incremental store of everything above.
- **Inputs:** scanner/extract/resolve outputs.
- **Outputs:** query APIs: FTS5 symbol search, file/symbol lookup, edge CSR, snapshot ids.
- **Algorithms/data:** SQLite via rusqlite, WAL mode, batched transactions (10k rows).
  Incrementality keyed on blake3 content hash: unchanged hash ⇒ skip parse; changed ⇒
  re-extract that file and re-resolve its edges (resolution is per-file-pair, so a
  changed file only rewrites edges where it is `src`; `dst`-side invalidation is handled
  by a nightly `--rebuild` and by `doctor` detection).
- **Storage:** `.contextslice/index.db` at repo root (self-contained, easy to inspect
  with the sqlite3 CLI — a feature, not an accident). Schema in §5.
- **Performance:** 10k files cold < 60 s; incremental < 2 s for typical dirty sets;
  index size 5–15 MB/10k files.
- **Failure modes:** corruption (detected via `PRAGMA quick_check` at open in doctor
  mode; remediation = rebuild), schema version mismatch (migrate forward; refuse
  backward with instructions), concurrent writers (file lock; second invocation fails
  fast with a clear message).
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

## 5. Data model (SQLite schema, normative sketch)

```sql
-- schema_version in meta; migrate forward only
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- key 'schema_version', 'snapshot_id', 'git_head', 'created_at'

CREATE TABLE files (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,          -- repo-relative, '/'-separated, normalized
  lang TEXT NOT NULL,                 -- 'go','ts','tsx','python',... 'unknown'
  hash BLOB NOT NULL,                 -- blake3 of content
  size INTEGER NOT NULL,
  mtime INTEGER NOT NULL,
  parse_status TEXT NOT NULL,         -- ok | partial | skipped | unsupported
  tokens_est INTEGER                  -- whole-file estimate, for map mode
);

CREATE TABLE symbols (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  qual_name TEXT NOT NULL,            -- module-path-qualified
  kind TEXT NOT NULL,                 -- func|method|class|struct|interface|type|const|var|enum...
  line INTEGER NOT NULL, end_line INTEGER NOT NULL,
  signature TEXT,                     -- rendered one-liner for L2/L3
  container_id INTEGER REFERENCES symbols(id),  -- enclosing symbol
  doc TEXT                            -- first doc-comment paragraph
);
CREATE INDEX idx_symbols_name ON symbols(name);
CREATE INDEX idx_symbols_file ON symbols(file_id);

CREATE TABLE refs (
  id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  line INTEGER NOT NULL,
  container_id INTEGER REFERENCES symbols(id),
  resolved_symbol_id INTEGER REFERENCES symbols(id)  -- NULL = unresolved (approx graph)
);
CREATE INDEX idx_refs_name ON refs(name);

CREATE TABLE imports (
  file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  raw TEXT NOT NULL,                  -- specifier as written
  resolved_file_id INTEGER REFERENCES files(id),    -- NULL = external/unresolved
  kind TEXT NOT NULL                  -- import|export-from|require|dynamic...
);

CREATE TABLE edges (
  src INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  dst INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,                 -- import_out|ref_def|test_affinity
  weight REAL NOT NULL,
  PRIMARY KEY (src, dst, kind)
);

CREATE VIRTUAL TABLE symbols_fts USING fts5(name, qual_name, content='symbols');
```

In-memory (built at slice time, never persisted): CSR adjacency over `edges` — 100k
edges materialize in <50 ms. `import_in` edges are derived (reverse of `import_out`) at
CSR build time rather than stored, to halve writes.

Determinism note: ids are assigned after a deterministic sort (by path), so the same
repository content yields the same database rows and the same slice bytes regardless of
parallel scheduling.

---

## 6. Index lifecycle

- **Create:** `contextslice index` (or first slice). Walk → parse → resolve → one
  transaction per 1k files → snapshot id computed (blake3 over sorted `(path, hash)`
  pairs) → stored in `meta`.
- **Update:** re-walk; for changed hashes, re-extract and rewrite that file's rows and
  its outgoing edges; recompute snapshot id. Typical cost ∝ dirty set.
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
| Cold 10k < 60 s | rayon parse (≈10 MB/s/core), batched inserts, skip vendored/lockfiles by default |
| Cold 100k < 12 min | same + 1 MiB parse cap, candidate-list caps in resolver |
| Incremental < 2 s | content-hash diffing; only dirty files re-parsed/edge-rewritten |
| Warm slice < 1 s | snapshot-pinned reads; CSR built once per invocation; FTS5 with a prepared query |
| Startup < 50 ms | lazy grammar loading (only languages present); clap minimal; no runtime config parsing beyond one TOML |
| RSS < 1 GB @50k | streaming extraction (never hold all CSTs; extract facts, drop tree); SQLite batches |
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
| Parse timeout / error nodes | partial extraction, `parse_status=partial` | warning row in `doctor`, marker in header |
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
