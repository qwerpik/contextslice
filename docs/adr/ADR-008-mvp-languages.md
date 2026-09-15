# ADR-008: MVP languages: Go, TypeScript/JavaScript, Python

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), verified 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

Language support costs an extraction adapter, a resolver, golden fixtures, and benchmark
tasks. With limited effort, breadth and quality trade off directly.

## Decision

Ship three Tier 1 adapters at v0.1: **Go** first (the reference adapter), then
**TypeScript/JavaScript**, then **Python**. Everything else is detected and labeled but
runs in heuristic mode.

## Consequences

- Go goes first because module-relative imports are filesystem-shaped, making it the
  cheapest place to prove extraction → resolution → selection → rendering end-to-end
  (MASTER_PLAN.md §14).
- TypeScript ships despite having the hardest resolution problem (path aliases,
  re-export chains, `node_modules`), because dodging it would make the tool a toy for the
  audience that uses coding agents most. Its degradation path is explicit: unresolved
  aliases are recorded and published as a resolution rate.
- Python unlocks the SWE-bench-family extrinsic tasks (BENCHMARK.md §4.2), so it pays for
  itself twice.
- Three adapters, not one: a single adapter proves nothing about whether the
  `LanguageAdapter` seam is real, and six collapses quality.

## Verification (2026-09-15)

All three grammars were confirmed loadable and parseable against the current runtime
before this ADR was accepted; the check is now a test (`cs-extract`
`grammars_load_and_parse`), so an ABI regression fails CI instead of a user's first run.

One measurement changed the implementation: the TypeScript and TSX grammars are **not**
interchangeable, and neither is a superset — `LANGUAGE_TYPESCRIPT` cannot parse JSX,
`LANGUAGE_TSX` cannot parse angle-bracket assertions. The scanner already distinguishes
`.ts` from `.tsx`, so the correct grammar is chosen by extension, and a test pins that
choice. A `.ts` file that actually contains JSX is not guessed at: it parses partially
and is labeled `partial`.

## Alternatives rejected

- **Go only for v0.1** — would not exercise the adapter seam at all.
- **Adding Rust/Java/C# in v0.1** — Tier 2; each needs resolver work that does not
  improve the core engine's proven value.
- **C/C++** — include resolution requires build configuration; the value-to-effort ratio
  is wrong before Tier 2 proves the pattern (LANGUAGES.md §4).

## What would reopen this

Benchmark or user data showing agent usage concentrated elsewhere, or a Tier 2 language
whose adapter turns out to be a one-day port once the seam is stable.
