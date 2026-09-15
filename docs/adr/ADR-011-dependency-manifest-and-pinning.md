# ADR-011: Dependency manifest, pinning, and the reconciliation rule

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

ARCHITECTURE.md §14 named a candidate technology stack before any dependency had been
checked against reality: whether each crate exists, is maintained, is licensed
compatibly, and actually builds against the rest. A pre-implementation blueprint that
names a crate is a *hypothesis*, and hypotheses about the ecosystem decay fast.

This ADR records what was verified, what was changed as a result, and the rule that keeps
the manifest and the documents from drifting apart.

## Decision

**1. The workspace manifest is the single source of dependency versions.** Each crate
declares `foo.workspace = true`; versions and features are declared exactly once, in the
root `Cargo.toml`, under a heading that names the architecture section the dependency
serves.

**2. Every dependency is pinned to an exact version.** Grammars especially: LANGUAGES.md
§9 requires grammar pins because tree-sitter minor releases have changed node types
before, and a grammar change silently alters extraction. Pins are visible in the manifest,
not buried in a lockfile, so a bump is a reviewable diff.

**3. Verified inventory (2026-09-15, live crates.io and GitHub metadata):**

| Crate | Version | License | Verified status |
|---|---|---|---|
| `ignore` | 0.4.33 | Unlicense OR MIT | active (2026-08-04) |
| `globset` | 0.4.18 | Unlicense OR MIT | active |
| `blake3` | 1.8.7 | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception | active (2026-08-20) |
| `tree-sitter` | 0.27.0 | MIT | active (2026-08-30) |
| `tree-sitter-go` | 0.25.0 | MIT | active upstream (2026-09-13) |
| `tree-sitter-typescript` | 0.23.2 | MIT | see caveat below |
| `tree-sitter-python` | 0.25.0 | MIT | active (2026-09-11) |
| `rusqlite` | 0.40.2 | MIT | active (2026-08-08) |
| `git2` | 0.21.0 | MIT OR Apache-2.0 | active (2026-05-18) |
| `tiktoken-rs` | 0.12.0 | MIT | verified offline (ADR-014) |
| `rayon` | 1.12.0 | MIT OR Apache-2.0 | active |
| `clap` | 4.6.7 | MIT OR Apache-2.0 | active (2026-09-14) |
| `serde` / `serde_json` | 1.0.229 / 1.0.151 | MIT OR Apache-2.0 | active |
| `thiserror` | 2.0.20 | MIT OR Apache-2.0 | active |
| `tracing` | 0.1.44 | MIT | active |
| `toml` | 1.1.6 | MIT OR Apache-2.0 | active (2026-09-10) |

Dev-only (`tempfile`, `assert_cmd`, `predicates`, `proptest`) are likewise permissive;
none reaches a released binary.

**4. The grammar caveat, stated plainly.** `tree-sitter-typescript` has published no new
crates.io version since **2024-11-11**, while its upstream GitHub repository shows commits
on 2026-09-13. That gap is a *release* gap, not abandonment, and it is safe for us
because all three grammars depend on `tree-sitter-language ^0.1` rather than on
`tree-sitter` itself — the language ABI is decoupled from the runtime version. This was
confirmed by execution, not by reading: all pinned grammars load and parse under
`tree-sitter` 0.27.0 (see §Verification). It is recorded here so the next person does not
rediscover it and panic.

**5. The reconciliation rule.** ARCHITECTURE.md §14 originally listed technologies as a
one-line summary. It now states that dependency facts live in the manifest and the ADRs,
and that §14 is a pointer rather than a second source of truth. Two documents asserting
versions independently is how the `rmcp` mismatch in ADR-013 happened.

## Consequences

- A version bump is a one-line diff in the root manifest plus a golden-fixture re-run.
- `cargo-deny` enforces licenses, advisories and duplication in CI, so the verified table
  above is checked continuously rather than trusted once.
- Adding a dependency requires a one-paragraph justification in the PR
  (MASTER_PLAN.md §8.1), and if it changes an architectural assumption, an ADR.

## Verification (2026-09-15)

Executed, not looked up:

- A probe crate depending on `tree-sitter` 0.27 with all three pinned grammars compiled
  and parsed Go, TypeScript, TSX, JSX and Python source with no error nodes.
- `tiktoken-rs` 0.12.0 was built and used to encode a Go snippet with `o200k_base` with
  no network access, confirming the encoder tables are embedded.
- The TypeScript/TSX grammar asymmetry documented in ADR-008 was measured across eight
  syntax cases.

## Alternatives rejected

- **Pinning only in `Cargo.lock`** — invisible to review, and the lockfile is regenerated
  by unrelated commands.
- **Range constraints on grammars** — LANGUAGES.md §9 explains why: node-type drift must
  be impossible to miss.
- **Vendoring grammars** — adds an update burden and a supply-chain surface we would have
  to maintain ourselves.

## What would reopen this

A grammar crate going unmaintained for materially longer than the current release gap, or
a `tree-sitter` major release that changes the language ABI in a way
`tree-sitter-language` does not absorb — in which case the pinned set is re-verified and
this ADR superseded.
