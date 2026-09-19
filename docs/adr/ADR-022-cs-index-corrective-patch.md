# ADR-022: cs-index corrective patch — adversarial-review findings fixed in schema v2

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-19 |
| **Milestone** | cs-index step 5 hardening (post-M8 corrective pass) |
| **Follows** | [ADR-021](ADR-021-cs-index-streaming-architecture.md), [ADR-019](ADR-019-foundation-recovery.md) (disposition-by-execution rule) |

## Context

A 12-agent adversarial review of the cs-index milestones M2–M8 as built at
`b84944a`, followed by an independent arbitration, produced nine canonical
defect reports (CF-01…CF-09) and one verdict: **continue after targeted
fixes**. The ADR-021 architecture is unchanged — every confirmed finding is
an implementation defect or a frozen-contract gap inside that architecture,
not a flaw in the streaming design itself. The recovery applied the ADR-019
rule: nothing is accepted from the page. Each claim was re-confirmed at
HEAD, reproduced by a failing test, fixed minimally, and locked with a
regression test; the one claim already dissolved by an earlier stage's fix
(CF-08) is recorded here as fixed, with the verification that the final tree
carries a single derived-row writer. The schema version moves 1 → 2; why
that needs no migration path is D10.

## Decisions

**D1 — Schema v2: the `files` truth table is the scanner's.** (CF-02)
The v1 CHECK could not represent the scanner's `Unreadable` state — a
listed-but-unreadable file (I/O failure during the walk) has no hash and
parsed nothing, which the v1 constraint rejected — so the cold path
mislabeled such files `too_large` and the incremental path crashed on the
constraint. Worse, the v1 OR-chain evaluated to NULL for
`(hash NULL, skip NULL)`, and SQLite treats a NULL CHECK as satisfied: an
impossible row was admissible. The v2 CHECK (schema.rs, `files` table) pins
the scanner's four states exactly:

| Scanner state | hash | skip | parse_status |
|---|---|---|---|
| read, ≤ parse cap, parsed | blake3 | NULL | `ok` / `partial` / `timeout` (extraction's verdict); `skipped` for manifests, `unsupported` for Unknown/Unsupported languages |
| read, parse-cap band (hashed, > 1 MiB) | blake3 | `parse_skipped` | `skipped` |
| over read cap (> 50 MiB, never read) | NULL | `too_large` | `skipped` |
| unreadable at scan (never read) | NULL | `unreadable` | `skipped` |

```sql
CHECK (
    (hash IS NULL
        AND COALESCE(skip, '') IN ('too_large', 'unreadable')
        AND parse_status = 'skipped')
    OR (hash IS NOT NULL AND (skip IS NULL OR skip = 'parse_skipped'))
),
CHECK (parse_status IN ('ok', 'partial', 'timeout', 'skipped', 'unsupported'))
```

The `COALESCE` anchor makes the nullable arm total — never NULL — closing
the `(NULL, NULL)` hole. Both ingestion paths (cold `facts.rs` and
`incremental.rs`) share one `skip_label` mapping and consult it *before*
any `fs::read`: a file the scanner did not read or did not parse is not
read here either, so the read cap holds during incremental updates too. A
read that fails during ingest (the scanner hashed the file, this pass
cannot read it) persists the only truthful row — `(NULL, 'unreadable',
'skipped')` — dropping the scanner's hash, because the row must describe
what *this index* read; the next run's hash diff re-extracts the file if it
becomes readable. Schema v2 also adds `idx_imports_resolved_dir` (D2's
lookup, shipped with the schema so a later query cannot degrade into a
full scan) and makes `meta.key` and `manifests.path` explicitly `NOT NULL`
(a TEXT PRIMARY KEY on a rowid table does not imply it).

**D2 — Reverse invalidation is package-scoped, as ADR-021 D5 always
specified.** (CF-03) The as-built code joined `binding_targets` into the
*changed file* only, so an edit to a sibling file that no caller binds
into silently skipped the package's importers (their `import_out` edges
fan out over every file of the package). The fix computes the invalidation
set per *package*: for the pre-update packages of modified and deleted
files, re-resolve P itself, every package holding bindings into any file
of P (queried before the fact transaction, so targets into files about to
be deleted are still visible), and every package with an import whose
`resolved_dir` = P's directory — the only signal that covers blank
imports, which bind nothing (`idx_imports_resolved_dir` serves the
lookup). A post-transaction importer fan-out covers packages discovered
during extraction (added files). All sets are `BTreeSet`s iterated in
sorted order; the oracle in tests is full edge-set parity with a fresh
build of the same tree, including a blank-import case.

**D3 — Packages left with no files are garbage-collected.** (CF-04) No
`packages` DELETE existed, so a deleted or renamed-away package survived
as an empty ghost that `non_test_package` (which picks `min()` over the
directory's names) deterministically preferred over the real package in
every import resolution. The fact transaction now ends with
`DELETE FROM packages WHERE id NOT IN (SELECT DISTINCT package_id FROM
files WHERE package_id IS NOT NULL)`, covering deletions and in-place
renames alike.

**D4 — Torn incremental updates are marked, visible, and repaired by
re-derivation.** (CF-01) An incremental run is phased: classification
(read-only), one fact transaction, per-package derived transactions, one
finalize transaction. Between the facts commit and the finalize commit the
index was `ready` with new facts and stale derived rows — falsely coherent,
indistinguishable from a finished update, and the fast-path no-op would
skip it forever. The fact transaction now plants `meta.update_in_progress`
inside itself (a rollback leaves neither facts nor marker); only the
finalize transaction — which commits the new `snapshot_id` — clears it.
The next run reads the marker *before* the no-op fast path and repairs.
Repair is a **full derived rebuild from the hash-consistent facts**, not a
surgical set: the affected set is genuinely unknowable after the fact (the
importer fan-out keys on `imports.resolved_dir`, which the torn run
already reset to NULL for re-inserted rows; a crash mid-derivation may
have committed some packages already). Derived state is a deterministic
pure function of facts, so rebuild-all provably equals a fresh build —
asserted as full semantic parity in tests. The cost is repo-proportional
and paid only after a crash. `doctor` surfaces the marker as an
`UPDATE_INCOMPLETE` warning and detects actual projection damage
(`PROJECTION_INCOMPLETE`, `IMPORTS_UNCLASSIFIED`); `ResetToEmpty` clears
the marker. A run that creates or collects a package (D3) also takes the
rebuild-all path: importers of a package that was missing carry NULL
resolutions, so no scoped set can name them.

**D5 — Resume is hash-driven in the library; the CLI resumes and never
resets.** (CF-06) `ingest_facts` on a `building` index previously skipped
by *path*, so a file edited after the interrupt kept its stale facts
forever — and the CLI made matters worse by resetting `building` indexes
to empty, discarding completed batches. Resume now loads each stored
file's hash and skips only when it still equals the scanner's (a `None`
hash never matches `Some`); a changed file is re-ingested in place via
`INSERT … ON CONFLICT(path) DO UPDATE … RETURNING id`, so the file id is
stable and derived rows a torn resolve left behind keep a live FK target,
with the stale facts and edges deleted first. The `index` command resumes
a `building` index (`ingest_facts` + the idempotent `resolve_facts` of
D9); `--rebuild`, which deletes the database file, is the only reset. The
parity test asserts the resumed database equals a fresh build of the
modified tree on every fact table.

**D6 — Busy/locked failures funnel to `IndexError::Locked`; the pragma
contract is shared and verified.** (CF-05) `IndexError::Locked` existed
but was unreachable: `classify()` mapped only `NotADatabase`, and rusqlite
silently installs a 5-second busy timeout at open, so real writer
contention stalled 5 s per statement and then surfaced as a generic
SQLite error. `classify()` now maps `DatabaseBusy | DatabaseLocked` to
`Locked` and is the single funnel for statement, prepare, and commit
failures (a deferred BEGIN takes no write lock under WAL, so contention
first surfaces on the write, not the transaction open). The connection
contract — WAL, `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=0`
— lives in one `apply_connection_contract` applied by `open_or_create`
*and* `Doctor::repair_file`, with each pragma verified on the live
connection, not assumed. Blocked writers now fail as `Locked` in under
50 ms (pinned by tests at `begin_build` and on the fact write path). One
documented deviation: `Doctor::check_file` opens read-only and applies
only `busy_timeout=0` — a read-only diagnostic handle cannot re-journal
the foreign or damaged files doctor exists to diagnose.

**D7 — Lifecycle transitions are guarded; the open gate validates
structure, not strings.** (CF-07 + the two-builder race) `begin_build`
and `mark_ready` are conditional UPDATEs (`AND value = 'empty'` /
`'building'`) with a rows-affected check, so two concurrent builders
cannot both pass a read-then-write race; zero rows yields
`StateConflict { found, expected }` (or `Corrupt` if the row is missing
or unparsable). The open gate previously accepted any file whose `meta`
table carried plausible strings — a table-less database claiming `ready`
opened fine and failed at the first query. `validate_existing` now
requires the exact schema version, a parseable `index_state`, all ten
schema tables (one `sqlite_master` count), and — for `ready` — a
`snapshot_id` that is present and non-empty, since the flip commits the
snapshot atomically with itself.

**D8 — `nearest_module` always has a root.** (CF-09) Pass 2 pushed a root
pseudo-module only when `modules` was *empty*, so a tree whose only
manifest was `sub/go.mod` attributed root-level files to the sub-module.
`build_exported_index` now pushes a synthetic root entry (`dir ""`, path
`""`) whenever no module with `dir == ""` exists; the existing
longest-dir-first sort guarantees it sorts last, making
`nearest_module`'s last-entry fallback correct by construction. The
in-workspace resolver already masked the wrong attribution with its own
fallback, so the fix is behavior-preserving for resolution and closes the
defect in the public `ExportedIndex::nearest_module` API; a consequence
test pins that a root file's import into a sub-only module tree stays
cross-module.

**D9 — One derived-row writer.** (CF-08) At `b84944a` the derived-row
writes (`imports.resolved_*`, `bindings`, `binding_targets`, `edges`) were
duplicated between `resolve_pass.rs` and `incremental.rs` with divergent
pre-cleanliness (incremental pre-deleted its rows; the cold path did not).
The crash-coherence work (D4) needed a per-package writer that repair
could call, which dissolved the duplication: `DeriveEngine::resolve_package`
is now the single definition of the projection, used by the cold resolve
pass, the incremental derived phase, and torn-update repair. It pre-clears
every derived row it is about to rewrite (bindings, binding_targets, and
outgoing edges per file), so re-running against facts that already carry
derived rows converges to exactly the fresh-build rows — no duplicates, no
orphans — which is also what makes `resolve_facts` idempotent for D5's
resume. Verified by grep: production writes to the derived tables exist
only inside `DeriveEngine::resolve_package`.

**D10 — Schema v2 ships without a migration path.** ADR-021 D6's policy
is unchanged and is *why* v2 needs none: until a released on-disk format
exists, a version mismatch means rebuild (`IndexError::SchemaMismatch`
names the remedy). A v1 index opened by this build fails the open with
that error; `contextslice index --rebuild` produces a v2 index. Migration
machinery is deferred until a released format makes it earn its
complexity.

## Consequences

- All nine canonical findings are fixed and regression-locked; the
  strongest oracles are fresh-build parity: incremental edits (including
  sibling edits, blank imports, package renames, parse-cap transitions,
  package recreation) must leave every derived table identical to a cold
  build of the same tree, and a scripted multi-step edit history must
  parity with a cold build of the final tree.
- `Ready` now means what ARCHITECTURE §4.4 promises: either a coherent
  complete index, or a *marked* torn one that the next run repairs before
  anything trusts it.
- Known residual, verified against the final code: `doctor`'s
  `FILES_CONSISTENCY` predicate still encodes the v1 rules and flags the
  legitimate `(hash NULL, skip 'unreadable')` row of D1 as an error; it
  should be aligned with the v2 CHECK the next time doctor is touched.
  (The v2 CHECK itself is unaffected — the row is stored fine; only the
  diagnostic over-reports.) The cs-index crate doc's claim that a mid-run
  crash "leaves an index that is merely as-of an earlier prefix" is
  likewise superseded by D4's marked torn window.
- Determinism is preserved end to end: invalidation sets, package
  iteration, and repair all iterate sorted keys; no new dependencies; no
  new `parse_status`/`skip` vocabulary beyond what the v2 CHECK pins.

## What was deliberately NOT done

- **No staging tables, no generation counters.** ADR-021 D3's position
  held under attack: the marker plus rebuild-from-facts (D4) buys crash
  coherence without a second copy of the truth or a schema-wide generation
  stamp on every row.
- **No migrations pre-release** (D10), and no backfill for v1 indexes.
- **No busy timeout.** Contention still fails fast by design; D6 made the
  failure honest and immediate, not patient.
- **No change to ADR-021's pass structure, transaction sizes, or the
  resolver seam.** The fixes are inside the architecture; none of them
  reopens it.

## What would reopen this

- A released on-disk format users persist across upgrades — migration
  machinery and stable vN semantics (ADR-021's reopen list, D10 here).
- A repository where post-crash full re-derivation is measurably too
  expensive — surgical torn-update repair would need a durable affected
  set written *before* the facts commit; measure first.
- A real multi-writer requirement (still fail-fast by design).
- A scanner state outside D1's four-row truth table (a new skip reason is
  a schema change and a new ADR).
