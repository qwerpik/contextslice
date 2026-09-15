# ContextSlice — Language Support Strategy

| | |
|---|---|
| **Status** | Pre-implementation specification |
| **Version** | 1.0 — 2026-09-14 |
| **Depends on** | [ARCHITECTURE.md](ARCHITECTURE.md) (cs-extract / cs-resolve) · [ALGORITHM.md](ALGORITHM.md) |

---

## 1. Criteria and tiering

Languages are ranked on five axes, scored 1–5: **ecosystem size** (repos in the wild),
**coding-agent usage** (how often agent users work in it), **parser quality**
(tree-sitter grammar maturity), **resolution tractability** (how well static
import/symbol resolution works without a compiler), **adapter effort** (queries +
resolver work, lower is better). Tier 1 = MVP (ship by v0.1); Tier 2 = next; Deferred =
explicitly later or never.

| Language | Ecosystem | Agent usage | Grammar | Resolution tractability | Effort | Verdict |
|---|---|---|---|---|---|---|
| Go | 4 | 5 | 5 | **5** (module paths are filesystem-shaped; `go/packages` exists as a sidecar) | low | **Tier 1** |
| TypeScript/JS | 5 | 5 | 5 | 3 (aliases, re-export chains — hard parts known) | medium | **Tier 1** |
| Python | 5 | 5 | 5 | 3 (imports filesystem-shaped; dynamic cases) | medium | **Tier 1** |
| Rust | 4 | 4 | 5 | 4 (module paths from `mod` decls; rust-analyzer emits SCIP for deep mode) | medium | Tier 2 |
| Java | 4 | 3 | 5 | 3 (package→path convention; build-system variance) | medium | Tier 2 |
| C# | 4 | 3 | 4 | 3 | medium | Tier 2 |
| C/C++ | 4 | 3 | 3 | **1** (include paths/build config required) | high | Deferred |
| Ruby/PHP/others | 2–3 | 2 | 4 | 3 | medium | Deferred |

Why not one language for v0.1? Because the architecture's entire claim is that the
engine is language-agnostic with thin adapters — three languages at shipping quality
proves the seam; one proves nothing; six proves nothing either (quality collapses).

## 2. Tier 1 rationales

### 2.1 Go — the reference adapter (first)

Module-relative imports are filesystem-shaped (`example.com/mod/internal/auth` →
`internal/auth/`), the grammar is mature and stable, generics are well-covered, and
tooling (`go list -json`) offers an optional precision sidecar. Go repos are heavily
represented among coding-agent users. Everything (queries, resolver, goldens, the first
milestone — MASTER_PLAN §14) is proven on Go before breadth begins.

### 2.2 TypeScript / JavaScript — the reach adapter

The largest concentration of agent-assisted development. Extraction is easy; resolution
is the known hard part (§6.2), which is exactly why it must ship in v0.1 — a context
engine that dodges TS is a toy for this audience. JS is carried along (same grammar
family, weaker type signal, same resolver with `jsconfig`/`package.json` handling).

### 2.3 Python — the ubiquity adapter

Huge agent usage; import syntax is filesystem-shaped so static resolution covers most
real code; the spec to copy is pyright's documented static import resolution. Bonus:
Python unlocks SWE-bench-family extrinsic tasks (BENCHMARK §4.2).

## 3. Tier 2 (post-v0.1, in order)

1. **Rust** — strong grammar, `mod`-based module tree, agent-popular; deep mode free
   later via rust-analyzer's SCIP emission.
2. **Java** — grammar excellent; package→path convention covers most sources; build
   systems (Maven/Gradle) only matter for external-dep resolution, which Tier 1
   heuristics already treat as `external`.
3. **C#** — similar profile to Java.

## 4. Deferred and why

C/C++ (include resolution requires build config — the value/effort ratio is wrong until
Tier 2 proves the pattern); Ruby/PHP/Swift/Kotlin/PHP (fine grammars, welcome as
community adapters once the plugin seam stabilizes in Phase 6); Markdown/configs
(**not** languages — handled as data files by path heuristics, capped per ALGORITHM §7);
JSON/YAML/SQL (data files, not adapters).

## 5. The LanguageAdapter contract

One trait, one module per language, no core changes required to add a language
(the seam's whole purpose — MASTER_PLAN §10):

```text
LanguageAdapter {
  // identity
  language_id, file_extensions[], shebang_patterns[]

  // extraction (cs-extract)
  def_query, ref_query, import_query     // owned .scm query sources
  signature_extractor                    // def node → one-line signature
  doc_extractor                          // def node → first doc paragraph
  symbol_kind_map                        // grammar node type → canonical kind

  // resolution (cs-resolve)
  resolve_import(raw, context) → FileId | External | Unresolved(reason)
  module_qualifier(file) → module path string
  export_visibility(def) → exported?     // language-specific rules
  candidate_defs_filter(ref, defs)       // container scoping rules

  // conventions
  test_file_matcher(code_path) ⇄ test_path
  config_file_patterns[]                 // for signal S8 (ALGORITHM §5)
}
```

A new language = this struct + golden fixtures (§8). Nothing in cs-select, cs-render,
cs-index, or cs-cli knows a language exists beyond `lang` strings.

## 6. Per-language specifications

### 6.1 Go

*Implemented (2026-09-15); this section is the as-built contract. Deviations
from the pre-implementation sketch are recorded in ADR-017.*

- **Extraction output:** `package_name` from the package clause (needed by the
  resolver for same-package scoping; `None` for empty/broken files); defs,
  refs, imports per the shared `ExtractedFile` shape.
- **Defs (top-level only, one per name):** `function_declaration` →
  `Function`; `method_declaration` → `Method` (container = receiver base
  type, `*`/generics stripped: `(s *Session)` → `Session`, `(p Pair[T])` →
  `Pair`); `type_spec` → `Struct`/`Interface`/`Type` by its type child;
  `type_alias` → `Type`; each `const_spec`/`var_spec` name → `Const`/`Var`
  (multi-name specs `var X, Y int` share the spec span; blank `_` names are
  dropped — never addressable). Declarations *inside* function bodies are
  never defs (queries are rooted at `source_file` to guarantee this).
- **`exported`:** Unicode-aware first-rune uppercase, per the Go spec
  (`Ünicode` is exported).
- **Signatures:** functions/methods = declaration header to the body,
  whitespace-normalized (receiver and type parameters included);
  struct/interface = `type Name[T any] struct { … }` (literal ellipsis;
  bodies render from spans at L3, never stored); named types/aliases = full
  spec; const/var specs = text with every func-literal *body* replaced by
  `…` so `var Handler = func(w R) error {…}` keeps its type shape.
- **Docs:** a contiguous comment block immediately above the declaration —
  *no blank line*, verified on line numbers because blank lines leave no
  trace in the tree (the query `.` anchor is only a candidate filter);
  first paragraph only; `//` and `/* */` forms both stripped. Package
  comments are not stored (no def to attach to; nothing downstream needs
  them). Struct-field docs are not extracted (fields are not defs).
- **Refs, four kinds:** `name_ref` (identifiers in expression position,
  including the package qualifier of `qualified_type`/selector expressions —
  the resolver needs it for import-scoped binding); `field_ref`
  (field_identifier kept **only** under `selector_expression`, *not* in
  call position — the same node type also spells method/field/interface-
  method *names*, which are declarations); `call_ref` (a selector in call
  position, `x.Foo()`/`pkg.Foo()` — the callee is a method or function,
  never a data field, so its binding rules differ; ADR-018);
  `type_ref` (type_identifier kept unless in a type_spec/type_alias name
  position).
- **Declaration positions never become refs:** function/param/receiver names
  (incl. `variadic_parameter_declaration`), leading identifier runs of
  var/const specs (top-level and local), type-parameter names, `:=` left
  sides (short vars, if/for init, range clauses with `:=`, type-switch
  `alias` variables), and identifier keys of **struct-shaped** composite
  literals (`RouteInfo{Handler: x}` names a field; map-literal keys remain
  references since their type child is a `map_type` — the named-map-type
  case is a documented loss, ADR-018). Local *uses* do remain refs —
  whether a name is local is scope information beyond syntax; the
  resolver's sqrt-damped binding absorbs the approximation.
- **Universe filter:** references to Go's predeclared identifiers (`int`,
  `error`, `make`, `any`, … full list in `cs-extract/src/go/mod.rs`) are
  dropped — they can never bind to a repository definition and would
  otherwise dominate the refs table. The blank identifier `_` likewise
  never appears.
- **Imports:** raw path with quotes stripped; `alias` as written — named
  (`f`), dot (`.`), blank (`_`) imports all recorded; Go has only the
  static `Import` kind.
- **Degradation policy (normative):** a def survives a partial file only if
  its declaration subtree contains no error node; refs are dropped if any
  ancestor is an error node **or** if they fall inside a dropped
  declaration's span (recovery may leave such regions un-`ERROR`-wrapped);
  the file is labeled `partial` with whatever survived cleanly.
- **Resolution (as built, ADR-018):** filesystem-only, no `go list`.
  Imports resolve through nearest-`go.mod` module mapping: own-module
  prefix → repo-relative dir (a *directory*: an import binds the package's
  non-test files); relative `./`/`../` against the importer dir
  (`EscapesRoot` when it leaves the repo); `vendor/<path>` when vendor
  exists; dotless first segment → stdlib `External`; else third-party
  `External`. An import landing in a **different (nested) module's subtree
  is `External`** even when the path looks in-repo (chi `_examples` trap).
  Package identity is `(dir, package_name)` — `foo_test` is a different
  package than `foo`, internal test files share identity but are invisible
  to importers, and no non-test source ever binds into a test file.
  Qualifiers come from the alias or the target's **package clause**, never
  the path tail. Binding: unqualified names/types bind all same-package
  defs (build-tag variants — gin `codec/json` ships 4); package-qualified
  selectors bind exported defs, kind-appropriate; bare method calls bind
  only when unique in the package and not on Go's universal interface
  surface (`Close`, `ServeHTTP`, … — own unbound reason
  `universe_method`); bare field accesses and method values never bind
  (`needs_type_info`). Every unbound ref carries a documented reason,
  published as a histogram (ARCHITECTURE §4.3). Optional precision sidecar
  (`go list`/SCIP) stays behind `--deep=go`, never required. Dot/blank
  import rules are fixture-validated only — zero occurrences in 875
  real-world import lines (census).
- **Measured (docs/benchmarks/resolver-gin-chi.md):** in-repo import
  resolution 100% on gin and chi; sampled binding precision 97.4%/96.9%;
  resolve ≤ 11 ms per repo.
- **Hard parts:** embedded/promoted fields and methods (approximate: treat
  promoted methods as defs of the embedded type — labeled
  over-approximation); build-tagged files (index all — extraction is
  per-file, so same-named defs in differently-tagged files coexist);
  `vendor/` (indexed but excluded from seeds unless `--include`).
- **Tests:** `*_test.go` ⇄ same-dir non-test files; `example_test.go`
  attached to the package file defining the symbol.
- **Config files:** `go.mod`, `*.yaml` next to code, `Makefile`.

### 6.2 TypeScript / JavaScript

- **Extraction:** defs = `function_declaration`, `class_declaration` (+ methods),
  `variable_declarator` with function/arrow/class initializers, `interface_declaration`,
  `type_alias_declaration`, `enum_declaration`, `export_statement` unwrapped to its
  inner def; refs = `identifier` + `member_expression.property` (typed `field_ref`,
  matched against interface/class properties only — this is the approximation's main
  noise source and is sqrt-damped by design); imports = `import_statement`,
  `export_statement ... from`, `call_expression require()`, dynamic `import()`
  (recorded as `kind=dynamic`, weight ×0.5). Extraction uses
  `LANGUAGE_TYPESCRIPT` for `.ts` and `LANGUAGE_TSX` for `.tsx`/`.js`/`.jsx` — see §9 for
  why neither grammar can serve both.
- **Resolution:** specifier classes, in order — (1) relative (`./x`) → resolve against
  importer dir, trying `x.ts x.tsx x.js x/index.ts...`; (2) tsconfig/jsconfig `paths`
  + `baseUrl` longest-prefix match; (3) workspace/package aliases from
  `package.json` `workspaces` + self-name; (4) `node_modules/<pkg>` → **External**
  (we do not index dependencies in v0.1; the *type* surface of externals is out of
  scope, noted in the header when an L5 file imports unresolved externals);
  (5) bundler aliases (webpack/vite/tsconfig-paths) → best-effort from
  `webpack.config`/`vite.config` literal reads, else `Unresolved(alias)`.
  Re-export chains (`export * from`, `export {x} from`) followed transitively with
  depth cap 8 and cycle detection; chain edges get weight ×0.75 (re-exports are weaker
  evidence than direct imports).
- **Hard parts & degradation:** every unresolved import is *recorded as such*; the
  header/doctor surface a **resolution rate** ("92% of imports resolved; 14 aliases
  unresolved — list in doctor"). JSX/TSX use the same grammar with node-type filters.
  `export =`/namespace patterns matched approximately. Monorepos: workspace globs from
  package.json define the root set.
- **Tests:** `*.test.ts`, `*.spec.ts`, `__tests__/` ⇄ nearest matching source file
  (basename minus suffix); test dirs (`test/`, `tests/`) mapped by basename.
- **Config files:** `package.json` (scripts/deps sections only at L3),
  `tsconfig.json`, `.env.example` (never `.env` — secret policy, SECURITY §5).

### 6.3 Python

- **Extraction:** defs = `function_definition`, `class_definition` (+ methods),
  assignments at module/class level (typed `var`, name-only); refs = `identifier`;
  imports = `import_statement` (`import a.b`, `from a.b import c`) incl. relative
  `from . import x` (dots counted).
- **Resolution (pyright-style static, no execution):** search order per module —
  (1) relative: walk up by dot-count from importer; (2) root/src-layout roots
  (`pyproject.toml`/`setup.py`/`setup.cfg` presence, `src/` convention);
  (3) `extraPaths`-style config if present (`pyrightconfig.json`,
  `[tool.pyright]`); (4) nearest venv's `site-packages` → **External** (listed, not
  indexed). `from pkg import name` resolves to module file when `name` is itself a
  module, else to a def in `pkg/__init__.py` or the package's modules. Namespace
  packages handled by directory existence. `__init__.py` re-exports followed like TS
  re-export chains (depth cap 8).
- **Hard parts & degradation:** dynamic imports (`importlib`, `__import__`) →
  `Unresolved(dynamic)`; star imports (`from x import *`) → edge to module only, no
  symbol-level matching; `getattr`-style indirection → out of scope, documented.
- **Tests:** `test_*.py`/`*_test.py` ⇄ same-package module; `tests/` mirrored onto
  package by path; `conftest.py` attached to the directory's files (test infrastructure
  worth L3 when its partners are L5).
- **Config files:** `pyproject.toml`, `setup.py`, `requirements*.txt` (deps at L1).

## 7. Fallback for unsupported languages

Files with no adapter: language detected (extension map), no defs/refs/edges; they
compete for seeds on path/basename/content signals only (S2/S3/S5, ALGORITHM §5); they
can reach L1/L5 but never L2–L4 (skeletons require parsing); the slice header states
how many files ran in heuristic mode. This keeps mixed-language repos functional
without pretending fidelity.

## 8. Adding a language — the contributor checklist

1. Read ARCHITECTURE §4.2–4.3 and this file's per-language specs.
2. Create `cs-extract/src/lang/<id>.rs` (queries + extractors) and
   `cs-resolve/src/lang/<id>.rs` (resolver) behind the LanguageAdapter trait.
3. Golden fixtures in `fixtures/<id>/`: 30+ files covering the per-language "hard
   parts" lists + 5 pathological files (broken syntax, deep nesting, unicode, huge
   signatures, comment tricks). Goldens assert exact extracted defs/refs/imports.
4. Resolver tests: a synthetic monorepo fixture exercising import classes, externals,
   re-export cycles, unresolved aliases.
5. Wire `language_id` into scanner's extension map and `doctor`'s report.
6. Add 10 tasks from real PRs of one real repo in that language to the intrinsic
   benchmark corpus (BENCHMARK §3.1 rules); report recall@8k.
7. Docs: fill in a §6-style spec for the language; note its approximations honestly.

Timebox expectation from the Go/TS/Python experience: 3–6 focused days for a
Tier-2-language-quality adapter, 1–2 days for a grammar-only "heuristic plus" adapter.

## 9. Grammar versioning policy

- Every grammar crate pinned to an exact version in the workspace manifest; versions
  bump only in dedicated PRs that re-run all goldens (tree-sitter minor releases have
  broken node types before; the pins make drift impossible to miss).
- **Grammars reach the runtime through `tree-sitter-language`, not through
  `tree-sitter` itself.** This decouples grammar releases from runtime releases, and it is
  why a grammar that has not published a crates.io release recently can still be current.
  The check that matters is not a release date but whether the pinned set loads and parses
  under the pinned runtime — which is asserted by the `grammars_load_and_parse` test, so
  an ABI break fails CI instead of a user's first run.
- **Verified 2026-09-15:** `tree-sitter-go` 0.25.0, `tree-sitter-python` 0.25.0 and
  `tree-sitter-typescript` 0.23.2 all load and parse under `tree-sitter` 0.27.0. Note that
  `tree-sitter-typescript` has published no crates.io release since **2024-11-11** while
  its upstream repository shows commits in September 2026: a *release* gap, not
  abandonment. Recorded here so it is not rediscovered as an alarm.
- **`.ts` and `.tsx` are different grammars and the difference is load-bearing.**
  `LANGUAGE_TYPESCRIPT` cannot parse JSX at all; `LANGUAGE_TSX` cannot parse
  angle-bracket type assertions (`<string>value`). Neither is a superset, so the grammar
  is selected per extension: `.ts` → `LANGUAGE_TYPESCRIPT`, `.tsx`/`.js`/`.jsx` →
  `LANGUAGE_TSX`. A file whose content contradicts its extension parses partially and is
  labeled `partial` rather than guessed at.
- Goldens store grammar-node assertions, not raw CST dumps, so cosmetic grammar changes
  do not produce false diffs — but semantic query changes do (that is the point).
- A `LANGUAGE_SUPPORT.md` matrix (generated from adapter registrations + golden pass
  rates) publishes per-language extraction coverage; "experimental" is a visible label,
  never a surprise.

## 10. Reference query sets and attribution

ADR-012 settles the decision: queries are first-party, in our own capture schema; the
tags-crate convention is unusable (its predicates are silently ignored by the core
bindings — measured, not assumed). External query sets may still be consulted for
**grammar node names**, under the rules below. Sets checked 2026-09-15:

| Source | License | Location | Use |
|---|---|---|---|
| aider tag queries | Apache-2.0 (verified in ADR-012) | `Aider-AI/aider` at `aider/queries/tree-sitter-language-pack/*.scm` (58 files) | node-name reference only; **no `typescript-tags.scm` exists** — TS node names need another source |
| nvim-treesitter queries | verify before consulting (rule 1) | `nvim-treesitter/nvim-treesitter` at `queries/<lang>/*.scm` | highlight-oriented; secondary node-name reference |

Rules: (1) a source enters this table only after its license is verified and dated;
(2) queries are never copied as a starting point (ADR-012); (3) any query file taking
substantial structural inspiration from a source carries a header comment naming the
source, its license, and the upstream commit consulted; (4) credit for conceptual
borrowings (sqrt-damping, the uninformative-name dampener) lives in ALGORITHM.md §15.
