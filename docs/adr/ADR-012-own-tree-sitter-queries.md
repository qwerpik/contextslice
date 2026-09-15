# ADR-012: Own `.scm` queries; reject the tags crate and aider's queries as a drop-in

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-15 |
| **Milestone** | Bootstrap |

## Context

Extraction needs per-language queries that capture definitions, references, imports,
signatures and doc comments. ARCHITECTURE.md §4.2 already said "owned `.scm` query sets
per language, never the stale tree-sitter-tags crate". The reconnaissance pass suggested
a cheaper route: reuse aider's tree-sitter tag queries, which exist for all our target
languages, as reference material.

Aider is Apache-2.0 and its query files are 58 `.scm` files in
`aider/queries/tree-sitter-language-pack/`. On licensing grounds alone, reuse would be
permissible with attribution. The question was technical fit.

## Decision

Write and own our `.scm` queries, using the **core** tree-sitter query language with our
own capture schema (`@def.name`, `@def.kind`, `@def.sig`, `@def.doc`, `@ref.name`,
`@import.module`, `@import.names`). Do **not** reuse aider's query files as a drop-in,
and do not depend on `tree-sitter-tags`.

Aider's queries remain useful as *reference material* for grammar node names — we credit
the borrowings we actually take (the sqrt-damping insight and the "uninformative name"
dampener, ALGORITHM.md §15) — but they are not the starting point for our query set.

## Why: the predicates are silently ignored

This is the decisive finding, and it was measured rather than assumed.

The `tree-sitter-tags` convention depends on query predicates that the core Rust binding
**does not implement**. The core binding handles exactly `eq?`, `not-eq?`, `any-eq?`,
`match?`, `not-match?`, `any-match?`, `any-not-match?`, `is?`, `is-not?`, `any-of?`,
`not-any-of?`. Anything else — including `#strip!` and `#set-adjacent!` — is parsed into
the query's predicate list and then never evaluated.

The core C parser documents this behaviour explicitly. In `lib/src/query.c`:

> Predicates are arbitrary S-expressions associated with a pattern which are meant to be
> handled at a higher level of abstraction, such as the Rust/JavaScript bindings.

So a tags-style query **compiles successfully** and produces captures that were never
post-processed. Verified experimentally against aider's Go query:

```
capture "doc"    = "// Login authenticates a user."   <- raw, #strip! never ran
capture "name.definition.function" = "Login"
NOTE: no post-processing predicate ran
```

The failure mode is the dangerous kind: no error, no warning, plausible-looking output,
and doc comments that silently retain their `//` prefixes while adjacency between a doc
comment and its definition is never computed. In a tool whose output feeds an LLM, a
quietly-malformed doc extraction is worse than a loud failure.

## Why: the capture schema is also wrong for us

Even ignoring predicates, aider's queries target the tags *consumer* convention
(`@name.definition.function`, `@reference.call`, `@definition.method`), which answers
"what symbols exist". Extraction here additionally needs signatures, doc spans and
import specifiers per ARCHITECTURE.md §4.2, plus container scoping for the approximate
binder in ADR-003. Adapting them would have meant rewriting most patterns anyway — at
which point we would be maintaining a fork of upstream queries with a provenance
question attached.

`#` predicates are also not the only tags feature we would need: adjacency
(`#set-adjacent!`) is what attaches a doc comment to the definition that follows it. Core
queries express the same intent directly with the anchor operator (`.`), which is
supported.

Importantly, **we lose nothing by writing our own**: the core query language supports
everything we need, including anchors for adjacency.

## Consequences

- Query files are first-party, versioned with the grammar pins, and covered by the golden
  fixtures LANGUAGES.md §8 requires.
- No dependency on an unmaintained crate, and no ambiguity about which predicates are
  actually evaluated.
- More work: roughly a query set per language instead of a copy. Accepted, because the
  alternative is silently wrong output.
- The test suite asserts ABI and grammar behaviour rather than the tags convention, so a
  future grammar bump that changes node types fails a golden rather than degrading a slice.

## Verification (2026-09-15)

- Compiled aider's Go query against the core `tree-sitter` 0.27 Rust binding: it
  **succeeds**, confirming predicates are not validated.
- Executed it over Go source and dumped captures: `@doc` retained raw `//` text and no
  adjacency was computed, proving the predicates are no-ops rather than errors.
- Inspected the core C parser's predicate handling and the Rust binding's predicate match
  arms to confirm the implemented set.
- Retrieved aider's 58 `.scm` files and confirmed license (Apache-2.0) and capture schema.

## Alternatives rejected

- **Reuse aider's queries verbatim** — rejected above: silently-ignored predicates plus a
  mismatched capture schema.
- **Depend on `tree-sitter-tags`** — rejected: unmaintained, and it is the thing the
  predicates exist to serve. Adopting it would put a stale crate on the extraction path.
- **Port aider's queries to core syntax wholesale** — a derived work needing attribution
  for patterns we would have to rewrite for our own schema anyway; the borrowing that is
  genuinely valuable (ALGORITHM.md §15) is credited directly.

## What would reopen this

Core tree-sitter adding first-class support for the tags predicates, which would make the
tags convention viable and remove the silent-failure hazard. Even then, the capture schema
mismatch would still argue for our own queries.
