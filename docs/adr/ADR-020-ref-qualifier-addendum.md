# ADR-020: Ref qualifier addendum — structural selectors, operand suppression, rule-table changes

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-09-16 |
| **Milestone** | Foundation Recovery (extraction contract follow-up) |
| **Follows** | [ADR-017](ADR-017-extraction-contract-as-built.md) (extraction contract), [ADR-018](ADR-018-go-resolver.md) (binding rules), [ADR-019](ADR-019-foundation-recovery.md) (recovery record) |

## Context

ADR-017 froze an extraction contract in which `Ref` carried `name`, `kind`,
`span`, `container` — and nothing about selectors. The resolver (ADR-018)
compensated with a *byte-distance heuristic*: an identifier that ended a few
bytes before a qualified reference was guessed to be its package qualifier.
That guess was the audit's F-11 false-positive class (chained selectors,
comma-separated arguments), it coupled extraction to resolver guesswork, and
it could not distinguish `rp.Add` (instance call) from `pkg.Add` (package
call) — two refs with identical resolver needs and identical bytes-adjacent
shapes. The recovery pass replaced the guess with structure. ADR-019 records
the decision; this addendum is the contract text: what changed in the
extraction schema, what the resolver's rule table now says, and what it did
to the goldens.

## Decisions

### 1. `Ref.qualifier: Option<String>` — the selector's operand, recorded once

`Ref` gains one field. For a reference that is the *selected* part of
`operand.name` (a `selector_expression` in Go), `qualifier` carries the
operand's identifier text when the operand is a plain identifier — `auth` of
`auth.Session`, `rp` of `rp.Add`, `s` of `s.UserID`. It is `None` in every
other case: no selector at all (`helper()`, a bare type name), or a
**computed operand** (`w.Header().Add`, `arr[0].Close`) whose receiver's
package no syntactic rule can name. For `qualified_type` targets
(`io.Closer` in type position) the qualifier is the `package` child.

**Why one field.** For selector refs, qualifier *presence* *is* the
identifier/expression distinction: `Some(_)` means the operand was a plain
identifier, `None` means "no selector or computed". No separate
`selector_base` boolean exists because the binding rules never need to
separate those two `None` cases — kind and scope do that (§3 below) — and a
field nobody reads is contract surface without a consumer (the same test
that killed `sig_spans` in ADR-017 §5).

### 2. Operand occurrences are suppressed, not emitted

The operand of a selector — the `auth` in `auth.Session`, the receiver in
`x.Foo()` — is **scope structure, not a name use**, and is no longer emitted
as an independent reference. Consequences, all deliberate:

- The resolver never has to decide whether an identifier *token* is a
  qualifier or a value; the *reference* says so.
- The gin-audit's "local shadows a package name" false-positive class
  (`r.Get` binding `r` to a package-level def named `r`) disappears at the
  source instead of being damped downstream.
- Ref counts drop; every remaining ref is a genuine name use. The go-resolve
  golden audit measured the effect: 31 refs removed across the Go goldens,
  every one accounted for as a suppressed operand or a universe-filtered
  builtin (`docs/reviews/golden_diff_audit.md`).

### 3. Resolver rule table after the migration

The binding table of ADR-018, restated over the structural qualifier (the
byte-distance heuristic is deleted):

| Ref shape | Rule | Unbound reason when nothing binds |
|---|---|---|
| `name_ref`, no qualifier | own-package defs (test-visibility filtered), plus dot-imported exports | `no_candidate` / `ambiguous_dot_import` |
| Qualified ref, qualifier names an in-repo import | exported defs of that package, kind-filtered (`type_ref` → types; `call_ref` → funcs/vars; `field_ref` → vars/consts/types) | `no_candidate` when the name exists but the kind doesn't match |
| Qualified ref, qualifier names an external import | never binds; the scope is external | `external_scope` |
| Qualified ref, **unknown identifier** operand (`x.T` where `x` is a value/typo) | no package scope; for `call_ref` keep the bare-instance rule — bind the unique same-package method named `name` | `method_ambiguous` / `no_candidate`; `universe_method` on Go's universal interface surface |
| **Computed operand** (`w.Header().Add`, `arr[0].Close`) | never binds — the receiver's package is unknowable syntactically, and a call through it can never be claimed as a local method | **`no_scope` (revived)** — previously dead code (audit F-35), now the honest label for the class |
| `field_ref`, any operand | never binds; needs receiver type info | `needs_type_info` |

The qualifier's *scope resolution* is unchanged from ADR-018 (alias, else
target package clause, never path tail, `/vN` stripped for externals); what
changed is that the resolver *reads* the qualifier instead of guessing it.

### 4. `package_qualifier_refs` stat meaning changes

The stat now counts **references whose recorded qualifier names one of the
file's import scopes** — a count of selector refs the resolver could
scope-check, not a count of identifier tokens that looked like packages. The
go-resolve golden's `14 → 18` delta is exactly the qualified refs the new
fixture code added (audited: `docs/reviews/golden_diff_audit.md` §3).

### 5. Golden impact

Every Go golden regenerated: every ref gained the `"qualifier"` key
(`null` where absent); operand name-refs vanished, with the qualified ref at
the same source span gaining the operand as its qualifier (the audit's 1:1
mapping check); no defs, imports, package clauses or statuses changed except
the E3 doc-override fix. Full hunk-by-hunk review: **PASS**,
`docs/reviews/golden_diff_audit.md`.

## Consequences

- LANGUAGES.md §6.1 documents the structural qualifier, operand suppression,
  `NoScope` emission, and `ParseStatus::Timeout` as contract; ARCHITECTURE §5
  gains `refs.qualifier`.
- TS/Python adapters inherit the shape for free: `obj.prop` in JS and
  `obj.attr` in Python are the same selector question, and answering it at
  extraction is where their trees are in hand.
- The extraction contract's ref schema now encodes one piece of
  *relationship* (operand → selection), the minimum that makes the resolver
  a pure rule table.

## What would reopen this

- A rendering or selection need for the *operand's own* reference (e.g. an
  L5 anchor on every qualifier occurrence) — would add a second ref kind,
  not a second field.
- A language whose selectors can name two different scopes at one operand
  (import vs value) in a way kind+scope cannot separate — would force the
  `selector_base` split this ADR declined.
- A measured false-positive class caused by identifier-operand bare-instance
  binding (§3 row 4) surviving the dampeners.
