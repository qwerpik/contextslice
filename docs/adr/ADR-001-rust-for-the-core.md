# ADR-001: Rust for the core

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), verified 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

ContextSlice needs a multi-language parser, a fast repository walker, an embedded
database, a CLI, and a static binary distribution story. The candidate host languages
were Rust, Go, TypeScript/Node, and Python.

## Decision

Implement the core selection engine, index, and CLI in Rust, as a cargo workspace of
focused crates (ARCHITECTURE.md §3).

## Consequences

- tree-sitter's Rust bindings are first-class, and the grammars reach us through a
  stable ABI crate (`tree-sitter-language`), so grammar and runtime versions decouple.
- The ripgrep family of crates (`ignore`, `globset`, `grep-searcher`) is directly
  embeddable rather than shelled out to.
- Static musl/macOS/Windows binaries with no runtime dependency.
- The distribution penalty — users need a binary, not `npx` — is recovered by an npm
  wrapper package that carries prebuilt binaries (MASTER_PLAN.md §15 step 15).
- Workspace MSRV is **1.90**, set by the highest floor among our dependencies:
  `tree-sitter` 0.27 and `tree-sitter-language` 0.1.8 both require 1.90, which outranks
  `cargo-deny` 0.20 (1.88) and `proptest` 1.11 / `assert_cmd` 2.2 (1.85). The toolchain
  used for bootstrap is 1.96.
- This number is **tested, not asserted**: the CI `msrv` job runs
  `cargo check --workspace --all-targets` on the declared toolchain. The first draft of
  this manifest declared 1.88 on the basis of the dev-tool floors alone and was wrong —
  the tree-sitter grammars set a higher bar — which is exactly why the job exists.

## Alternatives rejected

- **Go** — excellent concurrency and single-binary story, but tree-sitter bindings are
  cgo-based, and the crate ecosystem for PDF/BM25/CLI ergonomics is weaker here.
- **TypeScript/Node** — best install reach, but a per-file-parsing indexer over 100k
  files is the wrong runtime shape, and shipping a native parser dependency is fragile.
- **Python** — best benchmark-harness ergonomics (SWE-bench tooling lives there), but
  unacceptable for the parsing/indexing hot path. Python is used only on the *benchmark*
  side where the ecosystem dictates it (BENCHMARK.md §4.2); that is scripts, not core.

## What would reopen this

A requirement that the core run in a browser or inside a JS host without a native
binary, which would make a Rust core a liability rather than an asset. Nothing on the
roadmap implies this before Phase 6.
