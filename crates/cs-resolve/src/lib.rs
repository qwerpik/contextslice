//! Per-language import resolution and approximate symbol binding.
//!
//! Implements `cs-resolve` from ARCHITECTURE.md §4.3.
//!
//! # What this crate is honest about
//!
//! Reference edges are **approximate by construction** (ADR-003). tree-sitter
//! deliberately does not resolve names, GitHub's stack-graphs was archived, and
//! per-language indexers (SCIP, gopls, rust-analyzer) are too heavy for an
//! always-on local index. This crate therefore resolves *imports* structurally
//! and binds *references* by name against the candidate definitions reachable
//! through those imports, and marks everything it cannot resolve as
//! [`Resolution::Unresolved`] with a reason. Unresolved counts are published as
//! a resolution rate (LANGUAGES.md §6.2, ARCHITECTURE §11) rather than hidden.
//!
//! # Determinism
//!
//! Candidate sets are ordered by file path before any binding decision, so the
//! same inputs always bind the same references (ALGORITHM.md §12).

#![forbid(unsafe_code)]

use cs_scanner::Language;
use serde::{Deserialize, Serialize};

/// The Go resolver (MASTER_PLAN §15 step 4, ADR-018).
pub mod go;

pub use go::{parse_module_path, GoResolver};

/// A stable file identifier: the file's repo-relative, `/`-separated path.
///
/// The index assigns integer ids, but resolution runs *before* the index exists,
/// so it speaks in paths and lets `cs-index` map them to integers. This keeps
/// resolution testable without a database.
pub type FilePath = String;

/// Names with more candidates than this are treated as uninformative.
///
/// This is the generalization of aider's ">5 files ⇒ name is uninformative"
/// dampener (ALGORITHM.md §15): a name defined in 256 places carries almost no
/// signal, and skipping it bounds resolver cost.
pub const MAX_CANDIDATES_PER_NAME: usize = 256;

/// Maximum depth for re-export chain following (LANGUAGES.md §6.2/§6.3).
pub const REEXPORT_DEPTH_CAP: usize = 8;

/// The outcome of resolving one import specifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum Resolution {
    /// Resolved to a file inside the repository.
    Resolved(FilePath),
    /// Resolved to something outside the repository (stdlib, `node_modules`, a
    /// vendored dependency, a site-package). Listed, never indexed.
    External {
        /// The specifier that resolved externally.
        specifier: String,
    },
    /// Not resolved, with the reason recorded so `doctor` can report a rate
    /// rather than silently dropping an edge.
    Unresolved {
        /// The specifier that failed to resolve.
        specifier: String,
        /// Why it failed.
        reason: UnresolvedReason,
    },
}

/// Why an import could not be resolved. Each variant is a documented
/// degradation path in LANGUAGES.md §6, not an unexpected condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedReason {
    /// A path alias the resolver could not locate (`tsconfig` `paths`, bundler
    /// alias). Counted and surfaced as a resolution-rate shortfall.
    Alias,
    /// A dynamic import (`import()`, `importlib`, `__import__`) — unresolvable
    /// without execution, and out of scope by design.
    Dynamic,
    /// The specifier is malformed or empty.
    Malformed,
    /// Resolution would have escaped the repository root.
    EscapesRoot,
    /// The specifier names a directory that has no Go files (or does not
    /// exist) — a dangling or wrong-path import (ADR-018).
    NotFound,
    /// The target is an `internal/` package outside the importer's
    /// visibility tree (Go's internal rule): the code could not compile, so
    /// the dependency does not exist for this importer.
    Internal,
    /// The target exists conceptually but this adapter does not model it yet.
    Unsupported,
}

/// Cross-file relationship kinds (ARCHITECTURE §5, `edges.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// `src` imports `dst`.
    ImportOut,
    /// `src` references a definition in `dst`.
    RefDef,
    /// `src` is a test file for `dst` (bidirectional affinity).
    TestAffinity,
}

impl EdgeKind {
    /// Stable string used as the `edges.kind` column value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ImportOut => "import_out",
            Self::RefDef => "ref_def",
            Self::TestAffinity => "test_affinity",
        }
    }
}

/// A weighted edge between two files.
///
/// `import_in` is deliberately absent: it is the reverse of [`EdgeKind::ImportOut`]
/// and is derived at CSR build time rather than stored, to halve index writes
/// (ARCHITECTURE §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    /// Source file.
    pub src: FilePath,
    /// Destination file.
    pub dst: FilePath,
    /// Relationship kind.
    pub kind: EdgeKind,
    /// Edge weight, as specified in ALGORITHM.md §6.
    pub weight: f64,
}

/// Base weight for an `import_out` edge (ALGORITHM.md §6).
pub const W_IMPORT_OUT: f64 = 0.6;

/// Derived weight for an `import_in` edge (ALGORITHM.md §6). Not stored; exposed
/// so the selection stage and the docs cannot drift apart.
pub const W_IMPORT_IN: f64 = 0.45;

/// Scale factor for `ref_def` edges (ALGORITHM.md §6).
pub const W_REF_DEF: f64 = 0.5;

/// Saturation constant in the `ref_def` sqrt dampener (ALGORITHM.md §6).
pub const REF_DEF_SATURATION: f64 = 8.0;

/// Base weight for a `test_affinity` edge (ALGORITHM.md §6).
pub const W_TEST_AFFINITY: f64 = 0.7;

/// The sqrt-damped weight of a `ref_def` edge carrying `count` references.
///
/// Implements `0.5·√count/√(count+8)` from ALGORITHM.md §6, so the tenth
/// reference adds far less evidence than the second.
#[must_use]
pub fn ref_def_weight(count: u64) -> f64 {
    let count = count as f64;
    W_REF_DEF * count.sqrt() / (count + REF_DEF_SATURATION).sqrt()
}

/// The import-resolution, symbol-binding and edge-production seam
/// (LANGUAGES.md §5, ADR-018).
///
/// The original one-call-per-import shape could not work: a Go import
/// resolves to a *directory of files*, and reference binding needs the whole
/// repository's definitions. The seam is therefore two-phase: `prepare`
/// builds a repo-level package index once, `resolve` consumes it per file.
/// Adding a language means implementing this trait; nothing in `cs-select`,
/// `cs-render`, `cs-index` or `cs-cli` learns that languages exist beyond
/// `lang` strings.
pub trait LanguageResolver: Sized {
    /// The language this resolver handles.
    fn language(&self) -> Language;

    /// Build the repo-level index from an extracted snapshot plus any module
    /// manifests (`go.mod` contents for Go). Pure; no filesystem access.
    fn prepare(snapshot: &ResolveSnapshot) -> Self;

    /// Resolve imports, bind references and produce file-level edges for the
    /// whole snapshot. Deterministic: a pure function of the snapshot.
    fn resolve(&self, snapshot: &ResolveSnapshot) -> ResolvedRepo;
}

/// Everything the resolver is allowed to know: extracted files plus module
/// manifests, both sorted by path at construction so downstream ordering is
/// canonical regardless of caller iteration order (ALGORITHM.md §12).
#[derive(Debug, Clone, Default)]
pub struct ResolveSnapshot {
    /// Extracted files: repo-relative `/`-separated paths.
    pub files: Vec<(FilePath, cs_extract::ExtractedFile)>,
    /// Module manifest contents, e.g. `("go.mod", "module example.com/m\n")`.
    /// Passed in by the caller because the resolver performs zero I/O.
    pub manifests: Vec<(FilePath, String)>,
}

impl ResolveSnapshot {
    /// Construct from unsorted inputs; sorts both lists by path.
    #[must_use]
    pub fn new(
        mut files: Vec<(FilePath, cs_extract::ExtractedFile)>,
        mut manifests: Vec<(FilePath, String)>,
    ) -> Self {
        files.sort_by(|a, b| a.0.cmp(&b.0));
        manifests.sort_by(|a, b| a.0.cmp(&b.0));
        Self { files, manifests }
    }
}

/// Where a definition lives, as far as the resolver is concerned.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DefLoc {
    /// File containing the definition.
    pub file: FilePath,
    /// File-local qualified name (`Session.Validate`).
    pub qual_name: String,
    /// Canonical kind, kept so binding rules can be kind-appropriate.
    pub kind: cs_extract::DefKind,
}

/// Why a reference stayed unbound. The taxonomy is the honesty contract of
/// ADR-018: every unbound ref carries a reason, and the reasons are
/// documented behaviors, not failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnboundReason {
    /// No def with this name in any reachable scope.
    NoCandidate,
    /// The name is used through a package qualifier that resolved outside
    /// the repository (stdlib/third-party) — nothing to bind to, by design.
    ExternalScope,
    /// A call whose receiver is a computed expression (`w.Header().Add`,
    /// `arr[0].Close`): the selector's operand is not a plain identifier,
    /// so the receiver's package is unknowable without type information.
    /// Emitted since the Ref qualifier became structural (ADR-017 addendum);
    /// previously these fell into bare-call binding and produced false
    /// edges into same-named local methods.
    NoScope,
    /// A bare method call whose name is defined on several types in the
    /// package; receiver type information would be required.
    MethodAmbiguous,
    /// Both the file's own package and a dot import define the name; the
    /// spec-level precedence is not modeled (ADR-018; zero occurrences in
    /// the gin/chi census).
    AmbiguousDotImport,
    /// A bare field access or method value: binding requires the receiver's
    /// type, which extraction deliberately does not compute.
    NeedsTypeInfo,
    /// A bare method call whose name is on Go's universal interface surface
    /// (`Close`, `ServeHTTP`, `Value`, …): such names collide with stdlib
    /// methods on almost every receiver type, so binding them by name is
    /// noise even when the name is unique in the package (the gin/chi audit
    /// measured this as the dominant FP class; ADR-018).
    UniverseMethod,
}

/// The outcome for one reference: zero or more target definitions, or a
/// documented reason for having none. Multiple targets are legitimate
/// (build-tag variants of one package define the same names — gin's
/// `codec/json` ships four; ADR-018).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolBinding {
    /// Definitions the reference binds to (may be several).
    pub targets: Vec<DefLoc>,
    /// Why the reference stayed unbound, when it did.
    pub unbound_reason: Option<UnboundReason>,
}

impl SymbolBinding {
    /// A binding to one or more targets.
    #[must_use]
    pub fn bound(targets: Vec<DefLoc>) -> Self {
        Self {
            targets,
            unbound_reason: None,
        }
    }

    /// An unbound reference with its documented reason.
    #[must_use]
    pub fn unbound(reason: UnboundReason) -> Self {
        Self {
            targets: Vec::new(),
            unbound_reason: Some(reason),
        }
    }
}

/// Per-file resolution results: package identity, import resolutions and
/// reference bindings, all in source order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileResolution {
    /// The package this file belongs to: `(dir, package_name)`.
    pub package: (String, String),
    /// Imports with their resolutions, in source order.
    pub imports: Vec<(cs_extract::Import, Resolution)>,
    /// References with their bindings, in source order.
    pub refs: Vec<(cs_extract::Ref, SymbolBinding)>,
}

/// The whole repository's resolution output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResolvedRepo {
    /// Per-file results, keyed by path.
    pub files: std::collections::BTreeMap<FilePath, FileResolution>,
    /// File-level edges, sorted and deduplicated.
    pub edges: Vec<Edge>,
    /// Aggregate outcome statistics.
    pub stats: ResolutionStats,
}

/// Tally of resolution outcomes, published as a resolution rate
/// (LANGUAGES.md §6.2, ARCHITECTURE §11).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResolutionStats {
    /// Imports resolved to a file inside the repository.
    pub resolved: u64,
    /// Imports resolved to something outside the repository.
    pub external: u64,
    /// Imports left unresolved.
    pub unresolved: u64,
    /// References bound to at least one definition.
    pub refs_bound: u64,
    /// References left unbound.
    pub refs_unbound: u64,
    /// Names skipped because they had too many candidate definitions.
    pub names_skipped: u64,
    /// References whose recorded selector qualifier names an import scope
    /// of the file — in-repo or external (`auth.Session`, `fmt.Println`;
    /// ADR-017 addendum: the qualifier rides structurally on the reference,
    /// it is no longer an independent name ref).
    pub package_qualifier_refs: u64,
    /// Why each unbound reference stayed unbound (ADR-018's honesty
    /// contract: unbound is a documented behavior with a reason).
    pub unbound_reasons: std::collections::BTreeMap<UnboundReason, u64>,
}

impl ResolutionStats {
    /// Fraction of imports that resolved inside the repository, in `[0, 1]`.
    ///
    /// Returns `1.0` when there are no imports at all: an empty set has no
    /// shortfall, and reporting `0.0` would read as total failure.
    #[must_use]
    pub fn import_resolution_rate(&self) -> f64 {
        let total = self.resolved + self.external + self.unresolved;
        if total == 0 {
            return 1.0;
        }
        self.resolved as f64 / total as f64
    }

    /// Of the imports that were supposed to resolve inside the repository,
    /// the fraction that did: `resolved / (resolved + unresolved)`.
    /// External (stdlib/third-party) is a correct outcome and excluded —
    /// this is ADR-018 §7's "in-repo import resolution" metric; an empty
    /// set has no shortfall.
    #[must_use]
    pub fn in_repo_import_success_rate(&self) -> f64 {
        let total = self.resolved + self.unresolved;
        if total == 0 {
            return 1.0;
        }
        self.resolved as f64 / total as f64
    }

    /// Fraction of references bound to a definition, in `[0, 1]`.
    #[must_use]
    pub fn ref_binding_rate(&self) -> f64 {
        let total = self.refs_bound + self.refs_unbound;
        if total == 0 {
            return 1.0;
        }
        self.refs_bound as f64 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_def_weight_is_monotonic_and_saturating() {
        let w1 = ref_def_weight(1);
        let w10 = ref_def_weight(10);
        let w1000 = ref_def_weight(1000);
        let w_million = ref_def_weight(1_000_000);
        assert!(w1 < w10, "more references must not lower the weight");
        assert!(w10 < w1000);
        assert!(
            w_million < W_REF_DEF,
            "dampener must saturate below the scale factor, got {w_million}"
        );
        assert!(w_million > 0.499, "saturation must approach {W_REF_DEF}");
    }

    #[test]
    fn ref_def_weight_is_zero_at_zero_references() {
        assert!(
            ref_def_weight(0).abs() < f64::EPSILON,
            "zero references must contribute nothing"
        );
    }

    #[test]
    fn resolution_rate_of_empty_set_is_one() {
        // An empty set has no shortfall; 0.0 would misreport total failure.
        let empty = ResolutionStats::default();
        let one = (empty.import_resolution_rate() - 1.0).abs();
        assert!(one < f64::EPSILON, "empty import set must rate 1.0");
        let none = (empty.ref_binding_rate() - 1.0).abs();
        assert!(none < f64::EPSILON, "empty ref set must rate 1.0");
    }

    #[test]
    fn resolution_rate_counts_unresolved_against_total() {
        let stats = ResolutionStats {
            resolved: 92,
            external: 4,
            unresolved: 4,
            ..ResolutionStats::default()
        };
        assert!((stats.import_resolution_rate() - 0.92).abs() < 1e-12);
    }

    #[test]
    fn edge_kind_strings_match_the_schema() {
        // These strings are persisted as edges.kind (ARCHITECTURE §5).
        assert_eq!(EdgeKind::ImportOut.as_str(), "import_out");
        assert_eq!(EdgeKind::RefDef.as_str(), "ref_def");
        assert_eq!(EdgeKind::TestAffinity.as_str(), "test_affinity");
    }

    #[test]
    fn resolution_serializes_with_a_kind_tag() {
        let resolved = Resolution::Resolved("internal/auth/session.go".to_string());
        let json = serde_json::to_string(&resolved).expect("serialize");
        assert_eq!(
            json,
            r#"{"kind":"resolved","detail":"internal/auth/session.go"}"#
        );

        let unresolved = Resolution::Unresolved {
            specifier: "@/lib/x".to_string(),
            reason: UnresolvedReason::Alias,
        };
        let json = serde_json::to_string(&unresolved).expect("serialize");
        assert!(json.contains(r#""reason":"alias""#), "got {json}");
    }
}
