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
