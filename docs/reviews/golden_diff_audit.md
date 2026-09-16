# Golden Diff Audit — Foundation Recovery regeneration

- **Repo:** /home/user/contextslice, HEAD `3d178c0` (Foundation Recovery changes uncommitted in working tree)
- **Auditor:** golden-auditor (mandatory line-by-line review of every golden diff)
- **Date:** 2026-09-16
- **Scope:** `git diff -- fixtures/go/golden/` (16 files) and `git diff -- fixtures/go-resolve-golden/` (2 files), each hunk cross-checked against the fixture source it was generated from.
- **Method:** for every golden file, the old (`git show HEAD:…`) and new ref sets were diffed programmatically (kind+name+span keys), every hunk read manually, and every surviving/vanished ref checked against the `.go` fixture source. Additionally: a universe-name leak scan over all 20 new goldens, a qualifier-completeness scan (every ref must carry the `"qualifier"` key), and a byte-offset spot check of `:=` handling.

## Verdict

**PASS.** All 18 changed golden files are explained by the declared expected-diff list. Zero refs appeared or vanished that are not covered by items 1–5 of the expected list. No builtin (universe-name) refs remain in any golden. Every ref carries the new `"qualifier"` field. Both test suites pass.

---

## 1. Per-file verdict table (fixtures/go/golden/)

| Golden (fixture) | Changes seen | Verdict |
|---|---|---|
| 01_simple.go.json | `+qualifier` on 6 refs; `name_ref:errors` (L19) removed, `call_ref:New` gains `"qualifier": "errors"` | EXPECTED (item 2) |
| 02_methods.go.json | `+qualifier` on 7 refs; 3× `name_ref:s` (L12/17/21) removed; `ttl`/`UserID` field_refs gain `"qualifier": "s"` | EXPECTED (item 2) |
| 03_types.go.json | `+qualifier` on 5 refs; `name_ref:io` (L22) removed; `type_ref:Closer` gains `"qualifier": "io"` | EXPECTED (item 2) |
| 04_consts_vars.go.json | `ModeFast.doc`: `null` → `"ModeFast skips validation."`; `name_ref:time` (L19) removed; `field_ref:Second` gains `"qualifier": "time"` | EXPECTED (items 5, 2) |
| 05_grouped.go.json | `+qualifier: null` on 4 refs (localC/localV/localV2/localT); no ref added/removed | EXPECTED (serde addition) |
| 06_imports.go.json | `name_ref:f` (L12) and `name_ref:fmt` (L13) removed; `field_ref:Join` gains `"qualifier": "f"` (alias clause `f "strings"`), `field_ref:Sprint` gains `"qualifier": "fmt"`; `name_ref:Sqrt` (dot import) keeps `qualifier: null` | EXPECTED (item 2; alias qualifier correct) |
| 07_docs.go.json | `name_ref:sync` (L14) removed; `type_ref:Mutex` gains `"qualifier": "sync"` | EXPECTED (item 2) |
| 08_closures.go.json | 4× `name_ref:http` (L6 ×2, L18 ×2) removed → `type_ref:Request`/`ResponseWriter` gain `"qualifier": "http"`; `name_ref:r` (L7) removed → `field_ref:Body` gains `"qualifier": "r"`; `call_ref:Close` keeps `qualifier: null` (selector operand `r.Body` is not an identifier — correct per ADR-017 addendum) | EXPECTED (item 2) |
| 09_tests_test.go.json | 3× `name_ref:testing` (L5/14/26) removed → `type_ref:T`/`T`/`B` gain `"qualifier": "testing"`; 2× `name_ref:t` (L14/16) → `call_ref:Run`/`Fatal` gain `"qualifier": "t"`; 2× `name_ref:tc` (L14/15) → `field_ref:name`/`want` gain `"qualifier": "tc"`; `name_ref:b` (L27) → `call_ref:Loop` gains `"qualifier": "b"` | EXPECTED (item 2) |
| 10_buildtag_linux.go.json | `name_ref:syscall` (L8) removed; `field_ref:EINVAL` gains `"qualifier": "syscall"` | EXPECTED (item 2) |
| 10_buildtag_windows.go.json | same as linux variant | EXPECTED (item 2) |
| 11_unusual.go.json | `+qualifier: null` on 10 refs; no ref added/removed | EXPECTED (serde addition) |
| 12_generics.go.json | `name_ref:make` (L15), `name_ref:len` (L15), `name_ref:append` (L17) removed (universe filter); `name_ref:p` (L23) removed → `field_ref:Left` gains `"qualifier": "p"`; `+qualifier: null` on 26 refs | EXPECTED (items 1, 2) |
| 25_garbage.go.json | `name_ref:print` (L2) removed; `refs` now `[]` | EXPECTED (item 1, explicit) |
| 20_broken_brace / 21_broken_mid / 22_truncated / 23_empty / 24_package_only / 26_crlf | no diff | N/A (unchanged) |

**Ref-set delta summary (machine-checked):** across all 20 goldens, 0 refs added, 31 refs removed. Every removed ref is either (a) a package/instance qualifier now recorded in the `"qualifier"` field of the qualified ref (27 refs: errors, s×3, io, time, f, fmt, sync, http×4, r, testing×3, t×2, tc×2, b, syscall×2, p), or (b) a Go predeclared identifier now correctly filtered by the fixed universe list (4 refs: make, len, append, print). The only defs-section change in the entire set is `ModeFast.doc` (item 5); `package_name`, `imports`, and `status` are byte-identical everywhere.

## 2. Adversarial checks (all clean)

- **Universe leak scan:** every ref name in all 20 new goldens checked against the full Go predeclared-identifier set (append…any, comparable, clear, min, max, …). Zero leaks. `float64` (03_types L26/L29), `~int`/`~float64` (12_generics L5), `string`/`int`/`bool`/`error`/`nil`/`iota`/`any` correctly absent. No fixture uses `f32`/`f64` (grep), so their removal from the list is unobservable here.
- **Qualifier 1:1 mapping:** every vanished qualifier name_ref has exactly one `"qualifier"` landing spot at the same file and the qualified ref's line/span matches the selector expression in the fixture source (verified hunk-by-hunk, e.g. 09_tests_test.go L14 `t.Run(tc.name, func(t *testing.T) {` produces Run←t, name←tc, T←testing).
- **Item 3 (`:=` left-side fix) — no observable diff, verified correct:** 08_closures.go contains no `:=`. 11_unusual.go L5 `x := 1; y := x + 1; return y`: new golden refs are exactly `x`@bytes 93–94 (RHS operand) and `y`@bytes 107–108 (`return y`); the LHS declarations (bytes 80, 88) are correctly not refs, matching the unchanged spans in the old golden. 09 L13 `for _, tc := range cases`: RHS `cases` kept as ref, LHS `tc` is a definition. No golden gained or lost a `:=`-related ref.
- **Item 4 (type alias/named-type targets) — no observable diff, consistent:** only alias/named-type targets in the fixture set are `float64` (03 L26), `[2]float64` (03 L29, non-identifier), `string` (05 L9) — all universe-filtered or non-identifiers, so no new type_ref is expected or present.
- **Item 6 (generic instantiation → call_ref) — no observable diff:** no fixture in either golden set contains an explicit instantiation call (`lib.Factory[int]()`); 12_generics.go and gen/gen.go have none. Nothing to verify in goldens; the behavior is presumably covered by unit tests in cs-extract.
- **Item 7:** fixtures/go/06_imports.go is unmodified (not in `git status`); it still imports via plain quoted spec + alias + dot + blank forms, no backtick raw-string import. Confirmed.
- **Non-ref stability:** `defs`, `imports`, `package_name`, `status` compared old-vs-new in all 20 files — only 04's `ModeFast.doc` differs.

## 3. go-resolve golden audit

Fixture source changes under review: `go.mod` (comment only), `main.go` (+`github.com/x/inner/v2` import with `inner.Thing()`, +alias import `util "example.com/m/v2/mixed"` with `util.Mixed()`), `meth/types.go` (+types `D`, methods `D.Solo`, `D.Run`, func `Make`), `meth/use.go` (+`Make().Solo()`, `d := D{}`, `d.Solo()`, `t.Run`), new dirs `mixed/` (main.go pkg main + util.go pkg util), `deep/tools/` (+`internal/priv/`), `outside/o.go`.

### edges.json (+4 edges, 1 weight change, 27 → 31 edges)

| Edge | Kind | Weight | Check |
|---|---|---|---|
| deep/tools/tool.go → deep/tools/internal/priv/priv.go | import_out | 0.6 | EXPECTED (§6 constant) |
| deep/tools/tool.go → …/priv.go | ref_def | 1/6 = 0.16666666… | inverts to n=1 (`priv.Grant`) under `0.5·√n/√(n+8)` |
| main.go → mixed/util.go | import_out | 0.6 | EXPECTED |
| main.go → mixed/util.go | ref_def | 1/6 | inverts to n=1 (`util.Mixed`) |
| meth/use.go → meth/types.go | ref_def | 0.31008683647302115 → 0.3535533905932738 | n=5 → n=8: old bound set {A,B,C,S,F} + new {D, Solo (`d.Solo`), Make (`Make()` identifier operand)} = 8. Exact. |

- **All weights re-inverted programmatically:** every ref_def weight in the file solves to an exact integer n (1, 3, or 8); import_out is uniformly 0.6; test_affinity uniformly 0.7. Matches ALGORITHM §6 (docs/ALGORITHM.md L115–118).
- **Self-edges:** none (machine-checked, 31 edges).
- **New fixture dirs coherent:** `mixed/` contributes exactly main.go→mixed/util.go (the pkg-main sibling is correctly not a target — non_test_package prefers `util`); `deep/tools/` contributes the priv pair; `outside/o.go` contributes **no** edge, correct because its `internal/priv` import is Unresolved(internal) ("nothing may bind through it" per fixture comment).

### stats.json

- resolved 8→10 (+priv.go, +mixed/util.go — exactly the two new resolvable targets); external 7→8 (+`github.com/x/inner/v2`); refs_bound 27→35, refs_unbound 44→39 (total refs 71→74, consistent with the added fixture code plus extractor ref suppression).
- `package_qualifier_refs` 14→18: stat meaning changed as declared — it now counts refs whose recorded qualifier names an import scope. Delta is consistent with the new import-scoped qualified refs added by the fixture changes (inner.Thing, util.Mixed, priv.Grant ×2 sites). No stale-format residue.
- unbound_reasons deltas each trace to a specific fixture edit: no_candidate 30→21 (bogus bare qualifier/receiver refs no longer emitted), external_scope 7→9 (inner.Thing, util.Mixed), no_scope 1 new (Make().Solo() computed operand — new reason kind, matches use.go L16 comment), universe_method 1→2 (`t.Run` on unknown receiver, use.go L19), method_ambiguous 2 / ambiguous_dot_import 1 / needs_type_info 3 unchanged.

## 4. Test results (read-only verification)

- `cargo test -p cs-extract --test golden`: **5 passed, 0 failed** (no_orphan_goldens, build_tagged_files_extract_independently, degraded_files_have_defined_behavior, go_goldens_match_exactly, extraction_is_deterministic_across_runs_and_orderings).
- `cargo test -p cs-resolve`: **38 passed, 0 failed, 1 ignored** — unittests 11 passed; edges_golden 2 passed; fixture_matrix 25 passed; real_repo 1 ignored (requires `CS_RESOLVE_REPO`, pre-existing).

## 5. Unexpected items

**None.** Every hunk in every changed golden file is classified EXPECTED against the declared list (items 1–5 observable; items 3, 4, 6, 7 have no observable golden effect, each individually verified why). No flag-worthy discrepancies found.
