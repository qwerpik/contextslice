//! Go resolution: module mapping, package indexing, import resolution and
//! approximate reference binding (MASTER_PLAN §15 step 4, ADR-018).
//!
//! # What this resolver is
//!
//! A filesystem-and-syntax approximation, deliberately not a semantic
//! engine. It knows the Go *scope rules* that are visible without types:
//! packages live in directories, import paths map through `go.mod`, and
//! qualifiers come from package clauses (never path tails — chi's `/v5`
//! module path declares `package chi`). It does not know receiver types,
//! method sets, or local scoping, and every consequence of that is recorded
//! as an [`UnboundReason`](crate::UnboundReason) rather than guessed around.
//!
//! # Design decisions (ADR-018)
//!
//! - **Filesystem only**: no `go list`/gopls — they need a toolchain and,
//!   for full accuracy, module downloads (network). In-repo edges are what
//!   selection needs; everything else is honestly `External`.
//! - **Package identity = `(dir, package_name)`**: separates `foo` from
//!   `foo_test` in one directory (different packages with different
//!   visibility), groups build-tag variants, isolates broken name conflicts.
//! - **Nearest `go.mod`**: a file belongs to the deepest module whose root
//!   is an ancestor; an import landing inside a *different* module's subtree
//!   is `External` even when the path looks in-repo (the chi `_examples`
//!   trap).
//! - **Bind-all for same-package names**: gin's `codec/json` defines the
//!   same symbol set in four tag-variant files — every variant file is a
//!   legitimate target.
//! - **Unique-only methods**: `Render` is defined on 18 types in one gin
//!   package; name-only method binding would be noise. Ambiguous method
//!   names stay unbound.
//! - **Bare field accesses and method values are never bound** — the
//!   receiver's type is exactly the information extraction does not have.
//!
//! # Determinism
//!
//! Everything is derived from the sorted snapshot through BTree structures;
//! edges are accumulated in ordered maps and flattened once (ALGORITHM
//! §12). Two runs over the same snapshot produce byte-identical output.

use std::collections::BTreeMap;

use crate::ref_def_weight;
use crate::{
    DefLoc, Edge, EdgeKind, FilePath, FileResolution, LanguageResolver, Resolution,
    ResolveSnapshot, ResolvedRepo, SymbolBinding, UnboundReason, MAX_CANDIDATES_PER_NAME,
    W_IMPORT_OUT, W_TEST_AFFINITY,
};
use cs_extract::{DefKind, RefKind};
use cs_scanner::Language;

/// One `go.mod`: the directory it governs (`""` at the repo root) and the
/// module path it declares.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModuleInfo {
    dir: String,
    path: String,
}

/// A package: all files sharing one `(dir, name)` identity, with its
/// definitions indexed by name for binding.
///
/// Two views matter and they differ for internal test files (`foo_test.go`
/// with `package foo`): they share the package's *identity* — their refs
/// same-package-bind and pair by affinity — but they are **not** part of
/// the importable package: importers see neither their files nor their
/// defs (Go build semantics). `exported` is therefore built from non-test
/// files only; `defs`/`methods` keep everything for same-package binding.
#[derive(Debug, Clone, Default)]
struct Package {
    files: Vec<FilePath>,
    /// All defs by bare name (unexported included — same-package files see
    /// them).
    defs: BTreeMap<String, Vec<DefLoc>>,
    /// Exported defs from NON-TEST files by name (what importers may bind
    /// to; internal test files are invisible to importers).
    exported: BTreeMap<String, Vec<DefLoc>>,
    /// Method defs by bare method name (for unique-only method binding).
    methods: BTreeMap<String, Vec<DefLoc>>,
}

/// The built index: packages by identity, module list, per-directory
/// package names, vendor presence. Not `Clone`: the module list is built
/// once and shared by reference through `resolve`.
#[derive(Debug, Default)]
pub struct GoResolver {
    modules: Vec<ModuleInfo>,
    packages: BTreeMap<(String, String), Package>,
    /// Directory → the names of the packages it contains. The lookup index
    /// for `non_test_package` and test affinity: without it every import
    /// would scan the whole package map (quadratic in repo size).
    package_dirs: BTreeMap<String, Vec<String>>,
    /// Whether any indexed file lives under `vendor/` (enables vendor
    /// resolution; Go only honors vendor/ when it exists).
    has_vendor: bool,
}

/// What a qualifier name in a file resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum QualifierTarget {
    /// An in-repo package (`(dir, name)`).
    Package((String, String)),
    /// Stdlib or third-party: a name with nothing to bind to.
    External,
}

/// Minimal `go.mod` module-line parser: the first `module X` (or
/// `module "X"`), comments and arbitrary whitespace tolerated.
/// `require`/`replace`/`exclude` are deliberately ignored (ADR-018: replace
/// directives would need module graph resolution; repos relying on them get
/// those imports as External).
#[must_use]
pub fn parse_module_path(content: &str) -> Option<String> {
    for line in content.lines() {
        // Strip an inline comment first (`module m // staging`): a comment
        // left on the path breaks every own-module prefix match. `//` never
        // occurs inside a module path (empty path elements are invalid).
        let code = line.split_once("//").map_or(line, |(code, _)| code);
        let mut tokens = code.split_whitespace();
        if tokens.next() == Some("module") {
            if let Some(path) = tokens.next().map(|p| p.trim_matches('"')) {
                if !path.is_empty() {
                    return Some(path.to_owned());
                }
            }
        }
    }
    None
}

/// Directory part of a `/`-separated repo-relative path (`""` for roots).
fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Path join for repo-relative dirs (both `/`-separated, no trailing
/// slashes).
fn join_dir(base: &str, rest: &str) -> String {
    if base.is_empty() {
        rest.to_owned()
    } else {
        format!("{base}/{rest}")
    }
}

/// Resolve a relative import (`./x`, `../y`) against `from_dir`, refusing
/// to escape the repository root. Returns the normalized dir.
fn resolve_relative(from_dir: &str, raw: &str) -> Option<String> {
    let mut parts: Vec<&str> = from_dir.split('/').filter(|p| !p.is_empty()).collect();
    if from_dir.is_empty() {
        parts = Vec::new();
    }
    for segment in raw.split('/') {
        match segment {
            "." | "" => {}
            ".." => {
                parts.pop()?; // popping the root escapes the repository
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// Whether an import path's first segment marks it as stdlib (Go's rule of
/// thumb: no dot in the first element). Both stdlib and third-party imports
/// resolve as `External` — the distinction is diagnostic context for the
/// doctor, not behavior.
fn looks_like_stdlib(path: &str) -> bool {
    path.split('/').next().is_some_and(|seg| !seg.contains('.'))
}

/// The qualifier an unaliased external import is addressed by. Go's
/// major-version suffix (`…/chi/v5`) is not an identifier: the code writes
/// `chi.NewRouter`, never `v5.NewRouter`, so the default qualifier steps
/// over a trailing `v<N>` segment.
fn default_external_qualifier(path: &str) -> String {
    let last = path.rsplit('/').next().unwrap_or(path);
    let is_major_suffix =
        last.len() > 1 && last.starts_with('v') && last[1..].bytes().all(|b| b.is_ascii_digit());
    if is_major_suffix {
        let stem = path.strip_suffix(last).unwrap_or(path);
        let stem = stem.strip_suffix('/').unwrap_or(stem);
        if let Some(prev) = stem.rsplit('/').next() {
            if !prev.is_empty() {
                return prev.to_owned();
            }
        }
    }
    last.to_owned()
}

/// Go's internal visibility rule, module-relative: an import path whose
/// element is `internal` is importable only from within the tree rooted at
/// that element's parent. An `internal` element at the module root is
/// visible module-wide.
fn internal_visibility_violation(candidate: &str, from_dir: &str) -> bool {
    let segs: Vec<&str> = candidate.split('/').collect();
    let Some(pos) = segs.iter().position(|&s| s == "internal") else {
        return false;
    };
    if pos == 0 {
        return false;
    }
    let allowed = segs[..pos].join("/");
    from_dir != allowed && !from_dir.starts_with(&format!("{allowed}/"))
}

/// A directory as seen from its module root (`""` root → unchanged).
fn module_relative<'a>(dir: &'a str, module_dir: &str) -> &'a str {
    if module_dir.is_empty() {
        return dir;
    }
    match dir.strip_prefix(module_dir) {
        Some(rest) => rest.strip_prefix('/').unwrap_or(rest),
        None => dir,
    }
}

impl LanguageResolver for GoResolver {
    fn language(&self) -> Language {
        Language::Go
    }

    fn prepare(snapshot: &ResolveSnapshot) -> Self {
        let mut modules: Vec<ModuleInfo> = Vec::new();
        for (path, content) in &snapshot.manifests {
            // Exact file name: `my_go.mod` is a text file, not a manifest.
            if path.rsplit('/').next() != Some("go.mod") {
                continue;
            }
            if let Some(module_path) = parse_module_path(content) {
                modules.push(ModuleInfo {
                    dir: dir_of(path).to_owned(),
                    path: module_path,
                });
            }
        }
        // A repo with no go.mod at all still gets a synthetic root module
        // with an empty path: same-directory binding and relative imports
        // keep working; module-relative imports cannot (documented).
        if modules.is_empty() {
            modules.push(ModuleInfo {
                dir: String::new(),
                path: String::new(),
            });
        }
        modules.sort_by(|a, b| b.dir.len().cmp(&a.dir.len()).then(a.dir.cmp(&b.dir)));

        let mut resolver = GoResolver {
            modules,
            packages: BTreeMap::new(),
            package_dirs: BTreeMap::new(),
            has_vendor: false,
        };

        for (path, extracted) in &snapshot.files {
            // Go tooling is case-sensitive: `.GO` is not a Go file.
            #[allow(clippy::case_sensitive_file_extension_comparisons)]
            if !path.ends_with(".go") {
                continue; // go.mod and friends arrive as files but are not Go source
            }
            if path.starts_with("vendor/") {
                resolver.has_vendor = true;
            }
            // Files without a package clause get a singleton pseudo-package
            // keyed by their own path: they bind nothing same-package, and
            // they cannot contaminate real packages in the same directory.
            let dir = dir_of(path).to_owned();
            let name = extracted
                .package_name
                .clone()
                .unwrap_or_else(|| format!("<<no-package>>:{path}"));
            let entry = resolver
                .packages
                .entry((dir.clone(), name.clone()))
                .or_default();
            entry.files.push(path.clone());
            let is_test_file = path.ends_with("_test.go");
            for def in &extracted.defs {
                let loc = DefLoc {
                    file: path.clone(),
                    qual_name: def.qual_name.clone(),
                    kind: def.kind,
                };
                entry
                    .defs
                    .entry(def.name.clone())
                    .or_default()
                    .push(loc.clone());
                if def.exported && !is_test_file {
                    entry
                        .exported
                        .entry(def.name.clone())
                        .or_default()
                        .push(loc.clone());
                }
                if def.kind == DefKind::Method {
                    entry.methods.entry(def.name.clone()).or_default().push(loc);
                }
            }
            resolver.package_dirs.entry(dir).or_default().push(name);
        }
        for names in resolver.package_dirs.values_mut() {
            names.sort();
        }
        resolver
    }

    fn resolve(&self, snapshot: &ResolveSnapshot) -> ResolvedRepo {
        let mut repo = ResolvedRepo::default();
        // (src, dst, kind) -> weight; ordered for deterministic flattening.
        let mut edges: BTreeMap<(FilePath, FilePath, EdgeKind), f64> = BTreeMap::new();
        let mut ref_def_counts: BTreeMap<(FilePath, FilePath), u64> = BTreeMap::new();

        for (path, extracted) in &snapshot.files {
            // Go tooling is case-sensitive: `.GO` is not a Go file.
            #[allow(clippy::case_sensitive_file_extension_comparisons)]
            if !path.ends_with(".go") {
                continue;
            }
            let outcome = self.resolve_file(path, extracted, &mut repo.stats);

            for dst in outcome.import_targets {
                edges
                    .entry((path.clone(), dst, EdgeKind::ImportOut))
                    .or_insert(W_IMPORT_OUT);
            }
            for dst in outcome.affinity_targets {
                edges
                    .entry((path.clone(), dst.clone(), EdgeKind::TestAffinity))
                    .or_insert(W_TEST_AFFINITY);
                edges
                    .entry((dst, path.clone(), EdgeKind::TestAffinity))
                    .or_insert(W_TEST_AFFINITY);
            }
            for dst in outcome.ref_targets {
                *ref_def_counts.entry((path.clone(), dst)).or_insert(0) += 1;
            }
            repo.files.insert(path.clone(), outcome.resolution);
        }

        for ((src, dst), count) in ref_def_counts {
            edges.insert((src, dst, EdgeKind::RefDef), ref_def_weight(count));
        }
        repo.edges = edges
            .into_iter()
            .map(|((src, dst, kind), weight)| Edge {
                src,
                dst,
                kind,
                weight,
            })
            .collect();
        repo
    }
}

/// What resolving one file contributes to the repo-level output.
struct FileOutcome {
    resolution: FileResolution,
    /// Non-test files of resolved imports (`import_out` edge targets).
    import_targets: Vec<FilePath>,
    /// Non-test files in the same directory (test affinity, both ways).
    affinity_targets: Vec<FilePath>,
    /// One entry per bound ref target in another file (`ref_def` counting
    /// needs multiplicity for the sqrt dampener).
    ref_targets: Vec<FilePath>,
}

impl GoResolver {
    /// Resolve imports, build the file's qualifier scope, bind its refs
    /// and collect edge targets for one file. Pure; mutations flow out
    /// through the returned outcome plus the shared stats counter.
    fn resolve_file(
        &self,
        path: &str,
        extracted: &cs_extract::ExtractedFile,
        stats: &mut crate::ResolutionStats,
    ) -> FileOutcome {
        let own = Self::own_package_of(path, extracted);
        let src_is_test = path.ends_with("_test.go");

        // --- A. imports + qualifier scope -----------------------------
        let mut imports_out = Vec::new();
        let mut qualifiers: BTreeMap<String, QualifierTarget> = BTreeMap::new();
        let mut dot_packages: Vec<(String, String)> = Vec::new();
        let mut import_targets: Vec<FilePath> = Vec::new();
        for import in &extracted.imports {
            let resolution = self.resolve_import(&import.raw, path);
            match &resolution {
                Resolution::Resolved(_) => stats.resolved += 1,
                Resolution::External { .. } => stats.external += 1,
                Resolution::Unresolved { .. } => stats.unresolved += 1,
            }
            match (&import.alias, &resolution) {
                (Some(a), _) if a == "." => {
                    if let Resolution::Resolved(dir) = &resolution {
                        if let Some(id) = self.non_test_package(dir) {
                            dot_packages.push(id);
                        }
                    }
                }
                (Some(a), _) if a == "_" => {} // blank: no names visible
                (alias, resolution) => {
                    let qualifier_name = match alias {
                        Some(a) => a.clone(),
                        None => match resolution {
                            Resolution::Resolved(dir) => self
                                .non_test_package(dir)
                                .map_or_else(|| last_segment(&import.raw), |(_, name)| name),
                            _ => default_external_qualifier(&import.raw),
                        },
                    };
                    let target = match resolution {
                        Resolution::Resolved(ref dir) => match self.non_test_package(dir) {
                            Some(id) => QualifierTarget::Package(id),
                            None => QualifierTarget::External,
                        },
                        _ => QualifierTarget::External,
                    };
                    // First import wins on (illegal) qualifier collisions.
                    qualifiers.entry(qualifier_name).or_insert(target);
                }
            }
            // import_out fans out to every non-test file of the target
            // package (an import binds the package, not one file).
            if let Resolution::Resolved(dir) = &resolution {
                if let Some(id) = self.non_test_package(dir) {
                    if let Some(package) = self.packages.get(&id) {
                        for target_file in &package.files {
                            if target_file != path && !target_file.ends_with("_test.go") {
                                import_targets.push(target_file.clone());
                            }
                        }
                    }
                }
            }
            imports_out.push((import.clone(), resolution));
        }

        // --- B. reference binding -------------------------------------
        let mut refs_out = Vec::with_capacity(extracted.refs.len());
        let mut ref_targets: Vec<FilePath> = Vec::new();
        let refs = &extracted.refs;
        for r in refs {
            // A reference whose recorded qualifier names one of the file's
            // import scopes is a scoped selection (in-repo or external):
            // counted as package-scoped signal, then bound by rule below.
            if r.qualifier
                .as_ref()
                .is_some_and(|q| qualifiers.contains_key(q))
            {
                stats.package_qualifier_refs += 1;
            }
            let (binding, damped) =
                self.bind_one_ref(r, &qualifiers, own.as_ref(), src_is_test, &dot_packages);
            if damped {
                stats.names_skipped += 1;
            }
            if binding.targets.is_empty() {
                stats.refs_unbound += 1;
                if let Some(reason) = binding.unbound_reason {
                    *stats.unbound_reasons.entry(reason).or_insert(0) += 1;
                }
            } else {
                stats.refs_bound += 1;
                // Keep multiplicity: the ref_def dampener (ALGORITHM §6)
                // needs the reference COUNT per file pair, not the set.
                for target in &binding.targets {
                    if target.file != path {
                        ref_targets.push(target.file.clone());
                    }
                }
            }
            refs_out.push((r.clone(), binding));
        }

        // --- C. test affinity -----------------------------------------
        // Test files pair with every non-test .go file in the same
        // directory (LANGUAGES §6.1), bidirectionally.
        let mut affinity_targets = Vec::new();
        if src_is_test {
            let dir = dir_of(path).to_owned();
            if let Some(names) = self.package_dirs.get(&dir) {
                for name in names {
                    if name.starts_with("<<no-package>>") {
                        continue;
                    }
                    let Some(package) = self.packages.get(&(dir.clone(), name.clone())) else {
                        continue;
                    };
                    for target_file in &package.files {
                        if !target_file.ends_with("_test.go") && target_file != path {
                            affinity_targets.push(target_file.clone());
                        }
                    }
                }
            }
        }

        let package_id =
            own.unwrap_or_else(|| (dir_of(path).to_owned(), format!("<<no-package>>:{path}")));
        FileOutcome {
            resolution: FileResolution {
                package: package_id,
                imports: imports_out,
                refs: refs_out,
            },
            import_targets,
            affinity_targets,
            ref_targets,
        }
    }

    /// The binding for one reference under ADR-018's rule table plus the
    /// structural qualifier of the ADR-017 addendum. The boolean is the
    /// dampener: `true` when the name was uninformative (>256 candidates)
    /// and must be counted into `names_skipped`.
    fn bind_one_ref(
        &self,
        r: &cs_extract::Ref,
        qualifiers: &BTreeMap<String, QualifierTarget>,
        own: Option<&(String, String)>,
        src_is_test: bool,
        dot_packages: &[(String, String)],
    ) -> (SymbolBinding, bool) {
        let own_defs = |name: &str| -> Option<&Vec<DefLoc>> {
            own.and_then(|id| self.packages.get(id))
                .and_then(|p| p.defs.get(name))
        };
        let type_kind =
            |l: &DefLoc| matches!(l.kind, DefKind::Struct | DefKind::Interface | DefKind::Type);
        // The operand's scope, when the recorded qualifier names an import
        // of this file. `None` covers plain refs AND operands that are not
        // import names (locals/params/typoed packages) — distinguished by
        // `r.qualifier.is_some()` below.
        let qualifier_target = r.qualifier.as_ref().and_then(|q| qualifiers.get(q));

        match r.kind {
            RefKind::NameRef => {
                let own_hits = own_defs(&r.name).cloned().unwrap_or_default();
                let dot_hits = self.dot_hits(dot_packages, &r.name, &|_| true);
                Self::bind_or_reason(src_is_test, own_hits, dot_hits)
            }
            RefKind::TypeRef => match qualifier_target {
                Some(QualifierTarget::Package(id)) => {
                    let hits = self.qualified_hits(id, &r.name, &type_kind);
                    Self::capped_or_unbound(hits)
                }
                Some(QualifierTarget::External) => {
                    (SymbolBinding::unbound(UnboundReason::ExternalScope), false)
                }
                None if r.qualifier.is_some() => {
                    // `x.T` in type position where `x` names no import: the
                    // operand is a value (or a typo) — no package scope.
                    (SymbolBinding::unbound(UnboundReason::NoScope), false)
                }
                None => {
                    let candidates = own_defs(&r.name)
                        .map(|locs| locs.iter().filter(|l| type_kind(l)).cloned().collect())
                        .unwrap_or_default();
                    let dot_hits = self.dot_hits(dot_packages, &r.name, &type_kind);
                    Self::bind_or_reason(src_is_test, candidates, dot_hits)
                }
            },
            RefKind::CallRef | RefKind::FieldRef => {
                fn call_kind(l: &DefLoc) -> bool {
                    matches!(l.kind, DefKind::Function | DefKind::Var)
                }
                fn field_kind(l: &DefLoc) -> bool {
                    matches!(
                        l.kind,
                        DefKind::Var
                            | DefKind::Const
                            | DefKind::Type
                            | DefKind::Struct
                            | DefKind::Interface
                    )
                }
                match qualifier_target {
                    Some(QualifierTarget::Package(id)) => {
                        let kind_ok: &dyn Fn(&DefLoc) -> bool = if r.kind == RefKind::CallRef {
                            &call_kind
                        } else {
                            &field_kind
                        };
                        let hits = self.qualified_hits(id, &r.name, kind_ok);
                        Self::capped_or_unbound(hits)
                    }
                    Some(QualifierTarget::External) => {
                        (SymbolBinding::unbound(UnboundReason::ExternalScope), false)
                    }
                    None => {
                        // No import scope named the operand.
                        match (r.qualifier.is_some(), r.kind) {
                            // Computed operand (`w.Header().Add`,
                            // `arr[0].Close`): the receiver's package is
                            // unknowable syntactically — a call through it
                            // can never be claimed as a local method.
                            (false, RefKind::CallRef) => {
                                (SymbolBinding::unbound(UnboundReason::NoScope), false)
                            }
                            // Field access or method value on any operand:
                            // binding needs the receiver's type, which
                            // extraction deliberately does not compute.
                            (_, RefKind::FieldRef) => {
                                (SymbolBinding::unbound(UnboundReason::NeedsTypeInfo), false)
                            }
                            // Bare call through a plain identifier operand
                            // (`rp.Add`, `t.Run`): an instance call on some
                            // value of this package's world. Bind by unique
                            // method name — never names from Go's universal
                            // interface surface, which collide with stdlib
                            // methods on almost any receiver (the gin/chi
                            // audit's dominant FP class).
                            (true, RefKind::CallRef) if is_universe_method(&r.name) => {
                                (SymbolBinding::unbound(UnboundReason::UniverseMethod), false)
                            }
                            (true, RefKind::CallRef) => {
                                let methods = own
                                    .as_ref()
                                    .and_then(|id| self.packages.get(id))
                                    .and_then(|p| p.methods.get(&r.name))
                                    .map_or(&[][..], |v| v.as_slice());
                                let visible: Vec<&DefLoc> = methods
                                    .iter()
                                    .filter(|m| src_is_test || !m.file.ends_with("_test.go"))
                                    .collect();
                                let binding = match visible.len() {
                                    1 => SymbolBinding::bound(vec![visible[0].clone()]),
                                    0 => SymbolBinding::unbound(UnboundReason::NoCandidate),
                                    _ => SymbolBinding::unbound(UnboundReason::MethodAmbiguous),
                                };
                                (binding, false)
                            }
                            // Unreachable: NameRef and TypeRef are handled in
                            // their own arms above; the arm exists for
                            // exhaustiveness of the tuple match.
                            (_, RefKind::NameRef | RefKind::TypeRef) => {
                                unreachable!("NameRef/TypeRef are matched in their own arms")
                            }
                        }
                    }
                }
            }
        }
    }

    /// Exported defs of `name` in an in-repo package, kind-filtered.
    fn qualified_hits(
        &self,
        id: &(String, String),
        name: &str,
        kind_ok: &dyn Fn(&DefLoc) -> bool,
    ) -> Vec<DefLoc> {
        self.packages
            .get(id)
            .and_then(|p| p.exported.get(name))
            .map(|locs| locs.iter().filter(|l| kind_ok(l)).cloned().collect())
            .unwrap_or_default()
    }

    /// Exported defs of `name` contributed by dot-imported packages.
    fn dot_hits(
        &self,
        dot_packages: &[(String, String)],
        name: &str,
        kind_ok: &dyn Fn(&DefLoc) -> bool,
    ) -> Vec<DefLoc> {
        let mut hits = Vec::new();
        for dot in dot_packages {
            if let Some(p) = self.packages.get(dot) {
                hits.extend(
                    p.exported
                        .get(name)
                        .map(|locs| {
                            locs.iter()
                                .filter(|l| kind_ok(l))
                                .cloned()
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                );
            }
        }
        hits
    }

    /// Bind hits unless the name was uninformative (>256 candidates).
    /// Returns the binding and whether the dampener fired (counted by the
    /// caller into `names_skipped`).
    fn capped_or_unbound(hits: Vec<DefLoc>) -> (SymbolBinding, bool) {
        if hits.len() > MAX_CANDIDATES_PER_NAME {
            (SymbolBinding::unbound(UnboundReason::NoCandidate), true)
        } else if hits.is_empty() {
            (SymbolBinding::unbound(UnboundReason::NoCandidate), false)
        } else {
            (SymbolBinding::bound(hits), false)
        }
    }

    /// The `(dir, name)` identity of a file's own package.
    fn own_package_of(
        path: &str,
        extracted: &cs_extract::ExtractedFile,
    ) -> Option<(String, String)> {
        extracted
            .package_name
            .as_ref()
            .map(|name| (dir_of(path).to_owned(), name.clone()))
    }

    /// The non-test package of a directory (Go's rule: an import binds the
    /// package, and `foo_test` is a different, test-only package). `main`
    /// is unimportable and skipped when any other package exists in the
    /// directory; with a name conflict (invalid Go, broken repos), the
    /// lexicographically first non-test name wins, deterministically.
    fn non_test_package(&self, dir: &str) -> Option<(String, String)> {
        let names = self.package_dirs.get(dir)?;
        let eligible: Vec<&str> = names
            .iter()
            .map(String::as_str)
            .filter(|name| !name.ends_with("_test") && !name.contains("<<no-package>>"))
            .collect();
        let best = eligible
            .iter()
            .filter(|name| **name != "main")
            .min()
            .or_else(|| eligible.iter().min())?;
        Some((dir.to_owned(), (*best).to_owned()))
    }

    /// Resolve one import path per ADR-018 §3A.
    fn resolve_import(&self, raw: &str, from_file: &str) -> Resolution {
        let from_dir = dir_of(from_file);
        let own_module = self.nearest_module(from_dir);

        // Relative imports resolve against the importing file's directory.
        if raw == "." || raw == ".." || raw.starts_with("./") || raw.starts_with("../") {
            return match resolve_relative(from_dir, raw) {
                None => Resolution::Unresolved {
                    specifier: raw.to_owned(),
                    reason: crate::UnresolvedReason::EscapesRoot,
                },
                Some(dir) => self.dir_resolution(&dir, from_dir, &own_module, raw),
            };
        }

        // Own-module prefix mapping (exact root match first).
        if !own_module.path.is_empty() {
            if raw == own_module.path {
                return self.dir_resolution(&own_module.dir, from_dir, &own_module, raw);
            }
            let prefix = format!("{}/", own_module.path);
            if let Some(rest) = raw.strip_prefix(&prefix) {
                let candidate = join_dir(&own_module.dir, rest);
                return self.dir_resolution(&candidate, from_dir, &own_module, raw);
            }
        }

        // vendor/ applies to external dependencies only (never own-module
        // paths, which were handled above).
        if self.has_vendor {
            let vendor_dir = join_dir("vendor", raw);
            if self.package_dirs.contains_key(&vendor_dir) {
                return Resolution::Resolved(vendor_dir);
            }
        }

        // Stdlib heuristic: no dot in the first path segment. Everything
        // else is third-party. Both are External — correct outcomes, not
        // failures; the distinction is diagnostic context only.
        tracing::debug!(
            specifier = raw,
            stdlib = looks_like_stdlib(raw),
            "external import"
        );
        Resolution::External {
            specifier: raw.to_owned(),
        }
    }

    /// Classify a candidate directory: a package with files (but never one
    /// belonging to a different module — the chi `_examples` trap, and
    /// never an `internal/` tree the importer cannot see), or a documented
    /// miss.
    fn dir_resolution(
        &self,
        dir: &str,
        from_dir: &str,
        own_module: &ModuleInfo,
        raw: &str,
    ) -> Resolution {
        let nearest = self.nearest_module(dir);
        if &nearest != own_module {
            // The target directory belongs to a nested/other module.
            return Resolution::External {
                specifier: raw.to_owned(),
            };
        }
        // Go's internal rule: an `internal` element is visible only inside
        // the tree rooted at its parent. The code could not compile, so the
        // dependency does not exist for this importer.
        let candidate_rel = module_relative(dir, &own_module.dir);
        let from_rel = module_relative(from_dir, &own_module.dir);
        if internal_visibility_violation(candidate_rel, from_rel) {
            return Resolution::Unresolved {
                specifier: raw.to_owned(),
                reason: crate::UnresolvedReason::Internal,
            };
        }
        if self.package_dirs.contains_key(dir) {
            Resolution::Resolved(dir.to_owned())
        } else {
            Resolution::Unresolved {
                specifier: raw.to_owned(),
                reason: crate::UnresolvedReason::NotFound,
            }
        }
    }

    /// The deepest module whose root directory is `dir` or an ancestor of it.
    fn nearest_module(&self, dir: &str) -> ModuleInfo {
        // The root module (dir "") owns everything: formatting "{}/" for it
        // would yield "/", which no repo-relative path starts with.
        self.modules
            .iter()
            .find(|m| dir == m.dir || m.dir.is_empty() || dir.starts_with(&format!("{}/", m.dir)))
            .cloned()
            .unwrap_or_else(|| ModuleInfo {
                dir: String::new(),
                path: String::new(),
            })
    }

    /// Bind-or-reason for unqualified names: own-package hits vs dot-import
    /// hits, with the ambiguity rule between them. The boolean is the
    /// dampener flag (`>256` candidates ⇒ counted in `names_skipped`).
    ///
    /// Go build semantics apply BEFORE the ambiguity check: test files are
    /// compiled only into test binaries, so a non-test source can never
    /// reference a test-file def — and a name defined only in `_test.go`
    /// files therefore does not conflict with a dot import.
    fn bind_or_reason(
        src_is_test: bool,
        own_hits: Vec<DefLoc>,
        dot_hits: Vec<DefLoc>,
    ) -> (SymbolBinding, bool) {
        let own_hits: Vec<DefLoc> = own_hits
            .into_iter()
            .filter(|loc| src_is_test || !loc.file.ends_with("_test.go"))
            .collect();
        if !own_hits.is_empty() && !dot_hits.is_empty() {
            // Both scopes define the name; Go's file-vs-package precedence
            // is not modeled (zero real-world occurrences in the census).
            return (
                SymbolBinding::unbound(UnboundReason::AmbiguousDotImport),
                false,
            );
        }
        let mut hits = own_hits;
        hits.extend(dot_hits);
        if hits.is_empty() {
            (SymbolBinding::unbound(UnboundReason::NoCandidate), false)
        } else if hits.len() > MAX_CANDIDATES_PER_NAME {
            (SymbolBinding::unbound(UnboundReason::NoCandidate), true)
        } else {
            (SymbolBinding::bound(hits), false)
        }
    }
}

/// Method names from Go's pervasive stdlib interfaces (io, fmt, context,
/// net/http handler/transport, sort, testing's `t.Run`). A bare `x.Close()`
/// binds to nothing even when `Close` is unique in the package: the receiver
/// is more likely an external type implementing a stdlib interface than the
/// one local definition. Measured as the dominant FP class in the gin/chi
/// audit; `Run` was added after the audit caught `t.Run` binding to
/// `Engine.Run` in gin's test files.
///
/// Deliberately NOT listed, because the audit measured their bare-call
/// bindings as more often correct than not (receivers are frequently the
/// package's own router/context types): `Get`, `Set`, `Name`, `Next`,
/// `Find`. The remaining cost of this filter is documented false negatives
/// (`n.endpoints.Value()` on a local type) that file-level import and
/// affinity edges already cover.
const UNIVERSE_METHOD_NAMES: &[&str] = &[
    "Close",
    "Error",
    "Flush",
    "Format",
    "Header",
    "Len",
    "Less",
    "Partial",
    "Read",
    "ReadFrom",
    "RoundTrip",
    "Run",
    "Scan",
    "Seek",
    "ServeHTTP",
    "String",
    "Swap",
    "Value",
    "Write",
    "WriteString",
];

fn is_universe_method(name: &str) -> bool {
    UNIVERSE_METHOD_NAMES.binary_search(&name).is_ok()
}

fn last_segment(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module_path_of(content: &str) -> Option<String> {
        parse_module_path(content)
    }

    #[test]
    fn go_mod_parsing_handles_plain_quoted_and_comments() {
        assert_eq!(
            module_path_of("module example.com/m\n"),
            Some("example.com/m".to_owned())
        );
        assert_eq!(
            module_path_of("// c\nmodule \"example.com/m/v2\"\n\ngo 1.24\n"),
            Some("example.com/m/v2".to_owned())
        );
        // Inline comments and tab separators must not poison the path: a
        // poisoned module path breaks EVERY own-module import.
        assert_eq!(
            module_path_of("module example.com/m // staging\n"),
            Some("example.com/m".to_owned())
        );
        assert_eq!(
            module_path_of("\tmodule\t\"example.com/m\"\n"),
            Some("example.com/m".to_owned())
        );
        assert_eq!(module_path_of("go 1.24\nrequire x v1.0.0\n"), None);
    }

    #[test]
    fn major_version_suffixes_do_not_become_qualifiers() {
        // Go code writes `chi.NewRouter`, never `v5.NewRouter`.
        assert_eq!(
            default_external_qualifier("github.com/go-chi/chi/v5"),
            "chi"
        );
        assert_eq!(default_external_qualifier("example.org/mod/v2"), "mod");
        assert_eq!(default_external_qualifier("github.com/x/y"), "y");
        assert_eq!(default_external_qualifier("fmt"), "fmt");
        // A path ending in a non-major `v`-prefixed segment is untouched.
        assert_eq!(default_external_qualifier("example.org/vapor"), "vapor");
        assert_eq!(default_external_qualifier("example.org/v"), "v");
    }

    #[test]
    fn internal_visibility_follows_go_scope() {
        // Module-root internal: visible module-wide.
        assert!(!internal_visibility_violation(
            "internal/auth",
            "somewhere/else"
        ));
        // Nested internal: only within the parent tree.
        assert!(!internal_visibility_violation("a/b/internal/x", "a/b"));
        assert!(!internal_visibility_violation("a/b/internal/x", "a/b/c"));
        assert!(internal_visibility_violation("a/b/internal/x", "a"));
        assert!(internal_visibility_violation("a/b/internal/x", "outside"));
        // No internal element: no rule.
        assert!(!internal_visibility_violation("a/b/c", "zzz"));
    }

    #[test]
    fn relative_imports_normalize_and_refuse_to_escape() {
        assert_eq!(resolve_relative("a/b", "./c"), Some("a/b/c".to_owned()));
        assert_eq!(resolve_relative("a/b", "../c"), Some("a/c".to_owned()));
        assert_eq!(resolve_relative("a", "../.."), None);
        assert_eq!(resolve_relative("", "./x"), Some("x".to_owned()));
    }

    #[test]
    fn stdlib_heuristic_uses_first_segment_dots() {
        assert!(looks_like_stdlib("fmt"));
        assert!(looks_like_stdlib("net/http"));
        assert!(!looks_like_stdlib("github.com/x/y"));
    }
}
