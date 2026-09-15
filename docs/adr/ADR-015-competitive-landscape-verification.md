# ADR-015: Competitive landscape verified; three claims corrected

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-15 |
| **Milestone** | Bootstrap |
| **Supersedes** | The competitive claims in MASTER_PLAN.md §2.2–2.3 and §4 as originally drafted |

## Context

The founding documents were written from a reconnaissance pass that asserted several
things about the competitive landscape. Two of those assertions are load-bearing: they
justify the product's differentiation (MASTER_PLAN.md §5) and the existence of the
benchmark subsystem (ADR-009). A load-bearing claim about a competitor is exactly the
kind that must be checked against the competitor's own code and documentation, because it
is the claim a knowledgeable reader will test first.

A verification pass was run on 2026-09-15. It cloned the relevant repositories read-only
and read the implementations rather than trusting READMEs, because for two of the claims
the documentation and the code disagreed. **Three claims did not survive.**

## Decision

Correct the documents, and adopt a rule that prevents the class of error.

**1. The claim "Aider's repo map is ~1k tokens" was stale — the docs say it, the code
does not.** `Model.get_repo_map_tokens()` in `aider/models.py` computes:

```python
map_tokens = 1024
max_inp_tokens = self.info.get("max_input_tokens")
if max_inp_tokens:
    map_tokens = max_inp_tokens / 8
    map_tokens = min(map_tokens, 4096)
    map_tokens = max(map_tokens, 1024)
return map_tokens
```

Any model with ≥32,768 input tokens therefore gets **4096**, not 1024; 1024 is the floor
and the fallback when the model's input size is unknown. Aider's *documentation* still
says "defaults to 1k tokens", which is where the figure came from. Corrected in
MASTER_PLAN.md §4 and ALGORITHM.md §15.

**2. Aider's budget is soft, and its mixed granularity is not budget-driven.**
`repomap.py` accepts a candidate tree within **15% error** (`ok_err = 0.15`), and the CLI
help calls the value *"Suggested"*, with the docs warning it "does expand the repo map
significantly at times". Aider genuinely produces mixed granularity — files already in
the chat get full source and are *skipped* from the map, everything else is a skeleton —
but that split is decided by **chat membership**, a fixed rule, not by the budget.
Our "hard never-exceed invariant" contrast stands, and is now stated as the specific
difference rather than implied by a token count.

**3. "Nobody publishes an agent-outcome benchmark" is false — and the counterexamples are
strong, not marginal.** The first version of this record reached that conclusion from a
single weak counterexample (Graft: n=50, harness later removed from the public repo).
A follow-up sweep found a much broader set, and **each of the following was verified
against its abstract or live site on 2026-09-15** (the follow-up also handed us a power
figure attributed to a competitor's internal protocol; we did not adopt it, and computed
the number ourselves instead — see the note on statistical hygiene at the end of this
point):

- **RepoGraph** (ICLR 2025, arXiv 2410.14684): a repository-context plug-in module
  evaluated *"on the SWE-bench by plugging it into four different methods… substantially
  boosts the performance of all systems"*, with code released and runnable scripts. This
  is a reproducible ablation of a repo-context mechanism on agent task outcomes — exactly
  the artifact the withdrawn claim said did not exist.
- **SWE-ContextBench** (arXiv 2602.08316): evaluates coding agents under context-reuse
  strategies and compares **named shipped context tools**, reporting resolution accuracy,
  runtime and token cost. Its central finding supports our mechanism directly:
  *"accurately summarized and retrieved previous experience can significantly improve
  resolution accuracy and reduce runtime and token cost… unfiltered or incorrectly
  selected context provides limited or negative benefits."*
- **ContextBench** (arXiv 2602.05892): public fixed-harness leaderboard carrying Recall,
  **Pass@1**, Context F1 and cost, updated 2026-09-14.
- **Aider**: a reproducible end-to-end polyglot leaderboard (public harness repo,
  per-run cost, tokens and commit hash) shipped for years. It varies the model and never
  ablates the repo map — but the artifact exists, which is what the claim denied.
- **Cursor** (cursor.com/blog/semsearch) reports measured agent-accuracy deltas from
  semantic search. Not reproducible, but it defeats "nobody *measures* outcomes".

**Statistical hygiene note.** The follow-up report also carried the claim that n=50
detects a 12-point effect "27% of the time", sourced from another competitor's internal
protocol document. Rather than adopt a number produced by a third party with an interest
in the comparison, we computed it: the exact two-sided McNemar power at n=50 for a true
12-point difference is **2–6%**, depending on the discordant-pair rate, rising only to
13–37% at n=400 (BENCHMARK §4.2). The direction of the finding survives; the borrowed
figure did not, and the discrepancy is a reminder that a statistic is only as good as its
derivation. This is the same rule the benchmark applies to itself: no borrowed numbers.

**The verdict is FALSE as a universal, and the claim is withdrawn rather than reworded
into something that merely survives.** Any formulation starting "nobody publishes…" is
now banned by the rule in point 6 below. What we assert instead is only what *we* will
do: publish our harness, corpus manifests, seeds and per-task logs, so our own numbers
are checkable. That is a claim about our artifact, which we control, and it cannot be
falsified by a competitor shipping something tomorrow.

**4. Repomix's budget behaviour needed precision, not correction.** Verified at source
level (`src/cli/cliTokenBudget.ts`): `--token-budget` is a *post-hoc CI guard*, not a
refusal. It writes the over-budget pack and then exits non-zero; its docs state "the
output is still generated; only the exit code signals the overflow". It does not fit,
degrade, or truncate — it tells the user to narrow the scope themselves. Separately,
Repomix **does** have three per-file inclusion levels (`output.patterns`: full /
tree-sitter-compressed / directory-structure-only), but they are hand-declared globs in a
config file with no CLI flag, so the *user* picks the granularity rather than the budget.
The differentiator is therefore "it makes you do the narrowing", not "it errors out".

**5. The differentiation is narrower than the founding documents implied.** Graft was
absent from the original landscape table and belongs in it: `graft ask` does
deterministic, no-LLM, task-conditioned ranking, and `graft skeleton` emits signatures
only. So *task-conditioned selection* (a) and *mixed granularity* as a capability are not
unique. What remains defensible is the conjunction: the **budget itself chooses each
file's level under a hard never-exceed ceiling**. Graft's controls are a result count
(`--limit`, `--max-dirs`), not a token ceiling, and the level is a manual flag; Repomix's
levels are glob-configured; Aider's are chat-decided with a soft budget. MASTER_PLAN §2.3
now states this narrowly and credits Graft by name.

**6. Most seriously: published evidence contradicts the category's premise, including
ours.** arXiv 2602.11988 (v2, June 2026, *Evaluating AGENTS.md: Are Repository-Level
Context Files Helpful for Coding Agents?*) finds that providing context files *"does not
generally improve task success rates, while increasing inference cost by over 20% on
average"* — holding across different LLMs, different coding agents, and for both
LLM-generated and developer-committed context files. Its sharpest finding: *"while
instructions in the context files are well followed by coding agents, repository
overviews, although popular and recommended by model providers, are not helpful."*

This is not a competitive observation; it is a **negative result aimed at the premise**,
and it is recorded as risk #4 in MASTER_PLAN §11 with a stop/go consequence rather than
argued away. Three commitments follow:

- **We will not pitch repository overview.** The study discredits static overview
  material. ContextSlice's claim must rest on delivering the exact files a change will
  touch. Map mode is the closest thing we ship to an overview, and it is explicitly the
  fallback for an empty seed set, not the product.
- **The honest gap is a hypothesis, not a rebuttal.** The study measures static context
  and instruction files; we select task-conditioned source. That difference is plausible
  but *untested*, and it may not matter — a well-chosen file set could still fail to move
  task success. Phase 3's extrinsic gate is therefore the project's real go/no-go, and
  MASTER_PLAN §7's pivot clause applies if it fails.
- **The study's own remedy is our existing discipline.** It concludes that performance
  claims "should be rigorously evaluated before deployment". That is what BENCHMARK.md
  already mandates, which is the only defensible response available: measure, do not
  argue.

## Consequences

- **New rule, enforced in BENCHMARK.md §7: claims of absence are forbidden.** No public
  artifact may say "nobody does X" or "no tool ships Y". We cannot verify a negative, such
  claims decay silently, and one counterexample discredits the whole document. We assert
  what *we* measure on our corpus with our harness; competitor behaviour is described
  precisely, with a source and a date.
- Graft is added to the competitive landscape with a citation, and is treated as the
  nearest serious comparison point rather than an unmentioned one.
- BENCHMARK.md §6 now records the counterexample and derives a concrete obligation from
  it: publish the harness, corpus manifests, seeds and per-task logs, and size the
  extrinsic tier for the effect we intend to claim. "First to publish" is explicitly not
  a claim we make.
- Naming a competitor honestly in our own documents costs nothing and buys the credibility
  that the benchmark strategy is designed to earn.

## Rejected: the specific mechanism attributed to us by the reconnaissance

The reconnaissance report suggested ContextSlice's differentiation be framed as
*"task-conditioned, deterministic selection"*. That framing is now rejected as the
headline: Graft satisfies both properties, so the phrase describes table stakes rather
than a differentiator. The headline is the budget-fitting conjunction.

## Rejected: two lead-generation artifacts as evidence

Two items surfaced during verification were **not** accepted as evidence and are recorded
here so they are not quietly promoted later:

- A **vendor-authored comparison page** enumerating ~26 tools and flagging several as
  claiming "token-budgeted retrieval" (notably LemonCrow and SigMap). A competitor's own
  comparison table is marketing, not evidence. These are unverified leads.
- **Serena's "evaluation"**, which is a self-assessment protocol ("the agent evaluates
  itself", one prompt, ~20 tasks the agent chooses, reporting call counts and payload
  sizes). It has no fixed task set and no success metric, so citing it as a benchmark
  would be a category error. Recorded so a future reader does not mistake it for a
  counterexample.

## What would reopen this

A tool appearing that makes the budget itself choose per-file granularity under a hard
ceiling would remove the remaining differentiator (b) and force a genuine repositioning —
most plausibly toward the benchmark and evaluation data as the moat, which MASTER_PLAN §5
already names as the primary asset. Verification of LemonCrow or SigMap could also reopen
it. Any such check must be repeated at each release, since every fact in this record is
about a moving target.
