#!/usr/bin/env bash
set -euo pipefail

cat > ADR-007-apache-2-license.md <<'EOF'
# ADR-007: Apache-2.0

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), verified 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

ContextSlice is a developer tool that people will run against private source code. The
license must be unambiguous for corporate users and must not create doubt about patent
exposure.

## Decision

Apache-2.0 for the whole workspace. No CLA; contributions are inbound under the same
license (CONTRIBUTING.md).

## Consequences

- Explicit patent grant, which MIT lacks — material for a tool adopted inside companies.
- Matches the ecosystem norm for this space: aider, ast-grep, and SCIP are all
  Apache-2.0, so users already have the compliance conversation done.
- **Dependency license compatibility**, verified on 2026-09-15 against crates.io
  metadata: every direct dependency is permissively licensed — `rmcp` Apache-2.0,
  `tiktoken-rs` MIT (should it be adopted), `tree-sitter` and all three grammars MIT,
  `rusqlite` MIT, `ignore` Unlicense OR MIT, `blake3` CC0-1.0 OR Apache-2.0 OR
  Apache-2.0 WITH LLVM-exception, `clap`/`rayon`/`git2`/`serde`/`serde_json`/
  `thiserror`/`toml`/`tempfile`/`assert_cmd`/`predicates`/`proptest` MIT OR Apache-2.0,
  `tracing` MIT, `insta` Apache-2.0. `cargo-deny` enforces this list in CI so a
  copyleft dependency cannot be merged unnoticed.
- The `blake3` CC0-1.0 option is fine but the Apache-2.0 option is what we rely on; the
  dual grant means no attribution obligation either way.

## Alternatives rejected

- **MIT** — no patent grant, and the extra permissiveness buys nothing for a CLI tool.
- **AGPL** — would exclude the internal-corporate use case that is a primary audience.
- **Dual MIT/Apache** — the Rust norm, but adds a compliance question without adding a
  benefit Apache-2.0 does not already provide.

## What would reopen this

A dependency with a copyleft license that is load-bearing and has no alternative. The
CI license gate is the tripwire, and the response would be a superseding ADR explaining
what was accepted and why, not a silent allowlist entry.
EOF

cat > ADR-008-mvp-languages.md <<'EOF'
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
EOF

cat > ADR-009-benchmark-first-class.md <<'EOF'
# ADR-009: Benchmark is a first-class subsystem

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), verified 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

The category's marketing currency is token reduction. Token reduction is only
interesting relative to task success, and "the agent succeeded more often" is an
extraordinary claim that invites scrutiny. Meanwhile every tunable constant in
ALGORITHM.md needs an oracle, or tuning becomes aesthetic drift.

## Decision

A two-tier benchmark is part of the product, not a side project. **Intrinsic**
(gold-context recall/precision per token, per merged PR) runs in CI and gates
regressions. **Extrinsic** (agent task success per token/cost) runs nightly and is the
basis for any public claim. Weights move only with a before/after benchmark delta
attached.

## Consequences

- Corpus governance is committed before extraction runs: pinned SHAs, rule-based
  exclusions applied uniformly, held-out repo for overfitting checks.
- Reports include a mandatory negative-results section — where ContextSlice hurt.
- The publication gate is explicit and pre-marketing: variant D must beat the naive
  dependency crawl on recall@same-tokens **and** beat full-repo on tokens@comparable-recall,
  with CIs excluding zero, on ≥3 repositories. If it fails, the algorithm iterates.
- Engineering cost: the harness, corpus tooling and statistics are real work that ships
  no user-visible feature. It is accepted because without it the project has no compass
  and no credibility.

## Verification (2026-09-15)

The reconnaissance claim that "nobody in this category publishes an extrinsic
agent-outcome benchmark" was treated as a claim to verify rather than a fact to assert,
since it is the sort of statement that becomes false quietly. See ADR-015 for the
outcome and the resulting constraint on how we state it.

## Alternatives rejected

- **Manual spot checks** — not reproducible, not a regression gate, and not evidence.
- **Publishing token-reduction percentages only** — the exact claim the project exists to
  replace; also unverifiable without a task-outcome baseline.
- **Third-party benchmark only (ContextBench/SWE-bench)** — valuable as cross-validation
  and adopted for the extrinsic tier, but we also need a fast intrinsic loop we control,
  and third-party corpora cannot gate our own PRs.

## What would reopen this

Nothing anticipated. If the extrinsic tier proves too noisy to detect effects at n=50,
the response is to raise n and pre-register the primary metric, not to drop the tier.
EOF

cat > ADR-010-no-telemetry.md <<'EOF'
# ADR-010: No telemetry, ever by default

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), verified 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

Usage data would genuinely help prioritize languages, tune weights, and detect failures.
ContextSlice also reads private source code, which makes any data collection a trust
question rather than a metrics question.

## Decision

No telemetry, no update checks, no crash reporting, no phone-home — not opt-out, not
anonymized, not aggregated. The core never opens a network socket, and CI enforces this
with a network-blocked test that fails the build on any socket attempt.

## Consequences

- We lose product analytics permanently. Prioritization comes from the benchmark,
  GitHub issues, and explicit user reports instead.
- The privacy claim is verifiable rather than a promise: no network capability, an
  inspectable SQLite file, and no source text stored at all (only spans and signatures).
- Whenever the MCP adapter lands, its dependency footprint becomes a security-relevant
  decision, because a transitive HTTP client would undermine this guarantee — hence the
  explicit dependency-tree assertion required by ADR-013.

## Alternatives rejected

- **Opt-in telemetry** — the failure mode is a user enabling it once on a work machine
  and forgetting; and maintaining a collection endpoint conflicts with having no service.
- **Anonymized aggregate counts** — still needs egress, still needs a server, still
  cannot be audited by the user.
- **Local-only counters reported by `doctor`** — kept as an option; it collects nothing
  off-machine, so it does not violate this ADR.

## What would reopen this

Nothing. This is a stated product commitment (SECURITY.md §1), and changing it would be
a breaking trust change, not an engineering trade-off.
EOF

echo "wrote 007-010"
