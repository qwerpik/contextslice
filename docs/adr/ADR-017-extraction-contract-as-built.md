# ADR-017: Extraction contract as built — package, kinds, exported, aliases, degradation

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-15 |
| **Milestone** | Go extraction (MASTER_PLAN §15 step 3) |
| **Follows** | [ADR-012](ADR-012-own-tree-sitter-queries.md) (own queries), [ADR-003](ADR-003-approximate-reference-graph.md) (approximate edges) |

## Context

Implementing the Go adapter (the reference adapter for the extraction seam)
required resolving contract questions the pre-implementation documents left
open or answered differently. Each deviation below was found by building,
pinned by golden fixtures, and is now the documented contract in
LANGUAGES.md §6.1. This record exists so the TypeScript and Python adapters
inherit decisions, not accidents.

## Decisions

1. **`ExtractedFile` carries `package_name`.** The original schema sketch had
   no home for the package clause, yet the resolver scopes same-package
   unqualified names by it — without it, step 4 cannot bind anything. It is
   file metadata, not a `Module` def: package names are common words and
   would pollute symbol search (signal S1).

2. **`Ref` carries `kind`: `name_ref` / `field_ref` / `type_ref`.** The
   original sketch had one undifferentiated ref type and a "field refs typed
   `field_ref`" aside. In Go, binding rules genuinely differ: field refs
   match struct fields / interface methods only; type refs match type defs
   (exported-only cross-package); name refs match funcs/vars/consts. Making
   the kind an extraction fact (from grammar node type + parent context)
   lets the resolver stay a pure rule table.

3. **`Def` carries `exported`.** Export visibility is decided at extraction,
   where the language's casing rule and the name are in hand; goldens pin
   it (including the Unicode rule: `Ünicode` is exported). The
   `LanguageResolver::is_exported` seam remains for languages whose rules
   need more than syntax.

4. **`Import` carries `alias`** — including Go's dot (`.`) and blank (`_`)
   forms spelled as written. The resolver needs them: dot imports change
   name scoping entirely; blank imports carry no names at all.

5. **Signatures and docs are extracted strings, not spans.** The sketch had
   `sig_spans`/`doc_spans`. Strings are what L2/L3 render directly, what
   goldens pin byte-for-byte, and what the future `symbols.signature`/
   `symbols.doc` columns store; spans alone would force every consumer to
   re-derive the same text. Def spans are kept (bodies render from them at
   L4/L5); ref spans are kept (renderers anchor `path:line`).

6. **Degradation policy is normative:** a def survives a partial file only
   if its subtree has no error node; refs are dropped if any ancestor is an
   error node **or** they fall inside a dropped declaration. The second
   clause was added during golden review: tree-sitter recovery can leave a
   broken declaration's region *not* wrapped in an `ERROR` node, so the
   ancestor check alone let a truncated function's parameter leak through
   as a phantom `type_ref`.

7. **Declaration-position filtering is a Rust walk, not queries.** The
   filters need parent-kind context and, for `:=` vs `=`, the source text —
   beyond the query language. The rule table (params, receivers, variadics,
   leading spec runs, type params, short-var/range/type-switch left sides)
   is documented in LANGUAGES.md §6.1 and pinned by goldens.

8. **Go's predeclared identifiers are filtered at extraction.** `int`,
   `error`, `make`, … can never bind to a repository definition; keeping
   them would fill the refs table with zero-signal rows (`int` would
   dominate any corpus). This is binding knowledge, stated as an explicit
   list in the adapter rather than implicit resolver behavior.

9. **Blank `_` defs are dropped** (`const _ = iota` skip placeholders).
   They are never addressable; their occurrences stay declaration
   positions so they never become refs either.

## What stayed as specified

Queries are first-party per ADR-012 (capture schema adjusted to
`@def.node` carrying the declaration for kind/signature derivation — a
capture naming node *roles*, not re-implementing tags). tree-sitter-go
0.25.0 pinned; determinism by span re-sorting; `ParseStatus` semantics
unchanged.

## Consequences

- cs-index's schema sketch gained `files.package_name`, `symbols.exported`,
  `symbols.start_byte/end_byte`, `refs.kind`, `imports.alias`
  (ARCHITECTURE §5) — updated in the same change set so extraction output
  and the planned schema cannot drift.
- TS/Python adapters must answer the same nine questions; LANGUAGES §6.1
  is the template.
- Golden fixtures are the contract's enforcement: any behavior change shows
  up as a golden diff that a reviewer must read.

## What would reopen this

A resolver requirement that cannot be expressed with these facts (e.g. a
needed distinction among name-ref *positions*), or an L3 rendering need
that makes span-based signature construction necessary after all.
