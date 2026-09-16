//! tree-sitter parsing and `.scm` query extraction into definitions, references
//! and imports.
//!
//! Implements `cs-extract` from ARCHITECTURE.md §4.2.
//!
//! # Owned queries, not the tags crate
//!
//! Queries are owned `.scm` sources compiled against the grammar by this crate.
//! The upstream `tree-sitter-tags` crate is deliberately not used: its tags
//! convention depends on `#strip!` / `#set-adjacent!` predicates that the core
//! Rust binding does **not** implement (it handles only `eq?`, `not-eq?`,
//! `any-eq?`, `match?`, `not-match?`, `any-match?`, `is?`, `is-not?`,
//! `any-of?`, `not-any-of?`). Core parsing accepts unknown predicates without
//! error and then ignores them, so a tags-style query compiles cleanly and
//! silently produces un-stripped, non-adjacent captures — a failure mode that
//! reports no diagnostic. See `docs/adr/ADR-012`.
//!
//! # Grammar pinning
//!
//! Grammars are pinned to exact versions and reach the runtime through the
//! `tree-sitter-language` ABI crate (LANGUAGES.md §9), which decouples grammar
//! releases from `tree-sitter` releases. The `grammars_load_and_parse` test
//! asserts the pinned set actually loads and parses, so an ABI mismatch fails
//! the build instead of failing at a user's first run.

#![forbid(unsafe_code)]

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use cs_scanner::Language;
use serde::{Deserialize, Serialize};

/// Per-file parse timeout in milliseconds (ARCHITECTURE §4.2, SECURITY.md §6).
///
/// Enforced *inside* the parse via tree-sitter's progress callback
/// (`Parser::parse_with_options` with `ParseOptions`): when the budget is
/// exceeded the parse is aborted cooperatively and the file is labeled
/// [`ParseStatus::Timeout`] with no extracted facts. The budget is wall-clock
/// per file; at tree-sitter's parse speed a legitimate file at the
/// [`PARSE_SIZE_CAP`] finishes orders of magnitude under it.
pub const PARSE_TIMEOUT_MS: u64 = 250;

/// Files at or below this size are parsed at all (ARCHITECTURE §4.1).
pub const PARSE_SIZE_CAP: u64 = 1024 * 1024;

/// A span of source. Lines are 1-based for display (`path:line` anchors,
/// ARCHITECTURE §9); byte offsets are 0-based into the file's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// First line of the construct, 1-based.
    pub start_line: u32,
    /// Last line of the construct, 1-based and inclusive.
    pub end_line: u32,
    /// Byte offset of the first byte.
    pub start_byte: u32,
    /// Byte offset one past the last byte.
    pub end_byte: u32,
}

/// Canonical symbol kind (ARCHITECTURE §5, `symbols.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefKind {
    /// A free function.
    Function,
    /// A method bound to a receiver or class.
    Method,
    /// A class.
    Class,
    /// A struct or record type.
    Struct,
    /// An interface, trait, or protocol.
    Interface,
    /// A type alias or named type.
    Type,
    /// A named constant.
    Const,
    /// A variable binding.
    Var,
    /// An enumeration.
    Enum,
    /// A module or package declaration.
    Module,
}

impl DefKind {
    /// Stable string used as the `symbols.kind` column value and in rendering.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Function => "func",
            Self::Method => "method",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Interface => "interface",
            Self::Type => "type",
            Self::Const => "const",
            Self::Var => "var",
            Self::Enum => "enum",
            Self::Module => "module",
        }
    }
}

/// How completely a file was analyzed.
///
/// Extraction never fails a run: an unparseable file is *labeled*, and the label
/// travels into `files.parse_status` and the slice header (ARCHITECTURE §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseStatus {
    /// Parsed with no error nodes.
    Ok,
    /// Parsed, but the tree contains error or missing nodes; extraction kept
    /// whatever resolved.
    Partial,
    /// The parse exceeded the per-file time budget
    /// ([`PARSE_TIMEOUT_MS`]) and was aborted before a tree existed; the
    /// file is listed with no extracted facts (SECURITY.md §6).
    Timeout,
    /// Not parsed: no adapter for the language, or the file exceeded
    /// [`PARSE_SIZE_CAP`].
    Skipped,
    /// Explicitly excluded from parsing.
    Unsupported,
}

/// A definition found in a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Def {
    /// Bare name as written in source (`Login`).
    pub name: String,
    /// File-local qualified name (`Session.Validate` for a method on
    /// `Session`). The module prefix (`internal/auth.`) is added by the
    /// resolver, which knows the import graph; extraction only sees one file.
    pub qual_name: String,
    /// Canonical kind.
    pub kind: DefKind,
    /// Source span (the whole declaration, body included, so L4/L5 rendering
    /// can slice bodies from it later).
    pub span: Span,
    /// Enclosing symbol's qualified name, when nested (a method on a type).
    pub container: Option<String>,
    /// Whether the name is visible to importers. Language-specific casing
    /// rules are applied at extraction, where the name is in hand, so goldens
    /// pin the behavior (LANGUAGES.md §6.1). The resolver's
    /// `LanguageResolver::is_exported` seam remains the cross-language API.
    pub exported: bool,
    /// One-line signature for L2/L3 rendering.
    pub signature: Option<String>,
    /// First doc-comment paragraph, or `None`.
    pub doc: Option<String>,
}

/// A reference to a name from inside a file.
///
/// The kind tells the resolver which binding rules apply (LANGUAGES.md §6.1):
/// name refs bind against function/var/const candidates, field refs against
/// struct fields and interface methods only, type refs against type
/// definitions. Extraction reports syntax; the resolver assigns semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    /// A plain identifier in expression position (`Login`, `pkg` in
    /// `pkg.Call`).
    NameRef,
    /// A field selection **not** in call position (`Session.UserID`, the
    /// method value `f := s.Validate`). The resolver does not bind these
    /// without receiver type information (ADR-018).
    FieldRef,
    /// A selection in call position (`s.Validate()`, `pkg.Login()`). Binding
    /// rules differ from field accesses: the callee is a method or function,
    /// never a data field — the distinction is made here, where the tree is
    /// in hand, because the resolver only sees flat refs (ADR-018).
    CallRef,
    /// A type usage (`*Session`, `map[string]Widget`, `io.Closer` — the name
    /// part).
    TypeRef,
}

impl RefKind {
    /// Stable string used in goldens, docs and the future `refs` table.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NameRef => "name_ref",
            Self::FieldRef => "field_ref",
            Self::CallRef => "call_ref",
            Self::TypeRef => "type_ref",
        }
    }
}

/// A reference to a name from inside a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ref {
    /// Referenced name as written.
    pub name: String,
    /// What kind of reference this is; selects the resolver's binding rule.
    pub kind: RefKind,
    /// The selector's operand identifier, when this reference is the selected
    /// part of `operand.name` and the operand is a plain identifier
    /// (`auth` of `auth.Session`, `rp` of `rp.Add`). `None` means either that
    /// the reference has no selector (`helper()`, a bare type name) or that
    /// its operand is a computed expression (`w.Header().Add`,
    /// `arr[0].Close`) — the resolver distinguishes these by kind and scope.
    ///
    /// Recorded here because extraction owns the syntax tree; the resolver
    /// sees flat refs and must not re-derive selector relationships from byte
    /// adjacency (ADR-017 addendum). The operand occurrence itself is *not*
    /// emitted as a separate reference: it is scope structure, not a name use.
    pub qualifier: Option<String>,
    /// Source span.
    pub span: Span,
    /// Enclosing symbol's qualified name, when the reference sits inside one.
    pub container: Option<String>,
}

/// How an import was written; affects graph weight (LANGUAGES.md §6.2: dynamic
/// imports are weaker evidence than static ones).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportKind {
    /// A static import (Go `import`, Python `import`, TS `import ... from`).
    Import,
    /// A re-export (`export * from`, `__init__.py` re-export).
    ExportFrom,
    /// A CommonJS `require()`.
    Require,
    /// A dynamic `import()` or `importlib` call.
    Dynamic,
}

/// An import or export-from statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    /// The specifier exactly as written, quotes stripped but nothing
    /// unescaped (`./util`, `example.com/m/internal/auth`).
    pub raw: String,
    /// The local alias, when the import carries one. Go spells the special
    /// forms as written: `Some("." /* dot import */)` and
    /// `Some("_" /* blank import */)`; `None` is a plain named import.
    pub alias: Option<String>,
    /// Statement kind.
    pub kind: ImportKind,
    /// Source span.
    pub span: Span,
}

/// Everything extraction learned about one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedFile {
    /// The package clause's name (`package auth` → `Some("auth")`). The
    /// resolver scopes same-package name binding by this; a file without a
    /// package clause (empty or broken) yields `None`.
    pub package_name: Option<String>,
    /// Definitions, in source order.
    pub defs: Vec<Def>,
    /// References, in source order.
    pub refs: Vec<Ref>,
    /// Imports, in source order.
    pub imports: Vec<Import>,
    /// How completely the file was analyzed.
    pub status: ParseStatus,
}

impl ExtractedFile {
    /// An empty result for a file that was deliberately not parsed.
    #[must_use]
    pub const fn skipped(status: ParseStatus) -> Self {
        Self {
            package_name: None,
            defs: Vec::new(),
            refs: Vec::new(),
            imports: Vec::new(),
            status,
        }
    }
}

/// Extraction failures.
///
/// Malformed source is *not* an error — it yields [`ParseStatus::Partial`]. Only
/// conditions that make extraction impossible reach this type.
#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    /// The grammar could not be installed into a parser. This indicates an ABI
    /// mismatch between a pinned grammar and the `tree-sitter` runtime, which is
    /// a build defect rather than a user error.
    #[error("grammar for language '{lang}' is incompatible with this tree-sitter runtime")]
    IncompatibleGrammar {
        /// The language whose grammar failed to load.
        lang: &'static str,
    },
    /// tree-sitter returned no tree.
    #[error("tree-sitter produced no syntax tree for this file")]
    NoTree,
    /// The language is Tier 1 and has a pinned grammar, but its extraction
    /// adapter has not been implemented yet (TypeScript/JS and Python during
    /// the Go-first milestone, MASTER_PLAN.md §15 step 3).
    #[error("extraction adapter for language '{lang}' is not implemented yet")]
    NotImplemented {
        /// The language awaiting an adapter.
        lang: &'static str,
    },
}

/// The tree-sitter grammar for a language, as a runtime [`tree_sitter::Language`].
///
/// Returns `None` for languages without an adapter, which callers handle by
/// taking the heuristic path (LANGUAGES.md §7).
///
/// # Why the TypeScript family needs two grammars
///
/// The TypeScript and TSX grammars are not interchangeable, and neither is a
/// superset — this was measured, not assumed:
///
/// | Grammar | plain JS | JSX | `.ts` | angle-bracket assertion |
/// |---|---|---|---|---|
/// | `LANGUAGE_TYPESCRIPT` | ok | **error** | ok | ok |
/// | `LANGUAGE_TSX` | ok | ok | ok | **error** |
///
/// `LANGUAGE_TYPESCRIPT` cannot parse JSX at all, and `LANGUAGE_TSX` cannot
/// parse `<string>value` assertions (the `<` reads as JSX). Because the scanner
/// records `.tsx` and `.js`/`.jsx` as distinct [`Language`] values, the correct
/// grammar is chosen by extension — the same split LANGUAGES.md §6.2 describes.
///
/// A file whose *actual* content disagrees with its extension (a `.ts` file
/// containing JSX) is not guessed at: it parses partially and is labeled
/// [`ParseStatus::Partial`].
#[must_use]
pub fn grammar_for(language: Language) -> Option<tree_sitter::Language> {
    match language {
        Language::Go => Some(tree_sitter_go::LANGUAGE.into()),
        Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        // `.tsx` and `.jsx` both map here; both may contain JSX.
        Language::Tsx | Language::JavaScript => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Language::Unsupported | Language::Unknown => None,
    }
}

/// Parse `source` as `language` with the default per-file budget
/// ([`PARSE_TIMEOUT_MS`]) and report how complete the result is.
///
/// See [`parse_source_with_timeout`] for the budgeted form.
///
/// # Errors
///
/// Returns [`ExtractError::IncompatibleGrammar`] if the language has no adapter
/// or its pinned grammar cannot be installed, and [`ExtractError::NoTree`] if
/// tree-sitter yields no tree.
pub fn parse_source(
    source: &str,
    language: Language,
) -> Result<(tree_sitter::Tree, ParseStatus), ExtractError> {
    match parse_source_with_timeout(source, language, Duration::from_millis(PARSE_TIMEOUT_MS))? {
        Parsed::Tree(tree, status) => Ok((tree, status)),
        Parsed::TimedOut => Err(ExtractError::NoTree),
    }
}

/// The outcome of a budgeted parse.
#[derive(Debug)]
pub enum Parsed {
    /// A syntax tree plus its honest [`ParseStatus`].
    Tree(tree_sitter::Tree, ParseStatus),
    /// The parse exceeded its time budget and was aborted cooperatively; no
    /// tree exists and the caller must label the file, not retry.
    TimedOut,
}

/// Parse `source` as `language`, aborting cooperatively once `timeout` of
/// wall-clock time has elapsed (ARCHITECTURE §4.2, SECURITY.md §6).
///
/// The budget is enforced inside tree-sitter via the parse progress callback:
/// an adversarial-but-parse-capped input can never hang the indexer. An
/// aborted parse yields [`Parsed::TimedOut`] — a labeled degradation, never a
/// panic or a wait.
///
/// # Errors
///
/// Returns [`ExtractError::IncompatibleGrammar`] if the language has no adapter
/// or its pinned grammar cannot be installed, and [`ExtractError::NoTree`] if
/// tree-sitter yields no tree (distinct from [`Parsed::TimedOut`], which is a
/// budget outcome, not a failure).
pub fn parse_source_with_timeout(
    source: &str,
    language: Language,
    timeout: Duration,
) -> Result<Parsed, ExtractError> {
    let Some(grammar) = grammar_for(language) else {
        return Err(ExtractError::IncompatibleGrammar {
            lang: language.as_str(),
        });
    };
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|_| ExtractError::IncompatibleGrammar {
            lang: language.as_str(),
        })?;

    let bytes = source.as_bytes();
    let len = bytes.len();
    let started = Instant::now();
    // A budget already spent aborts before parsing: tree-sitter consults the
    // progress callback only periodically, so a small input can finish without
    // ever invoking it — the pre-check makes degenerate budgets deterministic.
    if started.elapsed() >= timeout {
        return Ok(Parsed::TimedOut);
    }
    let mut timed_out = false;
    let mut progress = |_state: &tree_sitter::ParseState| {
        if started.elapsed() >= timeout {
            timed_out = true;
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = tree_sitter::ParseOptions::new().progress_callback(&mut progress);
    let tree = parser.parse_with_options(
        &mut |i: usize, _| {
            if i < len {
                &bytes[i..]
            } else {
                &[][..]
            }
        },
        None,
        Some(options),
    );

    if timed_out {
        return Ok(Parsed::TimedOut);
    }
    let tree = tree.ok_or(ExtractError::NoTree)?;
    let status = if tree.root_node().has_error() {
        ParseStatus::Partial
    } else {
        ParseStatus::Ok
    };
    Ok(Parsed::Tree(tree, status))
}

/// The Go extraction adapter (MASTER_PLAN.md §15 step 3).
mod go;

/// Extract definitions, references and imports from `source`, under the
/// default per-file parse budget ([`PARSE_TIMEOUT_MS`]).
///
/// See [`extract_with_timeout`] for the budgeted form.
///
/// # Errors
///
/// - [`ExtractError::IncompatibleGrammar`] for languages without an adapter.
/// - [`ExtractError::NotImplemented`] for Tier 1 languages whose adapter has
///   not been built yet (TS/JS, Python during the Go-first milestone).
/// - [`ExtractError::NoTree`] if tree-sitter yields no tree at all.
pub fn extract(source: &str, language: Language) -> Result<ExtractedFile, ExtractError> {
    extract_with_timeout(source, language, Duration::from_millis(PARSE_TIMEOUT_MS))
}

/// Extract definitions, references and imports from `source`, aborting the
/// parse once `timeout` of wall-clock time has elapsed.
///
/// This is `cs-extract`'s main entry point: parse plus query-driven
/// extraction (ARCHITECTURE.md §4.2). The result is a pure function of
/// `(source, language, timeout)` — no paths, no clocks beyond the budget,
/// no randomness — which is what makes the determinism contract (ALGORITHM.md
/// §12) testable.
///
/// Malformed source is not an error: it yields
/// [`ParseStatus::Partial`] with whatever survived error recovery, under the
/// documented degradation policy (LANGUAGES.md §6.1): a definition survives
/// only if its declaration subtree contains no error node, and references
/// with an error node among their ancestors are dropped. A source that
/// outruns the budget yields [`ParseStatus::Timeout`] with no facts — also
/// not an error (SECURITY.md §6).
///
/// # Errors
///
/// - [`ExtractError::IncompatibleGrammar`] for languages without an adapter.
/// - [`ExtractError::NotImplemented`] for Tier 1 languages whose adapter has
///   not been built yet (TS/JS, Python during the Go-first milestone).
/// - [`ExtractError::NoTree`] if tree-sitter yields no tree at all.
pub fn extract_with_timeout(
    source: &str,
    language: Language,
    timeout: Duration,
) -> Result<ExtractedFile, ExtractError> {
    match language {
        Language::Go => go::extract_with_timeout(source, timeout),
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Python => {
            Err(ExtractError::NotImplemented {
                lang: language.as_str(),
            })
        }
        Language::Unsupported | Language::Unknown => Err(ExtractError::IncompatibleGrammar {
            lang: language.as_str(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The load-bearing ABI test: every pinned grammar must load and parse under
    /// the pinned runtime. If a grammar bump breaks the ABI, this fails in CI
    /// rather than at a user's first run (LANGUAGES.md §9).
    #[test]
    fn grammars_load_and_parse() {
        let cases = [
            (
                Language::Go,
                "package main\n\nfunc Login() error { return nil }\n",
            ),
            (Language::TypeScript, "export function login(): void {}\n"),
            (Language::Tsx, "export const A = () => <div>hi</div>;\n"),
            (Language::JavaScript, "export function login() {}\n"),
            (Language::Python, "def login() -> None:\n    pass\n"),
        ];
        for (language, source) in cases {
            let (tree, status) = parse_source(source, language)
                .unwrap_or_else(|e| panic!("{language:?} failed to parse: {e}"));
            assert_eq!(
                status,
                ParseStatus::Ok,
                "{language:?} reported errors on valid source"
            );
            assert!(
                tree.root_node().child_count() > 0,
                "{language:?} produced an empty tree"
            );
        }
    }

    #[test]
    fn jsx_parses_under_the_javascript_mapping() {
        let (_, status) = parse_source(
            "export const A = () => <div>hi</div>;\n",
            Language::JavaScript,
        )
        .expect("parse");
        assert_eq!(status, ParseStatus::Ok);
    }

    #[test]
    fn tsx_and_ts_use_different_grammars_because_neither_is_a_superset() {
        // `LANGUAGE_TYPESCRIPT` cannot parse JSX; `LANGUAGE_TSX` cannot parse
        // angle-bracket assertions. The extension decides, and this test pins
        // that choice so a future "simplification" to one grammar fails loudly.
        let jsx = "export const A = () => <div>hi</div>;\n";
        let (_, jsx_as_ts) = parse_source(jsx, Language::TypeScript).expect("parses, with errors");
        assert_eq!(
            jsx_as_ts,
            ParseStatus::Partial,
            "a .ts mapping must not silently claim JSX parsed cleanly"
        );
        let (_, jsx_as_tsx) = parse_source(jsx, Language::Tsx).expect("parse");
        assert_eq!(jsx_as_tsx, ParseStatus::Ok);

        let assertion = "const x = <string>someValue;\n";
        let (_, assertion_as_tsx) = parse_source(assertion, Language::Tsx).expect("parse");
        assert_eq!(
            assertion_as_tsx,
            ParseStatus::Partial,
            "TSX grammar must fail on angle-bracket assertions"
        );
        let (_, assertion_as_ts) = parse_source(assertion, Language::TypeScript).expect("parse");
        assert_eq!(assertion_as_ts, ParseStatus::Ok);
    }

    #[test]
    fn malformed_source_is_partial_not_an_error() {
        let (_, status) =
            parse_source("func ( { ] broken", Language::Go).expect("must still produce a tree");
        assert_eq!(status, ParseStatus::Partial);
    }

    #[test]
    fn zero_budget_parse_times_out_as_a_labeled_degradation() {
        // A zero budget aborts the parse at the first progress callback: the
        // outcome must be an honest Timeout with no facts, never a hang.
        let file = extract_with_timeout(
            "package p\n\nfunc F() { return 1 }\n",
            Language::Go,
            Duration::ZERO,
        )
        .expect("timeout is not an error");
        assert_eq!(file.status, ParseStatus::Timeout);
        assert!(file.defs.is_empty() && file.refs.is_empty() && file.imports.is_empty());
        assert!(file.package_name.is_none());

        // At the parse level: TimedOut, distinct from a no-tree failure.
        let parsed = parse_source_with_timeout("package p\n", Language::Go, Duration::ZERO)
            .expect("budget outcome");
        assert!(matches!(parsed, Parsed::TimedOut));

        // A real budget still parses cleanly (the guard never fires on
        // honest input).
        let (_, status) = parse_source("package p\n", Language::Go).expect("parse");
        assert_eq!(status, ParseStatus::Ok);
    }

    #[test]
    fn languages_without_adapters_have_no_grammar() {
        assert!(grammar_for(Language::Unknown).is_none());
        assert!(grammar_for(Language::Unsupported).is_none());
    }

    #[test]
    fn unknown_language_parse_is_an_error_naming_the_language() {
        let err = parse_source("x = 1", Language::Unknown).expect_err("must fail");
        assert!(err.to_string().contains("unknown"));
    }

    #[test]
    fn skipped_constructor_yields_empty_facts() {
        let file = ExtractedFile::skipped(ParseStatus::Skipped);
        assert!(file.defs.is_empty() && file.refs.is_empty() && file.imports.is_empty());
        assert_eq!(file.status, ParseStatus::Skipped);
        assert!(file.package_name.is_none());
    }

    #[test]
    fn extract_dispatches_and_reports_unimplemented_languages() {
        let ts = extract("export function f() {}", Language::TypeScript)
            .expect_err("TS adapter is not implemented yet");
        assert!(matches!(ts, ExtractError::NotImplemented { lang: "ts" }));
        let py = extract("def f(): pass", Language::Python)
            .expect_err("Python adapter is not implemented yet");
        assert!(matches!(
            py,
            ExtractError::NotImplemented { lang: "python" }
        ));
    }

    #[test]
    fn ref_kind_strings_are_stable() {
        // Goldens and the future refs table depend on these spellings.
        assert_eq!(RefKind::NameRef.as_str(), "name_ref");
        assert_eq!(RefKind::FieldRef.as_str(), "field_ref");
        assert_eq!(RefKind::TypeRef.as_str(), "type_ref");
        let json = serde_json::to_string(&RefKind::TypeRef).expect("serialize");
        assert_eq!(json, r#""type_ref""#);
    }

    #[test]
    fn def_kind_strings_are_stable() {
        assert_eq!(DefKind::Function.as_str(), "func");
        assert_eq!(DefKind::Interface.as_str(), "interface");
    }
}
