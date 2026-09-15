;;; Go reference queries — first-party, capture schema per ADR-012.
;;;
;;; These captures are deliberately raw: they include declaration positions
;;; (function names, parameters, local variables). Filtering those out is a
;;; syntactic rule implemented in Rust (see `declaration_spans` in mod.rs),
;;; because it needs parent-kind context the query language cannot express.
;;;
;;; Kinds:
;;;   @ref.name  — identifiers in expression position, including the package
;;;                qualifier of a qualified type (`io` in `io.Writer`), which
;;;                the resolver needs for import-scoped binding.
;;;   @ref.field — field/property selections. Kept only when the parent is a
;;;                selector_expression; the same node type also spells method
;;;                names, struct field names and interface method names, which
;;;                are declarations, not references. Call position
;;;                (`x.Foo(…)`) is upgraded to CallRef in Rust, where the
;;;                parent chain is in hand — binding rules differ (ADR-018).
;;;   @ref.type  — type usages. Kept unless the parent is a type_spec or
;;;                type_alias, i.e. the type's own name position.

(identifier) @ref.name

(field_identifier) @ref.field

(type_identifier) @ref.type

(qualified_type
  package: (package_identifier) @ref.name)
