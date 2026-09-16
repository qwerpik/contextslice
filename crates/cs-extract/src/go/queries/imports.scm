;;; Go import queries — first-party, capture schema per ADR-012.
;;;
;;; import_spec appears both directly under import_declaration (single import)
;;; and under import_spec_list (grouped `import ( … )`); unrooted patterns
;;; match both because import_spec exists nowhere else in the grammar.
;;;
;;; The bare pattern matches every spec; alias patterns add the alias capture.
;;; Rust merges by node identity, so a spec is emitted once with its alias.
;;;
;;; Alias node shapes (verified, tree-sitter-go 0.25):
;;;   named alias  f "fmt"   -> (package_identifier)
;;;   dot import   . "math"  -> (dot)
;;;   blank import _ "embed" -> (blank_identifier)
;;;
;;; The path may be an interpreted OR a raw string literal — `import `re` is
;;; legal Go (the Go spec: "The import path is a string literal"), so both
;;; spellings are captured; Rust strips either quoting style.

(import_spec
  path: [
    (interpreted_string_literal) @import.path
    (raw_string_literal) @import.path
  ]) @import.node

(import_spec
  name: (package_identifier) @import.alias
  path: [
    (interpreted_string_literal) @import.path
    (raw_string_literal) @import.path
  ]) @import.node

(import_spec
  name: (dot) @import.alias
  path: [
    (interpreted_string_literal) @import.path
    (raw_string_literal) @import.path
  ]) @import.node

(import_spec
  name: (blank_identifier) @import.alias
  path: [
    (interpreted_string_literal) @import.path
    (raw_string_literal) @import.path
  ]) @import.node
