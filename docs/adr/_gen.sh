#!/usr/bin/env bash
# one-shot generator for the remaining bootstrap ADRs
set -euo pipefail

cat > ADR-002-sqlite-as-the-only-store.md <<'EOF'
# ADR-002: SQLite (rusqlite + FTS5) as the only store

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), verified 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

The index must hold files, symbols, references, imports, and a weighted edge graph, be
incrementally updatable, survive being copied and inspected, and require no server or
daemon for a single-user local tool (ARCHITECTURE.md §2.6).

## Decision

One SQLite database per repository at `.contextslice/index.db`, accessed through
`rusqlite` with WAL mode and FTS5 for symbol search. No second store, ever.

## Consequences

- Zero operations: no daemon, no port, no upgrade path to manage.
- The index is inspectable with the stock `sqlite3` CLI, which is a feature for users
  debugging why a file was or was not selected.
- `PRAGMA quick_check` gives `doctor` a real corruption test.
- FTS5 provides BM25 ranking for signal S4 (ALGORITHM.md §5) without a bespoke index.
- `rusqlite` is pinned to 0.40.2 with the `bundled` feature, so the SQLite version does
  not depend on the host's shared library. This trades compile time for reproducibility,
  which is the right trade for a tool that must behave identically on three platforms.
- Incrementality keys on the blake3 content hash of each file; unchanged hashes skip
  re-parsing.

## Alternatives rejected

- **A bespoke on-disk format** — explicitly on the over-engineering guard list
  (MASTER_PLAN.md §8.2). We would reimplement durability, transactions and migrations
  badly.
- **An embedded key-value store (sled/LMDB)** — no query language, no FTS, and we would
  hand-roll every lookup `doctor` and `inspect` need.
- **A file-per-symbol or JSON sidecar layout** — no atomic snapshot semantics, poor
  incremental story.

## What would reopen this

Measured evidence that SQLite is the bottleneck at the 100k-file target (MASTER_PLAN.md
§9). The performance plan already assumes batched transactions and WAL; if that proves
insufficient, the answer is a read-optimized derived structure built *at slice time*,
not a different store.
EOF

cat > ADR-003-approximate-reference-graph.md <<'EOF'
# ADR-003: Approximate reference graph, not precise

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), **re-verified 2026-09-15** |
| **Milestone** | Bootstrap |

## Context

Selection quality depends on knowing which files relate to which. The tempting
shortcut is a precise call graph. The reconnaissance pass proposed using GitHub's
`stack-graphs` for name resolution.

## Decision

Build an **approximate** reference graph: resolve imports structurally per language,
then bind references by name against the candidate definitions reachable through those
imports, scoped by module and container. Everything unresolved is marked unresolved with
a reason, and the unresolved share is published as a resolution rate. Precise
SCIP/LSP-backed edges remain a later, optional "deep mode".

## Consequences

- Edges are labeled approximate in the docs, the `--explain` output, and the header —
  never implied to be IDE-grade.
- `cs-resolve` carries `Resolution::Unresolved { reason }` variants for alias, dynamic,
  malformed, escape, and unsupported cases (ARCHITECTURE.md §11).
- The resolution rate becomes a published per-corpus metric, so approximation quality is
  measured rather than asserted.
- Mis-binding is damped rather than hidden: sqrt-damped `ref_def` weights, container
  scoping, and a 256-candidate cap on uninformative names.

## Verification (2026-09-15)

`stack-graphs` was re-checked against the GitHub API: the repository returns
`"archived": true` with `pushed_at: 2025-09-09`. The original finding holds — it was
archived in September 2025 and is not a viable building block.

## Alternatives rejected

- **stack-graphs** — archived upstream; depending on it would mean adopting an
  unmaintained resolver as a load-bearing component.
- **tree-sitter-tags** — low maintenance, and its tags convention relies on query
  predicates the core Rust binding does not implement (see ADR-012).
- **SCIP/gopls/rust-analyzer as the always-on baseline** — heavyweight per-language
  sidecars with their own startup and correctness profiles; too fragile and slow for an
  always-on local index. Retained as the Phase 6 deep mode.

## What would reopen this

The intrinsic benchmark showing the approximation caps recall below the Phase 3 gate
(BENCHMARK.md §3.6), or a maintained cross-language resolver appearing that is cheap
enough to run always-on.
EOF

cat > ADR-004-bounded-walk-before-pagerank.md <<'EOF'
# ADR-004: Bounded graph walk before personalized PageRank

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), verified 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

Aider's repo map ranks a file graph with personalized PageRank run to convergence,
finishing with a binary search over tag count to fit roughly a 1k-token map
(ALGORITHM.md §15). That is proven prior art and the obvious thing to copy.

## Decision

Use a **2-hop bounded, order-normalized walk** with hop decay 0.5 and a 64-file frontier
cap as the baseline ranking. Keep a personalized-PageRank variant behind a flag purely
for benchmark comparison.

## Consequences

- Ranking is `O(frontier × degree)` and trivially deterministic: no iteration to
  convergence, no damping-factor sensitivity across repo topologies, no float
  accumulation whose result depends on node ordering.
- Every contribution is explainable in one sentence — "0.42 came from
  `auth/session.go` via `import_out` 0.6 × hop-decay 0.5" — which is what `--explain`
  renders and what makes a slice auditable.
- We give up whatever recall PageRank's global view would buy. That is a deliberate bet,
  and the benchmark is the arbiter: if PageRank measurably wins on recall@tokens, this
  ADR is superseded *with data*.

## Alternatives rejected

- **Personalized PageRank as the baseline** — harder to reason about across topologies,
  harder to render as an explanation, and iteration-to-convergence is an awkward fit with
  the byte-stability requirement in ADR-006.
- **Pure BM25 without graph propagation** — this is baseline variant C in
  BENCHMARK.md §3.2, and beating it is a publication gate. If a graph walk cannot beat
  BM25, the graph is not earning its complexity.

## What would reopen this

The Phase 3 intrinsic benchmark showing the PageRank variant beats the bounded walk on
`recall_strong@8k` by a margin that survives the CI regression gate.
EOF

cat > ADR-005-six-representation-levels.md <<'EOF'
# ADR-005: Six representation levels (L0–L5) with a demotion ladder

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), verified 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

Context tools in this category either dump whole files or emit a single
signatures-only map. Repomix's `--token-budget` **errors out** when a pack exceeds the
budget, and uithub truncates by size. A tool whose premise is "smallest sufficient
slice" cannot treat a budget as an alarm.

## Decision

Represent every candidate file at one of six levels — L0 excluded, L1 path, L2 names,
L3 declarations, L4 selected bodies, L5 full source — assign levels from the ranking,
then fit the budget by greedy demotion (maximizing gain-per-loss) followed by
opportunistic promotion.

## Consequences

- Mixed granularity in a single artifact becomes the product's differentiation: full
  source where the agent will work, skeletons where it matters, path-only for the rest.
- The budget is a *hard invariant*, property-tested against adversarial inputs, because
  fitting and measuring share one tokenizer (ADR-014).
- Higher implementation cost than a boolean include/exclude decision, and four more
  tunable constants (the 0.75/0.5/0.3/0.15 bands) that may only move with a benchmark
  delta (ALGORITHM.md §13).
- Exactly one failure is permitted: when header + tree + minimal seed skeletons exceed
  the budget, the tool exits 3 and *suggests the smallest workable budget*. That is the
  difference between a constraint and an alarm.

## Alternatives rejected

- **Binary include/exclude** — loses the middle ground that makes a fixed budget usable
  on a large repository.
- **Single-level output (signatures only, or full files only)** — this is the Repomix and
  Aider shapes respectively; both are already shipped and neither fits a budget
  gracefully.
- **Repomix's "error past budget"** — hostile in exactly the case the tool exists for.

## What would reopen this

Evidence that agents do not benefit from the middle levels — i.e. that L2/L3 entries are
never read and only cost tokens. The intrinsic benchmark's precision metric
(BENCHMARK.md §3.3) is designed to detect this.
EOF

cat > ADR-006-deterministic-core.md <<'EOF'
# ADR-006: Deterministic core; semantic/LLM strictly optional

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), verified 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

Selection could be improved by embeddings or an LLM re-ranker. Both would introduce
non-determinism, network egress, per-run cost, and an inability to reproduce a published
number — in a tool whose audience runs it on private repositories.

## Decision

Stages 1–5 of selection are pure functions of (index snapshot, task, flags). No network
access, no randomness, no wall-clock in the artifact. Same snapshot + task + flags
produces byte-identical output. A semantic layer may exist later only behind
`--semantic`, strictly additive, and only if it beats the deterministic baseline on the
intrinsic benchmark by ≥5 points.

## Consequences

- Reproducible slices make benchmark claims checkable and make security review possible:
  a reviewer can diff exactly what a repository tried to inject into agent context.
- An index snapshot id is frozen into every slice header, so a slice is reproducible
  even after unrelated commits.
- Parallelism is permitted only where results are order-normalized before they can
  affect output (ARCHITECTURE.md §7). The scanner implements this literally: it sorts by
  path before hashing, so two runs over one tree produce identical bytes regardless of
  scheduling.
- We forgo recall that semantic ranking might buy, until it is measured.

## Alternatives rejected

- **Embeddings-first retrieval** — requires a provider or a local model, a vector store,
  and network or GPU; all three are on the explicit not-yet list (MASTER_PLAN.md §12).
- **LLM query expansion in the core** — makes output non-reproducible and couples a
  local-first tool to an API key.
- **Determinism-only-approximately (stable within a run)** — would forfeit the golden
  tests and the regression gate that the whole benchmark design rests on.

## What would reopen this

The Phase 5 gate: a semantic variant that beats deterministic recall by ≥5 points on the
intrinsic benchmark, in which case it ships *enabled*. Falling short of that, it ships
disabled — the guardrail is in ALGORITHM.md §14, not left to judgement.
EOF

echo "wrote 002-006"
