# Go resolver — first real-repository audit (gin, chi)

ADR-018 §7 requires measured evidence before the resolver becomes the
foundation for indexing. This report records the first run: 2026-09-15,
resolver as of this milestone, release build, single-threaded.

Method: the full snapshot (extract + prepare + resolve) runs over each
repository; every import resolution is classified; a deterministic audit
sample (every stride-th bound reference, n≥150 per repo) was verified
against the source by hand. Harness:
`CS_RESOLVE_REPO=<repo> CS_RESOLVE_DUMP_AUDIT=1 cargo test --release -p
cs-resolve --test real_repo -- --ignored --nocapture`.

## Headline numbers

| Metric | gin (99 files) | chi (84 files) | Gate | Verdict |
|---|---|---|---|---|
| In-repo import resolution | **31/31 = 100%** (0 unresolved) | **27/27 = 100%** (0 unresolved) | ≥95% | pass |
| Sampled binding precision (manual) | 149/153 = **97.4%** | ~157/162 = **96.9%** | ≥95% point | pass |
| Combined precision | 306/315 ≈ **97.2%** (Wilson 95% CI LB ≈ 94.7%) | | | reported, not gated |
| Resolve wall time | 0.010 s | 0.011 s | <1 s | pass |
| Determinism | byte-identical edges/stats on re-run and shuffled input order | | required | pass |
| Cross-`_test` same-package binds | 0 (fixture-pinned) | 0 | required | pass |
| Binds into nested modules | 0 (fixture-pinned; chi `_examples` trap exercised) | | required | pass |

Census cross-check: the earlier import census counted 31 own-module import
lines in gin and 27 in chi's main module — the resolver found and resolved
exactly those, no more, no fewer.

## Unbound-reference accounting (the honesty table)

Refs that stay unbound are documented behaviors, not failures. gin / chi:

| Reason | gin | chi | What it is |
|---|---|---|---|
| `no_candidate` | 14,492 | 7,941 | locals (invisible by design), external-package unqualified names, generics plumbing |
| `external_scope` | 6,047 | 2,448 | selectors through stdlib/third-party packages — nothing to bind to |
| `needs_type_info` | 2,736 | 1,160 | bare field accesses and method values (receiver type required) |
| `universe_method` | 803 | 566 | bare calls named on Go's universal interface surface (`Close`, `ServeHTTP`, …) |
| `method_ambiguous` | 588 | 59 | method name defined on several types in the package (gin `render`: `Render` ×18) |
| `ambiguous_dot_import` | 0 | 0 | no dot imports exist in either repo (census: 0/875 import lines) |

Bound refs: gin 6,258 (plus 6,295 package-qualifier occurrences), chi 2,098
(plus 2,566). Edges: gin 1,900 (230 import_out / 340 ref_def / 1,330
test_affinity); chi 2,081 (395 / 136 / 1,550).

## False-positive classes (measured, from the manual samples)

The audit found three FP classes; two were fixed during the milestone and
one remains, quantified:

1. **Composite-literal keys** (largest class in the first gin pass: 7/153)
   — `RouteInfo{Handler: x}` keys bound to same-named defs. **Fixed** in
   extraction: identifier keys of struct-shaped literals are field-name
   positions (map-literal keys remain refs).
2. **Bare calls on external receivers with stdlib method names** (14/312 at
   the second pass: `t.Run`, `ts.Close`, `next.ServeHTTP`, `ctx.Value`) —
   **fixed** by the universe-method filter: those names never bind bare
   calls. Cost: documented FNs (`n.endpoints.Value()` on a local type) that
   import/affinity edges already cover at file level. Contested names
   (`Get`, `Set`, `Name`, `Next`, `Find`) are deliberately NOT filtered —
   the audit measured their bare-call bindings as mostly correct
   (`r.Get`→`Mux.Get`, `c.Set`→`Context.Set`).
3. **Local shadowing of package-level names** (remaining: ~7/315) — a local
   `route` or `routes` shadows a same-package def; unfixable without scope
   analysis, dampened by ref-count weighting, documented.
4. **Cross-package interface method names** (remaining: ~3/315) — `r.Route`
   in chi's middleware tests binds the middleware package's unique `Route`
   instead of chi.Router's; the receiver's package is unknown. Documented.

Residual FP rate ≈ 3%: classes 3 and 4, both bounded and both pointing at
same-package files that import/affinity edges usually connect anyway.

## What this does not claim

- This is **not** semantic resolution. Receiver types, method sets, promoted
  methods across packages, and local scoping are out of scope by design
  (ADR-003/018); every consequence is a counted, labeled bucket above.
- Precision is a **sampled point estimate** on 315 hand-checked bindings;
  the Wilson 95% CI lower bound (~94.7%) is reported rather than hidden.
- Two repositories, both router/middleware-shaped; other shapes (generated
  heavy, interface-heavy) may shift the reason mix.

## Reproduce

```text
git clone --depth 1 https://github.com/gin-gonic/gin /tmp/gin
git clone --depth 1 https://github.com/go-chi/chi /tmp/go-chi
CS_RESOLVE_REPO=/tmp/gin CS_RESOLVE_DUMP_AUDIT=1 \
  cargo test --release -p cs-resolve --test real_repo -- --ignored --nocapture
```
