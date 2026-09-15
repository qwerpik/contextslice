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
