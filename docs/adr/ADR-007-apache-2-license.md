# ADR-007: Apache-2.0

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), verified 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

ContextSlice is a developer tool that people will run against private source code. The
license must be unambiguous for corporate users and must not create doubt about patent
exposure.

## Decision

Apache-2.0 for the whole workspace. No CLA; contributions are inbound under the same
license (CONTRIBUTING.md).

## Consequences

- Explicit patent grant, which MIT lacks — material for a tool adopted inside companies.
- Matches the ecosystem norm for this space: aider, ast-grep, and SCIP are all
  Apache-2.0, so users already have the compliance conversation done.
- **Dependency license compatibility**, verified on 2026-09-15 against crates.io
  metadata: every direct dependency is permissively licensed — `rmcp` Apache-2.0,
  `tiktoken-rs` MIT (should it be adopted), `tree-sitter` and all three grammars MIT,
  `rusqlite` MIT, `ignore` Unlicense OR MIT, `blake3` CC0-1.0 OR Apache-2.0 OR
  Apache-2.0 WITH LLVM-exception, `clap`/`rayon`/`git2`/`serde`/`serde_json`/
  `thiserror`/`toml`/`tempfile`/`assert_cmd`/`predicates`/`proptest` MIT OR Apache-2.0,
  `tracing` MIT, `insta` Apache-2.0. `cargo-deny` enforces this list in CI so a
  copyleft dependency cannot be merged unnoticed.
- The `blake3` CC0-1.0 option is fine but the Apache-2.0 option is what we rely on; the
  dual grant means no attribution obligation either way.

## Alternatives rejected

- **MIT** — no patent grant, and the extra permissiveness buys nothing for a CLI tool.
- **AGPL** — would exclude the internal-corporate use case that is a primary audience.
- **Dual MIT/Apache** — the Rust norm, but adds a compliance question without adding a
  benefit Apache-2.0 does not already provide.

## What would reopen this

A dependency with a copyleft license that is load-bearing and has no alternative. The
CI license gate is the tripwire, and the response would be a superseding ADR explaining
what was accepted and why, not a silent allowlist entry.
