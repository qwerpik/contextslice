# ADR-019: Foundation recovery — audit fixes accepted, refuted, and measured

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-16 |
| **Milestone** | Foundation Recovery (post–step-4 hardening pass) |
| **Follows** | [ADR-017](ADR-017-extraction-contract-as-built.md), [ADR-018](ADR-018-go-resolver.md), [ADR-011](ADR-011-dependency-manifest-and-pinning.md) (reconciliation rule) |

## Context

An independent adversarial audit (the 50-finding Gemini review,
`docs/reviews/gemini-independent-audit.md`) attacked everything built through
MASTER_PLAN §15 step 4 — scanner, extractor, resolver, CLI contract, and the
documents' own claims. The recovery pass worked through every finding by one
rule: **nothing is accepted or refuted from the page; each item is settled by
execution** — a probe, a test, or a measurement. This ADR is the durable
record of that settlement, in three registers: fixes accepted (E1–E8),
findings refuted on evidence (F-21, F-43, F-26, F-40), and claims about our
own documents corrected by measurement. The numbering below is this record's
inventory of the recovery work items.

## Decisions

### A. Fixes accepted (E1–E8), with the evidence that closed each

| # | Fix | Evidence one-liner |
|---|---|---|
| E1 | **Scanner as-built hardening**: never descend into `.git/`; `require_git(false)` so extracted archives get ignore semantics; caps enforced *during* the walk; 50 MiB read-cap (listed, never read/hashed) layered over the 1 MiB parse cap; `mtime` captured; sort by normalized **string** path, not `PathBuf` component order (F-27) | Unit tests pin each behavior; string ordering now matches SQLite BINARY collation (`"a-b/c.go"` and `"a/b/c.go"` order differently under the two comparisons — the old code silently disagreed with its own database) |
| E2 | **Parse timeout actually enforced**: tree-sitter `ParseOptions` progress callback aborts the parse cooperatively; new `ParseStatus::Timeout` labels the file with no facts; a spent-budget pre-check makes degenerate budgets deterministic | `zero_budget_parse_times_out_as_a_labeled_degradation` test; a 250 ms wall-clock budget against a measured 66–74 ms max-cap parse (see D below) |
| E3 | **Extraction facts fixed**: universe = the complete 44-name Go predeclared set, strictly sorted for binary search (`f32`/`f64` are *not* predeclared and were dropped); `:=` suppresses left sides only; alias/named-type targets become `type_ref`s; grouped-decl inner docs override the group comment (F-20: `ModeFast.doc` golden fix); raw-string imports captured; `//go:` directives do not break doc adjacency; generic instantiation calls (`pkg.F[T]()`) are `call_ref` | All pinned by goldens + unit tests; universe test asserts `len == 44` and strict sortedness |
| E4 | **`Ref.qualifier` is structural** (see [ADR-020](ADR-020-ref-qualifier-addendum.md)): the selector's operand identifier rides on the qualified ref; operand occurrences are no longer emitted as refs; deletes the resolver's byte-distance `preceding_qualifier` heuristic (F-11's chained-selector FP class) and the local-shadows-package FP class at the source | `comma_separated_arguments_never_form_a_package_qualifier` regression test; golden diff audited hunk-by-hunk (`docs/reviews/golden_diff_audit.md`: 31 refs removed, all explained) |
| E5 | **Resolver selector rules rebuilt on the structural qualifier**: qualified refs bind through the import scope; **computed operands revive `NoScope`** (F-35's dead reason now fires: `w.Header().Add` can never be claimed as a local method); unknown-identifier operands keep the bare-instance binding rule (`rp.Add` → unique method) | Fixture-matrix pins for all three operand classes; gin/chi precision re-audit at pinned SHAs: 152 sampled bindings, 0 false positives (`docs/benchmarks/resolver-gin-chi.md`) |
| E6 | **Robustness rules**: `/vN` major-version suffixes never become qualifiers (chi `/v5` → `chi`); `go.mod` parsing survives tabs and inline comments (F-10); exact `go.mod` filename match (F-38's `backup_go.mod`); dir→packages index (F-13's `non_test_package` O(N²) → map lookup); `package main` excluded from import-target fan-out; `Run` added to the universe-method list (F-08); dot-import ambiguity decided **after** test-visibility (F-36); Go `internal/` visibility enforced module-relatively with a dedicated `UnresolvedReason::Internal` (F-24-adjacent); candidate dampener becomes a **returned flag** instead of a shared `AtomicU64` (F-40, see C) | `cs-resolve/tests/foundation_regressions.rs` (3 regression tests) + `internal_visibility_follows_go_scope` unit tests; go-resolve golden regenerated and audited (`docs/reviews/golden_diff_audit.md` §3, every weight re-inverted to an exact integer n) |
| E7 | **Quadratics removed**: `container_for` binary-searches the def list; `collect_defs` uses a HashMap; `only_directive_lines` scans only the comment gap, O(gap) | 1 MiB single-file extraction: **11.6 s → ~270–340 ms**, linear in the sweep (D below) |
| E8 | **Harness and CLI contract**: `mcp --stdio` bare-flag form accepted (`SetTrue`); failures produce stable exit codes and **no artifact on stdout**; three proptest suite files at 256 cases each (scanner walk determinism/sort-completeness; extractor over arbitrary bytes and Go-shaped mixtures; resolver over random graphs) | `mcp_stdio_accepts_the_documented_bare_flag`; `doctor produces no artifact` contract test; `cargo test -p cs-extract` green after scratch-file removal |

### B. Findings refuted on evidence (recorded so they stay refuted)

- **F-21 (untyped nested composite literals leak field names).** The audit's
  premise — a `composite_literal` node with `type: None` causing an early
  `break` that leaks the key — does not survive contact with the grammar:
  tree-sitter-go never produces a typeless `composite_literal`. A typeless
  nested `{…}` is a plain `literal_value`, and the key-suppression walk-up
  crosses `literal_value` to the owning *typed* literal, so the key is
  suppressed. Probe: `Config{Database: {Host: Host{N: 2}}}` emits no
  `Database`/`Host` reference at all. No code change needed or made.
- **F-43 (qualified type conversions `types.ID(s)` silently dropped).**
  Measured, not read: `types.ID(s)` extracts as a `call_ref` carrying
  `qualifier: types`, and the *same selector used as a type* (`var x
  types.ID`) is a `type_ref` with the qualifier and **binds through the
  package-qualified TypeRef path** to the type definition — the conversion's
  type identity is not lost. The call occurrence itself stays honestly
  unbound as `no_candidate` when the name is type-only: a *labeled* outcome
  in the published histogram, which is what refutes the "silently" in the
  finding. Extending the call filter to types was considered and declined:
  calls bind functions/vars, and letting them bind types would re-open the
  audit's dominant false-positive class.
- **F-26 (non-object-safe `LanguageResolver` blocks dynamic dispatch).**
  Refuted as a non-requirement: language dispatch is one exhaustive
  `Language` match at a single call site — total, deterministic, and
  compiler-checked against new languages. No consumer ever holds mixed
  resolvers behind a `dyn` trait object; demanding object safety would force
  boxing and a narrower (`&self`-only) trait shape for zero buyers. Reopen
  only if a plugin API (ARCHITECTURE §12.5) actually needs heterogeneous
  dispatch.
- **F-40 (data race on the `AtomicU64` dampener under Rayon).** Refuted as
  vacuous and then made *impossible*: `cs-resolve` has no parallelism — none
  exists to race — and the shared atomic counter was deleted entirely in
  favor of a per-ref boolean returned to the caller, who aggregates into
  `names_skipped`. The race class is removed by removing the shared state,
  not by synchronizing it.

### C. Corrections to previously recorded claims (measurement over impression)

- **"~100 s at 10k files" — refuted.** It was an impression, never a run.
  Measured (synthetic GIN-density corpus, release, single-threaded,
  `docs/benchmarks/scaling_report.md`): prepare + resolve together measure
  ≈ 0.6 s at 10k (163 ms + 437 ms) on the post-fix resolver, with the
  per-import package lookups themselves sub-millisecond after E6's
  dir→packages index; the whole cold extract+prepare+resolve pipeline is
  **9.72 s at 10k**. The ARCHITECTURE §8
  budget (cold 10k < 60 s) has ~6× headroom single-threaded.
- **"2.5–4.2 GB RSS at 50k files" — confirmed in order.** Measured peak RSS
  at 50k: **3.30 GiB** — inside the claimed band, and 3.3× over
  ARCHITECTURE §8's "<1 GB @50k" target. The breach is structural (the
  materializing snapshot + resolver index + `ResolvedRepo` API), so the
  target is re-annotated in ARCHITECTURE §8 as **requiring the streaming
  design; streaming is mandatory in cs-index** (extract → write → drop,
  never hold the whole repo). Under the current design the 1 GB wall sits
  near 15k files.
- **BENCHMARK §4.2 power table — mathematically wrong, recomputed.** The old
  table showed 2–6% power at n = 50 with power *rising* in the discordant
  rate. Both properties are impossible for a level-α test against a true
  effect; the parameterization had treated a marginal 12-point effect as a
  conditional one. Recomputed exactly (script inline in BENCHMARK §4.2):
  power at n = 50 is ≈ 16–47% and, for a fixed marginal effect, *falls* as
  the discordant rate rises. The qualitative conclusion survives with
  corrected numbers: small-n extrinsic runs are underpowered; size for
  ≥80% (n ≈ 200 at p_d = 0.30).

### D. New discoveries from measurement, and the budget-scope decision

The scaling work also found a quadratic nobody had claimed: doc-adjacency's
`only_directive_lines` re-scanned from the declaration start, making
extraction O(defs²) on declaration-dense files. Fixed to O(gap) (E7): the
1 MiB cap-file worst case fell from 11.6 s to ~270–340 ms and the sweep is
now linear.

That fix forced an explicit decision about what `PARSE_TIMEOUT_MS` guards.
The 250 ms budget is enforced *inside the parse* (E2's progress callback);
post-parse fact collection is a separate phase it cannot see. Extending the
deadline over collection — the natural reading of "extraction timeout" —
would mislabel legitimate max-cap files as `Timeout` and **destroy their
facts**: measured worst-case collection at the 1 MiB cap is ~0.34 s, linear
in a capped input, and a real 8.9k-def machine-generated file is exactly the
kind of file whose symbols are worth keeping. **Decision: the budget guards
the parse only; collection stays linear under the 1 MiB input cap with a
measured ~0.34 s worst case.** Revisit in cs-index if a stricter per-file
bound is ever needed there; do not re-litigate here without new evidence.

### E. The `Ref.qualifier` schema decision

One field: `qualifier: Option<String>` — the selector operand's identifier
text when the operand is a plain identifier (`auth` of `auth.Session`), else
`None`. `None` deliberately covers both "no selector" (`helper()`) and
"computed operand" (`w.Header().Add`): for selector refs, **qualifier
presence *is* the identifier/expression distinction**, and the ref's kind
plus the resolver rule table already distinguish the remaining cases — a
second `selector_base` field would encode information the binding rules
never read. The operand occurrence itself is suppressed: receivers and
qualifiers are scope structure, not name uses. Full semantics, rule-table
changes, and golden impact: [ADR-020](ADR-020-ref-qualifier-addendum.md).

### F. internal/ visibility

Go's `internal/` rule is enforced module-relatively: an import path with an
`internal` element is importable only from within the tree rooted at that
element's parent (module-root `internal` is module-wide). A violating import
resolves as `Unresolved { reason: Internal }` — a new, documented
`UnresolvedReason`, because code that could not compile is a *different*
degradation than a wrong path (`NotFound`), and `doctor`'s histogram should
distinguish them.

### G. ADR-011 reconciliation: globset 0.4.18 → 0.4.20

ADR-011's own reconciliation rule applies: **the manifest is the single
source of dependency versions**; the lockfile is not a second source of
truth, and ADR-011's verified inventory is a point-in-time record, not a
normative pin. During the recovery pass the lockfile drifted to globset
0.4.20 (within the manifest's `0.4.18` caret range) as a side effect of the
property-test wiring (proptest 1.11 plus its dependency tree — `fnv`,
`getrandom 0.3`, `ppv-lite86`, `rand_*`, `quick-error 1.2.3` and the rest of
the dev-only graph — now in the lock exactly as ADR-011's dev-only note
contemplates: none of it reaches a released binary). Reconciled the way
ADR-011 prescribes, by aligning the manifest: `globset = "0.4.20"`, a
one-line reviewable diff, lockfile unchanged and consistent. ADR-011's
verified-inventory table still naming 0.4.18 is the expected artifact of an
append-only record; this paragraph is the pointer that supersedes it.

### H. Golden regeneration is a reviewed artifact, not a chore

Every behavior fix above that touches extraction or binding required
regenerating 18 golden files. The diff was audited line-by-line by a
dedicated reviewer against the declared expected-diff list, with
machine-checked ref-set deltas, a universe-leak scan, and qualifier
1:1 mapping checks: **PASS**, zero unexplained hunks —
`docs/reviews/golden_diff_audit.md`. That review is part of the fix, not
bookkeeping after it: goldens are the contract's enforcement (ADR-017),
and an unaudited regeneration would silently launder behavior changes.

## Consequences

- The audit's finding list is fully dispositioned: accepted (E1–E8), refuted
  with evidence (B), or corrected by measurement (C). The 2% power figure
  and the "~100 s" claim are dead; citing either after this ADR is an error.
- ARCHITECTURE §8's RSS row now states a design requirement (streaming) with
  a measured breach behind it, so cs-index cannot quietly inherit the
  materializing pipeline.
- LANGUAGES §6.1, ARCHITECTURE §4.1/§4.2/§5, SECURITY §3/§6/§11 and
  MASTER_PLAN §17 are updated in the same change set; the qualifier contract
  details live in ADR-020.
- The `tmp`-harness measurement discipline (synthetic corpus, committed
  generator, per-stage RSS) is the precedent for the cs-index scaling gate.

## What would reopen this

- A resolver or renderer requirement that the single-field `qualifier`
  cannot express (ADR-020's reopen list governs).
- Real-corpus evidence that a per-file bound must cover post-parse
  collection (the D decision).
- A plugin API needing `dyn LanguageResolver` (reopens the F-26 refutation).
- Any reintroduction of shared mutable counters on the resolve path (the
  returned-flag contract in E6 is the determinism argument).
- A globset release with behavioral changes to ignore semantics — re-verify
  per ADR-011's grammar-pinning discipline.
