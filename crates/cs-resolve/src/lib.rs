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
    /// The target exists conceptually but this adapter does not model it yet.
    Unsupported,
}

/// Cross-file relationship kinds (ARCHITECTURE §5, `edges.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// The import-resolution and symbol-binding seam (LANGUAGES.md §5).
///
/// Adding a language means implementing this trait plus the extraction queries;
/// nothing in `cs-select`, `cs-render`, `cs-index` or `cs-cli` learns that
/// languages exist beyond `lang` strings.
pub trait LanguageResolver {
    /// The language this resolver handles.
    fn language(&self) -> Language;

    /// Resolve one import specifier written in `from_file`.
    fn resolve_import(&self, raw: &str, from_file: &FilePath) -> Resolution;

    /// The module qualifier for a file, used to build qualified names
    /// (`internal/auth` for `internal/auth/session.go`).
    fn module_qualifier(&self, file: &FilePath) -> String;

    /// Whether a definition is visible to importers of its file.
    fn is_exported(&self, def: &cs_extract::Def) -> bool;

    /// The test file paired with a code file, if the language has a convention.
    fn test_partner(&self, code_file: &FilePath) -> Option<FilePath>;

    /// Config files that carry signal S8 (ALGORITHM.md §5).
    fn config_files(&self) -> Vec<FilePath>;
}

/// Tally of resolution outcomes, published as a resolution rate
/// (LANGUAGES.md §6.2, ARCHITECTURE §11).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionStats {
    /// Imports resolved to a file in the repository.
    pub resolved: u64,
    /// Imports resolved to something outside the repository.
    pub external: u64,
    /// Imports left unresolved.
    pub unresolved: u64,
    /// References bound to exactly one definition.
    pub refs_bound: u64,
    /// References left unbound.
    pub refs_unbound: u64,
    /// Names skipped because they had too many candidate definitions.
    pub names_skipped: u64,
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
