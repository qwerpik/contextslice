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

**3. "Nobody publishes an agent-outcome benchmark" is false.** Graft
(`trailhq/Graft`, MIT, ~8k★) publishes **SWE-bench Verified** results using the official
`swebench` 4.1.0 grader at n=50 (+12 pts correctness, +23% tokens). The claim was already
barred from public artifacts, but it existed in the founding documents and had to go.
The defensible formulation — and the one BENCHMARK.md now uses — is *reproducible **and**
adequately powered*, for two verified reasons:

- Graft removed its benchmark harness from the public repo (CHANGELOG v0.7.0: "The
  `bench/` benchmark harness is no longer part of the published repo"), so its
  controlled sweep is not reproducible from the published tree.
- n=50 is underpowered. A paired McNemar exact test at α=0.05 detects a true 12-point
  effect only ~27% of the time. Graft's headline is exactly a 12-point effect at n=50.

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
