# ADR-014: tiktoken-rs for budget measurement

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-15 |
| **Milestone** | Bootstrap (dependency verified); used from step 6 |

## Context

The product's central invariant is that the rendered artifact's measured token count
never exceeds the budget (ALGORITHM.md §8). That is only a real invariant if **estimation
and measurement use the same code**. If budget fitting estimated with one tokenizer and
the renderer measured with another, "fits" would be an approximation with a silent error
term — precisely the failure mode the invariant exists to prevent.

Candidates: `tiktoken-rs` (BPE encoders), HuggingFace `tokenizers` with a downloaded
vocabulary, a hand-rolled heuristic (characters ÷ 4), or the consuming model's own
tokenizer via API.

## Decision

Use **`tiktoken-rs` 0.12.0** with the `o200k_base` encoder as the default, configurable.
One code path serves both budget estimation and final measurement, so the invariant is
exact.

## Consequences

- The invariant "measured ≤ budget" is checkable rather than approximate, and is
  property-tested against adversarial inputs (files that are entirely signature lines,
  pathological unicode).
- `o200k_base` is the right default for the audience: it is the encoding family used by
  current frontier models, and it is what the benchmark harness measures with, so the
  numbers in a public report correspond to what users experience.
- The encoder tables are embedded in the crate, so counting requires **no network and no
  downloaded vocabulary file** — which is what makes an offline, deterministic core
  possible (ADR-006). This was verified by execution, not assumed.
- We accept a modest, bounded inaccuracy: no third-party tokenizer is exact for every
  model. That is why the header reports measured tokens against a *declared* budget and
  why BENCHMARK.md fixes the tokenizer for all variants — the comparison is internally
  consistent even where absolute counts differ from a given provider.
- Dependency cost: `tiktoken-rs` pulls `fancy-regex`, `regex`, `base64`, `bstr` and
  `lazy_static`. `fancy-regex` is the notable one (backtracking regex engine); it is only
  exercised on tiktoken's own pattern splitting over *our generated artifact text*, not on
  untrusted repository input, so it does not widen the parser attack surface described in
  SECURITY.md §6.

## Verification (2026-09-15)

Built `tiktoken-rs` 0.12.0 against the pinned toolchain and encoded a Go source snippet
with `o200k_base` with no network access available to the process; the encoder resolved
from embedded data and returned a token count. This confirms the offline claim.

## Alternatives rejected

- **HuggingFace `tokenizers`** — requires shipping or downloading a vocabulary file,
  which either bloats the binary or breaks the zero-network guarantee.
- **Characters ÷ 4 heuristic** — cheap and dependency-free, but it is exactly the
  "approximation mismatch" this ADR exists to eliminate: the artifact would fit a
  *heuristic* budget while exceeding the model's real one.
- **Provider tokenizer APIs** — requires network and an API key, violating ADR-006 and
  ADR-010.
- **A custom BPE implementation** — on the explicit over-engineering guard list
  (MASTER_PLAN.md §8.2).

## What would reopen this

The audience consolidating on an encoding `tiktoken-rs` does not ship, or a measured
discrepancy large enough to break the budget invariant in practice. Because the estimator
and the measurer share one implementation, changing the tokenizer is a one-line change
that keeps the invariant intact — which is the point of deciding it this way.
