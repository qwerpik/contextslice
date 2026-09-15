# ADR-018: Go resolver — filesystem-only, package identity, honest approximation

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-15 |
| **Milestone** | Go resolver (MASTER_PLAN §15 step 4) |
| **Follows** | [ADR-003](ADR-003-approximate-reference-graph.md), [ADR-017](ADR-017-extraction-contract-as-built.md) |

## Context

Step 4 turns extracted facts into file-level edges. The design was
challenged against a census of two real repositories (gin: 99 files / 518
imports; chi: 84 files / 357 imports) before implementation, and the
implementation was audited against them afterward
([report](../benchmarks/resolver-gin-chi.md)). Three census findings drove
the decisions: package qualifiers come from package clauses, not path tails
(chi `/v5` → `package chi`; 13 dir/name mismatches); nested `go.mod` modules
create in-repo-*looking* cross-module imports (chi `_examples`); and
build-tag duplicate symbols are common (gin `codec/json`: identical symbol
set in 4 files) while method-name ambiguity is severe (`Render` on 18 types
in one package).

## Decisions

1. **Filesystem only — no `go list`/gopls.** Toolchains are not guaranteed
   on user machines and full accuracy needs module downloads (network).
   In-repo edges — what selection needs — are derivable from `go.mod` plus
   the file list. `replace`/`require` directives are ignored (documented:
   imports relying on replaces resolve as External). Precision sidecar
   stays a `--deep=go` idea for later.
2. **Package identity = `(dir, package_name)`.** Separates `foo_test` from
   `foo` in one directory, groups build-tag variants, isolates broken
   name conflicts. Files without a package clause become singleton
   pseudo-packages.
3. **Nearest-`go.mod` module mapping.** A file belongs to the deepest
   module rooting at an ancestor; an import landing in a *different*
   module's subtree resolves `External` — the chi `_examples` trap. No
   `go.mod` at all → synthetic root module with an empty path (relative
   imports and same-dir binding still work).
4. **Qualifier scope per file:** alias, else the resolved target's package
   clause name; externals get pseudo-qualifiers (unresolvable markers).
   Dot imports contribute exported defs to the unqualified scope;
   own-package-vs-dot conflicts are `ambiguous_dot_import` (zero real-world
   occurrences in the census). Blank imports contribute nothing.
5. **Binding rules:** unqualified names/types bind all same-package defs
   (tag variants); package-qualified selectors bind exported defs of the
   target, kind-appropriate; **bare method calls bind only when the name is
   unique in the package** and not on Go's universal interface surface;
   bare field accesses and method values are never bound
   (`needs_type_info`).
6. **Go build semantics for test files:** internal test files share the
   package identity (their refs same-package-bind) but are invisible to
   importers — no import edge, no importer binding, and no binding from a
   non-test source into a test file. The gin/chi audit found production
   files binding test-helper methods before this rule; it eliminated them.
7. **The one extraction amendment: `call_ref`.** Selectors in call position
   (`x.Foo()`) are marked at extraction, where the tree is in hand; the
   binding rules for calls and field accesses differ fundamentally. A
   second extraction refinement came out of the audit: identifier keys of
   struct-shaped composite literals are field-name positions, not
   references (map-literal keys remain references). Both are contract
   changes recorded here as ADR-017 follow-ups; goldens were regenerated
   and re-reviewed.
8. **Universe-method filter.** Bare calls named `Close`, `ServeHTTP`,
   `Value`, `String`, `Write`, `Read`, `Error`, `Header`, `RoundTrip` and
   the rest of Go's pervasive interface surface never bind — the receiver
   is more likely an external type implementing a stdlib interface than
   the one local definition. `Get`, `Set`, `Name`, `Next`, `Find` are
   deliberately NOT filtered: the audit measured their bare-call bindings
   as mostly correct (`r.Get`→`Mux.Get`). Own reason:
   `UnboundReason::UniverseMethod`.

## Measured outcome (the point of this ADR)

Full numbers in `docs/benchmarks/resolver-gin-chi.md`. Gates: in-repo
import resolution 100%/100% (≥95% required), sampled precision 97.4% gin /
96.9% chi, combined ≈97.2% (Wilson 95% CI LB ≈94.7%, reported not gated),
resolve ≤11 ms per repo (budget <1 s), byte-identical determinism. Residual
FP classes — local shadowing (~2%) and cross-package interface method names
(~1%) — are documented and damped, not hidden.

## What stays out of scope

Implements-edges (needs method sets), split-weight multi-target bindings,
method-value binding, cross-module resolution, `vendor` beyond the path
check, and any form of type inference. All deferred until the selection
benchmark (Phase 3) proves a need.

## What would reopen this

- The selection stage finding that ref_def edges are too noisy/sparse at
  file level (revisit weights, not the model, first).
- A `--deep=go` mode built on SCIP/gopls for opt-in precision.
- TS/Python adapters revealing a need for binding rules this trait shape
  cannot express.
