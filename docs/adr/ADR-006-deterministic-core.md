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
