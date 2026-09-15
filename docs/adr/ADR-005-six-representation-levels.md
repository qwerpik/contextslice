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
