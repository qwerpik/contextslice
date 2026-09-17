# ADR-021: cs-index streaming architecture — facts-first, three-pass, SQLite as the fact store

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-17 |
| **Milestone** | MASTER_PLAN §15 step 5 (cs-index), milestones M0–M8 |
| **Follows** | [ADR-002](ADR-002-sqlite-as-the-only-store.md), [ADR-006](ADR-006-deterministic-core.md), [ADR-017](ADR-017-extraction-contract-as-built.md), [ADR-019](ADR-019-foundation-recovery.md), [ADR-020](ADR-020-ref-qualifier-addendum.md) |

## Context

The Foundation Recovery measurements (`docs/benchmarks/scaling_report.md`)
settled the two numbers that constrain this design. Holding the entire
extracted and resolved graph in memory — the resolver's `ResolveSnapshot` →
`prepare` → `ResolvedRepo` pipeline as built through step 4 — peaks at
**3.30 GiB RSS on a 50k-file synthetic repository at verified gin density**,
breaching ARCHITECTURE §8's "<1 GB @50k" target threefold. Meanwhile the
cold pipeline is fast: **9.72 s for 10k files**, dominated by extraction
(9.1 s of it), well inside the 60 s cold-index budget. The memory problem
and the performance budget pull in opposite directions: the fix must remove
repo-proportional *retention* without adding per-file overhead that would
jeopardize the cold target.

The measured peak decomposes into three specific materializations, each
needing its own antidote: the `Vec<(path, ExtractedFile)>` snapshot
(991 MiB stage), `prepare`'s triplication of every `DefLoc` across the
`defs`/`exported`/`methods` maps (1.90 GiB stage), and the accumulated
`ResolvedRepo` with its edge and multiplicity buffers (3.30 GiB peak).

One further fact forces the data flow: incremental re-resolution of a
dirtied package needs the *references of its unchanged files* — without
reparsing them. Raw facts must therefore live somewhere durable. Since
SQLite is the only store (ADR-002), the facts belong there.

## Decisions

**D1 — Facts-first, three-pass pipeline.** Pass 1 (fact pass): scan →
per-file extract → persist raw facts (`files`, `symbols`, `refs`,
`imports`, `manifests`) in transactions of ≤1,000 files; at most one
`ExtractedFile` in memory at a time. Pass 2 (index pass): three streaming
SELECTs build the only repo-proportional memory — a compact integer-keyed
index of **exported definitions only** (`name → [(file_id, symbol_id,
kind)]`), the package table, and the module list — and compute
`snapshot_id`. Pass 3 (resolve pass): packages in sorted `(dir, name)`
order; per package, load own facts, resolve, and **write through** derived
rows (`imports.resolved_*`, `bindings`, `binding_targets`, `edges`) in one
transaction. Nothing repo-sized is ever accumulated in memory.

**D2 — SQLite is the source of truth for raw extracted facts.** `files`,
`symbols`, `refs`, `imports`, and `manifests` are raw facts: written once
per file state, replaced atomically on re-extraction. Import resolutions,
bindings, and edges are *derived projections*, persisted for incremental
and selection speed but always re-derivable from raw facts plus the
package/module tables; `doctor` may verify the projection at any time.

**D3 — Per-file and per-package transaction atomicity with a two-state
lifecycle; no staging tables, no generation IDs.** WAL journaling plus
SQLite's transaction atomicity make every committed state valid: a file's
hash row and its fact rows are replaced in one transaction, and a package's
derived rows in one transaction. `meta.index_state` is `'empty'` or
`'building'` for a fresh build (slices refuse; `contextslice index`
resumes — the hash-driven fact pass skips completed batches for free) and
`'ready'` for a queryable index. Incremental runs stay `'ready'`
throughout: a crash mid-run leaves a coherent index as of an earlier
prefix. Staging/generation schemes would simulate validity that
transactions already provide.

**D4 — Bindings replace `refs.resolved_symbol_id`.** Binding targets are
stored as `(ref_id → target_file_id, target_qual_name, target_kind)` in
`bindings`/`binding_targets`, indexed on `target_file_id`. Rationale: def
rowids churn on re-extraction (delete+insert), so cross-file rowid
references go stale; path+qual_name targets survive unrelated churn *and*
make the reverse-invalidation query — which source files bind into package
P? — one indexed lookup.

**D5 — Package- and reverse-level invalidation retires the §4.4
src-only-rewrite assumption.** The previous claim — "a changed file only
rewrites edges where it is `src`" — was already flagged as wrong during
Foundation Recovery: changing a definition affects incoming edges from
files that did not change. The contract is now: a modified file re-extracts
and re-resolves its whole package P (own-package defs and method uniqueness
may have moved); then sources whose bindings point into P and no longer
resolve against P's new def set are re-resolved within their own package
contexts (the indexed reverse lookup of D4). Content hashing (blake3) is
always computed on every run; `mtime` is recorded for diagnostics and never
trusted as a skip shortcut (`touch`, git checkouts, and archive extraction
all invalidate it).

**D6 — Frozen schema deltas** (normative text: ARCHITECTURE §5): a
`packages (id, dir, name, lang)` table with `files.package_id` FK replaces
`files.package_name`; `symbols.container` and `refs.container` are TEXT
qual-names (facts insert with zero id lookups; no churn); `refs` gains
`start_byte`/`end_byte` (re-derivation and audit sampling); `imports`
gains a rowid, `ordinal`, and the resolution columns
(`resolved_dir`/`resolved_file` exactly-one convention for
directory-packages vs file-modules, plus `unresolved_reason`); a
`manifests (path, content)` table carries module manifests so resolution
can rerun without the filesystem; `meta` drops `created_at` (not a
function of input; it would break byte-identical semantic dumps) and adds
`schema_version`, `index_state`, `snapshot_id`, and `config_fingerprint`
(parse/read caps + grammar/query pins — a mismatch means file
classification may have changed and `doctor` offers a rebuild). The
partial index `symbols(name, file_id) WHERE exported = 1` is the binding
hot path. `PRAGMA user_version` mirrors `meta.schema_version` for `sqlite3`
CLI inspection.

**D7 — Determinism contract: semantic dump, not file bytes.** Batches are
contiguous slices of the scanner's sorted path list; packages process in
`(dir, name)` order; rowids therefore assign in sorted-path order, so a
fresh full index of an identical tree yields a byte-identical *ordered
semantic dump* (`SELECT * ORDER BY` over every table) and an identical
`snapshot_id`. Physical `.db` bytes are explicitly not promised (WAL
checkpointing, freelist layout). `PRAGMA synchronous=NORMAL` under WAL:
consistent after crash, may lose the last commits on power loss — the
correct trade for a local tool. Foreign keys are ON per connection.
Writer-writer contention fails fast (no busy timeout): a second writer
gets a clear "another process is indexing" error, per ARCHITECTURE §4.4.

**D8 — Batch sizes:** ≤1,000 files per fact transaction (≈90k rows/txn at
measured density — well under a second of WAL growth; rollback discards one
batch); one transaction per package during resolution (≤64 packages or
≤4k files per transaction, whichever comes first, so tiny packages do not
degenerate into per-3-file commits). These are reasoned defaults, not
tuned constants; both are revisited only on a measured miss.

**D9 — cs-resolve keeps its logic; the feeding seam changes.** The
resolver's per-file core is exposed as `resolve_facts(&FileFacts)` where
`FileFacts` is exactly what extraction and a SQLite row-load both produce
(path, package name, imports, refs); the package index gains a
constructor from row tuples. The snapshot-based `resolve()` remains and is
reimplemented *on top of* the same seam, so the fixture matrices and goldens
stay the authoritative behavior spec. The trait is **not** made
object-safe (ADR-019 rejected dynamic dispatch; per-language enum
dispatch is the project shape).

**D10 — FTS5 is deferred to cs-select (MASTER_PLAN §15 step 6).** It is
selection-time data. Keeping virtual-table maintenance out of step 5
protects the cold-index budget and avoids write amplification during the
milestone that must prove ingestion and incrementality. The schema freeze
already reserves the raw text and line/byte spans FTS5 will need.

**D11 — The index stays approximate and local.** No compiler, LSP, or
`go list` involvement (ADR-003/ADR-018 unchanged); no network (ADR-010);
bundled SQLite only; the index directory is repo-local and treated as
trusted-as-the-repo (opening a planted database is guarded by
`schema_version` verification and integrity checks, documented in
SECURITY.md).

## Consequences

- 50k-file indexing memory drops from a measured 3.30 GiB to a targeted
  **< 512 MiB** (gate: measured, not estimated — the exported index is the
  only repo-proportional structure, ~60–150 MiB at 50k).
- Cold 10k stays comfortably inside 60 s (extraction's measured 9.1 s
  dominates; SQLite writes add a bounded few seconds; no parallelism needed
  — it remains a measured-miss option only).
- `refs.resolved_symbol_id` and `files.package_name` die before any
  released format depends on them; the schema-version policy is
  **mismatch ⇒ rebuild** until a released format exists (no migration
  machinery before it is needed).
- `doctor` gains cheap, precise invariants: projection re-derivability
  (D2), the reverse-invalidation query as a dangling-binding detector
  (D4/D5), and hash/skip/status consistency rules.
- Golden testing moves up a level: a fresh index of the fixture tree has a
  byte-identical ordered semantic dump across runs — the index-level
  successor of the edge goldens.

## What would reopen this

- A scale gate failure that the exported-index representation cannot absorb
  (name interning is the first remedy; a facts-in-temp-table resolve pass
  is the last).
- A real need for concurrent writers (today: fail fast, by design).
- A second language whose imports are neither directory- nor file-shaped —
  the exactly-one target convention would need a third column or a kind
  tag, which is a schema change and a new ADR.
- A released on-disk format that users persist across upgrades — at that
  point migration machinery earns its complexity.
