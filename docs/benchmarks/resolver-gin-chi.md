# Go resolver — real-repository audit (gin, chi), post-recovery

ADR-018 §7 requires measured evidence before the resolver becomes the
foundation for indexing. This report records the re-audit run: 2026-09-16,
resolver as of this milestone (structural `Ref.qualifier`, `Run` in the
universe-method filter, universe builtins excluded from `no_candidate`,
`NoScope` for chained selectors), release build, single-threaded.

This **supersedes the 2026-09-15 report** (its headline claims — in-repo
imports 100%, sampled precision 97.2% / Wilson LB 94.7% — are **PRE-RECOVERY**
and are no longer asserted). That first pass was manual and the adversarial
audit showed it was incomplete in three ways: `t.Run` bound to gin's
`Engine.Run` (`Run` was missing from the universe-method filter), Go's
universe builtins leaked into the `no_candidate` bucket (inflating it,
PRE-RECOVERY, to 14,492 gin / 7,941 chi), and chained selectors like
`w.Header().Add` bound to local same-named methods. All numbers below were
re-derived from scratch at pinned commits; they are **not comparable to the
2026-09-15 numbers**, because extraction changed too (the qualifier operand is
no longer emitted as its own reference — it rides structurally on the
reference, ADR-017 addendum), so the reference population itself is different.

Method: the full snapshot (extract + prepare + resolve) runs over each
repository at a pinned commit; every import resolution is classified; a
deterministic audit sample (bound references ordered by `(file, byte offset)`,
every stride-th taken: stride 79 of 5,971 for gin, 27 of 2,051 for chi —
76 per repo, 152 total) was verified against the source by hand, entry by
entry. Canonical harness (repo test):

`CS_RESOLVE_REPO=<repo> CS_RESOLVE_DUMP_AUDIT=1 cargo test --release -p
cs-resolve --test real_repo -- --ignored --nocapture`

The determinism and module-boundary tallies below came from a scratch harness
(this run: `/tmp/gin-audit/harness`) that mirrors
`crates/cs-resolve/tests/common/mod.rs` (same snapshot construction, same
resolver calls) plus a second `resolve` over a reversed input order and
module-of-file bookkeeping; it is not part of the repo. The repo test itself
was also executed against both pins and passed with identical tallies.

## Pins and reproduce

```text
git clone https://github.com/gin-gonic/gin /tmp/gin-audit/gin
git clone https://github.com/go-chi/chi /tmp/gin-audit/chi
git -C /tmp/gin-audit/gin rev-parse HEAD   # 5c6a15f8f9566612076bd209e623861bf92a6283
git -C /tmp/gin-audit/chi rev-parse HEAD   # b1c9ab47626cc46b34393ad4d35779c4363c4e1e

CS_RESOLVE_REPO=/tmp/gin-audit/gin CS_RESOLVE_DUMP_AUDIT=1 \
  cargo test --release -p cs-resolve --test real_repo -- --ignored --nocapture
CS_RESOLVE_REPO=/tmp/gin-audit/chi CS_RESOLVE_DUMP_AUDIT=1 \
  cargo test --release -p cs-resolve --test real_repo -- --ignored --nocapture
```

Both SHAs are the clone HEADs on 2026-09-16 (gin `master`, chi `master`) and
are the pin: upstream moves, the numbers are only guaranteed for those
commits. File counts: gin 99 `.go` files / 1 `go.mod`; chi 84 `.go` files /
3 `go.mod` (main module plus `_examples/rest` and `_examples/versions`, which
are independent modules).

## Headline numbers

| Metric | gin (99 files) | chi (84 files) | Gate | Verdict |
|---|---|---|---|---|
| In-repo import resolution | **31/31 = 100%** (0 unresolved) | **44/44 = 100%** (0 unresolved) | ≥95% | pass |
| Sampled binding precision (manual) | 76/76 = **100%** | 76/76 = **100%** | ≥95% point | pass |
| Combined precision | 152/152 = **100%** (Wilson 95% CI LB ≈ **97.5%**) | | | reported, not gated |
| Ref binding coverage | 5,971/25,633 = **23.3%** | 2,051/11,151 = **18.4%** | reported, not gated | — |
| Resolve wall time | 0.013–0.014 s | 0.013–0.014 s | <1 s | pass |
| Determinism | byte-identical serialized output on re-run with reversed input order (both repos) | | required | pass |
| Cross-`_test` same-package binds | 0 ref_def edges from non-test files into `_test` files (measured, both repos; also 0 at ref level) | | required | pass |
| Binds into nested modules | 0 cross-module edges of any kind; chi's 11 own-path-looking imports inside `_examples/rest`/`_examples/versions` resolve external, never into the parent module | | required | pass |

Census cross-check (re-derived from raw source by an independent import-block
parser, comments stripped): gin has exactly 31 own-module import specs, chi 44
in its main module — the resolver resolved exactly those, no more, no fewer.
One additional own-path import line exists inside a gin doc-comment example
(`doc.go:10`, inside the `/* … */` package doc) and is correctly ignored (a
raw-text census sees 520 spec-shaped lines, extraction 519). Totals: gin 519
import specs (31 in-repo / 488 external), chi 357 (44 / 313); dot imports 0,
blank imports 0 in both, so `ambiguous_dot_import` never fires.

## Unbound-reference accounting (the honesty table)

Refs that stay unbound are documented behaviors, not failures. gin / chi:

| Reason | gin | chi | What it is |
|---|---|---|---|
| `no_candidate` | 8,900 | 4,615 | locals (invisible by design) and other names with no same-package or dot-import candidate |
| `external_scope` | 6,090 | 2,463 | selectors through stdlib/third-party packages — nothing to bind to |
| `needs_type_info` | 2,753 | 1,160 | bare field accesses and method values (receiver type required) |
| `no_scope` | 899 | 260 | calls through computed operands (`w.Header().Add`, `arr[0].Close`) — receiver package unknowable; new since qualifiers became structural |
| `universe_method` | 621 | 550 | bare instance calls named on Go's universal interface surface (`Close`, `ServeHTTP`, `Run`, …) |
| `method_ambiguous` | 399 | 52 | method name defined on several types in the package |
| `ambiguous_dot_import` | 0 | 0 | no dot imports exist in either repo (census above) |

The universe-name exclusion is a name filter over bare refs; this run verified
it directly: 0 refs named after Go's predeclared identifiers in gin, 3 in chi —
all three `tt.recover` field accesses in `middleware/compress_test.go:188–191`,
where `recover` is a test-table struct *field*, not the builtin; they land in
`needs_type_info`, the correct bucket for a field access regardless, and none
land in `no_candidate`.

`names_skipped` = 0 in both repos (no name exceeded the 256-candidate cap).
Package-qualifier occurrences riding on references (not counted as refs since
the structural-qualifier change): gin 6,327, chi 2,576. Edges: gin 1,890
(230 import_out / 330 ref_def / 1,330 test_affinity); chi 2,078 (395 / 133 /
1,550).

**Comparability warning:** the PRE-RECOVERY table (gin `no_candidate` 14,492 /
chi 7,941, `universe_method` 803 / 566, total refs 30,924 / 14,272) was
produced by a different extraction (qualifier operands were separate refs) and
a different binding rule set, and was **not re-derived in this run** — the old
clones and the old extraction no longer exist. The drops in `no_candidate`
reflect both the universe-builtin fix and the qualifier change; they cannot be
decomposed post hoc, so no attempt is made to.

## What changed in recovery (each with its measured effect)

1. **`Run` added to the universe-method filter (the `t.Run` fix).** Measured
   this run: all qualified instance calls named `Run` are unbound — 36 in gin,
   55 in chi, every one with reason `universe_method`; zero bind. Before the
   fix they bound bare to gin's `Engine.Run` — the dominant FP the adversarial
   audit flagged. Documented cost: a genuine local `Run` method value would
   now be a false negative.
2. **Universe builtins no longer leak into `no_candidate`.** The bucket is now
   8,900 (gin) / 4,615 (chi) against the PRE-RECOVERY 14,492 / 7,941. The
   direct check this run: refs with predeclared names, 0 gin / 3 chi, none of
   them in `no_candidate` (see the footnote above). The remaining delta is the
   qualifier extraction change, not the fix; the two are not separable from
   these runs.
3. **Structural qualifiers (`Ref.qualifier`).** The qualifier operand of
   `pkg.Name` / `recv.Name` is scope structure, not a name use, so it is no
   longer emitted as a reference. Total refs: gin 25,633 (PRE-RECOVERY bound +
   unbound 30,924), chi 11,151 (PRE-RECOVERY 14,272). Qualifier occurrences
   are counted separately: 6,327 / 2,576.
4. **`NoScope` for chained selectors.** Calls whose operand is a computed
   expression (`w.Header().Add`) can no longer enter bare-call binding and
   produce false edges into same-named local methods; they are now a counted
   bucket: 899 (gin) / 260 (chi).

## False-positive classes (from the manual sample)

The 152 sampled bindings contained **zero false positives**. Cases that
required actual verification before being accepted as correct, with what
settled them:

- gin `binding`: `Validator.ValidateStruct(obj)` →
  `defaultValidator.ValidateStruct` — correct because the package var is
  initialized `var Validator StructValidator = &defaultValidator{}`
  (`binding/binding.go:72`, checked).
- chi `tree`: bare `mGET`/`mPOST`/`mQUERY`/`mDELETE`/`mALL` names — correct;
  they are identifiers in a `const`/`var` block in the same package (checked;
  a declaration-line heuristic alone misses them).
- Multi-target bindings — correct by construction: build-tag variant pairs
  (gin `binding.go`/`binding_nomsgpack.go`) and name collisions where the
  true def is among the targets (`URLParam` → package function **and**
  `Context.URLParam` method; the bare call's callee is included).
- Promoted/instance methods on interface or embedded receivers
  (`router.GET` → `RouterGroup.GET`, `Engine` embeds `RouterGroup`; chi
  `f.BytesWritten` → `basicWriter.BytesWritten`, `f` is an `*httpFancyWriter`
  embedding `basicWriter` — checked) — counted correct: the bound def is the
  method actually executed for the receiver's plausible type.

The two residual FP classes the 2026-09-15 audit documented (local shadowing
of package-level names; cross-package interface method names) did **not**
appear in this sample. With n = 152 and 0 observed, the Wilson 95% lower
bound is 97.5% — i.e. the data still admits a true FP rate of up to ~2.5%;
it does not prove the classes are gone.

## What this does not claim

- This is **not** semantic resolution. Receiver types, method sets, promoted
  methods across packages, and local scoping are out of scope by design
  (ADR-003/018); every consequence is a counted, labeled bucket above.
- Precision is a **sampled point estimate on 152 hand-checked bindings**,
  judged by a single reviewer as "at least one target is the true
  definition"; the Wilson 95% CI lower bound (97.5%) is reported rather than
  hidden. Only bound refs are sampled — unbinding mistakes (false negatives
  like the deliberate universe-method FNs) are covered by the reason table,
  not by the precision number.
- Ref-level numbers are **not comparable to the 2026-09-15 report**;
  extraction changed underneath them. Every PRE-RECOVERY figure quoted here
  is historical context, not a re-derived measurement.
- Two repositories, both router/middleware-shaped; other shapes (generated
  heavy, interface-heavy) may shift the reason mix.
- The determinism and nested-module rows were produced by a scratch harness
  (same resolver, same snapshot construction as the repo test) that is not
  checked into the repository; the repo-test commands above were run against
  both pins and passed with identical tallies, so everything else reproduces
  verbatim.

## Reproduce

See "Pins and reproduce" above. The pinned SHAs are the honest pin: the
upstream repositories move, and the numbers are only guaranteed for those
commits.
