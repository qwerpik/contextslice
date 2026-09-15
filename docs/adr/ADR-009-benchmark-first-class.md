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
