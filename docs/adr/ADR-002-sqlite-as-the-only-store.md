# ADR-002: SQLite (rusqlite + FTS5) as the only store

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-14 (recorded), verified 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

The index must hold files, symbols, references, imports, and a weighted edge graph, be
incrementally updatable, survive being copied and inspected, and require no server or
daemon for a single-user local tool (ARCHITECTURE.md §2.6).

## Decision

One SQLite database per repository at `.contextslice/index.db`, accessed through
`rusqlite` with WAL mode and FTS5 for symbol search. No second store, ever.

## Consequences

- Zero operations: no daemon, no port, no upgrade path to manage.
- The index is inspectable with the stock `sqlite3` CLI, which is a feature for users
  debugging why a file was or was not selected.
- `PRAGMA quick_check` gives `doctor` a real corruption test.
- FTS5 provides BM25 ranking for signal S4 (ALGORITHM.md §5) without a bespoke index.
- `rusqlite` is pinned to 0.40.2 with the `bundled` feature, so the SQLite version does
  not depend on the host's shared library. This trades compile time for reproducibility,
  which is the right trade for a tool that must behave identically on three platforms.
- Incrementality keys on the blake3 content hash of each file; unchanged hashes skip
  re-parsing.

## Alternatives rejected

- **A bespoke on-disk format** — explicitly on the over-engineering guard list
  (MASTER_PLAN.md §8.2). We would reimplement durability, transactions and migrations
  badly.
- **An embedded key-value store (sled/LMDB)** — no query language, no FTS, and we would
  hand-roll every lookup `doctor` and `inspect` need.
- **A file-per-symbol or JSON sidecar layout** — no atomic snapshot semantics, poor
  incremental story.

## What would reopen this

Measured evidence that SQLite is the bottleneck at the 100k-file target (MASTER_PLAN.md
§9). The performance plan already assumes batched transactions and WAL; if that proves
insufficient, the answer is a read-optimized derived structure built *at slice time*,
not a different store.
