# ADR-023: cs-select v1 contract freeze

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-20 |
| **Milestone** | MASTER_PLAN §15 step 6 (`cs-select` v1) |
| **Follows** | ADR-004 (bounded walk), ADR-005 (L0–L5 ladder), ADR-014 (tiktoken-rs), ADR-021 (streaming index, D10 FTS5 deferral), ADR-022 (corrective patch — index coherent) |

## Context

`cs-index` is complete and hardened: `Ready` means a coherent index, incremental
updates are package-scoped and crash-repairing, and the scale gates hold
(13.1 s cold @10k, 417.72 MiB RSS @50k). Step 6 is the selection engine — the
product's core. `docs/ALGORITHM.md` (v1.0, 2026-09-14) is the normative
end-to-end specification: task parsing → seeding → 2-hop propagation → level
assignment → budget fitting. It leaves four doc→code decisions open, which this
ADR freezes before implementation. Nothing in ALGORITHM.md is amended; the
decisions below only *place* the specified behavior in the code.

## Decisions

### D1 — Scope of v1 (the step-6 boundary)

v1 implements ALGORITHM.md stages 1–5 as pure functions over a loaded
snapshot, producing `SlicePlan { path, level, score, symbols, reasons }`
entries plus the map-mode fallback and the `cs_select::tuning` constants
module. Explicitly OUT (later steps, no partial work):

- rendering of any level to bytes — step 7 (`cs-render`);
- CLI end-to-end wiring incl. the stdout-sacred emission contract and the
  `"task"` sugar — step 8; `contextslice slice` keeps failing with
  `NotImplemented` and writes nothing;
- git recency signal S7 — step 12 (`cs-git`); the weight stays in the tuning
  table, its value is fixed at 0 until cs-git lands, so `--no-git` and "no
  git" are the same code path from day one;
- the optional semantic layer — Phase 5 (ALGORITHM §14);
- stack-trace *extraction* beyond what task parsing already yields: a token
  that resolves to an existing indexed path (after stripping a `:line`
  suffix) enters as a pinned seed of the S9 class — the ALGORITHM §10
  stack-trace row is covered by this rule, nothing more.

### D2 — Snapshot read-side seam (additive, read-only)

`cs-index` gains one read API, `IndexDatabase::load_selection_snapshot()` →
`SelectionSnapshot`: plain owned structs (files: path/lang/size/package;
symbols: file/path/name/kind/exported/line/signature/doc/container;
edges: src-path/dst-path/kind/weight; `snapshot_id`), loaded by straight
SELECTs over the existing tables. No schema change, no FTS table in the index
database, selection never writes the index — ADR-021 D10's deferral is
honored by placing FTS5 entirely on the select side (D3). `cs-select` already
depends on `cs-index`; no new dependency edges.

### D3 — FTS5 in-memory, built per selection

Verified 2026-09-20: the workspace's `rusqlite` bundled build compiles with
`-DSQLITE_ENABLE_FTS5` (libsqlite3-sys 0.38.2 `build.rs:159`), so S1/S4 run
against a real FTS5 table. That table is an **in-memory** virtual table
created at selection start and populated from the snapshot's symbol rows; the
FTS5 index lives exactly as long as the selection. A persistent FTS cache
(written next to the index) is rejected for v1: it would make selection a
writer of index-adjacent state, and its worth is a measurement question, not
an article of faith — if the measured build+query time at 10k shows the
per-selection rebuild dominating the <100 ms warm gate, a snapshot-keyed
persistent cache becomes its own ADR with numbers attached.

FTS5 `bm25()` returns negative values (better match ⇒ more negative); the
`b/(b+3)` normalization of ALGORITHM S4 consumes `-bm25(...)`. This sign is
pinned by a unit test so it cannot silently flip.

### D4 — Cost model: over-estimate, never under-estimate

Budget fitting needs per-file per-level token costs before anything is
rendered. The estimator shares one code path with the renderer's measurement
(ADR-014: tiktoken-rs, o200k default) and is conservative by construction:

- **L1/L2/L3** — encoded exactly from snapshot rows (tree line; symbol names
  + kinds; signatures + doc first lines): the same strings the renderer will
  emit, tokenized directly. No disk I/O.
- **L4/L5** — exact: the candidate's source is read from disk and tokenized
  at estimation time. This is bounded by construction: only level-assigned
  files (frontier-capped, ALGORITHM §6) are read, one at a time, streamed —
  the selection stays O(bounded), never O(repo).
- **Slack** — each per-file cost carries a fixed multiplicative slack (v1:
  +15%) covering render furniture (line numbers, `path:line` anchors, elision
  markers), and the fixed header/tree/footer reserve is frozen at the top of
  ALGORITHM's 2–8% band (v1: 8%) until measurement says otherwise.
- The step-6 property is **estimated plan cost ≤ budget, always**; the
  rendered-artifact ≤ budget cross-check becomes a hard property in step 7,
  where the renderer exists to measure. Staleness (file changed between index
  and selection) is not selection's problem — cost estimation reads current
  disk bytes for bounding only; hash verification and the `possibly-stale`
  marker are cs-render's contract (ARCHITECTURE §4.7).

### D5 — Graph traversal over directed stored edges

The index stores directed edges (`edges.src → edges.dst`). Stage 3's
asymmetric weights are traversal directions over those rows: `import_out`
(0.6) follows the stored direction, `import_in` (0.45) traverses it backwards,
`ref_def` follows the stored direction with `0.5·√(count/(count+8))` damping,
`test_affinity` (0.7, ×1.25 under `test_bias`) is bidirectional. The walk's
adjacency is two in-memory CSR-style arrays (out and in) built per selection
from the snapshot's edge list — an internal representation, not persisted
anywhere.

### D6 — Determinism and the property-test floor

ALGORITHM §12 is binding: every map iteration in stages 2–5 is over
path-sorted keys or explicit `(score desc, path asc)` tie-breaks; the tuning
constants live only in `cs_select::tuning` as a named table. The step-6 test
floor (property tests, MASTER_PLAN §8.1):

1. **Input-order independence** — shuffling the snapshot's vector inputs
   yields the identical `SlicePlan` (this is the honest stand-in for "two
   runs", since same-process repetition cannot randomize iteration order that
   must not exist).
2. **Budget never exceeded** — across randomized tasks/budgets.
3. **Pinned seeds never below L2** while invariant 2 is achievable.
4. **≥3 files at ≥L2 whenever any seed exists.**
5. **Empty seed ⇒ map mode** (ALGORITHM §10 first row), tested.
6. **Impossible budget** ⇒ the one permitted failure, with the smallest
   workable `--tokens` suggested (exit-code semantics land with the CLI in
   step 8; the library error carries the suggestion now).

### D7 — Goldens and serialization

The five golden fixture tasks (step 6 done-when) run against an indexed
`fixtures/go-resolve` and pin **`SlicePlan` JSON** (serde, stable field
order) — not rendered bytes, which are step 7's goldens. `SlicePlan`
serialization is part of the public contract from day one (harnesses and the
step-8 CLI both consume it); field additions are additive, renames are
breaking and need an ADR note.

## Consequences

- `cs-select` becomes the determinism-property crate ARCHITECTURE §4.6
  promised; `cs-index` gains one read-only seam with no schema impact (v2
  stays frozen; no version bump).
- Selection on an empty/missing/stale index fails loudly with the existing
  error shapes; it never triggers an index build (mirrors the MCP
  staleness-hint rule, ARCHITECTURE §4.9).
- The <100 ms warm gate is *measured* at 10k in this milestone and reported;
  a miss is a finding to act on, not a number to restate from the docs.

## What was deliberately NOT done

- No persistent FTS cache, no git priors, no rendering, no CLI emission, no
  PageRank variant flag (ADR-004 leaves it behind a flag *for benchmark
  comparison* — the flag arrives with the benchmark that would justify it),
  no semantic layer.

## What would reopen this

- Measured per-selection FTS5 build dominating the warm gate (→ persistent,
  snapshot-keyed cache ADR).
- Step-7 render measurement breaking the slack assumption of D4 (→ re-freeze
  the slack with numbers).
- TS/Python adapters (steps 10–11) needing snapshot fields Go does not have
  (→ extend `SelectionSnapshot` additively; never rename).
