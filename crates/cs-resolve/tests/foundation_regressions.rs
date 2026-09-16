//! Foundation-regression integration tests for the Go resolver: end-to-end
//! assertions for the audit's §9 matrix items that the fixture matrix does
//! not already pin. Each test states the failure mode it must keep dead,
//! and is built from inline sources (not the shared fixture tree) so the
//! scenario stays exactly as narrow as the regression it guards.

use cs_extract::RefKind;
use cs_resolve::{GoResolver, LanguageResolver, ResolveSnapshot, ResolvedRepo, UnboundReason};

/// Extract and resolve an inline multi-file snapshot. A `go.mod` entry
/// becomes a module manifest; every other entry is extracted as Go source —
/// the same handoff the index stage performs.
fn resolve_sources(files: &[(&str, &str)]) -> ResolvedRepo {
    let mut snapshot_files = Vec::new();
    let mut manifests = Vec::new();
    for (path, source) in files {
        if *path == "go.mod" {
            manifests.push(((*path).to_owned(), (*source).to_owned()));
            continue;
        }
        let extracted = cs_extract::extract(source, cs_scanner::Language::Go)
            .expect("go extraction never fails");
        snapshot_files.push(((*path).to_owned(), extracted));
    }
    let snapshot = ResolveSnapshot::new(snapshot_files, manifests);
    let resolver = GoResolver::prepare(&snapshot);
    resolver.resolve(&snapshot)
}

/// The binding of the first ref named `name` with kind `kind` in `path`.
fn binding_of(
    repo: &ResolvedRepo,
    path: &str,
    name: &str,
    kind: RefKind,
) -> cs_resolve::SymbolBinding {
    repo.files
        .get(path)
        .unwrap_or_else(|| panic!("{path} missing from resolution output"))
        .refs
        .iter()
        .find(|(r, _)| r.name == name && r.kind == kind)
        .map_or_else(
            || panic!("ref {name:?} ({kind:?}) not found in {path}"),
            |(_, b)| b.clone(),
        )
}

/// The deleted `preceding_qualifier` heuristic bound `Session` to the
/// `auth` package purely because the identifier `auth` ended two bytes
/// (`, `) before it started. The qualifier now rides structurally on the
/// reference, so comma-separated call arguments can never form a package
/// qualification: `Session` must stay honestly unbound, the `auth`
/// argument must survive as a plain name ref (not be dropped for matching
/// an import name), and no `ref_def` edge may be manufactured across the
/// package boundary — only the import's own edge may cross it.
#[test]
fn comma_separated_arguments_never_form_a_package_qualifier() {
    let repo = resolve_sources(&[
        ("go.mod", "module example.com/m\n"),
        (
            "auth/auth.go",
            "package auth\n\n// Session is exported: the old byte-adjacency heuristic bound\n// consumer's `Session{}` literal against it.\ntype Session struct{}\n",
        ),
        (
            "consumer/main.go",
            "package consumer\n\nimport \"example.com/m/auth\"\n\nfunc run(auth string) {\n\tcheck(auth, Session{})\n}\n\nfunc check(s string, v struct{}) {}\n",
        ),
    ]);

    // `Session` is a composite-literal type with NO structural qualifier:
    // byte adjacency to the `auth` argument must not manufacture one.
    let session = binding_of(&repo, "consumer/main.go", "Session", RefKind::TypeRef);
    assert!(
        session.targets.iter().all(|t| t.file != "auth/auth.go"),
        "comma adjacency must not bind into the auth package: {session:?}"
    );
    assert_eq!(
        session.unbound_reason,
        Some(UnboundReason::NoCandidate),
        "no consumer-scoped Session exists, so the ref stays honestly unbound"
    );

    // The `auth` ARGUMENT is a genuine name use, not a qualifier: it must
    // not be dropped for matching an import name, and it must not resolve
    // into the auth package either.
    let arg = binding_of(&repo, "consumer/main.go", "auth", RefKind::NameRef);
    assert!(
        arg.targets.iter().all(|t| t.file != "auth/auth.go"),
        "the argument must not resolve into the auth package: {arg:?}"
    );

    // The import itself still resolves: the only cross-package edge is the
    // import's, never a fabricated reference.
    assert!(
        repo.edges.iter().any(|e| e.src == "consumer/main.go"
            && e.dst == "auth/auth.go"
            && e.kind == cs_resolve::EdgeKind::ImportOut),
        "the import edge must survive"
    );
    assert!(
        !repo.edges.iter().any(|e| e.src == "consumer/main.go"
            && e.dst == "auth/auth.go"
            && e.kind == cs_resolve::EdgeKind::RefDef),
        "no ref_def edge may be manufactured from byte adjacency"
    );
}

/// The dot-import ambiguity check must run AFTER Go's test-visibility
/// rule. A name defined only in a `_test.go` file of the dot-importing
/// package does not conflict for non-test files (Go build semantics hide
/// test defs from them), so `Some` binds through the dot import. The
/// internal test file itself, which DOES see both definitions, stays
/// honestly ambiguous — proving the check still runs, just ordered right.
#[test]
fn dot_import_ambiguity_is_decided_after_test_visibility() {
    let repo = resolve_sources(&[
        // Tab-indented, quoted module line with an inline comment: the
        // manifest parser must not be poisoned by any of it (F-10), or the
        // dot import below would stop resolving through the own module.
        ("go.mod", "\tmodule\t\"example.com/m\" // staging\n"),
        ("vals/vals.go", "package vals\n\nfunc Some() int { return 1 }\n"),
        (
            "dotted/user.go",
            "package user\n\nimport . \"example.com/m/vals\"\n\nfunc Use() int {\n\treturn Some()\n}\n",
        ),
        (
            "dotted/helper_test.go",
            "package user\n\nimport . \"example.com/m/vals\"\n\nfunc Some() int { return 4 }\n\nfunc TestSome() {\n\t_ = Some()\n}\n",
        ),
    ]);

    // Non-test source: the same-package candidate lives only in _test.go,
    // which a non-test file can never see — so the dot import binds, and
    // the name is not ruled ambiguous.
    let from_source = binding_of(&repo, "dotted/user.go", "Some", RefKind::NameRef);
    assert_eq!(from_source.targets.len(), 1, "exactly the dot-imported def");
    assert_eq!(from_source.targets[0].file, "vals/vals.go");
    assert_eq!(
        from_source.unbound_reason, None,
        "a test-only shadow must not create ambiguity for non-test files"
    );

    // The internal test file sees BOTH definitions: the ambiguity reason
    // must fire there, or the visibility filter has drifted the other way.
    let from_test = binding_of(&repo, "dotted/helper_test.go", "Some", RefKind::NameRef);
    assert_eq!(
        from_test.unbound_reason,
        Some(UnboundReason::AmbiguousDotImport),
        "the test file itself sees both defs and stays ambiguous"
    );
}

/// A cmd-style directory whose only package is `main` must resolve with
/// its real identity `(dir, "main")` — never a pseudo-package — and its
/// same-package names must bind inside the command. The fixture matrix
/// pins that `main` does not hijack a MIXED directory; this pins the
/// complement: a main-ONLY directory is a valid package, not a degenerate
/// one, and produces no phantom edges.
#[test]
fn package_main_only_directory_keeps_sane_identity() {
    let repo = resolve_sources(&[
        ("go.mod", "module example.com/m\n"),
        (
            "cmd/tool/main.go",
            "package main\n\nfunc main() {\n\tif run() != 0 {\n\t\tpanic(\"boom\")\n\t}\n}\n\nfunc run() int { return 0 }\n",
        ),
    ]);

    let resolution = repo
        .files
        .get("cmd/tool/main.go")
        .expect("a main-only directory must resolve");
    assert_eq!(
        resolution.package,
        ("cmd/tool".to_owned(), "main".to_owned()),
        "identity comes from the package clause, not a pseudo-package"
    );

    // Same-package binding works inside a command: `run` binds to the
    // command's own definition.
    let run = binding_of(&repo, "cmd/tool/main.go", "run", RefKind::NameRef);
    assert_eq!(run.targets.len(), 1, "the local run definition");
    assert_eq!(run.targets[0].file, "cmd/tool/main.go");
    assert_eq!(run.unbound_reason, None);

    // Universe builtins never become refs, and a self-contained command
    // manufactures no edges at all.
    assert!(
        !resolution.refs.iter().any(|(r, _)| r.name == "panic"),
        "universe builtins must not leak into refs"
    );
    assert!(
        repo.edges.is_empty(),
        "no phantom edges from a self-contained command: {:?}",
        repo.edges
    );
}
