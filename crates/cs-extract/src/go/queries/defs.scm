;;; Go definition queries — first-party, capture schema per ADR-012.
;;;
;;; Patterns that carry @def.doc come BEFORE their bare twins so the Rust
;;; merge step can prefer a doc-carrying match; the merge is still correct if
;;; match order changes, because it merges by node identity.
;;;
;;; Every pattern is rooted at (source_file …) on purpose: Go allows local
;;; const/var/type declarations inside function bodies, and those must never
;;; match — only file-level declarations are definitions.
;;;
;;; Verified node shapes (tree-sitter-go 0.25, CST dumps 2026-09-15):
;;;   - grouped consts:  const_declaration > const_spec          (no list node)
;;;   - grouped types:   type_declaration  > type_spec | type_alias (no list node)
;;;   - grouped vars:    var_declaration   > var_spec_list > var_spec
;;;     single var:      var_declaration   > var_spec
;;;   - a type alias `type A = B` is a distinct type_alias node.

;;; --- functions ---------------------------------------------------------
(source_file
  (comment) @def.doc .
  (function_declaration name: (identifier) @def.name) @def.node)

(source_file
  (function_declaration name: (identifier) @def.name) @def.node)

;;; --- methods (receivers are read in Rust from @def.node) ----------------
(source_file
  (comment) @def.doc .
  (method_declaration name: (field_identifier) @def.name) @def.node)

(source_file
  (method_declaration name: (field_identifier) @def.name) @def.node)

;;; --- constants (single and grouped share one shape) ---------------------
(source_file
  (comment) @def.doc .
  (const_declaration
    (const_spec (identifier) @def.name) @def.node))

(source_file
  (const_declaration
    (const_spec (identifier) @def.name) @def.node))

(source_file
  (const_declaration
    (comment) @def.doc .
    (const_spec (identifier) @def.name) @def.node))

;;; --- types (type_spec and type_alias; single and grouped share one shape)
(source_file
  (comment) @def.doc .
  (type_declaration
    [(type_spec name: (type_identifier) @def.name)
     (type_alias name: (type_identifier) @def.name)] @def.node))

(source_file
  (type_declaration
    [(type_spec name: (type_identifier) @def.name)
     (type_alias name: (type_identifier) @def.name)] @def.node))

(source_file
  (type_declaration
    (comment) @def.doc .
    [(type_spec name: (type_identifier) @def.name)
     (type_alias name: (type_identifier) @def.name)] @def.node))

;;; --- variables (single: var_spec direct; grouped: var_spec_list) --------
(source_file
  (comment) @def.doc .
  (var_declaration
    (var_spec (identifier) @def.name) @def.node))

(source_file
  (var_declaration
    (var_spec (identifier) @def.name) @def.node))

(source_file
  (var_declaration
    (var_spec_list
      (var_spec (identifier) @def.name) @def.node)))

(source_file
  (var_declaration
    (var_spec_list
      (comment) @def.doc .
      (var_spec (identifier) @def.name) @def.node)))
