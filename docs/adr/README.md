# Architecture Decision Records

Every decision that outlives the code lives here (MASTER_PLAN.md §10). Each record
states the decision, the evidence behind it, the alternatives rejected, and what would
reopen it.

Records are numbered `ADR-NNN` and are never edited after acceptance. A decision that
changes gets a new record that supersedes the old one, so the reasoning trail stays
intact.

## Index

| ADR | Title | Status | Milestone |
|---|---|---|---|
| [001](ADR-001-rust-for-the-core.md) | Rust for the core | Accepted | Bootstrap |
| [002](ADR-002-sqlite-as-the-only-store.md) | SQLite (rusqlite + FTS5) as the only store | Accepted | Bootstrap |
| [003](ADR-003-approximate-reference-graph.md) | Approximate reference graph, not precise | Accepted | Bootstrap |
| [004](ADR-004-bounded-walk-before-pagerank.md) | Bounded graph walk before personalized PageRank | Accepted | Bootstrap |
| [005](ADR-005-six-representation-levels.md) | Six representation levels (L0–L5) with a demotion ladder | Accepted | Bootstrap |
| [006](ADR-006-deterministic-core.md) | Deterministic core; semantic/LLM strictly optional | Accepted | Bootstrap |
| [007](ADR-007-apache-2-license.md) | Apache-2.0 | Accepted | Bootstrap |
| [008](ADR-008-mvp-languages.md) | MVP languages: Go, TypeScript/JavaScript, Python | Accepted | Bootstrap |
| [009](ADR-009-benchmark-first-class.md) | Benchmark is a first-class subsystem | Accepted | Bootstrap |
| [010](ADR-010-no-telemetry.md) | No telemetry, ever by default | Accepted | Bootstrap |
| [011](ADR-011-dependency-manifest-and-pinning.md) | Dependency manifest, pinning, and the reconciliation rule | Accepted | Bootstrap |
| [012](ADR-012-own-tree-sitter-queries.md) | Own `.scm` queries; reject the tags crate and aider's queries as a drop-in | Accepted | Bootstrap |
| [013](ADR-013-mcp-protocol-revision-and-transport.md) | MCP target revision and transport (rmcp vs hand-rolled) | **Proposed** | Phase 2 step 14 |
| [014](ADR-014-tokenizer-choice.md) | tiktoken-rs for budget measurement | Accepted | Bootstrap |
| [015](ADR-015-competitive-landscape-verification.md) | Competitive landscape verified; three claims corrected | Accepted | Bootstrap |
| [016](ADR-016-rejected-reconnaissance-findings.md) | Reconnaissance findings rejected, and why | Accepted | Bootstrap |
| [017](ADR-017-extraction-contract-as-built.md) | Extraction contract as built (package, ref kinds, exported, aliases, degradation) | Accepted | Go extraction |
| [018](ADR-018-go-resolver.md) | Go resolver: filesystem-only, package identity, honest approximation | Accepted | Go resolver |
| [019](ADR-019-foundation-recovery.md) | Foundation recovery: audit fixes accepted, refuted, and measured | Accepted | Foundation Recovery |
| [020](ADR-020-ref-qualifier-addendum.md) | Ref qualifier addendum: structural selectors, operand suppression | Accepted | Foundation Recovery |
| [021](ADR-021-cs-index-streaming-architecture.md) | cs-index streaming architecture: facts-first three-pass, SQLite as the fact store | Accepted | cs-index (step 5) |
| [022](ADR-022-cs-index-corrective-patch.md) | cs-index corrective patch: adversarial-review findings fixed in schema v2 | Accepted | cs-index hardening (post-step-5) |
| [023](ADR-023-cs-select-v1-contract-freeze.md) | cs-select v1: scope, snapshot seam, in-memory FTS5, conservative cost model, determinism floor | Accepted | cs-select (step 6) |

## How these were verified

The bootstrap milestone began from a reconnaissance pass that produced a list of candidate
dependencies and competitive claims. Per the project's standing rule, no finding was
accepted because an agent reported it. Each was checked against current repository state,
upstream activity, licensing, compatibility with this workspace, and whether it actually
reduced work without creating architectural debt. The verification method and its result
are recorded in each ADR, and the findings that did **not** survive are recorded in
[ADR-016](ADR-016-rejected-reconnaissance-findings.md) rather than quietly dropped.

Competitive claims were verified separately and against the competitors' own source, not
their documentation, because for two of them the two disagreed. Three claims did not
survive and are corrected in [ADR-015](ADR-015-competitive-landscape-verification.md).
That record also introduces the rule now enforced in BENCHMARK.md §7: **claims of absence
are forbidden** — we may not write "nobody does X" in any public artifact.

Where a claim could be tested cheaply, it was tested rather than looked up. Three
decisions in this set (ADR-012, ADR-014 and the grammar split described in ADR-012) rest
on measurements executed against the pinned dependency versions, not on documentation.
