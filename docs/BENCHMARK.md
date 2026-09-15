# ContextSlice — Benchmark Methodology

| | |
|---|---|
| **Status** | Pre-implementation specification |
| **Version** | 1.0 — 2026-09-14 |
| **Principle** | No public claim without a reproducible run behind it. |

ContextSlice's differentiation is *proven* context selection. This document defines how
proof is manufactured: a two-tier benchmark — **intrinsic** (fast, CI-integrated,
measures selection quality directly) and **extrinsic** (slower, nightly, measures what
actually matters: agent task success per token/cost/latency).

---

## 1. Philosophy

1. **The product metric is task success at acceptable cost**, not tokens removed. Token
   counts are only interesting relative to success.
2. **Reproducibility or it didn't happen.** Harness, corpus manifests, seeds, raw logs,
   and the ContextSlice version are published with every report.
3. **Baselines must be strong and honest.** We compare against full-repo context,
   Repomix (the category leader), and a naive dependency-crawl — not against strawmen.
4. **Non-claims are stated.** Corpus limitations, agent variance, single-model results.
5. **The benchmark tunes the algorithm** (ALGORITHM §13). Weights move only with a
   benchmark delta attached.

## 2. Two-tier design

| | Intrinsic | Extrinsic |
|---|---|---|
| Question | "Did we select the right files?" | "Does the agent succeed better/cheaper?" |
| Unit | One merged PR (issue text + gold files) | One issue with runnable tests |
| Runtime | Seconds per task; whole corpus in CI minutes | Minutes per task; nightly job |
| Metrics | recall@budget, precision, tokens | fail-to-pass rate, input/output tokens, cost, wall time, turns |
| Used for | Regression gate, weight tuning | Publication, roadmap gates |

Both tiers share a corpus governance process (§8).

---

## 3. Intrinsic benchmark (specification)

### 3.1 Corpus construction

1. **Select repositories** (criteria, not vibes): 1k–50k stars; active; real test suite;
   issues linked to merged PRs; per-language: one small (<2k files), one medium
   (2k–20k), one large (>20k). v0.1 targets: Go ×3, TypeScript ×2, Python ×2 (≥200 PRs
   total; ContextBench's 66-reach is a Phase 6 ambition, not a v0.1 requirement).
2. **Pin** every repo by commit SHA (the merge commit's parent).
3. **Extract tasks:** task text = linked issue title+body if the PR closes an issue,
   else PR title+body. Keep code blocks and stack traces — that is realistic input.
4. **Gold context** = the set of files touched by the merge commit, partitioned into
   `gold.code` (production files), `gold.test` (test files), `gold.config`.
5. **Exclusions:** PRs touching >25 files (refactors/mechanical sweeps — gold sets too
   diffuse to be meaningful); PRs with no issue text and <20-char bodies; dependency
   bumps; PRs whose gold set is entirely vendored/generated paths.
6. **Dedup** near-identical tasks (same repo, Jaccard >0.8 on gold sets).

### 3.2 Variants under test

| ID | Variant | Construction |
|---|---|---|
| A | **Full-repo-truncated** | whole repo at L5, alphabetical by path, hard-truncated at budget |
| B | **Repomix pack** | `repomix --style xml` with default patterns, truncated at budget (Repomix errors past its own budget; we truncate downstream to keep the comparison budget-aligned) |
| C | **Naive dependency crawl** | BM25 top-k seeds, 1-hop import expansion, all files L5 until budget — the "obvious" version of our idea, our most important baseline |
| D | **ContextSlice (deterministic)** | the product, default settings, budget-fitted |
| E | ContextSlice `--semantic` | Phase 5 only, off by default |

All variants receive **identical** budget (primary points: 8k and 16k), identical
tokenization for measurement (tiktoken o200k), and identical repo snapshots.

### 3.3 Metrics

For each (task, variant, budget):

- **Recall@budget** — primary, two grades:
  - `recall_strong` = |gold.code ∪ gold.test included at ≥ L4| / |gold ∪ gold.test|
  - `recall_weak` = |gold at ≥ L2| / |gold| (declaration-level visibility counts as
    "found")
- **Precision** = tokens spent on gold files (and 1-hop neighbors of gold, since
  supporting context is legitimately useful) ÷ total slice tokens. Reported alongside
  recall because ContextBench found agents over-favor recall; we optimize the frontier,
  not recall alone.
- **Tokens used** (variants that fit budget will differ in fill efficiency).
- **Test recall** reported separately (`gold.test` inclusion) — the test-promotion rule
  (ALGORITHM §7) is validated here.

### 3.4 Statistics & reporting

Per corpus cell (repo × language × budget): median recall/precision, bootstrap 95% CIs
(2,000 resamples), and **paired per-task deltas** (D vs C, D vs A — same tasks,
McNemar-style sign test on "recall_strong ≥ 0.8" hit/miss, Wilcoxon signed-rank on
token counts). Reports include per-task CSVs; failures are listed, not hidden.

### 3.5 CI integration

- Curated 40-task subset (10 per language MVP) runs on every PR touching `cs-select`,
  `cs-extract`, `cs-resolve`, or any `tuning` constant.
- **Gate:** `recall_strong@8k` median drop > 2 points ⇒ CI fails.
- Full corpus runs nightly; results posted to `docs/benchmarks/` per release.

### 3.6 Publication gate (the claim we must earn)

Before any public comparison claim: ContextSlice (D) must beat naive crawl (C) on
recall@same-tokens **and** beat full-repo (A) on tokens@comparable-recall, both with
CIs excluding zero, on ≥3 repos. If D fails vs C, the algorithm iterates (that is the
point of the harness) — this is a *pre-marketing* gate, not a post-launch apology.

---

## 4. Extrinsic benchmark (specification)

### 4.1 Harness

- **Agent scaffold:** a minimal fixed loop (mini-SWE-agent style, ≤40 turns): model
  receives task + variant context, may call `read_file`, `grep`, `edit`, `run_tests`
  (via a sandboxed shell), then submits. Deliberately simple and fully published.
- **Model:** one pinned frontier model via API, temperature 0, fixed system prompt
  across variants. Model/version recorded; results are explicitly *about this
  configuration*.
- **Context variants:** A/B/C/D from §3.2 injected as the initial context. The agent's
  tools remain identical across variants — we measure the *starting context* effect,
  not tool differences.

### 4.2 Tasks

- ~50 tasks: Python subset drawn SWE-bench-lite-style (repos within our adapter set),
  plus curated Go/TS tasks from corpus repos (issue text + failing test reproduced at
  the parent commit; inclusion requires the test to fail pre- and pass post-merge —
  same standard as SWE-bench's fail-to-pass).
- **Sample size is computed before the run, not after.** Paired designs are far less
  efficient than they look when the outcome is binary and the arms agree on most tasks.
  Our exact two-sided McNemar computation (α = 0.05) for a *true* 12-point success-rate
  effect gives:

  | tasks (n) | discordant 15% | discordant 30% | discordant 50% |
  |---|---|---|---|
  | 50 | 2% | 4% | 6% |
  | 100 | 4% | 7% | 11% |
  | 200 | 7% | 13% | 20% |
  | 400 | 13% | 23% | 37% |

  The discordant-pair rate is the dominant term, and it is unknown until we run, so the
  procedure is: run a pilot to estimate it, then size the reported run for ≥80% power on
  the effect we intend to claim. **A 50-task run cannot support any correctness claim** —
  publishing one would be reporting noise. This is also why the extrinsic tier is a
  nightly job rather than a PR gate.
- The published counterexample (Graft, §6) reports a 12-point effect at n = 50. Under the
  table above that result is consistent with a true effect and also with no effect; we
  cite it as evidence that the *category* measures outcomes, not as evidence that context
  selection works.

### 4.3 Metrics

Per (task, variant): **resolved** (fail-to-pass tests pass; pass-to-pass not
regressed), input tokens, output tokens, turns, wall time, API cost. Aggregates:
resolution rate with bootstrap CI; paired McNemar D vs A/B/C; median cost-per-resolved-
task (cost ÷ expected resolutions — the honest efficiency frontier number).

### 4.4 Reporting

Nightly runs publish: harness commit, model id, prompts, per-task logs, aggregate
tables, and a **negative-results section** (where ContextSlice hurt: tasks whose gold
context our graph missed, tasks where full-repo won because the model needed breadth).
The negative-results section is mandatory — it is also our tuning signal.

---

## 5. Statistical rules (both tiers)

1. Paired designs everywhere (same task across variants); never compare across
   different task samples.
2. Bootstrap CIs (2,000 resamples) for rates and medians; McNemar / Wilcoxon for
   paired significance; α = 0.05, effects reported with CIs not bare p-values.
3. Multiple-comparison awareness: primary metric declared in advance
   (recall_strong@8k intrinsic; resolution rate extrinsic); everything else is
   secondary and labeled exploratory.
4. No cherry-picking: corpus construction rules are committed before extraction runs;
   exclusions must be rule-based (§3.1.5) and applied uniformly to all variants.

## 6. Relation to existing benchmarks

| Benchmark | What it is | How we relate |
|---|---|---|
| **ContextBench** (arXiv 2602.05892, verified 2026-09-15) | 1,136 tasks, 66 repos, 8 languages, human-annotated gold contexts; public fixed-harness leaderboard with **Recall, Pass@1, Context F1 and cost** | Our intrinsic metrics align deliberately (recall/precision/efficiency framing); cross-evaluating ContextSlice on their public gold contexts is a Phase 6 goal — third-party gold sets are the strongest credibility signal. **Two cautions.** (1) Their abstract scopes the work as one that *"augments existing end-to-end benchmarks with intermediate gold-context metrics"*, so we must not cite it as a substitute for outcome evaluation; its Pass@1 column exists but their contribution is the retrieval-centric process signal. (2) Their findings are load-bearing *for* us and must be engaged honestly: sophisticated scaffolding yields only marginal retrieval gains; LLMs favour recall over precision, introducing noise; balanced retrieval achieves higher accuracy at lower cost; and retrieved context is often never used in the final solution. |
| **SWE-ContextBench** (arXiv 2602.08316, verified 2026-09-15) | 1,100 base + 376 related tasks across 51 repos / 9 languages; measures resolution accuracy, runtime and token cost under context-reuse strategies, comparing **named shipped context/memory tools** | Partially validates our thesis and partially constrains it. It supports the core mechanism — *"accurately summarized and retrieved previous experience can significantly improve resolution accuracy and reduce runtime and token cost… unfiltered or incorrectly selected context provides limited or negative benefits"* — which is the strongest published evidence that context **selection** quality moves outcomes. But it also means "nobody evaluates context tools on outcomes" is **false**, so we must not make that claim (see §7). Our extrinsic tier adapts its cost-accounting. |
| **SWE-bench (Verified/Lite)** | Issue → patch, fail-to-pass judging | Source of Python extrinsic tasks; we do not claim SWE-bench leaderboard comparability (different scaffold) |
| **Graft** (trailhq/Graft, MIT) | Graph CLI/MCP; publishes **SWE-bench Verified** results (official `swebench` 4.1.0 grader, n=50, +12 pts correctness, +23% tokens) alongside a self-run controlled sweep | One of several counterexamples to "nobody publishes agent outcomes" (verified 2026-09-15; see §7 for the others, which are stronger). Two limits matter for this one: its controlled-sweep harness was removed from the public repo (CHANGELOG v0.7.0, "The `bench/` benchmark harness is no longer part of the published repo"), and n=50 is severely underpowered — our own exact two-sided McNemar computation puts the power to detect a true 12-point effect at n=50 at only **2–6%** (depending on the discordant-pair rate; see §4.2). **Consequence for us:** publish the harness, corpus manifests, seeds and per-task logs, and size the extrinsic tier for the effect we intend to claim (§4.2). "First to publish" is not a claim we make; being checkable is. |
| **Aider polyglot** | 225 Exercism exercises, edit-format focused | Not used: exercises are self-contained single files — context selection is untested by construction |
| **Terminal-bench** | Docker terminal tasks | Out of scope; no repo-context manipulation |

## 7. Claims policy

Allowed claims (with linked runs): "on our published corpus, ContextSlice selected
gold files with median recall X at Y tokens vs Z for naive crawl". Forbidden claims:
"2× better context", universal token-reduction percentages, implications about
arbitrary agents/models. Every README number links to `docs/benchmarks/<version>/`.

**Claims of absence are forbidden outright.** Statements of the form "nobody does X" or
"no tool in this category ships Y" are not permitted in any public artifact, because we
cannot verify a negative, they decay silently, and one counterexample makes the whole
document untrustworthy. We assert only what *we* measure, on our corpus, with our
harness. Where a competitor's behaviour is relevant, describe it precisely and cite their
source with a date — for example "Repomix's `--token-budget` exits non-zero after
producing the over-budget pack (their docs, 2026-09-15)" rather than "packers can't fit
a budget".

This rule is not theoretical. The project's founding documents asserted that nobody in
this category publishes agent-outcome evaluation. That is false, and the counterexamples
are strong: **RepoGraph** (ICLR 2025, arXiv 2410.14684) is a repository-context module
ablated on SWE-bench resolver rates with released code; **SWE-ContextBench** compares
named shipped context tools on resolution accuracy and cost; **ContextBench** publishes a
fixed-harness leaderboard carrying Pass@1; and **Aider** has shipped a reproducible
end-to-end polyglot leaderboard for years (it varies the model and never ablates its repo
map — but the artifact exists). Any of these alone would have discredited a public
negative claim. See ADR-015 for the full correction.

## 8. Corpus governance

- Repos pinned by SHA; manifests record repo, SHA, PR id, issue id, task text hash,
  gold file list. Regeneration is a single `cs-bench corpus rebuild` command.
- Licensing: corpus repos remain under their own licenses; we ship *manifests and task
  metadata*, never repo snapshots; regeneration fetches from upstream at pinned SHAs.
- Leakage guard: ContextSlice development may not special-case corpus repos; tuning
  constants must be justified by aggregate deltas, not per-repo fixes. A held-out repo
  (touched only at release time) checks for overfitting starting in Phase 3.
