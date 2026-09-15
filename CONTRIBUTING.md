# Contributing to ContextSlice

Thanks for considering it. This document is deliberately short: the substantive guidance
lives in `docs/`, and where the two disagree, the document in `docs/` wins.

Before anything else, read **[README.md](README.md)** (status — the engine is not
implemented yet) and the document covering the area you want to touch.

| You want to… | Read first |
|---|---|
| Add or improve a language adapter | [docs/LANGUAGES.md](docs/LANGUAGES.md) — especially §5 and §8 |
| Change selection, weights, or thresholds | [docs/ALGORITHM.md](docs/ALGORITHM.md) — especially §13 |
| Change the index, schema, or storage | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) §5–6 |
| Publish or use a number | [docs/BENCHMARK.md](docs/BENCHMARK.md) §7 |
| Touch parsing, traversal, or dependencies | [docs/SECURITY.md](docs/SECURITY.md) |
| Propose an architectural change | [docs/adr/](docs/adr/) — add a record |

---

## The rules that are actually enforced

These are not style preferences; CI or review will block them.

1. **Every gate must pass locally before you push.** Run `make check`. It runs exactly
   what CI runs: formatting, `clippy -D warnings`, the test suite, and rustdoc with
   warnings denied. `make msrv` additionally verifies the declared Rust floor.
2. **Algorithm constants may not be tuned by intuition.** Any change to a value in
   `cs-select::tuning` requires a before/after benchmark run attached to the PR
   (ALGORITHM.md §13). User anecdote, aesthetics and one repository's behaviour are
   explicitly not tuning oracles.
3. **Behaviour changes need a test.** New selection behaviour needs a golden slice or a
   property test; new extraction behaviour needs golden fixtures; new parsing needs a
   pathological case.
4. **New dependencies need a one-paragraph justification in the PR** (MASTER_PLAN.md
   §8.1), and if the dependency changes an architectural assumption, an ADR. Versions are
   declared once, in the root manifest (ADR-011).
5. **The core stays synchronous and offline.** No networking crate, no async runtime, no
   telemetry. CI enforces this structurally, and `deny.toml` denies the usual suspects
   outright. If you believe you need an exception, that is an ADR and a conversation, not
   a `Cargo.toml` edit.
6. **stdout is reserved for artifacts.** Progress, warnings and diagnostics go to stderr.
   `contextslice "task" | pbcopy` must always pipe exactly the artifact.
7. **Docs are part of the change.** A PR that changes behaviour updates the document that
   specifies it. The `docs/` files are normative, not commentary.

## Licensing

Contributions are accepted under **Apache-2.0**, the project license. By opening a PR you
agree your contribution may be distributed under those terms. There is no CLA.

Do not paste code from another project unless its license is compatible **and** you
attribute it in the source and in the PR description. This project has already declined
to reuse query files that would have been license-compatible but technically wrong
(ADR-012); provenance matters here.

## Commits and PRs

- Conventional commits (`feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`).
- One concern per PR. An adapter addition and a scorer change are two PRs.
- PR description states what changed, why, and how you verified it. If you changed
  selection behaviour, include the benchmark delta.
- Semver from v0.1. Note that algorithm weight changes alter output bytes and are called
  out in the CHANGELOG for that reason.

## Adding a language adapter

The highest-leverage contribution, and the seam is designed to make it a one-day job
(LANGUAGES.md §8). The short version:

1. Implement `LanguageAdapter` — extraction queries plus the resolver.
2. Add `fixtures/<lang>/` covering the "hard parts" list for that language, plus the five
   pathological fixtures.
3. Add golden assertions for extracted definitions, references and imports.
4. Add ten real tasks from that language to the benchmark corpus.
5. Write the per-language spec section and state its approximations honestly.

The point of the seam is that **nothing in `cs-select`, `cs-render`, `cs-index` or
`cs-cli` should need to know your language exists.** If you find yourself editing those
crates, that is a design bug worth an issue.

## Reporting bugs

Include the output of `contextslice doctor`, the exact command, and — for selection
problems — the `--explain` table. Because slices are byte-reproducible, a slice header
plus an index snapshot id is usually enough to reproduce a selection bug exactly.

Security issues: see [SECURITY.md §12](docs/SECURITY.md). Please do not open a public
issue for those.

## Code style

`rustfmt` and `clippy` decide; both are enforced, so do not argue style in review.
Beyond that, two project conventions:

- **Comments explain why, not what.** The codebase states its reasoning, including
  rejected alternatives, because the next reader is usually asking "why not the obvious
  thing?" A comment that restates the code is noise; a comment that records a
  measurement, a constraint, or a trap is valuable.
- **Errors name the path and suggest the fix** (MASTER_PLAN.md §8.1), and privacy rules
  apply to logs: never log file contents (SECURITY.md §7).
