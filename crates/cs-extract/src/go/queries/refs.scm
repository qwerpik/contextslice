;;; Go reference queries — first-party, capture schema per ADR-012.
;;;
;;; These captures are deliberately raw: they include declaration positions
;;; (function names, parameters, local variables). Filtering those out is a
;;; syntactic rule implemented in Rust (see `declaration_spans` in mod.rs),
;;; because it needs parent-kind context the query language cannot express.
;;;
;;; Selector relationships are NOT expressed here: the qualifier of
;;; `pkg.Foo` / `x.Foo` and the operand kind (identifier vs computed
;;; expression) are read in Rust from the `selector_expression` /
;;; `qualified_type` parents, where the tree is in hand, and recorded
;;; structurally on the reference (ADR-017 addendum). The operand identifier
;;; is suppressed — it is scope structure, not a name use — which also
;;; removes the "local shadows a package name" false-positive class at the
;;; source instead of dampening it in the resolver.
;;;
;;; Kinds:
;;;   @ref.name  — identifiers in expression position (plain calls, locals).
;;;   @ref.field — field/property selections. Kept only when the parent is a
;;;                selector_expression; the same node type also spells method
;;;                names, struct field names and interface method names, which
;;;                are declarations, not references. Call position
;;;                (`x.Foo(…)`, `pkg.Foo(…)`) is upgraded to CallRef in Rust,
;;;                where the parent chain is in hand — binding rules differ
;;;                (ADR-018).
;;;   @ref.type  — type usages. Kept unless in the `name` field of a
;;;                type_spec/type_alias (the type's own name position); the
;;;                *target* of an alias or named type is a real reference.

(identifier) @ref.name

(field_identifier) @ref.field

(type_identifier) @ref.type
