# ContextSlice — Master Plan

| | |
|---|---|
| **Status** | Pre-implementation blueprint (no code written yet, by design) |
| **Version** | 1.0 — 2026-09-14 |
| **Implementation language** | Rust (decided 2026-09-14; rationale in §16, ADR-001) |
| **Companion documents** | [ARCHITECTURE.md](ARCHITECTURE.md) · [ALGORITHM.md](ALGORITHM.md) · [BENCHMARK.md](BENCHMARK.md) · [SECURITY.md](SECURITY.md) · [LANGUAGES.md](LANGUAGES.md) |

---

## 0. How to read this document

This is the founding product and engineering plan for ContextSlice. It is written so that a
strong engineer who has never spoken to the author can pick up the project and build it
without re-deriving the architecture. It deliberately records *why* decisions were made,
including the parts of the original concept that research invalidated.

- **MASTER_PLAN.md** (this file) — product, competition, differentiation, MVP, roadmap, quality bar, risks, implementation sequence.
- **ARCHITECTURE.md** — crate-level system design, data model, storage, concurrency, failure modes, packaging.
- **ALGORITHM.md** — the context-selection algorithm in full detail: signals, scoring, graph propagation, levels, budget fitting, determinism, tuning.
- **BENCHMARK.md** — evaluation methodology, metrics, baselines, statistical rules, publication ethics.
- **SECURITY.md** — threat model, prompt injection, untrusted repositories, privacy, supply chain.
- **LANGUAGES.md** — language tiering, the LanguageAdapter contract, per-language extraction/resolution specs, how to add a language.

---

## 1. Executive summary

ContextSlice is a **local-first, deterministic context-selection engine for AI coding agents**.

Given a natural-language task and a repository, it decides **which files the agent should
see and at what level of detail**, and emits a single token-budgeted artifact: full source
for the files most likely to be modified, declarations/skeletons for relevant dependencies,
path-only entries or nothing for the rest. It ships as a CLI (`contextslice "fix the auth
timeout"`), a persistent local index, and an MCP server usable from Claude Code, Codex CLI,
OpenCode, Cursor, Cline, and anything else that can read stdin or speak MCP.

The core analysis is **deterministic and offline**: tree-sitter parsing, a symbol/reference
index in SQLite, per-language import resolution, and a bounded graph walk. No embeddings, no
LLM calls, no network access — those are strictly optional, additive layers in a later phase.

The project's credibility rests on a **published, reproducible benchmark** (intrinsic
gold-context recall/precision per token, plus extrinsic agent success/cost runs), not on
token-reduction marketing. Token reduction is only useful if it preserves task success.

---

## 2. Challenging the idea — research verdict

The original concept was stress-tested against the ecosystem as of September 2026 before
this plan was written. Verdict: **the core idea survives, but three assumptions do not.**

### 2.1 What is already solved (do not rebuild)

| Capability | Solved by |
|---|---|
| Whole-repo → single LLM-ready file, token counting, secret scanning, MCP packaging | Repomix (~28k★), gitingest (~15.5k★), code2prompt (~7.7k★), files-to-prompt (~2.8k★) |
| Signature-only repo maps under a token budget (tree-sitter tags → file graph → personalized PageRank) | Aider's repo map (~49k★) — proven algorithm, but welded inside Aider and seeded only by chat keywords |
| On-demand symbol navigation for agents over LSP, via MCP | Serena (~29.3k★) |
| Hybrid BM25 + embedding code search behind MCP | claude-context (~12.5k★; requires an embedding provider + vector DB) |
| Agentic grep/glob/read exploration loops with compaction and subagents | Claude Code, Codex CLI, OpenCode, Cline — *by design* they keep no repo index |
| Server-side embedding indexes with privacy controls | Cursor (Merkle-tree sync), Roo Code (local embeddings + Qdrant) |
| Gold-context evaluation methodology for context selection | ContextBench (arXiv 2602.05892, Feb 2026: 1,136 tasks, human-annotated gold contexts, recall/precision/efficiency) |

### 2.2 What is weaker than assumed (corrections)

1. **Precise call graphs are not available as a building block.** tree-sitter deliberately
   does not resolve names; GitHub's stack-graphs (the candidate solution) was **archived in
   September 2025** (re-verified 2026-09-15: `archived: true`), and the tree-sitter
   tags crate is in low maintenance — its tags convention depends on query predicates
   the core Rust binding silently ignores (ADR-012), so we own our queries. Precise
   reference edges exist only in heavyweight per-language indexers (SCIP, gopls,
   rust-analyzer) that are too slow/fragile for an always-on local index.
   **Consequence:** the MVP builds an *approximate* reference graph — import-scoped
   name matching — and is honest about it. Precise SCIP/LSP "deep modes" become optional
   later enhancements (see ARCHITECTURE §12).

2. **Agentic agents are not the enemy — and they are getting better at search.** Claude
   Code and Codex intentionally do agentic search instead of maintaining an index
   (Anthropic has written about this trade-off explicitly). Cursor added fast-grep and
   explore subagents on top of its embeddings. A tool whose only pitch is "agents can't
   find code" will decay as models improve.
   **Consequence:** ContextSlice is positioned as a **complement**: it front-loads
   navigation (fewer exploration turns, lower cost, lower latency, reproducible starting
   context), and it serves flows agentic search does not cover at all — one-shot context
   assembly, CI/review pipelines, planning documents, humans pasting context into a chat.

3. **"How many tokens did we remove?" is the wrong north star.** Repomix's
   `--token-budget` is a **post-hoc CI guard, not a fitter**: verified at source level
   (`src/cli/cliTokenBudget.ts`) it throws *after* writing the over-budget pack, and its
   own docs say "the output is still generated; only the exit code signals the
   overflow". Repomix does have three per-file inclusion levels
   (`output.patterns` — full / tree-sitter-compressed / directory-structure-only), but
   they are **hand-declared globs in a config file with no CLI flag**, so the *user*
   chooses the granularity; the budget does not. uithub truncates by file size.
   Aider's map budget is documented as "Suggested" and accepts a ±15% error, so it can
   exceed its own target.
   So: we could not find a tool that fits a budget by *choosing* granularity per file
   under a hard ceiling. Separately, the claim that "nobody publishes agent-outcome
   evaluation" is simply **false** and has been withdrawn: RepoGraph (ICLR 2025) ablates a
   repository-context module on SWE-bench resolver rates with released code,
   SWE-ContextBench compares named shipped context tools on resolution accuracy and cost,
   ContextBench publishes a fixed-harness leaderboard with Pass@1, and Aider has shipped a
   reproducible end-to-end leaderboard for years. What remains true and worth building is
   narrower: *we* will publish our harness, corpus and logs so our own numbers are
   checkable. **Consequence:** the benchmark is a first-class subsystem
   (BENCHMARK.md), and every public claim about ContextSlice must trace to a
   reproducible run. This project publishes no claim resting on a competitor's absence:
   the defensible statement is about what *we* measure, not about what others don't.

4. **There is published evidence against the category's core premise, and it is aimed at
   exactly the kind of artifact we produce.** arXiv 2602.11988 (v2, Jun 2026) evaluates
   coding agents with and without repository context files and finds that context files
   *"do not generally improve task success rates, while increasing inference cost by over
   20% on average"*, across different LLMs, agents, and for both LLM-generated and
   developer-committed files. Its most pointed finding: *"repository overviews, although
   popular and recommended by model providers, are not helpful."*
   **Consequence:** this is recorded as risk #4 in §11, treated as a falsification risk
   rather than a rebuttal exercise. It removes any legitimacy from an "agents need more
   repository context" pitch, and it makes phase 3's extrinsic gate the project's actual
   go/no-go rather than a marketing step. The one honest gap between that study and this
   product — it measures static overview/instruction files, not task-conditioned selection
   of the files a change will touch — is a hypothesis for Phase 3 to test, not a citation
   to hide behind.

### 2.3 The refined thesis

> Coding agents need the *smallest sufficient* slice of a repository, assembled
> deterministically, fitted to a token budget, and graded by task outcomes — not another
> packer, not another cloud index.

The differentiating trio, stated as narrowly as the evidence supports: **(a)**
task-seeded selection, **(b)** budget *fitting* that chooses per-file granularity (L0–L5)
under a hard never-exceed ceiling, **(c)** published evaluation against gold contexts
with an intrinsic regression gate in CI.

Honesty about each, as verified 2026-09-15 (and revised after a second verification
pass falsified part of the first draft of this section):

- **(a) is not unique.** Aider seeds from chat keywords; Graft's `graft ask` does
  deterministic task-conditioned ranking with no LLM. Task-seeding is table stakes.
- **(b) is narrower than we first wrote, and must be stated as a mechanism, not a
  capability.** An earlier draft said "nobody makes the budget itself decide each file's
  level". That is **wrong**, and it is corrected here: at least two tools ship
  budget-driven, per-item mixed granularity. jCodeMunch packs under `token_budget` with
  budget-derived per-item pruning; more directly, `ypollak2/llm-router` builds a
  *"progressive disclosure"* context in which each symbol is emitted at full source if it
  fits and **falls back to signature-only if it does not** (`code_context.py`,
  `_build_progressive_context`). The capability exists in shipped code.
  What distinguishes our approach is the *mechanism*, and it is worth being precise:
  llm-router is **inclusion-first and greedy** — it sorts symbols by size, tries each
  once, and either takes the full source or a signature *at the moment of insertion*. It
  never revisits a decision, so a large symbol admitted early is never demoted to make
  room for a more relevant later one; it simply stops at 90% of budget. Level assignment
  there is a per-item fallback, not an allocation.
  ContextSlice treats the budget as a **global allocation problem**: every candidate is
  assigned a level, then demotion proceeds by *gain-per-loss* across all files
  (ALGORITHM §8), with invariants no fallback loop can express — forced seeds never drop
  below L2, at least three files stay at ≥L2 whenever a seed exists, and the rendered
  artifact never exceeds the budget. That, plus task-conditioned weighting of levels
  rather than symbol-name matching, is the defensible claim.
- **(c) is thinner than we assumed, and the honest citation is RepoGraph, not a
  vendor.** "Nobody publishes agent outcomes" is false, and the decisive counterexample
  is **RepoGraph** (ICLR 2025, arXiv 2410.14684): a repository-context module ablated on
  SWE-bench with released code — a reproducible agent-outcome ablation of exactly the kind
  of mechanism we build. Aider adds the shipped-tool precedent (reproducible polyglot
  leaderboard, though it never ablates its repo map). The vendor evidence is weaker:
  Graft's SWE-bench run is n=50, and our own exact two-sided McNemar computation puts the
  power to detect a true 12-point effect at that size at only **2–6%** (BENCHMARK §4.2).
  The standard we hold ourselves to is therefore not "first to publish" but
  *reproducible and adequately powered* — a harder bar, and a more useful one.

**A fourth near-miss worth naming (verified 2026-09-15).** **SnapZip**
(`MTEnt/SnapZip`, ~3★) is the closest match to this project's *positioning*: local-first
CLI, SQLite + FTS5 index, task modes, read-only MCP stdio server, and packs that report
budget use and truncation. It satisfies task-conditioned selection with a budget
(`snapzip pack --query "…" --mode debug --limit 5 --budget 12000`), but it has **no
granularity levels at all** — a grep of its README for signature/skeleton/granular/stub
terms returns zero hits, and the dominant knob is `--limit`, a result count. It degrades
by *omitting* ranked results and blind-truncating, surfacing "truncation" as a quality
warning. That is the fourth independent near-miss doing task-conditioning without
budget-driven granularity (with Aider, jCodeMunch and Graft), and at ~3★ it also shows
the *idea* is being attempted by others. The differentiation must therefore be argued on
the mechanism, never on novelty of concept.

---

## 3. Product definition

1. **Core problem.** Agents today receive either whole-repository dumps (token bloat,
   attention dilution, cost) or must discover structure themselves through exploratory
   tool calls that consume turns, tokens, and wall-clock time — rediscovered from scratch
   every session. Nothing answers, deterministically and locally: *for this task, which
   files, at what level of detail, within N tokens?*
2. **Primary user.** A developer running terminal coding agents (Claude Code, Codex CLI,
   OpenCode, Cline, Aider) against a medium-to-large repository (500–100k files), who
   wants the agent to start from the right code instead of grep-poking for ten turns.
   Secondary: builders of agents and internal platforms who need a context layer they can
   call via CLI/library/MCP.
3. **Core workflow.**
   ```text
   $ contextslice "fix the authentication timeout bug"
   (first run: builds .contextslice/index.db — ~seconds to minutes)
   → prints a markdown slice to stdout:
     full source of auth/session.go, auth/middleware.go,
     declarations of auth/store.go, config/config.go,
     path-only entries for everything else, elision counters,
     ≈9.4k tokens instead of ≈310k
   ```
   The user pipes this into their agent, or the agent fetches it via the MCP server.
4. **One-sentence pitch.** *"ContextSlice gives coding agents exactly the code they need
   for a task — full source where you'll work, skeletons where it matters, nothing else —
   fitted to a token budget."*
5. **Minimum feature that creates real value.** Task in → ranked, budget-fitted,
   mixed-granularity slice out, for Go and TypeScript repositories, deterministic and
   fully offline. Everything else (Python, MCP, git signals, benchmark reports) amplifies
   this but does not replace it.
6. **What ContextSlice explicitly does NOT try to do.**
   - It is **not an agent** — it never edits code, runs tests, or converses.
   - It is **not a packer** — it will lose "pack my whole repo" beauty contests to Repomix
     and will not chase that feature set.
   - It is **not precise IDE navigation** — no go-to-def guarantees; approximate edges,
     clearly labeled (deep modes later).
   - It is **not cloud, not embeddings-first, not LLM-dependent** — the core must work
     with zero network, zero API keys, forever.
   - It does **not summarize code content** — it selects and truncates structurally;
     LLM summaries are out of scope for the core (optional later layer).

---

## 4. Competitive landscape (verified September 2026)

| Tool | ~Stars | What it does | Selection? | AST | Graph | Embeddings | LLM | Local | Install friction |
|---|---|---|---|---|---|---|---|---|---|
| Repomix | 28.3k | Pack repo → XML/MD/plain/JSON; token counts; secretlint; MCP | Filtering only; budget *errors* | Experimental `--compress` (tree-sitter) | No | No | No | Yes | `npx` / brew / docker |
| gitingest | 15.5k | Digest: summary + tree + files | No | No | No | No | No | Yes | pip |
| code2prompt | 7.7k | TUI packer, Handlebars templates, git context | No | Entity-map option (nav aid) | No | No | No | Yes | cargo / brew / pip |
| files-to-prompt | 2.8k | Minimal `cat` of files | No | No | No | No | No | Yes | pip |
| uithub | small | URL→text service | Size-ordered truncation | No | No | No | No | No (service) | none |
| Aider repo map | 49k | In-agent repo map: tags → graph → PageRank → signatures-only tree (1024–4096 tokens, soft target) | Yes (chat-keyword seeded) | tree-sitter | File graph | No | No | Yes | pip (inside aider) |
| Serena | 29.3k | MCP symbol tools over LSP; memories | None (on-demand tools) | LSP | No | No | No | Yes | uvx |
| claude-context | 12.5k | MCP hybrid BM25+dense chunk search | Retrieval, not assembly | Chunking only | No | Yes (provider) | No | Partial (needs provider) | npx + vector DB |
| Cursor | — | Editor with server-side embedding index + grep | Proprietary | Chunking | No | Yes | No | No (cloud) | editor |
| Graft | 8.0k | Graph CLI/MCP: task → ranked nodes + `file:line`; `skeleton` = signatures only; publishes SWE-bench Verified | Yes (`graft ask`, deterministic) | tree-sitter | Symbol graph | No | Optional (`--deep`) | Yes | npm |
| SnapZip | ~3 | Local codebase memory: SQLite+FTS5 index, task modes, MCP stdio; packs report budget use | Yes (`pack --query --budget`) | lexical + compression distance | Dependency graph | No | No | Yes | local CLI |
| ContextSlice | — | Task → budget-fitted mixed-granularity slice; persistent index; CLI+MCP | Yes (task-seeded, deterministic) | tree-sitter | File + approx. symbol graph | No (later, optional) | No (later, optional) | Yes | cargo / brew / npx wrapper |

Detailed per-tool analysis is in §2.1 and the research notes embedded in ALGORITHM.md §15
(aider comparison) and BENCHMARK.md §6 (ContextBench alignment).

---

## 5. Differentiation and moat

1. **The only standalone selector that *fits* a token budget instead of failing or
   truncating blindly.** Demotion ladder (L5→L3→L2→L1→L0) with invariant guarantees
   (ALGORITHM §8), elision markers, and a "how to ask for more" affordance.
2. **Mixed granularity in one artifact.** No tool automatically mixes full files,
   declaration skeletons, and a path-only tree graded per item. Repomix's per-glob detail
   levels are hand-configured; Aider's map is signatures-only.
3. **A persistent, cross-session index** (SQLite + FTS5) that any agent can consume via
   CLI, library, or MCP — instead of every agent re-implementing exploration per session.
4. **Benchmark-driven credibility.** First tool in the category to ship an intrinsic
   gold-context regression benchmark in CI and publish an extrinsic agent-outcome report.
   This is the moat: reproducible numbers, updated every release.
5. **Local-first, zero-dependency.** No embedding provider, no API key, no server.
   Privacy as a feature (SECURITY.md).

Honesty: there is no patent moat here — Aider proved the algorithm's value in 2023. The
moat is execution speed, engineering quality, and accumulated evaluation data plus tuning
derived from it. Claude-context could add deterministic ranking; Repomix could add
selection. Our answer is to be two phases ahead on benchmarking and adapter quality, and
to stay narrowly excellent.

---

## 6. MVP specification

**In scope (v0.1):**
- Languages: **Go, TypeScript/JavaScript, Python** (LANGUAGES.md).
- Full pipeline: scan → extract (tree-sitter + owned `.scm` queries) → resolve imports →
  index (SQLite, incremental by content hash) → select (seed + propagate + levels +
  budget fit) → render (markdown, JSON; XML experimental).
- Git signals: file recency only (co-change coupling postponed to Phase 5–6).
- CLI: `slice` (default), `map`, `index`, `inspect`, `doctor`, `--explain`, `--tokens`,
  `--include`, `--format`, `--out`. Full CLI spec in §6.1.
- MCP server: `contextslice_slice`, `contextslice_map`, `contextslice_get_file`,
  `contextslice_search_symbols`, `contextslice_index_status` (ARCHITECTURE §10).
- Intrinsic benchmark harness + published report on ≥3 real repositories (BENCHMARK.md).
- Docs: README quickstart (<5 minutes to first slice), plus these documents polished.

**Out of scope for v0.1** (see §12 for the full "not yet" list): embeddings, any LLM use,
co-change coupling, watch mode, SCIP/LSP deep modes, editor plugins, multi-repo,
custom tokenizers, Windows-specific polish beyond CI builds.

**Estimated effort:** 8–10 focused weeks to v0.1 for an experienced Rust engineer
(Phase 0: 1–2 wk; Phase 1: 3 wk; Phase 2: 4–5 wk including benchmark report).

**Top engineering risks:** see §11.

### 6.1 CLI specification (normative)

```
contextslice "fix the auth timeout bug"           # slice for task → stdout
contextslice "task text" [OPTIONS]                # OPTIONS:
  --tokens <N>          budget (default 16000; 0 = unlimited)
  --format <F>          markdown | json | xml (default markdown)
  --out <FILE|-\>       write to file instead of stdout
  --include <GLOB>...   force-include paths (seeds at max weight)
  --explain             print scoring table to stderr
  --no-git              disable git signals
  --index <PATH>        override index location
contextslice index [--rebuild|--prune]            # build/refresh/maintain index
contextslice map [--tokens N]                     # task-less repo map (aider-style)
contextslice inspect <PATH|SYMBOL>                # what the index knows
contextslice mcp [--stdio]                        # run MCP server
contextslice doctor                                # env/diag self-check
contextslice version | help
```

Defaults and conventions:
- Diagnostics → stderr, slice → stdout; clean piping always works.
- Exit codes: `0` success · `2` usage error · `3` index/budget error (with remediation
  hint) · `4` internal error (please report, with `doctor` output).
- First run auto-indexes with a progress line on stderr (never blocks stdout).
- `--explain` output names every included file's seed signals, propagated score, level,
  and the decision that placed it there — debuggability is a feature, not a debug mode.

---

## 7. Roadmap

Every phase ends with a **gate**; the next phase does not start until the gate passes.
Gates are benchmark- or test-based, never vibes-based.

| Phase | Duration | Content | Exit gate |
|---|---|---|---|
| **0 — Prototype** | 1–2 wk | Workspace bootstrap (CI, fmt, clippy, test), scanner crate, Go extraction prototype, throwaway selection on a real pinned Go repo | Seed+2-hop selection reaches **gold-file recall ≥ 0.8 @ 8k tokens** on 10 curated historical issues; CI green |
| **1 — Core engine** | 3 wk | `cs-extract` (Go), `cs-resolve` (Go), `cs-index` (SQLite + incremental), `cs-select` v1, `cs-render`, golden-file test harness | Determinism property tests pass; budget invariant holds; warm slice < 1s on 10k files |
| **2 — Strong MVP** | 4–5 wk | TS/JS adapter (tsconfig paths, re-exports), Python adapter, CLI polish, MCP server, intrinsic benchmark + 3-repo report, v0.1 release | v0.1 installable in one command on Linux/macOS/Windows; published benchmark report with CIs |
| **3 — Benchmark validation** | 2–3 wk | Extrinsic runs (fixed agent loop + fixed model, 4 context variants, ~50 tasks), publish report | ContextSlice beats naive dependency-crawl on recall@same-tokens **and** beats full-repo on tokens@same-recall, with statistical significance; otherwise iterate algorithm (weights, levels) before proceeding |
| **4 — Agent integrations** | 2 wk | AGENTS.md/CLAUDE.md skill snippets, npx wrapper (`@contextslice/cli`), homebrew tap, docs site | A new user goes from zero to a ContextSlice-seeded agent session in <5 min, verified on 3 agents |
| **5 — Semantic layer (optional)** | 3–4 wk | Opt-in embeddings + local LLM hooks (Ollama): query expansion, re-ranking, summaries — strictly additive behind `--semantic`; co-change git coupling | Semantic mode improves intrinsic recall by ≥5 pts over deterministic on the regression corpus, else it ships disabled |
| **6 — Production/community scale** | ongoing | Rust/Java/C# adapters, SCIP/LSP deep modes, watch mode, monorepo improvements, plugin API, ContextBench cross-evaluation | Continuous: every PR that changes selection must attach benchmark deltas |

**What must be proven before Phase 4 (the "is this useful?" checkpoint):** Phase 3's
extrinsic results show either improved task success at similar tokens, or equal success at
significantly fewer tokens/turns. If neither holds, the project pivots to the strongest
finding (e.g., pure repo-map mode, or CI context assembly) before investing in
integrations.

---

## 8. Engineering quality standards

### 8.1 What good looks like

- **Repository:** cargo workspace, one concern per crate (ARCHITECTURE §3), public docs
  target for every public item, `#![deny(missing_docs)]` on library crates.
- **Testing:** unit tests per crate; golden-file tests for extraction (fixture corpus per
  language, including pathological files) and rendering (byte-stable snapshots);
  property tests for the two hard invariants — *budget never exceeded* and *determinism
  (same index snapshot + task ⇒ byte-identical output)*; integration tests on pinned real
  repositories cached in CI; the intrinsic benchmark doubles as a regression test.
  Core crates target ≥85% line coverage.
- **Error messages:** every error names the file/path involved, states what was attempted,
  and suggests the fix; error codes `CS-Exxx` documented; `doctor` diagnoses common
  environment problems; **logs never print file contents** (privacy, SECURITY §7).
- **Logging/tracing:** `tracing` crate, levels via `--verbose`, span per pipeline stage;
  default output is quiet (one progress line) because the primary consumer is a pipe.
- **CI:** fmt + `clippy -D warnings` + test + coverage on PRs; `cargo-deny` and
  `cargo-audit`; cross-platform release builds (musl x86_64/aarch64, macOS, Windows);
  benchmark smoke job on a small corpus with recall regression gate.
- **Releases:** conventional commits, `cargo-release`, CHANGELOG maintained, semver from
  v0.1, GitHub Releases with static binaries; npm wrapper package version-pinned to
  release tags.
- **Cross-platform:** Linux/macOS first-class, Windows CI-green from Phase 1 (paths,
  encoding, file locking tested; perf tuning may lag).
- **Dependencies:** minimal, audited, pinned grammars (LANGUAGES §9); any new dependency
  needs a one-paragraph justification in the PR.

### 8.2 What would be overengineering before validation (explicit guard list)

No server/daemon, no gRPC, no plugin *marketplace*, no custom storage engine, no
distributed cache, no GPU code, no multi-repo/workspace graph, no streaming/watch mode,
no web UI, no telemetry infrastructure, no microservice split, no custom tokenizer
development, no IDE extension — **until** the Phase 3 gate proves the core value and user
demand justifies each one individually. Every item on this list has a trigger condition
in the roadmap; until the trigger fires, the answer is no.

---

## 9. Performance strategy (summary; details in ARCHITECTURE §8)

| Scenario | Target |
|---|---|
| Cold index, 1k files | < 8 s |
| Cold index, 10k files | < 60 s (8-core parallel parse) |
| Cold index, 50k files | < 5 min |
| Cold index, 100k files | < 12 min |
| Incremental refresh (typical <5% dirty) | < 2 s |
| Warm slice (index present) | < 1 s @10k files, < 3 s @50k |
| CLI startup overhead | < 50 ms |
| Memory (indexing 50k files) | < 1 GB RSS |
| Index size | 5–15 MB per 10k files |

Means: parallel parsing (rayon), content-hash incremental re-parse (blake3), SQLite WAL
with batched transactions, in-memory CSR graph built at slice time, FTS5 for name lookup,
ripgrep-family crates for traversal/content prefilter, parse-size caps with graceful
skipping.

---

## 10. Open-source strategy

- **License: Apache-2.0.** Matches the ecosystem norm for this space (aider, ast-grep,
  SCIP), provides an explicit patent grant, and is corporate-friendly — important for a
  tool people will run on private repositories.
- **Contribution model.** Algorithm changes require a benchmark delta attached to the PR
  (enforced socially, then by CI). Language adapters are the ideal first contribution:
  self-contained, template-guided, verifiable by golden fixtures (LANGUAGES §8).
  ADRs (architecture decision records) in `docs/adr/` for decisions that outlive code.
- **Release strategy.** Semver; minors carry adapter additions; algorithm weight changes
  are flagged in the CHANGELOG because they alter output bytes; every release re-runs the
  intrinsic benchmark and refreshes the public report.
- **Documentation strategy.** README = 5-minute quickstart with a real repository;
  `docs/` = these six documents kept current (docs PRs required for behavior PRs);
  mdBook site in Phase 4; every `--explain` string doubles as algorithm documentation.
- **Attractive to contributors because:** narrow, well-specified seams (LanguageAdapter,
  scorer signals), goldens instead of vibes, benchmark scores instead of arguments, and a
  documented path from issue → adapter → merged PR in a day.
- **What stays simple forever:** the storage layer (one SQLite file), the output contract
  (stdout artifact), the install (one binary). Plugin-based *eventually*: language
  adapters and scorer signals are the two plugin seams, but plugins ship as crates in-tree
  first; an out-of-tree plugin API is a Phase 6 decision.

---

## 11. Key technical risks and mitigations

| # | Risk | Impact | Mitigation |
|---|---|---|---|
| 1 | TypeScript import resolution incompleteness (path aliases, bundler aliases, `index.ts` re-export chains) | Missing edges → missing files in slice | Heuristic resolver with explicit *unresolved-alias* marking and degradation (LANGUAGES §6.2); publish resolution-rate % per corpus; optional ts-morph/SCIP deep mode later |
| 2 | Approximate refs over/under-linking without type info (overloads, `export *`, Go embedded promotion) | Noise or gaps in propagation | Sqrt-damping on ref counts, bounded hops, container scoping; weights tuned against the intrinsic benchmark, not intuition |
| 3 | Grammar/query churn (tree-sitter minor breaking releases) | CI breakage, extraction drift | Pinned grammar versions per release (LANGUAGES §9); golden fixtures catch drift early |
| 4 | **Published evidence that repo-context injection does not help.** arXiv 2602.11988 (v2, Jun 2026) finds that providing context files *"does not generally improve task success rates, while increasing inference cost by over 20% on average"*, across different LLMs and coding agents, and that *"repository overviews, although popular and recommended by model providers, are not helpful"* | **Existential to the premise.** If task-conditioned selection inherits that result, the product has no reason to exist | Treated as a genuine falsification risk, not a positioning problem. Three things follow. (a) **Do not pitch repo overview.** The study discredits static overview material, and ContextSlice's value proposition must rest on delivering the *exact files to edit*, not on orienting the agent. Map mode (§6.1) is the closest thing we have to an overview and is explicitly the fallback, not the product. (b) The study's own conclusion — that performance claims "should be rigorously evaluated before deployment" — is exactly the discipline BENCHMARK.md imposes, so the honest response is to *measure*, not to argue. (c) Phase 3's gate becomes a genuine stop/go: if extrinsic results show no task-success improvement at equal tokens, §7's pivot clause applies to the strongest actual finding — most plausibly CI/review context assembly, where a human reads the artifact and no agent-improvement claim is needed. Note the study evaluates *static context files and overviews*, which is a different intervention from task-conditioned selection of the files a PR will touch; that distinction is a hypothesis to test at Phase 3, **not** a rebuttal to cite. |
| 5 | Determinism vs. git signals and parallelism | Unreproducible slices, broken property tests | Index snapshot id frozen into slice header; parallel parse then deterministic sort-by-path before persistence; git signals read from the pinned snapshot |
| 6 | Solo-maintainer scope explosion | Stalled project | Adapter seams + benchmark-first culture let contributors extend without core access; §8.2 guard list; gates between phases |
| 7 | Benchmark credibility challenged (corpus leakage, agent variance) | Reputational | BENCHMARK.md governance: pinned SHAs, paired stats, published harness/seeds/logs, explicit non-claims |
| 8 | SQLite index contention/corruption in long-lived repos | User friction | WAL, single-writer discipline, `doctor --rebuild`, schema migrations with version pinning |

---

## 12. What NOT to build initially

Embeddings or any vector store; any LLM dependency in core; any network egress;
cloud/hosted offering; editor plugins (VS Code/JetBrains); precise type-aware navigation;
stack-graphs (archived upstream — rejected); watch/file-server mode; GUI/web dashboard;
multi-repo workspace graphs; custom tokenizers; a plugin marketplace; per-user accounts
or telemetry of any kind. Each of these has a roadmap trigger; none is triggered before
Phase 3's gate passes.

---

## 13. Definition of "excellent"

The project is *excellent* when all of these are simultaneously true:

1. **Determinism:** same index snapshot + task + flags ⇒ byte-identical output, enforced
   by property tests (ALGORITHM §12).
2. **Budget integrity:** the rendered artifact's measured token count never exceeds the
   budget — property-tested with adversarial inputs (ALGORITHM §8).
3. **Speed:** warm slice < 1 s on a 10k-file repo; incremental refresh feels instant.
4. **Defensibility:** every file's inclusion is explainable via `--explain`; every public
   performance claim cites a reproducible benchmark run.
5. **Trust:** zero network in core; logs and errors never leak file contents; works
   offline forever (SECURITY.md).
6. **Craft:** error messages name the file and the fix; docs let a newcomer add a
   language adapter in a day; CI blocks regressions in recall, not just in compilation.

---

## 14. First implementation milestone

**"A trustworthy slice on a real Go repository."**

Pin one real, mid-size Go repository at a fixed SHA. Curate 10 historical issues that
(a) reference a symptom, (b) were resolved by a merged PR, (c) have tests in the PR.
Success: `contextslice "<issue text>"` includes **≥ 80% of gold (PR-touched) files at
level ≥ L3 within an 8,000-token budget**, deterministically (two runs ⇒ identical
bytes), warm in < 1 s, with `--explain` accounting for every inclusion.

This milestone is deliberately narrow: it validates extraction, resolution, selection,
budgeting, and rendering end-to-end on the easiest language before any breadth is added.

---

## 15. Implementation sequence

Each step has a completion criterion; do not start step N+1 until step N's criterion
holds. Steps 1–9 are Phase 0+1, steps 10–15 are Phase 2.

1. **Bootstrap workspace + CI.** Crates skeleton, fmt/clippy/test CI, README stub,
   Apache-2.0, CONTRIBUTING, ADR folder.
   *Done when:* CI is green on a trivial PR with `-D warnings`.
2. **`cs-scanner`.** Walk with `ignore`/`globset`, extension→language map, size caps,
   symlink-loop protection, blake3 hashing.
   *Done when:* 10k-file synthetic tree traversed+hashed < 5 s; gitignore honored
   (fixture test); pathological fixtures (loops, unreadable files) fail gracefully.
3. **Go extraction.** Owned `.scm` queries: definitions, references, imports,
   signatures, doc comments.
   *Done when:* golden fixtures extract 100% of definitions/imports and ≥95% of
   references on the Go corpus, including generics and error-laden files.
4. **Go resolver.** Module-relative import paths → files; stdlib/external marked
   `external`; ref→def matching scoped by package + container.
   *Done when:* edge table verified correct on a fixture repo with internal packages,
   vendor dir, and external deps.
5. **`cs-index`.** SQLite schema (ARCHITECTURE §5, ADR-021 streaming-first 3-pass pipeline),
   incremental updates by content hash and surgical reverse-invalidation, snapshot ids,
   schema versioning, `doctor` corruption diagnosis and repair (FTS5 deferred to step 6).
   *Done when:* second run parses 0 unchanged files (tested, **6.6 ms** no-op); corruption → `doctor`
   detects and rebuilds (tested); 10k-file cold index < 60 s (measured **13.1 s**); 50k files
   RSS < 512 MiB (measured **417.72 MiB**, down 8× from 3.30 GiB). Completed in Milestone M0–M8.
6. **`cs-select` v1.** Seeding, bounded propagation, level assignment, budget fitting,
   fallbacks (ALGORITHM.md).
   *Done when:* determinism and budget property tests pass; golden slices for 5 fixture
   tasks; empty-seed fallback enters map mode (tested).
7. **`cs-render`.** L0–L5 rendering, markdown + JSON (+XML experimental), anchors,
   elision counters, header contract.
   *Done when:* byte-stable goldens; every symbol carries `path:line`; measured tokens ≤
   budget on adversarial corpus.
8. **`cs-cli`.** All commands from §6.1 with exit codes and stderr contract.
   *Done when:* end-to-end `contextslice "task"` on the pinned Go repo, warm < 1 s;
   `--explain` renders the full scoring table.
9. **First-milestone gate (§14).** Recall ≥ 0.8 @ 8k on the 10 curated Go issues;
   report written into `docs/benchmarks/go-milestone.md`.
   *Done when:* numbers reproduced twice from clean clone.
10. **TypeScript/JavaScript adapter.** Extraction + tsconfig/`node_modules`/`paths`
    resolution with unresolved-alias degradation.
    *Done when:* goldens cover aliases, `index.ts` re-export chains, monorepo packages;
    resolution-rate reported on 3 real TS repos.
11. **Python adapter.** pyright-style static import resolution (root/src layouts, venv
    site-packages best effort, relative imports).
    *Done when:* goldens cover relative/absolute/namespace-package cases; degradation
    documented.
12. **`cs-git`.** Recency signals (co-change deferred).
    *Done when:* score-delta tests on fixture git history; `--no-git` disables cleanly.
13. **Intrinsic benchmark + public report.** ≥200-PR corpus across ≥3 repos;
    recall/precision/tokens vs 4 baselines with bootstrap CIs (BENCHMARK §3).
    *Done when:* report published; CI regression gate active.
14. **`cs-mcp`.** Five tools over stdio (ARCHITECTURE §10).
    *Done when:* scripted end-to-end against Claude Code and OpenCode; untrusted-content
    warnings present in tool descriptions.
15. **Hardening + v0.1.** Error-message pass, `doctor`, README/docs, cargo-release with
    musl/macOS/Windows artifacts, npx wrapper, homebrew tap.
    *Done when:* one-command install works on all three platforms; v0.1 tagged;
    CHANGELOG and benchmark report attached to the release.

---

## 16. Decision log (summarized; full records in [`docs/adr/`](adr/))

| ADR | Decision | Why (one line) |
|---|---|---|
| 001 | Rust for the core | First-class tree-sitter bindings, embeddable ripgrep crates, static binaries, perf headroom; distribution reach recovered via npx wrapper (ast-grep/Biome model) |
| 002 | SQLite (rusqlite + FTS5) as the only store | De-facto architecture for this niche; incremental, zero-ops, portable; no server ever |
| 003 | Approximate reference graph, not precise | stack-graphs archived 2025; precise indexers too heavy for always-on local use; approximation is honest and benchmark-tunable |
| 004 | Bounded graph walk before personalized PageRank | Deterministic, cheap, explainable, order-independent; PageRank variant stays available behind a flag for comparison |
| 005 | Six representation levels (L0–L5) with a demotion ladder | Mixed granularity is the differentiation; budget must *fit*, never error |
| 006 | Deterministic core; semantic/LLM strictly optional and additive | Privacy, cost, reproducibility; Phase 5 gate requires measured uplift to even ship enabled |
| 007 | Apache-2.0 | Ecosystem norm + patent grant + corporate friendliness |
| 008 | MVP languages: Go, TypeScript/JS, Python | Coverage of agent usage × parser quality × resolution tractability (LANGUAGES §2) |
| 009 | Benchmark is a first-class subsystem | Claims must be reproducible; tuning must have a compass |
| 010 | No telemetry, ever by default | Trust for private-repo users (SECURITY §3) |
| 011 | Dependency manifest is the single source of versions; exact pins | Two documents asserting versions independently caused the `rmcp` mismatch |
| 012 | Owned `.scm` queries; no tags crate, no aider queries as a drop-in | tags predicates are silently ignored by core tree-sitter — measured |
| 013 | MCP revision + transport (**Proposed**, decided at Phase 2 step 14) | Current revision removed the handshake; `rmcp` needs tokio for stdio |
| 014 | tiktoken-rs with embedded tables | Budget estimation and measurement must share one code path, offline |
| 015 | Competitive landscape verified; three claims corrected | Verified against competitors' source; budget-fitting is a narrower moat than assumed |
| 016 | Reconnaissance findings rejected, with reasons and reopen triggers | Rejected findings must stay rejected on the record |
| 017 | Extraction contract as built: package_name, ref kinds, exported, import aliases, normative degradation policy | Implementing the Go adapter resolved nine open contract questions (ADR-017) |
| 018 | Go resolver: filesystem-only, package identity (dir, name), unique-only methods + universe-method filter, honest unbound reasons | Every approximation measured on gin/chi before freezing (ADR-018) |
| 019 | Foundation recovery: audit fixes accepted (E1–E8), four findings refuted on evidence, scaling claims corrected by measurement | Nothing is accepted or refuted from the page — each item settled by probe, test, or measurement (ADR-019) |
| 020 | `Ref.qualifier` structural, operands suppressed, rule table over the qualifier | Selector relationships are syntax: read them where the tree is in hand, never guessed from byte distance (ADR-020) |
| 021 | cs-index streaming architecture: facts-first three-pass pipeline, SQLite as the fact store, package + reverse invalidation, no staging tables | The 3.30 GiB @50k measurement forced the design; every schema delta is a named decision in ADR-021 |

Full records live in [`docs/adr/`](adr/) — see the [ADR index](adr/README.md). ADR-011
through ADR-016 were produced by the bootstrap verification pass: they record what was
checked against upstream reality, what the check changed, and what would reopen each
decision. ADR-017 came out of the Go extraction milestone.

---

## 17. Progress log

### 2026-09-15 — Go extraction complete (step 3 of §15)

**Delivered:** the Go extraction adapter (`cs-extract/src/go/`), the first
implementation of the extraction contract, plus the golden-fixture regime the
remaining adapters inherit: 20 fixtures in `fixtures/go/` covering every
category of §15 step 3 (functions, methods/pointer receivers, interfaces,
structs, embedding, aliases, generics, consts/vars, grouped declarations,
aliased/dot/blank imports, package/doc comments, closures, test files,
build-tagged pairs, pathological formatting, malformed files with defined
degradation), each with a byte-exact JSON golden; 30 unit tests; determinism
tests (re-extraction and reversed batch order); a perf harness.

**Verified:** all goldens reviewed line-by-line against their fixtures (three
real bugs found and fixed during review: variadic-parameter names leaking as
refs, refs surviving inside dropped error declarations, blank `_` defs).
Measured on real repositories (release build, single-threaded): gin — 99
files / 0.7 MiB in 0.24 s (2.74 MiB/s, 411 files/s, 1,699 defs, 0 partial);
chi — 84 files / 0.3 MiB in 0.11 s (2.94 MiB/s, 733 files/s, 539 defs,
0 partial). Against the §9 budget (~10 MB/s/core parse): 100k files
extrapolates to ≈2 min single-threaded, well inside the 12-minute cold-index
target. No premature optimization warranted by the measurements.

**Contract changes:** recorded in ADR-017 and LANGUAGES.md §6.1 (as-built),
ARCHITECTURE.md §4.2/§5 updated to match.

**Next bottleneck:** §15 step 4 — the Go resolver (`cs-resolve`'s Go
`LanguageResolver`: `go.mod` module-prefix mapping, directory→package
expansion, external marking, then same-package/export-only ref binding).
Extraction now produces everything the resolver consumes (`package_name`,
ref kinds, `exported`, import aliases); nothing in extraction blocks it.

### 2026-09-15 — Go resolver complete (step 4 of §15)

**Delivered:** the Go resolver (`cs-resolve/src/go.rs`, ~700 lines) behind
the reshaped two-phase `LanguageResolver` trait (`prepare`/`resolve`), with
package identity `(dir, package_name)`, nearest-`go.mod` module mapping
(nested-module trap handled), qualifier scopes from aliases/package
clauses (never path tails), bind-all for build-tag variants, unique-only
bare-method binding, universe-method filter, Go build semantics for test
files, and a full unbound-reason taxonomy. Extraction amendments in the
same milestone: `call_ref` (call-position selectors) and struct-shaped
composite-literal keys as field-name positions — both audit-driven, both
golden-regenerated and re-reviewed (ADR-018 §7).

**Verified:** 21 fixture-matrix tests (`fixtures/go-resolve/`, 28 files —
every rule pinned incl. the chi `_examples` trap, 4-way tag duplicates,
`foo`/`foo_test` isolation, dot/blank/relative/vendor/missing imports);
edge+stats golden with byte-identical determinism across runs and input
order; real-repo gates **met** (docs/benchmarks/resolver-gin-chi.md):
in-repo import resolution 31/31 (gin) and 27/27 (chi) = 100% ≥95%;
manual precision audit n=315 sampled bindings ≈97.2% ≥95% (Wilson 95% CI
LB ≈94.7% reported); zero cross-`_test` and zero nested-module binds;
resolve 10–11 ms per repo (budget <1 s). Two FP classes fixed mid-audit
(literal keys, universe-method receivers); residual ≈3% (local shadowing,
cross-package interface names) documented and damped.

**Contract changes:** ADR-018; ARCHITECTURE §4.3 rewritten as-built and
§5 schema fixed (`imports.resolved_dir` — an import binds a multi-file
package, not one file); LANGUAGES §6.1 resolution section as-built.

**Next bottleneck:** §15 step 5 — `cs-index` (SQLite schema per
ARCHITECTURE §5 incl. the resolver's bindings/edges, content-hash
incremental updates, snapshot ids, FTS5). The resolver now produces
everything the index persists (`ResolvedRepo` with per-file bindings and
damped edges); nothing in resolution blocks it.

### 2026-09-16 — Foundation Recovery complete (post–step-4 hardening pass)

**What was done:** every finding of the independent 50-item adversarial
audit (`docs/reviews/gemini-independent-audit.md`) dispositioned by
execution, not argument — accepted fixes E1–E8, empirically refuted
findings (F-21, F-43, F-26, F-40), and measured corrections to our own
claims, all recorded in [ADR-019](adr/ADR-019-foundation-recovery.md)
with the qualifier contract split into
[ADR-020](adr/ADR-020-ref-qualifier-addendum.md). Highlights: scanner
as-built hardening (`.git` exclusion, walk-time caps, read cap, mtime,
string-sort ordering); the parse timeout actually enforced (tree-sitter
progress callback → `ParseStatus::Timeout`); extraction facts fixed
(complete 44-name sorted universe, `:=` left-only, alias/named-type
targets, doc override, raw-string imports, `//go:`-adjacent docs, generic
calls as `call_ref`); `Ref.qualifier` made structural with operand
suppression, deleting the resolver's byte-distance qualifier heuristic;
`no_scope` revived for computed operands; resolver robustness (`/vN`
fallback, `go.mod` comment/tab parsing, exact manifest match, dir→packages
index, `main` exclusion, `Run` in the universe list, ambiguity-after-
test-visibility, `internal/` visibility as `UnresolvedReason::Internal`,
dampener as a returned flag — atomics deleted); three quadratics removed
(`container_for`, `collect_defs`, `only_directive_lines`: 1 MiB extraction
11.6 s → ~270–340 ms, linear); CLI `mcp --stdio` bare flag and
no-artifact-on-failure pinned; BENCHMARK §4.2's mathematically wrong
McNemar power table recomputed exactly (script inline in the doc).

**Headline evidence:**

- Golden regeneration audited line-by-line: **PASS** with zero unexplained
  hunks (`docs/reviews/golden_diff_audit.md`).
- Gin/chi gates re-passed at pinned SHAs (gin `5c6a15f8…`, chi
  `b1c9ab47…`): in-repo imports 100%/100%, 152 sampled bindings 0 false
  positives (Wilson LB ≈ 97.5%), coverage 23.3%/18.4%
  (`docs/benchmarks/resolver-gin-chi.md`).
- Property tests: three proptest suites × 256 cases (arbitrary bytes,
  Go-shaped mixtures, resolver graphs).
- Scaling measured, not assumed (`docs/benchmarks/scaling_report.md`):
  cold 10k pipeline **9.72 s** single-threaded (budget 60 s); peak RSS
  **3.30 GiB @ 50k** — the ARCHITECTURE §8 "<1 GB @50k" target is
  **exceeded** by the current materializing API, so **streaming is
  mandatory in cs-index**; the resolver's claimed "~100 s @10k" is
  refuted (sub-second).

- `cs-index` (Step 5) completed per ADR-021 streaming-first architecture:
  - 3-pass streaming pipeline: Pass 1 fact batching (≤1000 files/txn), Pass 2 in-memory exported definitions index + blake3 `snapshot_id`, Pass 3 package-by-package resolution and derived rows write-through.
  - Incremental engine: hash-diffing with zero re-parsing for unchanged files; surgical reverse-invalidation of external callers via `binding_targets`.
  - Doctor corruption diagnosis and repair engine (`PRAGMA quick_check`, `PRAGMA foreign_key_check`, meta lifecycle verification, dangling row audits).
  - CLI integration: `contextslice index [--rebuild|--prune] [dir]` and `contextslice doctor`.
  - All scale gates empirically verified: cold 10k index in **13.1 s** (< 60 s gate); incremental no-op in **6.6 ms** (< 2 s gate); peak RSS at 50k files in **417.72 MiB** (< 512 MiB gate, down 8× from 3.30 GiB).

**Next bottleneck:** §15 step 6 — `cs-select` v1: seeding, bounded propagation, level assignment, budget fitting, fallbacks (ALGORITHM.md).
