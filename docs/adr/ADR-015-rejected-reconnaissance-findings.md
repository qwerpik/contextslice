# ADR-015: Reconnaissance findings rejected, and why

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

The bootstrap milestone started from a reconnaissance pass that produced a list of
candidate dependencies, competitive claims, and scope suggestions. The project's standing
rule is that a finding reported by an agent is a **claim**, not a fact. Each was therefore
re-checked against current repository state, upstream activity, licensing, compatibility
with this workspace, and whether it reduced work without creating architectural debt.

This record exists so that rejected findings stay rejected *with reasons*. A finding that
disappears silently gets rediscovered in three months and re-litigated, usually by
someone who was not in the original discussion.

## Accepted, with the record elsewhere

| Finding | Outcome | Record |
|---|---|---|
| `rmcp` for the MCP server | **Rejected for now**; cannot do stdio without tokio, and MCP is Phase 2 | [ADR-013](ADR-013-mcp-protocol-revision-and-transport.md) |
| Maintained tree-sitter grammars, pinned | **Accepted**; all three verified loading and parsing under `tree-sitter` 0.27 | [ADR-011](ADR-011-dependency-manifest-and-pinning.md), [ADR-008](ADR-008-mvp-languages.md) |
| `tiktoken-rs` for offline token counting | **Accepted**; verified to embed its encoder tables and need no network | [ADR-014](ADR-014-tokenizer-choice.md) |
| Aider's `.scm` queries as reference material | **Rejected as a drop-in**; their predicates are silently ignored by core tree-sitter | [ADR-012](ADR-012-own-tree-sitter-queries.md) |
| `stack-graphs` | **Rejected**; re-verified archived (`archived: true`, last push 2025-09-09) | [ADR-003](ADR-003-approximate-reference-graph.md) |

## Rejected outright

### gitleaks rules as a secret-pattern source

**Finding:** import gitleaks' detection rules for the secret scan in SECURITY.md §5.

**Verdict: deferred, not during bootstrap; license is fine.** Verified on 2026-09-15:
gitleaks is **MIT** licensed (GitHub API reports `MIT`, ~29k stars, actively pushed
2026-09-09), so reuse with attribution would be permissible. It is rejected *for now*, on
technical fit rather than licensing:

- gitleaks rules are regex sets expressed in TOML for a scanner that shells over files
  and reports findings. Our requirement is narrower: scan only the regions already
  selected at L4/L5, before rendering, and mask matches inside *stored signatures*
  (SECURITY.md §5.3).
- Adopting the rule set wholesale would import several hundred patterns tuned for
  recall-over-precision in CI, where a false positive is a failed build. Here a false
  positive silently redacts code from an agent's context, which is a different cost
  model.
- The dependency would also need re-verification on every upstream rule change to keep
  our documented behaviour stable.

The secret scan is Phase 2 work behind `--secrets=skip|warn|allow`. The rule *source*
should be revisited then, with a measured false-positive rate on real repositories, which
is the number that decides it.

### `mini-swe-agent` as the extrinsic benchmark harness

**Finding:** use mini-swe-agent as the benchmark harness for agent evaluation.

**Verdict: premature.** BENCHMARK.md §4.1 already specifies a minimal fixed loop
(≤40 turns, fixed tools, temperature 0, published prompts). That specification exists
partly *because* the scaffold must be identical across the four context variants being
compared: the experiment measures the effect of starting context, so a scaffold with its
own retrieval, compaction or subagent behaviour would confound the variable under test.

The extrinsic tier is Phase 3 (MASTER_PLAN.md §15, roadmap Phase 3), two phases away. If
a third-party harness can be configured to hold the scaffold fixed, adopting it is
strictly better than maintaining our own. That evaluation is deferred to Phase 3, when
there is something to evaluate it against.

### scipy / statsmodels for benchmark analysis

**Finding:** consider scientific Python for statistical analysis of benchmark results.

**Verdict: not a dependency of this project.** Not because it is wrong — it may well be
the right tool — but because of a category error in the finding. These are Python
libraries. ContextSlice is a Rust workspace (ADR-001), and BENCHMARK.md §4.2 already
notes that Python appears only on the benchmark side, where the SWE-bench tooling
dictates it.

Even there, the required statistics are narrow and fixed: bootstrap confidence intervals
(2,000 resamples), McNemar's test, Wilcoxon signed-rank (BENCHMARK.md §5). These are
specified, small, and worth implementing once where the results are produced — a Rust
implementation keeps the harness a single buildable artifact and avoids a second
toolchain in CI. The rule "do not reimplement statistical methods unnecessarily" is
sound in general; here it does not apply, because there is no Rust-side alternative being
displaced and no Python-side integration being created.

If the analysis grows beyond the specified tests, that judgment changes, and the
superseding ADR should say so.

### SWE-bench instances as the benchmark corpus

**Finding:** use SWE-bench instances/tooling as a source of Python benchmark tasks.

**Verdict: accepted in part, and already in the plan.** BENCHMARK.md §4.2 specifies the
extrinsic Python subset as "SWE-bench-lite-style" and BENCHMARK.md §6 already records the
relationship to SWE-bench, including the explicit non-claim that we are not leaderboard
comparable (different scaffold).

The one thing to resist is treating SWE-bench as the *intrinsic* corpus. The intrinsic
tier needs gold contexts per merged PR across Go, TypeScript and Python with our own
pinned SHAs and exclusion rules (BENCHMARK.md §3.1), which is a different artifact from a
Python-only agentic task set. Scope is unchanged; the finding confirmed an existing plan
rather than adding to it.

### Aider repo-map and Repomix as dependencies or drop-ins

**Finding (implied):** reuse these instead of building the engine.

**Verdict: no, and no change to the plan.** Both are prior art that MASTER_PLAN.md §2
already credits. What was verified is that the *differentiation* claim survives contact
with them:

- Aider's repo map is proven (≈49k stars) but lives inside aider, is seeded only by chat
  keywords, and emits a signatures-only map under a ±15% binary-search budget
  (ALGORITHM.md §15). It does not mix granularity and is not a standalone tool.
- Repomix (≈28k stars) confirmed from its own README, 2026-09-15: `--compress` "uses
  Tree-sitter to extract key code elements, reducing token count while preserving
  structure", and `--token-budget` is documented as *"Fail with a non-zero exit code when
  the packed output exceeds N tokens… The output is still generated; only the exit code
  signals the overflow."*

That second sentence is the clearest available statement of the gap this project targets:
the category leader treats a budget as a **post-hoc alarm**, and fitting one is not a
solved problem. It is quoted here because it is a claim about a competitor drawn from
their own documentation, and it is the sort of thing that should be re-checked rather than
remembered.

Neither project is a dependency, and neither is used as a code source.

### Features explicitly not built during bootstrap

The reconnaissance report listed categories to resist, and they remain resisted
(MASTER_PLAN.md §12 and the guard list in §8.2): LLMLingua-style prompt compression,
generic repository packers (gitingest, code2prompt), stale tree-sitter wrapper crates,
`stack-graphs`, GUI or editor plugins, cloud or vector-database infrastructure, and
LLM-first semantic selection. None is triggered before the Phase 3 gate, and each has a
trigger condition in the roadmap rather than a blanket ban.

## Claim hygiene

Two claims from the reconnaissance were **not** adopted into the documentation, because
they are the kind that decay quietly:

- **"Nobody in this category publishes an extrinsic agent-outcome benchmark."** This is
  the sort of absolute that becomes false without notice. The product decision does not
  depend on it: BENCHMARK.md treats the benchmark as necessary regardless of what
  competitors publish, and the moat argument in MASTER_PLAN.md §5 rests on execution and
  accumulated evaluation data, not on being first. The claim is therefore stated, if at
  all, as "we are not aware of one" with a date, never as a standing fact.
- **"Repomix does not provide a task-conditioned, mixed-granularity, hard-budget
  selection model."** Verified as accurate on 2026-09-15 against Repomix's own README,
  and narrowed to what that document actually supports. It is a dated observation about a
  moving target, not a permanent property, and it must be re-checked before it appears in
  any public comparison — which BENCHMARK.md §7 requires anyway.

Star counts cited above were verified on 2026-09-15 via shields.io (rounded: Repomix 28k,
aider 49k, Serena 29k, claude-context 13k, gitleaks 29k). They are context for relative
maturity only. **No decision in this repository depends on a star count.**

## Consequences

- The bootstrap ships no MCP dependency, no async runtime, no Python dependency, and no
  third-party query or rule files.
- Four candidate dependencies were researched and deliberately not adopted; the reasoning
  is recorded so the same ground is not re-covered.
- Every rejected finding names the condition under which it would be reconsidered, so
  "no" is a decision with a trigger rather than a closed door.

## What would reopen this

Each item above states its own trigger. The general rule: a rejected finding reopens when
its *stated condition* is met — a measured false-positive rate, a Phase 3 harness
comparison, an analysis need beyond the specified statistical tests, or a protocol
revision that changes the transport calculus.
