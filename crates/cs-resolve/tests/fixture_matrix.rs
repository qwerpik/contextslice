//! The fixture-matrix spec: every import class and binding rule of ADR-018,
//! asserted against the synthetic monorepo in `fixtures/go-resolve/`.
//! Each assertion names the rule it pins; the matrix is the resolver's
//! executable specification.

mod common;

use common::build_snapshot;
use cs_extract::RefKind;
use cs_resolve::{GoResolver, LanguageResolver, Resolution, UnboundReason, UnresolvedReason};

use crate::common::matrix_root;

fn resolved() -> cs_resolve::ResolvedRepo {
    let snapshot = build_snapshot(&matrix_root());
    let resolver = GoResolver::prepare(&snapshot);
    resolver.resolve(&snapshot)
}

fn file<'a>(repo: &'a cs_resolve::ResolvedRepo, path: &str) -> &'a cs_resolve::FileResolution {
    repo.files
        .get(path)
        .unwrap_or_else(|| panic!("{path} missing from resolution output"))
}

fn import_resolution(repo: &cs_resolve::ResolvedRepo, path: &str, raw: &str) -> Resolution {
    let f = file(repo, path);
    f.imports
        .iter()
        .find(|(import, _)| import.raw == raw)
        .map_or_else(
            || panic!("import {raw:?} not found in {path}"),
            |(_, resolution)| resolution.clone(),
        )
}

/// The binding of the `n`-th ref named `name` (kind-filtered) in `path`.
fn binding_of(
    repo: &cs_resolve::ResolvedRepo,
    path: &str,
    name: &str,
    kind: RefKind,
) -> cs_resolve::SymbolBinding {
    let f = file(repo, path);
    f.refs
        .iter()
        .find(|(r, _)| r.name == name && r.kind == kind)
        .map_or_else(
            || panic!("ref {name:?} ({kind:?}) not found in {path}"),
            |(_, b)| b.clone(),
        )
}

fn assert_bound_to_file(binding: &cs_resolve::SymbolBinding, expected_file: &str) {
    assert!(
        !binding.targets.is_empty(),
        "expected a binding, got {binding:?}"
    );
    for target in &binding.targets {
        assert_eq!(
            target.file, expected_file,
            "binding went to the wrong file: {binding:?}"
        );
    }
}

// ---------------------------------------------------------------- imports

#[test]
fn own_module_imports_resolve_to_directories() {
    let repo = resolved();
    assert_eq!(
        import_resolution(&repo, "main.go", "example.com/m/v2/auth"),
        Resolution::Resolved("auth".to_owned())
    );
    assert_eq!(
        import_resolution(&repo, "main.go", "example.com/m/v2/internal/fs"),
        Resolution::Resolved("internal/fs".to_owned())
    );
    // The root package itself (exact module-path match).
    assert_eq!(
        import_resolution(&repo, "nested/n.go", "example.com/m/v2/auth"),
        Resolution::External {
            specifier: "example.com/m/v2/auth".to_owned()
        },
        "the chi trap: an in-repo-LOOKING import from a nested module must be External"
    );
}

#[test]
fn missing_dir_is_not_found_not_external() {
    let repo = resolved();
    assert_eq!(
        import_resolution(&repo, "main.go", "example.com/m/v2/nothere"),
        Resolution::Unresolved {
            specifier: "example.com/m/v2/nothere".to_owned(),
            reason: UnresolvedReason::NotFound,
        }
    );
}

#[test]
fn stdlib_and_third_party_are_external() {
    let repo = resolved();
    assert_eq!(
        import_resolution(&repo, "main.go", "fmt"),
        Resolution::External {
            specifier: "fmt".to_owned()
        }
    );
    assert_eq!(
        import_resolution(&repo, "auth/session.go", "errors"),
        Resolution::External {
            specifier: "errors".to_owned()
        }
    );
}

#[test]
fn vendored_dependency_resolves_into_vendor() {
    let repo = resolved();
    assert_eq!(
        import_resolution(&repo, "main.go", "example.org/dep"),
        Resolution::Resolved("vendor/example.org/dep".to_owned())
    );
}

#[test]
fn relative_imports_resolve_and_escape_is_reported() {
    let repo = resolved();
    assert_eq!(
        import_resolution(&repo, "rel/rel.go", "./sub"),
        Resolution::Resolved("rel/sub".to_owned())
    );
    assert_eq!(
        import_resolution(&repo, "rel2/esc.go", "../.."),
        Resolution::Unresolved {
            specifier: "../..".to_owned(),
            reason: UnresolvedReason::EscapesRoot,
        }
    );
}

// ------------------------------------------------------------ package ids

#[test]
fn package_identity_is_dir_plus_package_clause_name() {
    let repo = resolved();
    assert_eq!(
        file(&repo, "main.go").package,
        (String::new(), "m".to_owned())
    );
    // Nested module: same tree, different module, identity unchanged.
    assert_eq!(
        file(&repo, "nested/n.go").package,
        ("nested".to_owned(), "nested".to_owned())
    );
    // The external test package is a DIFFERENT package in the same dir.
    assert_eq!(
        file(&repo, "auth/extern_test.go").package,
        ("auth".to_owned(), "auth_test".to_owned())
    );
    // Files without a package clause become singleton pseudo-packages.
    let nopkg = file(&repo, "nopkg/nopkg.go");
    assert!(
        nopkg.package.1.starts_with("<<no-package>>"),
        "{:?}",
        nopkg.package
    );
}

// -------------------------------------------------------------- bindings

#[test]
fn qualifier_comes_from_alias_or_package_clause_never_path_tail() {
    let repo = resolved();
    // fs is an ALIAS: `fs "example.com/m/v2/internal/fs"` — the qualifier
    // `fs` binds the internal/fs package whose clause name is also fs.
    let fs_call = binding_of(&repo, "main.go", "Exists", RefKind::CallRef);
    assert_bound_to_file(&fs_call, "internal/fs/fs.go");
    // helpersvc is an ALIAS for dirhelper/, whose clause name is helpersvc
    // too — but even unaliased, the clause name would win over the tail.
    let h = binding_of(&repo, "main.go", "H", RefKind::CallRef);
    assert_bound_to_file(&h, "dirhelper/helpers.go");
}

#[test]
fn package_qualified_calls_bind_to_exported_defs() {
    let repo = resolved();
    let login = binding_of(&repo, "main.go", "Login", RefKind::CallRef);
    assert_bound_to_file(&login, "auth/session.go");
    assert_eq!(login.targets[0].qual_name, "Login");
    let do_ = binding_of(&repo, "main.go", "Do", RefKind::CallRef);
    assert_bound_to_file(&do_, "vendor/example.org/dep/dep.go");
}

#[test]
fn external_package_selectors_are_external_scope() {
    let repo = resolved();
    let println = binding_of(&repo, "main.go", "Println", RefKind::CallRef);
    assert_eq!(println.unbound_reason, Some(UnboundReason::ExternalScope));
    // Qualified type through an external package: io.Writer.
    let writer = binding_of(&repo, "types/types.go", "Writer", RefKind::TypeRef);
    assert_eq!(writer.unbound_reason, Some(UnboundReason::ExternalScope));
}

#[test]
fn same_package_names_bind_across_files() {
    let repo = resolved();
    // Version is defined in main.go; the reference lives in util.go.
    let version = binding_of(&repo, "util.go", "Version", RefKind::NameRef);
    assert_bound_to_file(&version, "main.go");
    // Login referenced from the internal test file, same package.
    let login = binding_of(&repo, "auth/session_test.go", "Login", RefKind::NameRef);
    assert_bound_to_file(&login, "auth/session.go");
}

#[test]
fn external_test_package_never_same_package_binds() {
    let repo = resolved();
    // extern_test.go references Login ONLY through the qualified import.
    let login = binding_of(&repo, "auth/extern_test.go", "Login", RefKind::CallRef);
    assert_bound_to_file(&login, "auth/session.go");
    // And its unqualified refs have no same-package candidates at all
    // (package auth_test defines nothing here).
    for (r, b) in &file(&repo, "auth/extern_test.go").refs {
        if r.kind == RefKind::NameRef {
            assert!(
                b.targets.iter().all(|t| t.file != "auth/extern_test.go"),
                "no same-package binding should exist: {r:?} -> {b:?}"
            );
        }
    }
}

#[test]
fn build_tag_variants_all_bind() {
    let repo = resolved();
    let decode = binding_of(&repo, "tags/user.go", "Decode", RefKind::NameRef);
    let mut files: Vec<&str> = decode.targets.iter().map(|t| t.file.as_str()).collect();
    files.sort_unstable();
    assert_eq!(
        files,
        vec!["tags/a.go", "tags/b.go", "tags/c.go", "tags/d.go"],
        "all four tag variants are legitimate targets"
    );
}

#[test]
fn unique_methods_bind_and_ambiguous_methods_do_not() {
    let repo = resolved();
    let check = binding_of(&repo, "meth/use.go", "Check", RefKind::CallRef);
    assert_bound_to_file(&check, "meth/types.go");
    assert_eq!(check.targets[0].qual_name, "C.Check");
    // Two distinct refs named Name (lines differ) — both ambiguous.
    let names: Vec<_> = file(&repo, "meth/use.go")
        .refs
        .iter()
        .filter(|(r, _)| r.name == "Name" && r.kind == RefKind::CallRef)
        .collect();
    assert_eq!(names.len(), 2);
    for (_, b) in names {
        assert_eq!(b.unbound_reason, Some(UnboundReason::MethodAmbiguous));
    }
}

#[test]
fn chained_selector_calls_are_never_local_methods() {
    // `Make().Solo()` — the operand is a computed expression, so the call
    // cannot be claimed as this package's method even though `Solo` is
    // unique here (the chi `w.Header().Add` → `RouteParams.Add` trap).
    let repo = resolved();
    let chained = binding_of(&repo, "meth/use.go", "Solo", RefKind::CallRef);
    // The FIRST Solo ref in source order is the chained one.
    assert_eq!(chained.unbound_reason, Some(UnboundReason::NoScope));
    // The identifier-operand call `d.Solo()` binds normally.
    let direct: Vec<_> = file(&repo, "meth/use.go")
        .refs
        .iter()
        .filter(|(r, _)| r.name == "Solo" && r.kind == RefKind::CallRef)
        .collect();
    assert_eq!(direct.len(), 2, "chained + identifier-operand call");
    assert_bound_to_file(&direct[1].1, "meth/types.go");
}

#[test]
fn unknown_receiver_testing_surface_names_stay_unbound() {
    // `t.Run("x", nil)` — `Run` is unique in the meth package, but `t` is
    // an unknown receiver: the name is on testing.T's surface (the gin
    // `t.Run` → `Engine.Run` trap), so it stays unbound.
    let repo = resolved();
    let run = binding_of(&repo, "meth/use.go", "Run", RefKind::CallRef);
    assert_eq!(run.unbound_reason, Some(UnboundReason::UniverseMethod));
}

#[test]
fn major_version_imports_qualify_by_module_name_not_version() {
    // `import "github.com/x/inner/v2"` + `inner.Thing()`: the qualifier is
    // `inner` (the module name), never `v2` — so the selector resolves to
    // an external scope instead of falling through as an unknown operand.
    let repo = resolved();
    let thing = binding_of(&repo, "main.go", "Thing", RefKind::CallRef);
    assert_eq!(thing.unbound_reason, Some(UnboundReason::ExternalScope));
}

#[test]
fn internal_packages_follow_go_visibility() {
    let repo = resolved();
    // Inside the parent tree: the import resolves and binds.
    assert_eq!(
        import_resolution(
            &repo,
            "deep/tools/tool.go",
            "example.com/m/v2/deep/tools/internal/priv"
        ),
        Resolution::Resolved("deep/tools/internal/priv".to_owned())
    );
    let grant = binding_of(&repo, "deep/tools/tool.go", "Grant", RefKind::CallRef);
    assert_bound_to_file(&grant, "deep/tools/internal/priv/priv.go");
    // Outside the parent tree: the import could not compile — Unresolved,
    // and nothing binds through it.
    assert_eq!(
        import_resolution(
            &repo,
            "outside/o.go",
            "example.com/m/v2/deep/tools/internal/priv"
        ),
        Resolution::Unresolved {
            specifier: "example.com/m/v2/deep/tools/internal/priv".to_owned(),
            reason: UnresolvedReason::Internal,
        }
    );
    let sneak = binding_of(&repo, "outside/o.go", "Grant", RefKind::CallRef);
    assert_eq!(sneak.unbound_reason, Some(UnboundReason::ExternalScope));
}

#[test]
fn package_main_does_not_hijack_its_directory() {
    // `mixed/` holds `package main` and `package util`; importers must
    // bind the importable `util`, never the command.
    let repo = resolved();
    let mixed = binding_of(&repo, "main.go", "Mixed", RefKind::CallRef);
    assert_bound_to_file(&mixed, "mixed/util.go");
    // And no edge from the importer lands in the main-package file.
    assert!(!repo
        .edges
        .iter()
        .any(|e| e.src == "main.go" && e.dst == "mixed/main.go"));
}

#[test]
fn universe_method_names_never_bind_bare_calls() {
    let repo = resolved();
    // F.Close is unique in the meth package, but `Close` is on Go's
    // universal interface surface: the bare call must stay unbound.
    let close = binding_of(&repo, "meth/use.go", "Close", RefKind::CallRef);
    assert_eq!(close.unbound_reason, Some(UnboundReason::UniverseMethod));
}

#[test]
fn bare_field_accesses_and_method_values_need_type_info() {
    let repo = resolved();
    let field = binding_of(&repo, "meth/use.go", "Field", RefKind::FieldRef);
    assert_eq!(field.unbound_reason, Some(UnboundReason::NeedsTypeInfo));
    // c.Check as a method value (not called) is a field_ref.
    let value = binding_of(&repo, "meth/use.go", "Check", RefKind::FieldRef);
    assert_eq!(value.unbound_reason, Some(UnboundReason::NeedsTypeInfo));
}

#[test]
fn dot_import_binds_and_conflicts_are_ambiguous() {
    let repo = resolved();
    let some = binding_of(&repo, "dotted/user.go", "Some", RefKind::NameRef);
    assert_bound_to_file(&some, "vals/vals.go");
    let conflict = binding_of(&repo, "dotted/user.go", "Conflict", RefKind::NameRef);
    assert_eq!(
        conflict.unbound_reason,
        Some(UnboundReason::AmbiguousDotImport)
    );
}

#[test]
fn type_refs_bind_unqualified_and_qualified() {
    let repo = resolved();
    let widget = binding_of(&repo, "main.go", "Widget", RefKind::TypeRef);
    assert_bound_to_file(&widget, "types/types.go");
    let reader = binding_of(&repo, "types/types.go", "Reader", RefKind::TypeRef);
    assert_bound_to_file(&reader, "types/types.go");
}

#[test]
fn package_scoped_refs_carry_structural_qualifiers() {
    let repo = resolved();
    // ADR-017 addendum: the qualifier of `pkg.Foo` rides on the reference
    // itself; the operand is never an independent ref.
    let f = file(&repo, "main.go");
    let login = binding_of(&repo, "main.go", "Login", RefKind::CallRef);
    assert_bound_to_file(&login, "auth/session.go");
    let login_ref = f
        .refs
        .iter()
        .find(|(r, _)| r.name == "Login" && r.kind == RefKind::CallRef)
        .expect("login ref");
    assert_eq!(login_ref.0.qualifier.as_deref(), Some("auth"));
    let println = f
        .refs
        .iter()
        .find(|(r, _)| r.name == "Println" && r.kind == RefKind::CallRef)
        .expect("println ref");
    assert_eq!(println.0.qualifier.as_deref(), Some("fmt"));
    // Instance receivers are recorded the same way — scope is the
    // resolver's decision, not extraction's.
    let exists = f
        .refs
        .iter()
        .find(|(r, _)| r.name == "Exists" && r.kind == RefKind::CallRef)
        .expect("fs call");
    assert_eq!(exists.0.qualifier.as_deref(), Some("fs"));
    assert!(
        repo.stats.package_qualifier_refs >= 7,
        "package-scoped refs must be counted, got {}",
        repo.stats.package_qualifier_refs
    );
}

#[test]
fn partial_files_participate() {
    let repo = resolved();
    // The broken file still belongs to its package and resolves without
    // failing the run; its clean facts participate (the extraction milestone
    // pinned what those are — here we assert identity and no crash).
    assert_eq!(
        file(&repo, "partial/broken.go").package,
        ("partial".to_owned(), "partial".to_owned())
    );
    assert!(!repo
        .edges
        .iter()
        .any(|e| e.src == "partial/broken.go" && e.dst == "partial/broken.go"));
}

// ------------------------------------------------------------------ edges

#[test]
fn edges_cover_import_refdef_and_test_affinity() {
    let repo = resolved();
    let has = |src: &str, dst: &str, kind: cs_resolve::EdgeKind| {
        repo.edges
            .iter()
            .any(|e| e.src == src && e.dst == dst && e.kind == kind)
    };
    assert!(has(
        "main.go",
        "auth/session.go",
        cs_resolve::EdgeKind::ImportOut
    ));
    // Internal test files share the package identity but are NOT part of
    // the importable package (Go build semantics): no import_out edge may
    // end at a *_test.go file.
    assert!(!repo
        .edges
        .iter()
        .any(|e| e.kind == cs_resolve::EdgeKind::ImportOut && e.dst.ends_with("_test.go")));
    assert!(has(
        "main.go",
        "auth/session.go",
        cs_resolve::EdgeKind::RefDef
    ));
    assert!(has(
        "auth/session_test.go",
        "auth/session.go",
        cs_resolve::EdgeKind::TestAffinity
    ));
    assert!(
        has(
            "auth/session.go",
            "auth/session_test.go",
            cs_resolve::EdgeKind::TestAffinity
        ),
        "test affinity is bidirectional"
    );
    // The external test package also pairs with the dir's non-test files.
    assert!(has(
        "auth/extern_test.go",
        "auth/session.go",
        cs_resolve::EdgeKind::TestAffinity
    ));
    // Ref edges fan to every tag variant with the damped weight.
    for variant in ["tags/a.go", "tags/b.go", "tags/c.go", "tags/d.go"] {
        let e = repo
            .edges
            .iter()
            .find(|e| {
                e.src == "tags/user.go"
                    && e.dst == variant
                    && e.kind == cs_resolve::EdgeKind::RefDef
            })
            .unwrap_or_else(|| panic!("missing ref edge to {variant}"));
        // ref_def_weight(1) = 0.5 * sqrt(1)/sqrt(9) (ALGORITHM §6).
        assert!(
            (e.weight - 0.5f64 / 3.0).abs() < 1e-9,
            "one ref, damped: {e:?}"
        );
    }
    // Never any self-edges.
    assert!(!repo.edges.iter().any(|e| e.src == e.dst));
    // No edges into the nested module from root-module resolution (its
    // imports were External).
    assert!(!repo
        .edges
        .iter()
        .any(|e| e.src == "nested/n.go" && e.dst == "auth/session.go"));
}
