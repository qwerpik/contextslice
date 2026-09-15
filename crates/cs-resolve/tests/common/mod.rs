//! Shared helpers for cs-resolve integration tests: build a
//! [`ResolveSnapshot`] from a directory of `.go` files plus `go.mod`
//! manifests, exactly the way the future index stage will.

use std::path::Path;

use cs_extract::{extract, ExtractedFile};
use cs_resolve::ResolveSnapshot;
use cs_scanner::Language;

// `.go` comparisons in this module are case-sensitive on purpose (Go
// tooling rejects `.GO`), which clippy's lint does not model.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
pub fn build_snapshot(root: &Path) -> ResolveSnapshot {
    let mut files = Vec::new();
    let mut manifests = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|e| e.expect("readable").path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name == ".git" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if name == "go.mod" {
                let content = std::fs::read_to_string(&path).expect("go.mod readable");
                let rel = rel_path(root, &path);
                manifests.push((rel, content));
                // Go tooling is case-sensitive: `.GO` is not a Go file.
            } else if name.ends_with(".go") {
                let source = std::fs::read_to_string(&path).expect("fixture readable");
                let extracted: ExtractedFile =
                    extract(&source, Language::Go).expect("go extraction never fails");
                files.push((rel_path(root, &path), extracted));
            }
        }
    }
    ResolveSnapshot::new(files, manifests)
}

/// Repo-relative, `/`-separated path (the `FilePath` contract).
pub fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .expect("inside root")
        .to_str()
        .expect("utf-8")
        .replace('\\', "/")
}

/// `fixtures/go-resolve` as a path. (Not referenced by every test binary
/// that links this module, hence the allow.)
#[allow(dead_code)]
pub fn matrix_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/go-resolve")
}
