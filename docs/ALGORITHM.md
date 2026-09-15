# ContextSlice — The Context-Selection Algorithm

| | |
|---|---|
| **Status** | Pre-implementation specification (normative for v0.1) |
| **Version** | 1.0 — 2026-09-14 |
| **Depends on** | [ARCHITECTURE.md](ARCHITECTURE.md) (data model) · [BENCHMARK.md](BENCHMARK.md) (tuning) |

This document specifies the deterministic baseline algorithm end-to-end. It is written
so that (a) an implementer needs no further design decisions, (b) every constant is
visible and justified, and (c) the whole thing is falsifiable — every weight is tunable
against the intrinsic benchmark (§13), and no weight may change without a benchmark run.

---

## 1. Design goals

1. **Deterministic.** No randomness, no timestamps in output, no order-dependence.
2. **Task-seeded.** The natural-language task drives selection; the repo graph refines it.
3. **Budget-fitting.** The output *always* fits the token budget; degradation is graceful
   and visible, never silent truncation and never an error (except the impossible-budget
   case, which exits with guidance).
4. **Explainable.** Every inclusion/exclusion is reconstructable via `--explain`.
5. **Cheap.** Warm selection adds < 100 ms at 50k files.
6. **LLM-free.** No model calls anywhere in this pipeline (§14 defines the optional
   additive semantic layer).

Non-goals: optimality (this is a ranked heuristic, graded empirically, not a solved
optimization problem); precision guarantees on reference edges (approximate by design).

## 2. Inputs and outputs

**Inputs:** task text (free-form, 1–2000 chars); flags: `--tokens` (default 16,000),
`--include` globs (forced seeds), `--no-git`; the index snapshot (files, symbols, refs,
edges, git priors — ARCHITECTURE §5).

**Output:** a `SlicePlan` — per-file `{level, score, symbols[], reasons[]}` plus header,
tree, and elision summary — rendered by cs-render.

## 3. Pipeline overview

```text
task ─► [1] parse task ─► terms & hints
        [2] seed files   ─► per-file seed score s ∈ [0,1]
        [3] propagate    ─► 2-hop bounded walk on file graph ─► total score
        [4] assign levels (L0–L5, thresholds on total score + test promotion)
        [5] fit budget   ─► greedy demotion/promotion ladder
        [6] render       ─► header + tree + sections + elision + hint
```

Stages 1–5 are pure functions of (snapshot, task, flags). Stage 6 reads source files.

---

## 4. Stage 1 — Task parsing

Deterministic tokenization, no NLP dependencies:

1. **Normalize:** NFKC; lowercase a copy (keep the original for exact-case matching).
2. **Split** on non-alphanumerics *and* identifier boundaries: camelCase (`authTimeout`
   → `auth`, `timeout`), snake_case, kebab-case, `::`, `.`, `/`.
3. **Filter:** drop terms shorter than 3 chars, pure digits, and a stopword list
   (~120 English function words + code-generic words: `file`, `files`, `code`, `repo`,
   `function`, `class`, `test`*, `bug`, `fix`*, `add`, `change`, `make`, `update`…
   *`test`/`fix` are kept as *modifier hints*, not terms — see below).
4. **Extract hints:**
   - `` `backticked` `` → treated as a verbatim identifier or path term (exact match).
   - `"quoted strings"` → verbatim content search terms (error messages).
   - Tokens containing `/` or matching `\.(go|ts|tsx|py|js|rs|java|cs)$` → path hints.
   - `pkg.Symbol` / `Module::item` → qualified-name hints (split both ways: whole and
     parts).
5. **Tag modifiers:** task words like *test, tests, flaky* set a `test_bias` flag
   (raises test-affinity weights in stages 3–4); *config, deploy, migration* raise
   config-file signal weights.

Everything downstream consumes: `terms[]` (with weights), `hints{symbols[], paths[],
verbatim[]}`, `flags{test_bias, config_bias}`.

## 5. Stage 2 — Seeding (per-file score)

Each signal contributes a non-negative amount; the seed score is a saturating sum:

```text
seed(file) = x / (x + κ),  κ = 2.0,   x = Σ signal_weightᵢ × signal_valueᵢ
```

Saturation keeps one strong signal sufficient (x=2 ⇒ seed 0.5; x=6 ⇒ 0.75; x=18 ⇒ 0.9)
while preventing a file named `test_util_test.go` from exploding.

| # | Signal | Signal value | Weight | Notes |
|---|---|---|---|---|
| S1 | Exact symbol-name match | 1.0 per distinct matched term (case-sensitive +1 variant scores 1.0, case-insensitive 0.7) | 3.0 | FTS5 lookup on `symbols.name`; the strongest signal — if the task names a symbol, its file matters |
| S2 | Basename stem match | 1.0 (exact stem), 0.6 (stem contains term) | 2.0 | `auth.go` ↔ term `auth` |
| S3 | Path-segment match | 0.5 per matching segment, capped 1.0 | 1.0 | `internal/auth/session.go` ↔ `auth` |
| S4 | BM25 over symbol names (FTS5) | bm25 normalized to [0,1] by `b/(b+3)` | 1.0 | catches `LoginHandler` for "login" |
| S5 | Identifier-term density in content | `min(1, hits/8)` | 0.5 | grep-prefiltered on candidate files only (top 2,000 by S1–S4 union), not the whole repo |
| S6 | Doc/comment term match | `min(1, hits/4)` | 0.5 | doc-comments only, not inline noise |
| S7 | Git recency prior | `min(1, log2(1+90/days)/6)` | 0.3 | recent files are a priori more relevant; ≤0.05 influence by design |
| S8 | Config-file affinity | 1.0 for config files whose *keys* match terms (`config.go`/`settings.py` containing `timeout`) | 1.0 | gated by `config_bias` or strong term overlap; config discovery per LANGUAGES §6 |
| S9 | Forced includes (`--include`) | 1.0 | 10.0 | user intent trumps everything |

`--include` files are pinned seeds: they enter stage 3 at seed 1.0 and may never be
demoted below L2 (§8 invariants).

**Empty-seed handling:** if no file reaches seed ≥ 0.05, fall back to **map mode**
(aider-style repo map under budget; task-less). This is the honest answer to "the task
doesn't relate to any code" — an orientation map, not a wrong slice.

## 6. Stage 3 — Graph propagation

**Graph:** files as nodes; edges from cs-resolve (ARCHITECTURE §4.3), weights:

| Edge kind | Weight | Direction | Meaning |
|---|---|---|---|
| `import_out` | 0.6 | A imports B | reading B explains A |
| `import_in` | 0.45 | B imports A | A's consumers hint at blast radius |
| `ref_def` | 0.5·sqrt-damped | A references defs in B | finer-grained than imports; weight `0.5·√count/√(count+8)` per pair |
| `test_affinity` | 0.7 (bidirectional) | A ↔ its test file(s) | tests and code travel together; ×1.25 when `test_bias` |

**Walk:** bounded, synchronous, order-normalized:

```text
frontier₀ = files with seed ≥ 0.05, capped at 64 by (seed desc, path asc)
score₀(f) = seed(f)
for hop in 1..=2:
    for each f in frontierₕ₋₁ (processed in path order):
        for each edge (f → g) with weight w:
            contrib = scoreₕ₋₁(f) · w · decay(hop),  decay = 0.5
            prop(g) += contrib
    frontierₕ = top 64 files by prop(g) not already scored, ties by path asc
total(f) = squash(seed(f) + 0.8 · prop(f))    # squash = x/(x+κ), κ = 2.0
```

Why a bounded walk and not personalized PageRank (ADR-004): PageRank needs
iteration-to-convergence with damping factors whose behavior across repo topologies is
hard to reason about and harder to make byte-stable under parallelism; a 2-hop walk is
O(frontier × degree), trivially deterministic, and directly explainable ("0.42 came from
auth/session.go via import_out 0.6 × hop-decay 0.5"). A PageRank variant stays behind a
flag for benchmark comparison; if it measurably wins on recall@tokens, ADR-004 is
revisited with data.

**Justification of asymmetry** (`import_out` 0.6 > `import_in` 0.45): when fixing code,
the definitions you depend on explain your bugs more often than your dependents define
your blast radius; but dependents matter for interface changes, so they are kept at a
dampened weight rather than dropped.

## 7. Stage 4 — Level assignment

Levels (refined from the original L0–L5 proposal — same hierarchy, defined precisely):

| Level | Name | Content | Approx. cost vs full file |
|---|---|---|---|
| L0 | Excluded | nothing (counted in elision footer) | 0% |
| L1 | Path | one tree line: path, language, size, symbol count | ~0.1% |
| L2 | Names | top-level symbol names + kinds (`func Login`, `type Session`) | ~2–5% |
| L3 | Declarations | L2 + full signatures, types/interfaces/structs/constants, doc-comment first lines | ~5–15% |
| L4 | Bodies | declarations + bodies of *selected* symbols only (task-matched and 1-hop callees within the file), rest elided | ~15–40% |
| L5 | Full | verbatim source with line numbers | 100% |

Initial assignment on `total(f)` (before budget):

```text
total ≥ 0.75            → L5
0.50 ≤ total < 0.75     → L3
0.30 ≤ total < 0.50     → L2
0.15 ≤ total < 0.30     → L1
total < 0.15            → L0
```

Rules layered on top:

- **Test promotion:** if a code file is L5, its test-affinity partners are raised to at
  least L4 (agents need the tests to verify fixes; ContextBench found agents over-fetch
  code and under-fetch tests — this rule encodes the correction).
- **Likely-edit boost:** files whose seeds came from S1/S2 (symbol or basename named in
  the task) get `+0.1` total — the task literally points at them.
- **Config demotion:** config files cap at L3 unless `config_bias` — full YAML dumps are
  rarely worth tokens; their keys usually suffice.
- **Data files** (`.sql`, migrations, `.json` fixtures) cap at L4 and only when
  path-adjacent to L5 code.

The threshold bands (0.75/0.5/0.3/0.15) are the algorithm's four most important tunable
constants; §13 defines how they move (benchmark only, never vibes).

## 8. Stage 5 — Budget fitting

Token cost model: per-file cost at level ℓ estimated by the same tokenizer the renderer
measures with (tiktoken, o200k default) — estimation and measurement share code, so
"fits" is a true invariant. Fixed overhead: header + tree + footer ≈ 2–8% of budget,
reserved first.

**Algorithm (greedy demotion, then opportunistic promotion):**

```text
1. reserve fixed overhead
2. assign levels per §7; compute cost(file, level)
3. while Σcost > budget:
     for every file above L1, compute demotion gain g = (cost_now − cost_next_level)
       and loss l = total(f) × level_import(next)   # level_import: L5→L3 = 1.0,
       pick the file maximizing g/l (ties: path asc) and demote it one level          # L3→L2 = 0.5,
4. while Σcost ≤ budget − 5%:                       # L2→L1 = 0.25, L1→L0 = 0.1
     promote (one level up) the file with the highest total(f)/Δcost that still fits
5. emit plan + elision summary
```

**Invariants (property-tested, MASTER_PLAN §8.1):**

1. Measured tokens of the rendered artifact ≤ budget — always, including adversarial
   inputs (files that are 100% signature lines, pathological unicode).
2. Forced seeds never below L2 while the invariant-1 minimum is achievable.
3. At least 3 files at ≥ L2 whenever any seed exists.
4. Determinism: identical (snapshot, task, flags) ⇒ identical bytes.

**Impossible budgets:** if header + tree + minimal seed skeletons (L2) exceed the
budget, exit code 3 with the smallest workable `--tokens` value computed and suggested.
This is the one failure the algorithm allows itself — it is the difference between
"budget as constraint" (ContextSlice) and "budget as alarm" (Repomix).

## 9. Stage 6 — Rendering contract

- **Header:** task (verbatim), budget, measured tokens, index snapshot id, staleness
  line, level legend, generation parameters (flags). No timestamps — determinism.
- **Tree:** full repository tree with per-file level markers (L1 rendered here and
  nowhere else); directories annotated with token totals.
- **Sections:** descending level order (L5 → L4 → L3 → L2); files within a level by
  total desc, then path asc. Each file: `### path` header, per-symbol `path:line`
  anchors, elision markers (`⋯ 42 lines, 6 symbols elided ⋯`).
- **Footer:** elision totals ("287 files excluded, 12 at path-only, ≈301k tokens
  elided") and the ask-for-more hint: `contextslice "<task>" --include <path>` (and the
  MCP equivalent via `get_file`).
- The artifact is **self-describing**: an agent reading it knows what it did not get and
  how to ask for more. This is the anti-"lost context" design.

## 10. Fallback behavior matrix

| Condition | Path taken |
|---|---|
| No seeds (all seed < 0.05) | map mode: budget-fitted L3 skeleton of highest-centrality files (import_in degree) |
| Unsupported language files only | S2/S3/S5 path+content heuristics; levels only L0/L1/L5 (no skeleton without parsing) |
| Very small budget (< ~1.5k) | tree + top-3 seeds at L3; hint suggests realistic budget |
| No git | S7 = 0; everything else unchanged |
| Task mentions only tests | `test_bias`: test files compete on equal seeding; code partners pulled via test_affinity |
| Task is a stack trace | verbatim hints → S5 dominates; `path:line` from the trace becomes forced seeds |

## 11. Worked example (full trace)

Task: **"Fix the authentication timeout — users get logged out after 60s even with
remember-me"** in a 500-file Go repo (310k tokens).

**Stage 1:** terms = {`authentication`, `auth`, `timeout`, `users`, `logged`, `out`,
`remember`, `rememberme`, `remember_me`→`rememberme`}; hints: none verbatim;
modifier: none. (`fix`, `get`, `even`, `with` dropped as stopwords/code-generic.)

**Stage 2 (seeds):** `auth/session.go` defines `SessionTimeout`, `RememberMe` (S1 ×2
terms, case-insensitive) + basename `session`… → x=8.2 → seed 0.80. `auth/auth.go`
(S2 basename `auth`, S4 BM25 on `Authenticate`) → x=3.9 → 0.66. `auth/middleware.go`
(S4 on `LoggedInUsers`) → x=2.7 → 0.57. `auth/session_test.go` (S1 on `RememberMe` in
test symbols) → x=3.1 → 0.61. `config/config.go` (S8: contains `timeout` key) →
x=1.0 → 0.33. `db/database.go` (S5 density `user` hits 6/8 ×0.5) → x=0.4 → 0.17.

**Stage 3 (propagation):** `session.go` imports `auth/store.go` (import_out 0.6) and
`config/config.go`; `middleware.go` imports `session.go`; `session_test.go`
test-affinity ↔ `session.go` (0.7). After 2 hops with decay 0.5: `store.go` total 0.71
(from 0.22 seed + prop), `middleware.go` 0.87, `session_test.go` 0.84, `config.go`
0.61, `db/database.go` 0.32, `web/theme.css` 0.00.

**Stage 4:** `session.go` 1.00 (likely-edit +0.1) → **L5**; `middleware.go` 0.87 →
**L5**; `session_test.go` 0.84 → test-promotion **L4**; `store.go` 0.71 → **L3**;
`config.go` 0.61 → config-capped **L3**; `db/database.go` 0.32 → **L2**; `theme.css`
→ **L0**.

**Stage 5 (fit 16k):** L5×2 ≈ 6.1k, L4 ≈ 2.2k, L3×2 ≈ 1.8k, L2 ≈ 0.4k, tree+header
1.4k ⇒ 11.9k ≤ 16k ⇒ opportunistic promotion: `session_test.go` → **L5** (its tests
will likely be edited), `db/database.go` stays L2. Final ≈ 14.6k tokens.

`--explain` renders this whole table with per-file signal breakdown — the above trace
*is* the explain output format.

## 12. Determinism contract

1. All map iteration in stages 2–5 is over path-sorted keys, or explicit (score, path)
   tie-breaks; parallelism is confined to order-normalized stages (index building).
2. Floating point: fixed operation order ⇒ identical results on a given platform; CI
   cross-compares golden slices across Linux/macOS/Windows and pins any divergence as a
   bug (f64 associativity is controlled by ordering, not by luck).
3. No wall-clock, no randomness, no environment-dependent values in the artifact.
4. Snapshot id pins the input world: git priors and index state are read from the
   snapshot, so a re-slice after unrelated commits still reproduces if the index is
   pinned (`--index-snapshot` flag exists for exactly this test).

## 13. Tuning methodology

Constants live in one module (`cs-select::tuning`) as a named table; changing any of
them requires:

1. A run of the intrinsic benchmark (BENCHMARK §3) before/after;
2. Deltas in the PR description (recall@8k, precision, tokens);
3. CI regression gate: recall@8k on the curated set may not drop > 2 points.

What may **not** tune the constants: user anecdote, aesthetic preference, a single
repo's behavior. The benchmark corpus is the only tuning oracle. This rule is the
project's defense against heuristic drift.

## 14. Optional LLM/semantic extensions (Phase 5, additive only)

Everything above is deterministic. The optional layer may add, strictly behind
`--semantic` and strictly locally (Ollama or user-configured endpoint):

- **Query expansion:** task → additional terms/synonyms feeding stage 1 (never removing
  deterministic terms).
- **Re-ranking:** reorder stage-3 output; may raise levels, never lower forced seeds.
- **Summaries:** one-line per-file purpose notes in the tree (clearly labeled
  `synthesized`).

Guardrails: deterministic output must remain available and default; semantic mode must
beat deterministic recall by ≥5 points on the intrinsic benchmark to ship *enabled*
(roadmap Phase 5 gate); no code leaves the machine (SECURITY §11).

## 15. Relationship to prior art

| Aspect | Aider repo map | ContextSlice |
|---|---|---|
| Intent signal | Chat keywords + files already in chat | The task itself, parsed deterministically |
| Graph | File graph from tree-sitter tags; edge weight `use_mul·√refs` | Same core idea; edge kinds split (import/ref/test) with distinct weights |
| Ranking | Personalized PageRank to convergence | 2-hop bounded walk, order-normalized |
| Output | One signatures-only map | Mixed L0–L5 artifact, budget-fitted via demotion ladder |
| Budget | Binary search over tag count, soft: accepts a tree within **15% error** (`ok_err = 0.15` in `repomap.py`), so the map may exceed its target; the CLI calls the value *"Suggested"* | Greedy demotion with a hard never-exceed invariant (property-tested) |
| Persistence | SQLite tag cache (mtime-keyed) | Full versioned index (symbols/refs/edges/FTS5) reused across sessions & agents |
| Availability | Inside aider only | Standalone CLI + MCP for any agent |
| Explainability | None | `--explain` traces every decision |

We also borrow, with credit: the sqrt-damping insight from aider's edge weights, the
">5 files ⇒ uninformative name" dampener (as our 256-candidate cap), and Repomix's
observation that AST-aware compression (~70% reduction) is viable — our L2/L3 levels
are the budget-aware generalization of it.

**Correction to a widely-repeated figure (verified 2026-09-15).** Aider's repo map is
often described as "~1k tokens". That comes from its documentation, not its code:
`Model.get_repo_map_tokens()` computes `clamp(max_input_tokens / 8, 1024, 4096)`, so any
model with ≥32,768 input tokens gets **4096**, and 1024 is only the floor and the
fallback when the model's input size is unknown. The same function's budget is also soft
rather than enforced. We state this because we compare against aider in public, and
because a stale competitor figure is exactly the kind of error this project's benchmark
discipline exists to prevent.
