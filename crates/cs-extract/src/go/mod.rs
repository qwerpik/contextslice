//! Go extraction: definitions, references and imports from a parsed file.
//!
//! This is the reference adapter (LANGUAGES.md §2.1) — the first
//! implementation of the extraction contract, against which the TypeScript
//! and Python adapters will be shaped. It implements MASTER_PLAN.md §15
//! step 3.
//!
//! # How it works
//!
//! 1. [`parse_source`](crate::parse_source) produces the tree and an honest
//!    [`ParseStatus`].
//! 2. Three owned `.scm` queries (see `queries/`, ADR-012) capture
//!    definition nodes + names + doc comments, raw name/field/type
//!    references, and import specs.
//! 3. Rust post-processing applies the rules the query language cannot
//!    express: declaration-position filtering for references, doc-comment
//!    adjacency and first-paragraph extraction, signature construction,
//!    receiver-derived containers, and the error-recovery degradation policy.
//!
//! # Determinism
//!
//! Output is a pure function of `source`: query captures are re-sorted by
//! span, containers are assigned by a lookup over sorted defs, and no
//! hash-map iteration order reaches the output (ALGORITHM.md §12).
//!
//! # Degradation policy (LANGUAGES.md §6.1)
//!
//! A definition survives a partially-broken file only if its declaration
//! subtree contains no error node. References survive only if no ancestor is
//! an error node. Everything inside an error region is dropped rather than
//! guessed at, and the file is labeled [`ParseStatus::Partial`].

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;

use crate::{
    grammar_for, parse_source_with_timeout, Def, DefKind, ExtractError, ExtractedFile, Import,
    ImportKind, ParseStatus, Parsed, Ref, RefKind, Span,
};
use cs_scanner::Language;
use tree_sitter::{Node, Query, QueryCursor, StreamingIterator};

mod queries {
    //! Query sources, compiled once per process against the pinned grammar.
    pub const DEFS: &str = include_str!("queries/defs.scm");
    pub const REFS: &str = include_str!("queries/refs.scm");
    pub const IMPORTS: &str = include_str!("queries/imports.scm");
}

/// Compile (once per process) one of the query sources against the pinned
/// Go grammar. A query that does not compile is a build defect, not a
/// runtime condition — hence the panic with the label in the message.
fn defs_query() -> &'static Query {
    static Q: OnceLock<Query> = OnceLock::new();
    Q.get_or_init(|| {
        let grammar = grammar_for(Language::Go).expect("Go grammar is pinned and loadable");
        Query::new(&grammar, queries::DEFS)
            .unwrap_or_else(|e| panic!("defs.scm must compile against pinned tree-sitter-go: {e}"))
    })
}

fn refs_query() -> &'static Query {
    static Q: OnceLock<Query> = OnceLock::new();
    Q.get_or_init(|| {
        let grammar = grammar_for(Language::Go).expect("Go grammar is pinned and loadable");
        Query::new(&grammar, queries::REFS)
            .unwrap_or_else(|e| panic!("refs.scm must compile against pinned tree-sitter-go: {e}"))
    })
}

fn imports_query() -> &'static Query {
    static Q: OnceLock<Query> = OnceLock::new();
    Q.get_or_init(|| {
        let grammar = grammar_for(Language::Go).expect("Go grammar is pinned and loadable");
        Query::new(&grammar, queries::IMPORTS).unwrap_or_else(|e| {
            panic!("imports.scm must compile against pinned tree-sitter-go: {e}")
        })
    })
}

/// Go's predeclared identifiers: universe types, builtins and constants,
/// sorted so [`is_universe`]'s binary search is sound.
///
/// (A previous version grouped the names into three internally sorted blocks
/// and binary-searched the concatenation; every builtin after the first
/// block's maximum silently failed to match and leaked into refs. The
/// sortedness is now pinned by a unit test.)
///
/// References to these can never bind to a repository definition — they are
/// universe-scoped, and package-level shadowing of them is pathological Go.
/// Keeping them would fill the refs table with zero-signal rows (`int`
/// alone would dominate any real corpus), so the Go adapter drops them at
/// extraction. This is a language-level *binding* fact, documented here and
/// in LANGUAGES.md §6.1 rather than hidden in the resolver.
const UNIVERSE_NAMES: &[&str] = &[
    "any",
    "append",
    "bool",
    "byte",
    "cap",
    "clear",
    "close",
    "comparable",
    "complex",
    "complex128",
    "complex64",
    "copy",
    "delete",
    "error",
    "false",
    "float32",
    "float64",
    "imag",
    "int",
    "int16",
    "int32",
    "int64",
    "int8",
    "iota",
    "len",
    "make",
    "max",
    "min",
    "new",
    "nil",
    "panic",
    "print",
    "println",
    "real",
    "recover",
    "rune",
    "string",
    "true",
    "uint",
    "uint16",
    "uint32",
    "uint64",
    "uint8",
    "uintptr",
];

fn is_universe(name: &str) -> bool {
    UNIVERSE_NAMES.binary_search(&name).is_ok()
}

/// Extract all facts from one Go source file.
///
/// See the [module docs](self) for the degradation and determinism rules.
///
/// # Errors
///
/// Only infrastructure failures ([`ExtractError::IncompatibleGrammar`],
/// [`ExtractError::NoTree`]); malformed Go is reported via
/// [`ParseStatus::Partial`], never as an error.
pub(super) fn extract_with_timeout(
    source: &str,
    timeout: Duration,
) -> Result<ExtractedFile, ExtractError> {
    // The per-file budget is enforced inside the parse (SECURITY.md §6); a
    // file that trips it is a labeled degradation with no facts, not a crash.
    let parsed = parse_source_with_timeout(source, Language::Go, timeout)?;
    let (tree, status) = match parsed {
        Parsed::Tree(tree, status) => (tree, status),
        Parsed::TimedOut => return Ok(ExtractedFile::skipped(ParseStatus::Timeout)),
    };
    let root = tree.root_node();
    let (mut defs, dropped_spans) = collect_defs(root, source);
    defs.sort_by(|a, b| {
        a.span
            .start_byte
            .cmp(&b.span.start_byte)
            .then(a.span.end_byte.cmp(&b.span.end_byte))
            .then(a.name.cmp(&b.name))
    });
    let mut imports = collect_imports(root, source);
    imports.sort_by(|a, b| {
        a.span
            .start_byte
            .cmp(&b.span.start_byte)
            .then(a.raw.cmp(&b.raw))
    });
    let refs = collect_refs(root, source, &defs, &dropped_spans, status);
    Ok(ExtractedFile {
        package_name: package_name(root, source),
        defs,
        imports,
        refs,
        status,
    })
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

/// Span from a node. The `as u32` casts cannot truncate: extraction only
/// runs on files at or below the 1 MiB parse cap (`crate::PARSE_SIZE_CAP`),
/// which bounds both byte offsets and line numbers far below `u32::MAX`.
#[allow(clippy::cast_possible_truncation)]
fn span_of(node: Node) -> Span {
    Span {
        start_line: node.start_position().row as u32 + 1,
        end_line: node.end_position().row as u32 + 1,
        start_byte: node.start_byte() as u32,
        end_byte: node.end_byte() as u32,
    }
}

/// Go export rule: the first rune is upper case, Unicode-aware (a `Ünicode`
/// function is exported; an ASCII-only check would say otherwise).
fn is_exported(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

/// Collapse all whitespace runs to single spaces. Signatures are one-liners
/// by contract (ARCHITECTURE.md §4.2); this makes them stable regardless of
/// how the source was formatted.
fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The file's package name, from the (single) package clause. Go files
/// without one are broken or empty; `None` says so honestly.
fn package_name(root: Node, source: &str) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "package_clause" {
            let name = child
                .child_by_field_name("name")
                .or_else(|| child.named_child(0));
            return name.map(|n| node_text(n, source).to_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------

/// Merge two doc-comment candidates for one spec node, preferring the one
/// adjacent to the declaration (directive lines in between allowed) — the
/// per-spec comment beats the enclosing group's comment, which the query
/// patterns deliver separately but only the former is the doc Go recognizes.
fn better_doc<'t>(
    source: &str,
    decl: Node<'t>,
    entry_doc: Option<Node<'t>>,
    new_doc: Option<Node<'t>>,
) -> Option<Node<'t>> {
    let adjacent = |doc: Node<'t>| {
        decl.start_position().row > doc.end_position().row
            && only_directive_lines(source, doc.end_byte(), decl.start_byte())
    };
    match (entry_doc, new_doc) {
        (None, new) => new,
        (old, Some(new)) if adjacent(new) => {
            if old.is_some_and(adjacent) {
                old // both adjacent (impossible per query shapes) — first wins
            } else {
                Some(new)
            }
        }
        (old, _) => old,
    }
}

/// One raw def match, before deduplication by node identity.
struct RawDef<'t> {
    node: Node<'t>,
    names: Vec<String>,
    doc_node: Option<Node<'t>>,
}

/// One raw ref capture, before filtering, with the selector qualifier
/// already read from the tree (ADR-017 addendum: extraction owns the
/// syntax; the resolver gets flat refs and must not reconstruct
/// selector relationships from byte adjacency).
struct RawRef<'t> {
    kind: RefKind,
    node: Node<'t>,
    qualifier: Option<String>,
}

/// Definitions plus the byte spans of declarations that were *dropped* by
/// the degradation policy (their subtree contained an error node). Refs
/// inside a dropped declaration are dropped too: the container def does not
/// exist and the region is suspect, whether or not tree-sitter wrapped it
/// in an explicit `ERROR` node.
fn collect_defs(root: Node, source: &str) -> (Vec<Def>, Vec<(usize, usize)>) {
    let query = defs_query();
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();

    let mut raw: Vec<RawDef> = Vec::new();
    // Node id → index into `raw`. A node id has at most one raw entry (any
    // later match over the same node merges into the first below), so one
    // map serves both merge shapes; the name check happens at the target.
    // This keeps the merges O(1) — a linear `raw` rescan per match measured
    // quadratic in def count (11.5 s for a 1 MiB, ~8.9k-def file).
    let mut raw_index: HashMap<usize, usize> = HashMap::new();

    let mut matches = cursor.matches(query, root, source.as_bytes());
    while let Some(m) = matches.next() {
        let mut node = None;
        let mut names: Vec<String> = Vec::new();
        let mut doc_node = None;
        for c in m.captures() {
            match capture_names[c.index as usize] {
                "def.node" => node = Some(c.node),
                "def.name" => names.push(node_text(c.node, source).to_owned()),
                "def.doc" => doc_node = Some(c.node),
                _ => {}
            }
        }
        let Some(node) = node else { continue };
        if names.is_empty() {
            continue;
        }
        // Multi-name specs (`var X, Y int`) yield one match per name over
        // the same spec node: merge by (node, name). A later match may carry
        // a *better* doc: the group-comment patterns and the per-spec
        // comment patterns can both hit the same spec, and only the
        // spec-adjacent one is the doc Go recognizes — a group comment above
        // `const (` is separated from every inner spec by the paren line.
        // Prefer an adjacent doc (directive lines in between allowed);
        // keep whatever arrives otherwise.
        match raw_index.get(&node.id()).copied() {
            // Same name again (one match per name over a multi-name spec):
            // only the doc can improve.
            Some(idx) if raw[idx].names.iter().any(|n| n == &names[0]) => {
                let entry = &mut raw[idx];
                entry.doc_node = better_doc(source, node, entry.doc_node, doc_node);
            }
            // A genuinely new name on a seen node: merge it in.
            Some(idx) => {
                let entry = &mut raw[idx];
                entry.names.extend(names);
                entry.doc_node = better_doc(source, node, entry.doc_node, doc_node);
            }
            None => {
                raw_index.insert(node.id(), raw.len());
                raw.push(RawDef {
                    node,
                    names,
                    doc_node,
                });
            }
        }
    }

    let mut dropped: Vec<(usize, usize)> = Vec::new();
    for r in &raw {
        if r.node.has_error() {
            dropped.push((r.node.start_byte(), r.node.end_byte()));
        }
    }

    let defs = raw
        .into_iter()
        .filter(|r| !r.node.has_error()) // degradation policy: clean subtrees only
        .flat_map(|r| {
            let kind = def_kind(r.node);
            let container = if r.node.kind() == "method_declaration" {
                receiver_type_name(r.node, source)
            } else {
                None
            };
            let signature = signature(r.node, source);
            let doc = r.doc_node.and_then(|d| doc_comment(d, r.node, source));
            r.names
                .into_iter()
                // The blank identifier never names anything addressable
                // (`const _ = iota` is a skip placeholder); it would only
                // add noise to the symbols table. Its occurrence stays a
                // declaration position, so it never becomes a ref either.
                .filter(|name| name != "_")
                .map(move |name| {
                    let qual_name = match &container {
                        Some(c) => format!("{c}.{name}"),
                        None => name.clone(),
                    };
                    Def {
                        exported: is_exported(&name),
                        name,
                        qual_name,
                        kind,
                        span: span_of(r.node),
                        container: container.clone(),
                        signature: signature.clone(),
                        doc: doc.clone(),
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect();
    (defs, dropped)
}

/// Canonical kind for a declaration node (LANGUAGES.md §6.1): struct and
/// interface type specs get their own kinds; everything named through
/// `type` that is neither is a plain type; aliases are types.
fn def_kind(node: Node) -> DefKind {
    match node.kind() {
        "function_declaration" => DefKind::Function,
        "method_declaration" => DefKind::Method,
        "const_spec" => DefKind::Const,
        "var_spec" => DefKind::Var,
        "type_alias" => DefKind::Type,
        "type_spec" => match node.child_by_field_name("type").map(|t| t.kind()) {
            Some("struct_type") => DefKind::Struct,
            Some("interface_type") => DefKind::Interface,
            _ => DefKind::Type,
        },
        other => {
            // The queries only capture the six node kinds above; anything
            // else means the query set and this match drifted apart.
            //
            // No source text in the message: panic output is a log (SECURITY
            // §7 — logs never print file contents).
            debug_assert!(
                false,
                "unexpected def node kind '{other}' — update def_kind"
            );
            DefKind::Type
        }
    }
}

/// Base type name of a method receiver: `*Session` → `Session`,
/// `Pair[T]` → `Pair`. The container is the file-local qual-name prefix
/// (`Session.Validate`); whether the receiver type lives in this file is the
/// resolver's concern, not extraction's.
fn receiver_type_name(method: Node, source: &str) -> Option<String> {
    let receiver = method.child_by_field_name("receiver")?;
    let mut cursor = receiver.walk();
    let param = receiver
        .named_children(&mut cursor)
        .find(|c| c.kind() == "parameter_declaration")?;
    let mut ty = param.child_by_field_name("type")?;
    if ty.kind() == "pointer_type" {
        ty = ty.named_child(0)?;
    }
    if ty.kind() == "generic_type" {
        ty = ty
            .child_by_field_name("name")
            .or_else(|| ty.named_child(0))?;
    }
    if ty.kind() == "type_identifier" || ty.kind() == "identifier" {
        return Some(node_text(ty, source).to_owned());
    }
    None
}

/// One-line signature for a declaration (ARCHITECTURE.md §4.2).
///
/// Rules, each pinned by fixtures:
/// - functions/methods: the declaration header up to (not including) the
///   body, whitespace-normalized — receivers and type parameters included.
/// - struct/interface: `type Name[T any] struct { … }` with a literal
///   ellipsis; the body is rendered from spans at L3, not stored here.
/// - named types and aliases: the full spec, normalized (`type Celsius
///   float64`, `type Point = [2]float64`).
/// - const/var: the spec with every func-literal *body* replaced by `…`, so
///   `var Handler = func(…) error {…}` keeps its type shape.
fn signature(node: Node, source: &str) -> Option<String> {
    match node.kind() {
        "function_declaration" | "method_declaration" => {
            let end = node
                .child_by_field_name("body")
                .map_or_else(|| node.end_byte(), |b| b.start_byte());
            Some(normalize_ws(&source[node.start_byte()..end]))
        }
        "type_spec" => {
            let name = node_text(node.child_by_field_name("name")?, source);
            let type_params = node
                .child_by_field_name("type_parameters")
                .map(|tp| normalize_ws(node_text(tp, source)))
                .unwrap_or_default();
            match node.child_by_field_name("type")?.kind() {
                "struct_type" => Some(format!("type {name}{type_params} struct {{ … }}")),
                "interface_type" => Some(format!("type {name}{type_params} interface {{ … }}")),
                _ => Some(format!("type {}", normalize_ws(node_text(node, source)))),
            }
        }
        "type_alias" => Some(format!("type {}", normalize_ws(node_text(node, source)))),
        "const_spec" => Some(format!("const {}", spec_text_elliding_bodies(node, source))),
        "var_spec" => Some(format!("var {}", spec_text_elliding_bodies(node, source))),
        _ => None,
    }
}

/// Spec text with each func-literal body replaced by `…` (sorted, spliced).
fn spec_text_elliding_bodies(node: Node, source: &str) -> String {
    let mut bodies: Vec<(usize, usize)> = Vec::new();
    visit_descendants(node, &mut |n| {
        if n.kind() == "func_literal" {
            if let Some(body) = n.child_by_field_name("body") {
                bodies.push((body.start_byte(), body.end_byte()));
            }
        }
    });
    bodies.sort_unstable();
    let mut out = String::new();
    let mut pos = node.start_byte();
    for (start, end) in bodies {
        if start >= pos {
            out.push_str(&source[pos..start]);
            out.push('…');
            pos = end;
        }
    }
    out.push_str(&source[pos..node.end_byte()]);
    normalize_ws(&out)
}

/// The doc comment for a declaration, or `None`.
///
/// Go's rule, applied exactly: a contiguous comment block *immediately*
/// above the declaration (no blank line — checked on line numbers, because
/// blank lines leave no trace in the tree). Only the first paragraph is
/// kept, per the `Def.doc` contract. Tree adjacency alone is not enough,
/// which is why the query's `.` anchor is only a candidate filter and the
/// authoritative check lives here.
///
/// `//go:`-style compiler directives are never docs: they are filtered out
/// of the assembled block, and a directive line sitting between the comment
/// block and the declaration (gofmt's preferred position) does not break
/// adjacency.
fn doc_comment(adjacent: Node, decl: Node, source: &str) -> Option<String> {
    // The declaration must start on the line right after the comment block
    // ends — allowing only directive lines in between. `adjacent` is the
    // query-adjacent comment; walk left through line-contiguous comments to
    // assemble the whole block.
    if decl.start_position().row <= adjacent.end_position().row
        || !only_directive_lines(source, adjacent.end_byte(), decl.start_byte())
    {
        return None;
    }
    let mut block = vec![adjacent];
    let mut current = adjacent;
    while let Some(prev) = current.prev_named_sibling() {
        if prev.kind() == "comment" && prev.end_position().row + 1 == current.start_position().row {
            block.push(prev);
            current = prev;
        } else {
            break;
        }
    }
    block.reverse();
    block.retain(|comment| !is_compiler_directive(*comment, source));
    let Some(last) = block.last() else {
        return None; // nothing but directives: no doc
    };
    if !only_directive_lines(source, last.end_byte(), decl.start_byte()) {
        return None;
    }

    // First paragraph: skip blank lines the comment opens with (common in
    // /* */ blocks), then keep lines until the next blank line.
    let mut lines: Vec<String> = Vec::new();
    for comment in &block {
        lines.extend(comment_lines(*comment, source));
    }
    let paragraph: Vec<&str> = lines
        .iter()
        .skip_while(|l| l.trim().is_empty())
        .take_while(|l| !l.trim().is_empty())
        .map(|l| l.trim_end())
        .collect();
    if paragraph.is_empty() {
        return None;
    }
    Some(paragraph.join("\n"))
}

/// Whether a comment is a `//go:` compiler directive (`//go:noinline`,
/// `//go:generate`, …): toolchain input, never a doc comment.
fn is_compiler_directive(comment: Node, source: &str) -> bool {
    node_text(comment, source).trim_start().starts_with("//go:")
}

/// Whether the lines strictly between two nodes — everything after the end
/// of `above`'s line and before the line `below` starts on — are all
/// `//go:` directives. Callers pass the nodes' byte spans; the gap between
/// them decomposes into the remainder of the upper node's own line, the
/// in-between lines (each `\n`-terminated), and `below`'s leading
/// indentation — so checking every newline-delimited segment except the
/// first and the last is exactly that row-range check, at a cost
/// proportional to the gap. (Counting `source.lines()` here instead scanned
/// the whole file per documented declaration: quadratic extraction, ~11.6 s
/// for a 1 MiB, ~8.9k-def file.) An empty in-between range is trivially
/// true; a gap that is not a substring of the source is treated as broken
/// adjacency.
fn only_directive_lines(source: &str, above_end_byte: usize, below_start_byte: usize) -> bool {
    let Some(gap) = source.get(above_end_byte..below_start_byte) else {
        return false;
    };
    let mut segments = gap.split('\n').peekable();
    segments.next(); // remainder of the upper node's own line: not a line "between"
    while let Some(segment) = segments.next() {
        if segments.peek().is_some() && !segment.trim_start().starts_with("//go:") {
            return false;
        }
    }
    true
}

/// The text lines of one comment node with markers stripped: `//`, `/* */`,
/// and the leading `* ` of block-comment continuation lines.
fn comment_lines(comment: Node, source: &str) -> Vec<String> {
    let text = node_text(comment, source);
    if let Some(inner) = text.strip_prefix("/*") {
        let inner = inner.strip_suffix("*/").unwrap_or(inner);
        return inner
            .lines()
            .map(|l| {
                l.trim_start()
                    .strip_prefix("* ")
                    .map_or_else(|| l.trim_start().to_owned(), str::to_owned)
            })
            .collect();
    }
    text.lines()
        .map(|l| {
            l.strip_prefix("//").map_or_else(
                || l.to_owned(),
                |rest| rest.strip_prefix(' ').unwrap_or(rest).to_owned(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

fn collect_imports(root: Node, source: &str) -> Vec<Import> {
    let query = imports_query();
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();

    // (node id, import, alias already seen); the bare pattern matches every
    // spec, the alias patterns match again — merge by node identity.
    let mut merged: Vec<(usize, Import, bool)> = Vec::new();
    let mut matches = cursor.matches(query, root, source.as_bytes());
    while let Some(m) = matches.next() {
        let mut node = None;
        let mut path = None;
        let mut alias = None;
        for c in m.captures() {
            match capture_names[c.index as usize] {
                "import.node" => node = Some(c.node),
                "import.path" => path = Some(node_text(c.node, source)),
                "import.alias" => alias = Some(node_text(c.node, source)),
                _ => {}
            }
        }
        let (Some(node), Some(path)) = (node, path) else {
            continue;
        };
        // Strip the surrounding quotes — interpreted (`"…"`) or raw
        // (`…` backticks; legal for import paths per the Go spec). Go
        // import paths forbid escapes, so the quoted content is the
        // specifier, verbatim.
        let raw = path.trim_matches(|c| c == '"' || c == '`').to_owned();
        match merged.iter_mut().find(|(id, _, _)| *id == node.id()) {
            Some((_, import, has_alias)) => {
                if let Some(alias) = alias.filter(|_| !*has_alias) {
                    import.alias = Some(alias.to_owned());
                    *has_alias = true;
                }
            }
            None => merged.push((
                node.id(),
                Import {
                    raw,
                    alias: alias.map(str::to_owned),
                    kind: ImportKind::Import,
                    span: span_of(node),
                },
                alias.is_some(),
            )),
        }
    }
    merged.into_iter().map(|(_, import, _)| import).collect()
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

fn collect_refs(
    root: Node,
    source: &str,
    defs: &[Def],
    dropped_spans: &[(usize, usize)],
    status: ParseStatus,
) -> Vec<Ref> {
    let query = refs_query();
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let declared = declaration_spans(root, source);

    // Pass 1: gather captures; derive qualifiers from selector/qualified_type
    // parents and record operand spans so the operand identifier is never
    // emitted as an independent reference.
    let mut raw: Vec<RawRef> = Vec::new();
    let mut qualifier_spans: HashSet<(usize, usize)> = HashSet::new();

    let mut matches = cursor.matches(query, root, source.as_bytes());
    while let Some(m) = matches.next() {
        for c in m.captures() {
            let kind = match capture_names[c.index as usize] {
                "ref.name" => RefKind::NameRef,
                "ref.field" => RefKind::FieldRef,
                "ref.type" => RefKind::TypeRef,
                _ => continue,
            };
            let node = c.node;
            match kind {
                RefKind::NameRef => raw.push(RawRef {
                    kind,
                    node,
                    qualifier: None,
                }),
                // field_identifier also spells method names, struct fields
                // and interface method elements; only selector fields are
                // references.
                RefKind::FieldRef => {
                    let Some(selector) =
                        node.parent().filter(|p| p.kind() == "selector_expression")
                    else {
                        continue;
                    };
                    let qualifier =
                        selector_operand_qualifier(selector, source, &mut qualifier_spans);
                    // Call position (`x.Foo()`, `pkg.Foo()`, `pkg.F[T]()`)
                    // upgrades to CallRef — the resolver binds calls and
                    // field accesses under different rules (ADR-018).
                    let kind = if in_call_position(selector) {
                        RefKind::CallRef
                    } else {
                        kind
                    };
                    raw.push(RawRef {
                        kind,
                        node,
                        qualifier,
                    });
                }
                // Type usage: a qualified type carries its package as the
                // structural qualifier (`io.Closer`); the `package` child is
                // a `package_identifier`, which no reference pattern
                // captures, so nothing needs suppressing here.
                RefKind::TypeRef => {
                    let qualifier = node
                        .parent()
                        .filter(|p| p.kind() == "qualified_type")
                        .and_then(|qt| qt.child_by_field_name("package"))
                        .map(|p| node_text(p, source).to_owned());
                    raw.push(RawRef {
                        kind,
                        node,
                        qualifier,
                    });
                }
                // CallRef is only produced by the FieldRef arm, never by a
                // capture; the arm exists for exhaustiveness.
                RefKind::CallRef => {}
            }
        }
    }

    // Pass 2: the degradation and declaration filters, in source order.
    raw.sort_by_key(|r| (r.node.start_byte(), r.node.end_byte()));
    let mut refs: Vec<Ref> = Vec::new();
    for r in raw {
        let node = r.node;
        let name = node_text(node, source);

        // Degradation policy: nothing from inside an error region, and
        // nothing from inside a declaration the policy dropped.
        if status == ParseStatus::Partial && has_error_ancestor(node) {
            continue;
        }
        if dropped_spans
            .iter()
            .any(|(start, end)| *start <= node.start_byte() && node.end_byte() <= *end)
        {
            continue;
        }
        match r.kind {
            // Declaration positions are not references; selector operands are
            // scope structure, not name uses; universe names can never bind.
            RefKind::NameRef => {
                if name == "_"
                    || is_universe(name)
                    || declared.contains(&(node.start_byte(), node.end_byte()))
                    || qualifier_spans.contains(&(node.start_byte(), node.end_byte()))
                {
                    continue;
                }
            }
            // The `name` field of a type_spec/type_alias is the definition's
            // own name — but only that position: the TARGET of an alias or
            // named type (`type ID = UUID`) is a real reference.
            RefKind::TypeRef => {
                let in_name_position = node.parent().is_some_and(|p| {
                    matches!(p.kind(), "type_spec" | "type_alias")
                        && p.child_by_field_name("name").map(|n| n.id()) == Some(node.id())
                });
                if is_universe(name) || in_name_position {
                    continue;
                }
            }
            RefKind::FieldRef | RefKind::CallRef => {}
        }

        refs.push(Ref {
            name: name.to_owned(),
            kind: r.kind,
            qualifier: r.qualifier,
            span: span_of(node),
            container: container_for(node, defs),
        });
    }

    refs.sort_by(|a, b| {
        a.span
            .start_byte
            .cmp(&b.span.start_byte)
            .then(a.span.end_byte.cmp(&b.span.end_byte))
            .then(a.name.cmp(&b.name))
    });
    refs
}

/// The qualifier of a selector reference: the operand's text when the operand
/// is a plain identifier (`auth.Session`, `rp.Add`), `None` for computed
/// operands (`w.Header().Add`, `arr[0].Close`). The operand occurrence is
/// scope structure, not a name use — its span is recorded so the
/// `(identifier) @ref.name` capture for it is suppressed (which also removes
/// the "local shadows a package-level name" false-positive class at the
/// source).
fn selector_operand_qualifier(
    selector: Node,
    source: &str,
    spans: &mut HashSet<(usize, usize)>,
) -> Option<String> {
    let operand = selector.child_by_field_name("operand")?;
    if operand.kind() != "identifier" {
        return None;
    }
    spans.insert((operand.start_byte(), operand.end_byte()));
    Some(node_text(operand, source).to_owned())
}

/// Whether a selector is the callee of a call, looking through generic
/// instantiation brackets: `lib.Factory[int]()` instantiates through an
/// `index_expression`, so the selector's immediate parent is not the call.
fn in_call_position(selector: Node) -> bool {
    let mut probe = selector;
    while let Some(parent) = probe.parent() {
        match parent.kind() {
            "index_expression" => probe = parent,
            "call_expression" => return parent.child_by_field_name("function") == Some(probe),
            _ => return false,
        }
    }
    false
}

/// Spans of identifiers in declaration position, collected by one tree walk:
///
/// - the name of every top-level function declaration;
/// - parameter and receiver names (`parameter_declaration` children);
/// - the leading identifier run of every var/const spec (top-level or local:
///   `A, B = f()` declares A and B, references f);
/// - type-parameter names (`[T any]`);
/// - the left side of `:=` (short var declarations, if/init statements);
/// - the left side of `range` clauses, when the `:=` form is used;
/// - the bound variable of a type switch (`switch v := x.(type)`).
///
/// The last three need the source text (to distinguish `:=` from `=`), which
/// is exactly what queries cannot do — hence this walk.
fn declaration_spans(root: Node, source: &str) -> HashSet<(usize, usize)> {
    /// Identifiers in declaration position directly under `node` — or `node`
    /// itself when it is one: parameter names, receiver names, the bound
    /// variable of a type switch's `alias` list, the `expression_list` left
    /// sides of `:=` forms.
    fn collect_direct_identifiers(node: Node, spans: &mut HashSet<(usize, usize)>) {
        if node.kind() == "identifier" {
            spans.insert((node.start_byte(), node.end_byte()));
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "identifier" {
                spans.insert((child.start_byte(), child.end_byte()));
            }
        }
    }

    /// The `left` field of a `:=` declaration (`short_var_declaration`,
    /// `:=` range clauses). Only the left side declares: tree-sitter-go
    /// spells BOTH sides of a short var declaration as `expression_list`
    /// fields, and collecting every `expression_list` child silently
    /// declared the right side too — dropping every bare identifier
    /// assigned from (`y := x` lost the reference to `x`).
    fn collect_left_side_identifiers(node: Node, spans: &mut HashSet<(usize, usize)>) {
        if let Some(left) = node.child_by_field_name("left") {
            collect_direct_identifiers(left, spans);
        }
    }

    let mut spans = HashSet::new();
    visit_descendants(root, &mut |node| match node.kind() {
        "function_declaration"
        | "parameter_declaration"
        | "variadic_parameter_declaration"
        | "type_parameter_declaration" => collect_direct_identifiers(node, &mut spans),
        "var_spec" | "const_spec" => {
            // Leading identifier run: names come first, then a type or `=`;
            // the grammar guarantees the split point.
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "identifier" {
                    spans.insert((child.start_byte(), child.end_byte()));
                } else {
                    break;
                }
            }
        }
        "type_switch_statement" => {
            // The bound variable lives under the `alias` field:
            // `switch v := x.(type)` → alias: (expression_list (identifier)),
            // value: (identifier) — the value side is a real reference.
            if let Some(alias) = node.child_by_field_name("alias") {
                collect_direct_identifiers(alias, &mut spans);
            }
        }
        "keyed_element" => {
            // Identifier keys of struct-shaped composite literals are
            // FIELD-NAME positions (`RouteInfo{Handler: x}`), not
            // references. Map literals keep their keys: their type is a
            // map_type at any nesting depth (`[]map[string]T{{k: v}}`).
            // Named map types (`type H map[…]`) are the documented loss.
            // Measured as the largest FP class in the gin audit (ADR-018).
            // Walk up through the literal nesting (elements of typed slice
            // literals add a level) to the owning composite_literal.
            let mut cursor = node.parent();
            let mut literal_type = None;
            while let Some(current) = cursor.filter(|c| {
                matches!(
                    c.kind(),
                    "literal_value" | "keyed_element" | "literal_element" | "composite_literal"
                )
            }) {
                if current.kind() == "composite_literal" {
                    literal_type = current.child_by_field_name("type");
                    break;
                }
                cursor = current.parent();
            }
            if literal_type.is_some_and(is_struct_shaped) {
                if let Some(key) = node.child_by_field_name("key") {
                    collect_direct_identifiers(key, &mut spans);
                }
            }
        }
        "short_var_declaration" => collect_left_side_identifiers(node, &mut spans),
        "range_clause" if has_assign_define_token(node, source) => {
            collect_left_side_identifiers(node, &mut spans);
        }
        _ => {}
    });
    spans
}

/// Whether a composite-literal type child makes the literal's keys field
/// names: struct types (named, qualified, generic instantiation, or
/// anonymous), recursing through slice/array/pointer wrappers. Map types
/// keep identifier keys as real references.
fn is_struct_shaped(ty: Node) -> bool {
    match ty.kind() {
        "type_identifier" | "qualified_type" | "struct_type" | "generic_type" => true,
        "slice_type" | "array_type" | "pointer_type" => {
            ty.named_child(0).is_some_and(is_struct_shaped)
        }
        _ => false,
    }
}

/// Whether a range clause uses `:=` (declaration) rather than `=` (plain
/// assignment to an existing variable).
fn has_assign_define_token(node: Node, source: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() && node_text(child, source) == ":=" {
            return true;
        }
    }
    false
}

fn has_error_ancestor(node: Node) -> bool {
    let mut current = Some(node);
    while let Some(n) = current {
        if n.is_error() {
            return true;
        }
        current = n.parent();
    }
    false
}

/// The enclosing def's qual-name. `defs` is sorted by
/// `(start_byte, end_byte, name)` and top-level def spans are disjoint
/// (multi-name specs share one span and produce sibling defs), so exactly
/// one candidate can contain the node: the last def starting at or before
/// it — found by binary search over the sorted spans — provided it has not
/// ended before the node does. No backward scan past a non-containing
/// candidate: if that one does not enclose the node, no def does.
#[allow(clippy::cast_possible_truncation)] // bounded by PARSE_SIZE_CAP, see span_of
fn container_for(node: Node, defs: &[Def]) -> Option<String> {
    let start = node.start_byte() as u32;
    let end = node.end_byte() as u32;
    let idx = defs.partition_point(|d| d.span.start_byte <= start);
    let candidate = defs.get(idx.checked_sub(1)?)?;
    (candidate.span.end_byte >= end).then(|| candidate.qual_name.clone())
}

/// Pre-order visit of every node in the subtree. Iterative, not recursive:
/// adversarial-but-valid nesting must not be able to overflow the stack
/// (SECURITY.md §6 — untrusted repositories).
fn visit_descendants(node: Node, f: &mut impl FnMut(Node)) {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        f(current);
        // Push children in reverse so the visit order matches pre-order
        // (leftmost child first), keeping output deterministic.
        let mut cursor = current.walk();
        for child in current
            .children(&mut cursor)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            stack.push(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract;

    fn defs_of(src: &str) -> Vec<Def> {
        extract(src, Language::Go).expect("go extraction").defs
    }

    fn names(defs: &[Def]) -> Vec<String> {
        defs.iter().map(|d| d.name.clone()).collect()
    }

    #[test]
    fn extracts_simple_functions_with_docs_and_signatures() {
        let defs = defs_of(
            "package p\n\n// Login logs in.\nfunc Login(name string) error {\n\treturn nil\n}\n",
        );
        assert_eq!(names(&defs), vec!["Login"]);
        let login = &defs[0];
        assert_eq!(login.kind, DefKind::Function);
        assert_eq!(
            login.signature.as_deref(),
            Some("func Login(name string) error")
        );
        assert_eq!(login.doc.as_deref(), Some("Login logs in."));
        assert!(login.exported);
        assert!(login.container.is_none());
        assert_eq!(login.qual_name, "Login");
    }

    #[test]
    fn unexported_is_unicode_aware() {
        let defs = defs_of(
            "package p\n\nfunc Ünicode() int { return 0 }\n\nfunc ünicode() int { return 1 }\n",
        );
        assert_eq!(names(&defs), vec!["Ünicode", "ünicode"]);
        assert!(
            defs[0].exported,
            "Ünicode is exported in Go: first rune is upper"
        );
        assert!(!defs[1].exported);
    }

    #[test]
    fn methods_carry_receiver_container_and_pointer_is_stripped() {
        let defs = defs_of("package p\n\ntype Session struct{}\n\nfunc (s *Session) Validate() bool { return true }\n\nfunc (s Session) Refresh() {}\n");
        let method = defs.iter().find(|d| d.name == "Validate").expect("method");
        assert_eq!(method.kind, DefKind::Method);
        assert_eq!(method.container.as_deref(), Some("Session"));
        assert_eq!(method.qual_name, "Session.Validate");
        assert_eq!(
            method.signature.as_deref(),
            Some("func (s *Session) Validate() bool")
        );
        let value_recv = defs.iter().find(|d| d.name == "Refresh").expect("method");
        assert_eq!(value_recv.container.as_deref(), Some("Session"));
    }

    #[test]
    fn struct_interface_alias_and_named_types_map_to_kinds() {
        let src = "package p\n\ntype S struct{ X int }\n\ntype I interface{ M() }\n\ntype C float64\n\ntype P = [2]float64\n";
        let defs = defs_of(src);
        let by_name = |n: &str| {
            defs.iter()
                .find(|d| d.name == n)
                .unwrap_or_else(|| panic!("{n} missing"))
        };
        assert_eq!(by_name("S").kind, DefKind::Struct);
        assert_eq!(by_name("I").kind, DefKind::Interface);
        assert_eq!(by_name("C").kind, DefKind::Type);
        assert_eq!(by_name("P").kind, DefKind::Type);
        assert_eq!(
            by_name("S").signature.as_deref(),
            Some("type S struct { … }")
        );
        assert_eq!(
            by_name("I").signature.as_deref(),
            Some("type I interface { … }")
        );
        assert_eq!(by_name("C").signature.as_deref(), Some("type C float64"));
        assert_eq!(
            by_name("P").signature.as_deref(),
            Some("type P = [2]float64")
        );
    }

    #[test]
    fn grouped_declarations_yield_one_def_per_name() {
        let src = "package p\n\nconst (\n\t// A docs\n\tA = 1\n\tB = 2\n)\n\nvar X, Y int\n\ntype (\n\t// T docs\n\tT struct{}\n\tK string\n)\n";
        let defs = defs_of(src);
        assert_eq!(names(&defs), vec!["A", "B", "X", "Y", "T", "K"]);
        assert_eq!(defs[0].doc.as_deref(), Some("A docs"));
        assert_eq!(
            defs.iter().find(|d| d.name == "T").unwrap().doc.as_deref(),
            Some("T docs")
        );
        let x = defs.iter().find(|d| d.name == "X").unwrap();
        assert_eq!(x.signature.as_deref(), Some("var X, Y int"));
    }

    #[test]
    fn local_declarations_inside_functions_are_not_defs() {
        let src = "package p\n\nfunc F() {\n\tconst localC = 1\n\ttype localT struct{}\n\tvar localV int\n\t_ = localC\n\t_ = localV\n}\n";
        assert_eq!(names(&defs_of(src)), vec!["F"]);
    }

    #[test]
    fn imports_capture_alias_dot_and_blank_forms() {
        let src =
            "package p\n\nimport (\n\t\"fmt\"\n\tf \"strings\"\n\t. \"math\"\n\t_ \"embed\"\n)\n";
        let imports = extract(src, Language::Go).expect("go").imports;
        assert_eq!(imports.len(), 4);
        assert_eq!(imports[0].raw, "fmt");
        assert_eq!(imports[0].alias, None);
        assert_eq!(imports[1].raw, "strings");
        assert_eq!(imports[1].alias.as_deref(), Some("f"));
        assert_eq!(imports[2].raw, "math");
        assert_eq!(imports[2].alias.as_deref(), Some("."));
        assert_eq!(imports[3].raw, "embed");
        assert_eq!(imports[3].alias.as_deref(), Some("_"));
    }

    #[test]
    fn references_exclude_declaration_positions() {
        let src = "package p\n\nimport \"fmt\"\n\ntype S struct{ N int }\n\nfunc F(s S, other int) int {\n\tx, err := g(other)\n\tif err != nil {\n\t\treturn 0\n\t}\n\tfor i, v := range s.Items() {\n\t\t_ = v\n\t\t_ = i\n\t}\n\tfmt.Println(x)\n\treturn s.N\n}\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        // Go makes every declared local used somewhere, so the *names* of
        // declarations also appear as legitimate references at their use
        // sites. What must be filtered is the declaration occurrence itself.
        // Counting occurrences proves it: `s` is declared once (param) and
        // used twice (s.Items, s.N) — but receiver occurrences are scope
        // structure now (qualifier on the selected ref), so `s` never
        // appears as an independent name ref at all.
        let count = |n: &str| {
            refs.iter()
                .filter(|r| r.kind == RefKind::NameRef && r.name == n)
                .count()
        };
        assert_eq!(count("s"), 0, "receiver occurrences are structural");
        assert_eq!(count("other"), 1, "param decl filtered, one use remains");
        assert_eq!(count("x"), 1);
        assert_eq!(count("err"), 1);
        assert_eq!(count("i"), 1);
        assert_eq!(count("v"), 1);
        assert_eq!(count("g"), 1);
        assert_eq!(count("fmt"), 0, "package operands are structural");
        let field_refs: Vec<&str> = refs
            .iter()
            .filter(|r| r.kind == RefKind::FieldRef)
            .map(|r| r.name.as_str())
            .collect();
        // `s.N` is the only field access (method values: none here).
        assert_eq!(field_refs, vec!["N"]);
        let call_refs: Vec<&str> = refs
            .iter()
            .filter(|r| r.kind == RefKind::CallRef)
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(call_refs, vec!["Items", "Println"]);
        // Struct field N is a declaration position: not even a field ref.
        // S: one usage (the param type); the struct's own name position is
        // a declaration, and `int` is universe (never a ref).
        let type_refs: Vec<&str> = refs
            .iter()
            .filter(|r| r.kind == RefKind::TypeRef)
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(type_refs, vec!["S"]);
    }

    #[test]
    fn type_switch_bound_variable_declaration_is_filtered() {
        let src = "package p\n\nfunc TS(x any) string {\n\tswitch v := x.(type) {\n\tcase string:\n\t\treturn v\n\tdefault:\n\t\treturn \"\"\n\t}\n}\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        // v: one declaration (alias, filtered) + one use (return) = 1 ref.
        // x: one use (the switched value) = 1 ref.
        let name_refs: Vec<&str> = refs
            .iter()
            .filter(|r| r.kind == RefKind::NameRef)
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(name_refs, vec!["x", "v"]);
    }

    #[test]
    fn qualified_type_carries_the_package_as_qualifier() {
        let src = "package p\n\nimport \"io\"\n\ntype R interface {\n\tio.Closer\n}\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        // The qualifier is structural, not a standalone name ref.
        assert!(
            !refs.iter().any(|r| r.name == "io"),
            "the package operand must not be emitted as an independent ref"
        );
        let closer = refs
            .iter()
            .find(|r| r.kind == RefKind::TypeRef && r.name == "Closer")
            .expect("type part");
        assert_eq!(closer.qualifier.as_deref(), Some("io"));
    }

    #[test]
    fn blank_identifier_is_never_a_reference() {
        let src = "package p\n\nfunc F() {\n\t_, err := g()\n\t_ = err\n}\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        assert!(
            !refs.iter().any(|r| r.name == "_"),
            "blank identifier must never appear as a ref"
        );
        assert!(refs.iter().any(|r| r.name == "g"));
    }

    #[test]
    fn references_inside_bodies_get_their_def_as_container() {
        let src = "package p\n\ntype S struct{}\n\nfunc (s *S) M() int { return helper() }\n\nfunc helper() int { return 1 }\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        let helper_ref = refs.iter().find(|r| r.name == "helper").expect("ref");
        assert_eq!(helper_ref.container.as_deref(), Some("S.M"));
    }

    #[test]
    fn call_position_distinguishes_calls_from_method_values() {
        let src = "package p\n\ntype S struct{ N int }\n\nfunc (s S) Val() int { return 0 }\n\nfunc F(s S) {\n\t_ = s.Val      // method value: field_ref\n\t_ = s.Val()    // call: call_ref\n\t_ = s.N        // field access: field_ref\n\tg()            // plain call: name_ref\n}\n\nfunc g() {}\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        let kind_of = |n: &str| {
            refs.iter()
                .find(|r| r.name == n)
                .map_or_else(|| panic!("{n} missing"), |r| r.kind)
        };
        // s.Val appears twice with different kinds; find by line.
        let val_field = refs
            .iter()
            .find(|r| r.name == "Val" && r.kind == RefKind::FieldRef)
            .expect("method value stays a field_ref");
        assert_eq!(val_field.span.start_line, 8);
        let val_call = refs
            .iter()
            .find(|r| r.name == "Val" && r.kind == RefKind::CallRef)
            .expect("call upgrades to call_ref");
        assert_eq!(val_call.span.start_line, 9);
        assert_eq!(kind_of("N"), RefKind::FieldRef);
        assert_eq!(kind_of("g"), RefKind::NameRef);
    }

    #[test]
    fn package_qualified_calls_are_call_refs_with_qualifier() {
        let src = "package p\n\nimport \"fmt\"\n\nfunc F() {\n\tfmt.Println(\"x\")\n}\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        assert!(
            !refs.iter().any(|r| r.name == "fmt"),
            "the package operand must not be emitted as an independent ref"
        );
        let println = refs.iter().find(|r| r.name == "Println").unwrap();
        assert_eq!(println.kind, RefKind::CallRef);
        assert_eq!(println.qualifier.as_deref(), Some("fmt"));
    }

    #[test]
    fn instance_selectors_carry_the_operand_as_qualifier() {
        // A non-package operand (`s`) is recorded the same way; deciding
        // scope is the resolver's job. The operand itself is not a ref.
        let src = "package p\n\ntype S struct{ N int }\n\nfunc F(s S) int {\n\treturn s.N\n}\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        assert!(
            !refs.iter().any(|r| r.name == "s"),
            "receiver occurrences must not leak as name refs"
        );
        let n = refs
            .iter()
            .find(|r| r.kind == RefKind::FieldRef && r.name == "N")
            .expect("field access");
        assert_eq!(n.qualifier.as_deref(), Some("s"));
    }

    #[test]
    fn computed_selector_operands_have_no_qualifier() {
        // `w.Header().Add` — the operand is a call expression, so the ref
        // carries no qualifier and the resolver must not guess a package.
        let src = "package p\n\nimport \"net/http\"\n\nfunc H(w http.ResponseWriter) {\n\tw.Header().Add(\"k\", \"v\")\n}\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        let add = refs
            .iter()
            .find(|r| r.kind == RefKind::CallRef && r.name == "Add")
            .expect("chained call");
        assert_eq!(add.qualifier, None);
    }

    #[test]
    fn generic_instantiation_calls_are_call_refs() {
        // `pkg.Factory[int]()` instantiates through an index_expression;
        // the callee is still a call, not a field access.
        let src = "package p\n\nimport \"lib\"\n\nfunc F() {\n\t_ = lib.Factory[int]()\n}\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        let factory = refs
            .iter()
            .find(|r| r.name == "Factory")
            .expect("instantiated callee");
        assert_eq!(factory.kind, RefKind::CallRef);
        assert_eq!(factory.qualifier.as_deref(), Some("lib"));
    }

    #[test]
    fn chained_selector_deeper_parts_have_no_qualifier() {
        // `pkg.sub.Deep`: `sub` is qualified by `pkg`; `Deep`'s operand is
        // a selector (a value), not a package identifier.
        let src = "package p\n\nimport \"pkg\"\n\nfunc F() {\n\t_ = pkg.sub.Deep\n}\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        let sub = refs
            .iter()
            .find(|r| r.kind == RefKind::FieldRef && r.name == "sub")
            .expect("inner selection");
        assert_eq!(sub.qualifier.as_deref(), Some("pkg"));
        let deep = refs
            .iter()
            .find(|r| r.kind == RefKind::FieldRef && r.name == "Deep")
            .expect("outer selection");
        assert_eq!(deep.qualifier, None);
    }

    #[test]
    fn doc_comment_rules_match_go_convention() {
        // Blank line between comment and func: not a doc.
        let defs = defs_of("package p\n\n// floating comment\n\nfunc A() {}\n");
        assert_eq!(defs[0].doc, None);

        // Two paragraphs: only the first is stored.
        let defs = defs_of(
            "package p\n\n// First para.\n// Still first.\n//\n// Second para.\nfunc B() {}\n",
        );
        assert_eq!(defs[0].doc.as_deref(), Some("First para.\nStill first."));

        // Block comment doc.
        let defs = defs_of(
            "package p\n\n/*\nBlocky is documented\nby a block comment.\n*/\nfunc Blocky() {}\n",
        );
        assert_eq!(
            defs[0].doc.as_deref(),
            Some("Blocky is documented\nby a block comment.")
        );

        // Package comment must not leak into the first def (the package
        // clause sits between them).
        let defs = defs_of("// Package docs for p.\npackage p\n\nfunc C() {}\n");
        assert_eq!(defs[0].doc, None);
    }

    #[test]
    fn var_with_func_literal_keeps_type_shape_in_signature() {
        let defs = defs_of("package p\n\nvar Handler = func(w int) error {\n\treturn nil\n}\n");
        assert_eq!(names(&defs), vec!["Handler"]);
        assert_eq!(
            defs[0].signature.as_deref(),
            Some("var Handler = func(w int) error …")
        );
    }

    #[test]
    fn malformed_files_keep_clean_definitions_and_drop_error_refs() {
        let src = "package broken\n\nfunc Works() int { return 1 }\n\nfunc Dangling() int {\n\treturn 2\n";
        let file = extract(src, Language::Go).expect("go");
        assert_eq!(file.status, ParseStatus::Partial);
        assert_eq!(names(&file.defs), vec!["Works"]);
        assert!(
            !file
                .refs
                .iter()
                .any(|r| r.name == "Works" && r.span.start_line > 3),
            "refs from the broken region must be dropped"
        );
    }

    #[test]
    fn garbage_file_is_partial_with_no_defs() {
        let file = extract("def hello():\n    print('hi')\n", Language::Go).expect("go");
        assert_eq!(file.status, ParseStatus::Partial);
        assert!(file.defs.is_empty());
        assert!(file.package_name.is_none());
    }

    #[test]
    fn empty_and_package_only_files() {
        let empty = extract("", Language::Go).expect("go");
        assert_eq!(empty.status, ParseStatus::Ok);
        assert!(empty.defs.is_empty());
        assert!(empty.package_name.is_none());

        let only = extract("package only\n", Language::Go).expect("go");
        assert_eq!(only.status, ParseStatus::Ok);
        assert_eq!(only.package_name.as_deref(), Some("only"));
        assert!(only.defs.is_empty() && only.refs.is_empty());
    }

    #[test]
    fn generics_extract_with_type_parameters_in_signature() {
        let src = "package p\n\ntype Pair[T any] struct{ L, R T }\n\nfunc Map[T, U any](in []T, f func(T) U) []U {\n\treturn nil\n}\n";
        let defs = defs_of(src);
        assert_eq!(
            defs.iter()
                .find(|d| d.name == "Pair")
                .unwrap()
                .signature
                .as_deref(),
            Some("type Pair[T any] struct { … }")
        );
        assert_eq!(
            defs.iter()
                .find(|d| d.name == "Map")
                .unwrap()
                .signature
                .as_deref(),
            Some("func Map[T, U any](in []T, f func(T) U) []U")
        );
        // Type-parameter *declarations* are filtered; their *usages* as
        // types (`[]T`, `func(T) U`, field types) are legitimate type refs.
        let file = extract(src, Language::Go).expect("go");
        assert!(
            !file
                .refs
                .iter()
                .any(|r| r.name == "T" && r.kind != RefKind::TypeRef),
            "T must only appear as a type ref"
        );
        assert!(
            !file
                .refs
                .iter()
                .any(|r| r.name == "U" && r.kind != RefKind::TypeRef),
            "U must only appear as a type ref"
        );
        assert!(file
            .refs
            .iter()
            .any(|r| r.name == "T" && r.kind == RefKind::TypeRef));
    }

    #[test]
    fn extraction_is_deterministic() {
        let src = "package p\n\nimport \"fmt\"\n\n// Doc.\nfunc F() { fmt.Println(1) }\n\ntype S struct{ N int }\n";
        let first = extract(src, Language::Go).expect("go");
        let second = extract(src, Language::Go).expect("go");
        let a = serde_json::to_string(&first).expect("ser");
        let b = serde_json::to_string(&second).expect("ser");
        assert_eq!(a, b, "same input must serialize identically");
    }

    #[test]
    fn package_clause_is_extracted() {
        let file = extract("package auth\n\nfunc F() {}\n", Language::Go).expect("go");
        assert_eq!(file.package_name.as_deref(), Some("auth"));
    }

    #[test]
    fn universe_names_are_strictly_sorted_and_complete() {
        // `is_universe` binary-searches this slice: an unsorted entry
        // silently fails to match (the bug that leaked `len`/`make`/`append`
        // into refs across every Go corpus).
        assert!(
            UNIVERSE_NAMES.windows(2).all(|w| w[0] < w[1]),
            "UNIVERSE_NAMES must be strictly sorted for binary_search"
        );
        // Every Go predeclared identifier is recognized (and `f32`/`f64`,
        // which are not Go names, are absent).
        for name in UNIVERSE_NAMES {
            assert!(is_universe(name), "{name} must match");
        }
        assert!(!is_universe("f32") && !is_universe("f64"));
        assert_eq!(UNIVERSE_NAMES.len(), 44, "the Go predeclared set");
    }

    #[test]
    fn universe_builtins_never_leak_into_refs() {
        use std::fmt::Write as _;
        let mut src = String::from("package p\n\nfunc F() {\n");
        for name in UNIVERSE_NAMES {
            writeln!(src, "\t_ = {name}").expect("write");
        }
        src.push_str("\t_ = realUser\n}\n\nvar realUser int\n");
        let refs = extract(&src, Language::Go).expect("go").refs;
        let leaked: Vec<&str> = refs
            .iter()
            .filter(|r| is_universe(&r.name))
            .map(|r| r.name.as_str())
            .collect();
        assert!(leaked.is_empty(), "universe names leaked: {leaked:?}");
        assert!(refs.iter().any(|r| r.name == "realUser"));
    }

    #[test]
    fn short_var_declaration_keeps_rhs_references() {
        // Only the LEFT side of `:=` declares; a bare identifier on the
        // right is a genuine use.
        let src = "package p\n\nfunc F() {\n\tx := 1\n\ty := x\n\t_ = y\n\t_ = x\n}\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        let xs: Vec<_> = refs
            .iter()
            .filter(|r| r.kind == RefKind::NameRef && r.name == "x")
            .collect();
        assert_eq!(xs.len(), 2, "rhs of `y := x` and `_ = x` are both uses");
    }

    #[test]
    fn type_alias_and_named_type_targets_are_references() {
        let src = "package p\n\ntype UUID string\n\ntype Widget2 struct{}\n\ntype ID = UUID\n\ntype Store Widget2\n";
        let refs = extract(src, Language::Go).expect("go").refs;
        let names: Vec<&str> = refs
            .iter()
            .filter(|r| r.kind == RefKind::TypeRef)
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["UUID", "Widget2"],
            "alias/named-type targets are uses, only the defined name is a declaration"
        );
    }

    #[test]
    fn spec_doc_overrides_group_doc() {
        // A comment on the group header must not suppress the per-spec doc
        // immediately above the spec (the ModeFast regression).
        let defs = defs_of(
            "package p\n\n// Modes for the parser.\nconst (\n\t// ModeFast skips validation.\n\tModeFast = iota\n\tModeSlow\n)\n",
        );
        let fast = defs.iter().find(|d| d.name == "ModeFast").expect("spec");
        assert_eq!(fast.doc.as_deref(), Some("ModeFast skips validation."));
        let slow = defs.iter().find(|d| d.name == "ModeSlow").expect("spec");
        assert_eq!(
            slow.doc, None,
            "no per-spec comment: the group comment is not adjacent, so no doc"
        );
    }

    #[test]
    fn raw_string_imports_are_captured() {
        let src = "package p\n\nimport `fmt`\n\nfunc F() { fmt.Println(1) }\n";
        let file = extract(src, Language::Go).expect("go");
        assert_eq!(file.imports.len(), 1);
        assert_eq!(file.imports[0].raw, "fmt");
        // The qualifier still rides structurally on the selected call.
        let println = file
            .refs
            .iter()
            .find(|r| r.name == "Println")
            .expect("qualified call");
        assert_eq!(println.qualifier.as_deref(), Some("fmt"));
    }

    #[test]
    fn compiler_directives_are_not_docs() {
        // A directive directly above the func is not the doc...
        let defs = defs_of("package p\n\n//go:noinline\nfunc A() {}\n");
        assert_eq!(defs[0].doc, None);

        // ...and a directive between the doc and the func (gofmt's
        // preferred placement) does not break doc adjacency.
        let defs = defs_of("package p\n\n// B does things.\n//go:noinline\nfunc B() {}\n");
        assert_eq!(defs[0].doc.as_deref(), Some("B does things."));
    }
}
