//! Repository traversal, language detection, and content hashing.
//!
//! Implements `cs-scanner` from ARCHITECTURE.md §4.1: it turns a repository root
//! into a deterministic, ordered list of [`ScannedFile`] facts that the rest of
//! the pipeline (extract → resolve → index) consumes.
//!
//! # Determinism
//!
//! The walk is single-threaded and entries are sorted by their normalized
//! `/`-separated path string before any decision is made, so two runs over the
//! same tree produce byte-identical output (ARCHITECTURE §7: order-normalize
//! before any persisting decision). Data parallelism, if the cold-index budget
//! ever demands it, belongs to the index stage and must reuse the same
//! sort-before-persist rule.
//!
//! # Not a security boundary
//!
//! The scanner reads whatever the user points it at. Path confinement and
//! symlink-escape rejection are enforced by the *open* path (SECURITY.md §6),
//! not here; the walker does not follow symlinks at all, which makes a symlink
//! loop impossible by construction rather than by detection.

#![forbid(unsafe_code)]

mod lang;

pub use lang::{detect_language, Language};

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default per-file parse cap (ARCHITECTURE §4.1): files larger than this are
/// listed and hashed, but never parsed.
pub const DEFAULT_PARSE_CAP: u64 = 1024 * 1024;

/// Default per-file read cap (ARCHITECTURE §4.1): files larger than this are
/// listed with their size but never read at all, so they carry no hash.
pub const DEFAULT_READ_CAP: u64 = 50 * 1024 * 1024;

/// Default total-bytes cap for a single index run (SECURITY.md §6).
pub const DEFAULT_TOTAL_BYTES_CAP: u64 = 2 * 1024 * 1024 * 1024;

/// Default total-file cap for a single index run (SECURITY.md §6).
pub const DEFAULT_TOTAL_FILES_CAP: u64 = 500_000;

/// Why a file carries no hash, or is excluded from parsing.
///
/// Every degradation is *labeled*, never silent (ARCHITECTURE §2.3); the
/// reason travels in [`ScannedFile::skip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Larger than the configured read cap: listed with its size, never read,
    /// therefore never hashed and never parsed.
    TooLarge,
    /// Larger than the parse cap but within the read cap: hashed for
    /// change detection, never parsed (ARCHITECTURE §4.1's 1 MiB–50 MiB
    /// band — the hash keys incremental indexing even without extraction).
    ParseSkipped,
    /// The file could not be read (permissions, race, I/O error).
    Unreadable,
}

/// One repository file as the scanner sees it.
///
/// `path` is always repo-relative and `/`-separated (ARCHITECTURE §5), so it is
/// stable across platforms and safe to use as a database key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScannedFile {
    /// Repo-relative, `/`-separated, normalized path.
    pub path: String,
    /// Detected language, or [`Language::Unknown`] when no adapter claims it.
    pub lang: Language,
    /// Size in bytes, as reported by the filesystem.
    pub size: u64,
    /// Modification time as seconds since the Unix epoch; `0` when the
    /// filesystem does not report one (ARCHITECTURE §5, `files.mtime`).
    pub mtime: i64,
    /// blake3 content hash, or `None` when the file was listed but not read.
    pub hash: Option<[u8; 32]>,
    /// Why the file was not hashed, when `hash` is `None`.
    pub skip: Option<SkipReason>,
}

/// Scanner configuration.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// Files above this size are hashed but never parsed.
    pub parse_cap: u64,
    /// Files above this size are listed but never read (no hash).
    pub read_cap: u64,
    /// Hard cap on files per run; exceeding it is an error, not a truncation.
    pub max_files: u64,
    /// Hard cap on total bytes considered per run.
    pub max_total_bytes: u64,
    /// Extra ignore-file names, applied on top of the built-in defaults.
    pub extra_ignore_files: Vec<String>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            parse_cap: DEFAULT_PARSE_CAP,
            read_cap: DEFAULT_READ_CAP,
            max_files: DEFAULT_TOTAL_FILES_CAP,
            max_total_bytes: DEFAULT_TOTAL_BYTES_CAP,
            extra_ignore_files: Vec::new(),
        }
    }
}

/// Scanner failures. Every variant names the path involved (MASTER_PLAN §8.1).
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// The repository root does not exist or is not a directory.
    #[error("repository root is not a readable directory: {path}")]
    BadRoot {
        /// The offending root.
        path: PathBuf,
    },
    /// A walk error that is fatal rather than per-file (e.g. the root vanished).
    #[error("failed to walk {path}: {source}")]
    Walk {
        /// Directory being walked.
        path: PathBuf,
        /// Underlying cause.
        #[source]
        source: ignore::Error,
    },
    /// The tree exceeded a configured cap. Caps are errors, not truncations,
    /// because a silently truncated index would produce a silently wrong slice.
    #[error(
        "repository exceeds {limit} {unit} cap ({seen}); \
         raise it explicitly or narrow the root"
    )]
    CapExceeded {
        /// Which cap was hit: `"file"` or `"byte"`.
        unit: &'static str,
        /// The configured limit.
        limit: u64,
        /// The observed value at the moment the cap tripped.
        seen: u64,
    },
}

/// One discovered file, before hashing: the path, the normalized sort key,
/// and the single stat this crate takes per file.
struct Discovered {
    path: PathBuf,
    /// The `/`-separated normalized path; the sort key everywhere downstream.
    key: String,
    size: u64,
    mtime: i64,
}

/// Walk `root` and return every included file, sorted by normalized path.
///
/// The walk honours `.gitignore`, `.ignore`, and `.contextsliceignore` (plus any
/// names in [`ScanConfig::extra_ignore_files`]) whether or not a `.git`
/// directory exists (`require_git(false)`: extracted tarballs and archives get
/// the same ignore semantics as checkouts), never follows symlinks, and never
/// descends into `.git/` (credential-bearing internals that must never reach
/// the index, SECURITY.md §7/§8) or a `.contextslice/` index directory.
///
/// # Errors
///
/// Returns [`ScanError::BadRoot`] if `root` is not a readable directory,
/// [`ScanError::Walk`] for fatal walk failures, and [`ScanError::CapExceeded`]
/// if the tree exceeds the configured file or byte caps. Caps are enforced
/// *during* the walk — an adversarial tree terminates the moment a cap trips,
/// not after being fully discovered.
pub fn scan(root: &Path, config: &ScanConfig) -> Result<Vec<ScannedFile>, ScanError> {
    if !root.is_dir() {
        return Err(ScanError::BadRoot {
            path: root.to_path_buf(),
        });
    }

    // Phase 1: discover (statting each file exactly once) and hash in the
    // normalized order. Caps abort the walk itself.
    let discovered = discover(root, config)?;

    // Phase 2: hash each file, in the now-stable order.
    Ok(discovered
        .into_iter()
        .map(|rec| inspect(root, rec, config))
        .collect())
}

/// Walk the tree and collect one stat per file: path, size, mtime.
///
/// Per-entry failures are labeled degradations, not fatal: an unreadable
/// subdirectory or a dangling symlink is skipped with a debug log. The single
/// exception is the root itself failing to open, which is reported with the path
/// named — otherwise a permissions problem at the root would produce the same
/// empty result as a genuinely empty repository, and the user would receive a
/// confidently empty slice instead of a diagnosis.
///
/// The root check reads the *first* item from the walk. The `ignore` walker opens
/// the root directory lazily, so a failure to open it is reported as the first
/// entry's error rather than as an error on an entry at some depth. This was
/// established by probing the walker's error shape, because the obvious
/// implementation — treating a shallow-depth error as a root error — is wrong:
/// an unreadable *subdirectory* also reports depth 1.
fn discover(root: &Path, config: &ScanConfig) -> Result<Vec<Discovered>, ScanError> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false) // include dotfiles; .gitignore governs instead
        .parents(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false) // ignore files govern even without a .git dir
        .follow_links(false); // symlink loops are impossible, not merely detected

    // `.git` must never be indexed: with `hidden(false)` the walker would
    // otherwise traverse it (332 of this repository's own 475 files were
    // `.git` internals before this filter existed), exposing `.git/config`
    // credentials and reflogs (SECURITY.md §7/§8). `.contextslice` is this
    // tool's own index directory. `.github` and other dotfiles stay included.
    builder.filter_entry(|entry| {
        let name = entry.file_name();
        name != OsStr::new(".git") && name != OsStr::new(".contextslice")
    });

    builder.add_custom_ignore_filename(".contextsliceignore");
    for name in &config.extra_ignore_files {
        builder.add_custom_ignore_filename(name);
    }

    let mut discovered: Vec<Discovered> = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut root_error: Option<ignore::Error> = None;

    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                // A failure to read the root itself must be fatal: the walker
                // reports it as an error carrying the root path, and swallowing
                // it would make an unreadable repository indistinguishable from
                // an empty one, handing the user a confidently empty slice.
                //
                // The path is what identifies this case, not the depth. Depth is
                // unreliable here: an *unreadable file* inside the root also
                // reports depth 1, and the walker yields an `Ok` entry for the
                // root before reporting that it could not read it.
                if is_root_error(root, &err) {
                    if root_error.is_none() {
                        root_error = Some(err);
                    }
                } else {
                    // Any other failure (unreadable subdirectory, broken
                    // symlink) is a labeled degradation: the rest of the tree is
                    // still useful.
                    tracing::debug!(error = %err, "skipping unreadable walk entry");
                }
                continue;
            }
        };
        if entry.depth() == 0 {
            continue; // the root directory entry itself
        }
        let Some(file_type) = entry.file_type() else {
            continue; // stdin, or an entry we cannot stat
        };
        if !file_type.is_file() {
            continue; // directories, sockets, fifos, symlinks: never indexed
        }
        let Some(rel) = normalize_relative(root, entry.path()) else {
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                tracing::debug!(path = %entry.path().display(), error = %err, "skipping unstatable entry");
                continue;
            }
        };
        total_bytes = total_bytes.saturating_add(metadata.len());
        // Caps abort the walk in place (dropping the iterator stops traversal):
        // an adversarial tree must not be fully discovered before tripping.
        if discovered.len() + 1 > usize::try_from(config.max_files).unwrap_or(usize::MAX) {
            return Err(ScanError::CapExceeded {
                unit: "file",
                limit: config.max_files,
                seen: config.max_files + 1,
            });
        }
        if total_bytes > config.max_total_bytes {
            return Err(ScanError::CapExceeded {
                unit: "byte",
                limit: config.max_total_bytes,
                seen: total_bytes,
            });
        }
        discovered.push(Discovered {
            key: to_slash(&rel),
            path: rel,
            size: metadata.len(),
            mtime: mtime_secs(&metadata),
        });
    }

    if let Some(source) = root_error {
        return Err(ScanError::Walk {
            path: root.to_path_buf(),
            source,
        });
    }

    // ARCHITECTURE §7: order-normalize before any persisting decision. The
    // sort key is the normalized path *string* — `PathBuf` ordering compares
    // components and disagrees with string order (`a-b/x.go` vs `a/x.go`),
    // while every downstream consumer (index keys, goldens) sorts strings.
    discovered.sort_unstable_by(|a, b| a.key.cmp(&b.key));
    Ok(discovered)
}

/// Modification time in seconds since the Unix epoch; `0` when unavailable
/// (pre-epoch clocks and exotic filesystems degrade instead of failing).
fn mtime_secs(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// Whether a walk error refers to the repository root rather than a descendant.
///
/// Uses the error's own path when it carries one, and falls back to depth 0.
/// Both are needed: an unreadable root is reported as a path-carrying error at
/// depth 0, while a walker-internal failure may report only a depth.
fn is_root_error(root: &Path, err: &ignore::Error) -> bool {
    error_path(err).map_or_else(|| err.depth() == Some(0), |path| path == root)
}

/// The path an [`ignore::Error`] is associated with, unwrapping nesting.
///
/// The walker wraps errors as `WithPath(WithDepth(Io(_)))`, so the path can sit
/// at any level; `WithLineNumber` can also appear in between for ignore-file
/// parse errors.
fn error_path(err: &ignore::Error) -> Option<&Path> {
    match err {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            error_path(err)
        }
        ignore::Error::Partial(errors) => errors.iter().find_map(error_path),
        _ => None,
    }
}

/// Hash a discovered file and produce its [`ScannedFile`] record.
///
/// The record carries the size and mtime from discovery — the values that keyed
/// every cap decision — rather than re-statting (and possibly observing a file
/// that changed in between). A file that grew past a cap *after* discovery is
/// hashed as discovered; the next run's hash diff catches it.
fn inspect(root: &Path, rec: Discovered, config: &ScanConfig) -> ScannedFile {
    let abs = root.join(&rec.path);
    let lang = detect_language(&rec.key);

    let (hash, skip) = if rec.size > config.read_cap {
        (None, Some(SkipReason::TooLarge))
    } else {
        match hash_file(&abs) {
            Ok(hash) => {
                let skip = (rec.size > config.parse_cap).then_some(SkipReason::ParseSkipped);
                (Some(hash), skip)
            }
            Err(err) => {
                tracing::debug!(path = %abs.display(), error = %err, "file unreadable");
                (None, Some(SkipReason::Unreadable))
            }
        }
    };

    ScannedFile {
        path: rec.key,
        lang,
        size: rec.size,
        mtime: rec.mtime,
        hash,
        skip,
    }
}

/// Hash a file's contents with blake3, streaming in 1 MiB chunks
/// (ARCHITECTURE §4.1) so a large-but-permitted file never balloons RSS.
fn hash_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Strip `root` from `path` and normalize separators; `None` if `path` escapes
/// `root` (which would be a walker bug, and is dropped rather than trusted).
fn normalize_relative(root: &Path, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(root).ok()?;
    let mut cleaned = PathBuf::new();
    for component in rel.components() {
        match component {
            std::path::Component::Normal(part) => cleaned.push(part),
            // `CurDir` carries no information; `ParentDir`/`RootDir`/`Prefix`
            // would mean the path escaped the root, so the entry is dropped.
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    if cleaned.as_os_str().is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Render a relative path with `/` separators on every platform, so index keys
/// are stable across Windows and Unix (ARCHITECTURE §5).
fn to_slash(path: &Path) -> String {
    let mut out = String::with_capacity(path.as_os_str().len());
    for component in path.components() {
        if let std::path::Component::Normal(part) = component {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(&part.to_string_lossy());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, contents).expect("write fixture");
    }

    #[test]
    fn returns_files_sorted_by_normalized_path_string() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "b.go", "package b\n");
        write(dir.path(), "a.go", "package a\n");
        write(dir.path(), "nested/c.ts", "export {}\n");
        // PathBuf component order and string order disagree here ('-' < '/'):
        // string order must win, because index keys are TEXT.
        write(dir.path(), "a-b/x.go", "package x\n");

        let files = scan(dir.path(), &ScanConfig::default()).expect("scan");
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["a-b/x.go", "a.go", "b.go", "nested/c.ts"]);
    }

    #[test]
    fn git_and_index_directories_are_never_scanned() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "kept.go", "package kept\n");
        write(
            dir.path(),
            ".git/config",
            "[remote \"origin\"]\n url = token\n",
        );
        write(dir.path(), ".git/objects/ab/cdef", "loose object");
        write(dir.path(), ".git/refs/heads/main", "0000000\n");
        write(dir.path(), ".contextslice/index.db", "not source\n");
        // Dotdirs that are not .git stay included.
        write(dir.path(), ".github/workflows/ci.yml", "name: CI\n");

        let files = scan(dir.path(), &ScanConfig::default()).expect("scan");
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec![".github/workflows/ci.yml", "kept.go"]);
    }

    #[test]
    fn assigns_languages_and_hashes() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "main.go", "package main\n");
        write(dir.path(), "app.tsx", "export const A = () => null\n");
        write(dir.path(), "script.py", "x = 1\n");
        write(dir.path(), "notes.txt", "hello\n");

        let files = scan(dir.path(), &ScanConfig::default()).expect("scan");
        let by_path = |p: &str| files.iter().find(|f| f.path == p).expect("present");
        assert_eq!(by_path("main.go").lang, Language::Go);
        assert_eq!(by_path("app.tsx").lang, Language::Tsx);
        // `.ts` and `.tsx` must stay distinct: they select different tree-sitter
        // grammars, because neither grammar is a superset (cs-extract docs).
        assert_eq!(by_path("app.tsx").lang, Language::Tsx);
        assert_eq!(detect_language("app.ts"), Language::TypeScript);
        assert_ne!(
            detect_language("app.ts"),
            detect_language("app.tsx"),
            "the TS/TSX split is load-bearing for grammar selection"
        );
        assert_eq!(by_path("script.py").lang, Language::Python);
        assert_eq!(by_path("notes.txt").lang, Language::Unknown);
        assert!(by_path("main.go").hash.is_some());
    }

    #[test]
    fn records_mtime_and_size_from_discovery() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "main.go", "package main\n");

        let files = scan(dir.path(), &ScanConfig::default()).expect("scan");
        assert_eq!(files.len(), 1);
        assert!(
            files[0].mtime > 1_600_000_000,
            "mtime should be a plausible unix timestamp, got {}",
            files[0].mtime
        );
        assert_eq!(files[0].size, "package main\n".len() as u64);
    }

    #[test]
    fn same_content_hashes_identically_and_differs_otherwise() {
        let one = tempfile::tempdir().expect("tempdir");
        let two = tempfile::tempdir().expect("tempdir");
        write(one.path(), "x.go", "package x\n");
        write(two.path(), "x.go", "package x\n");
        write(two.path(), "y.go", "package y\n");

        let a = scan(one.path(), &ScanConfig::default()).expect("scan");
        let b = scan(two.path(), &ScanConfig::default()).expect("scan");
        assert_eq!(a[0].hash, b[0].hash, "identical content must hash equally");
        assert_ne!(b[0].hash, b[1].hash, "different content must differ");
    }

    #[test]
    fn gitignore_is_honored_without_a_git_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), ".gitignore", "ignored/\n*.log\n");
        write(dir.path(), "kept.go", "package kept\n");
        write(dir.path(), "ignored/dropped.go", "package dropped\n");
        write(dir.path(), "debug.log", "noise\n");
        // Deliberately NO .git directory: extracted tarballs and CI archives
        // must get the same ignore semantics as checkouts (require_git(false)).

        let files = scan(dir.path(), &ScanConfig::default()).expect("scan");
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"kept.go"));
        assert!(
            !paths.iter().any(|p| p.starts_with("ignored/")),
            "ignored directory must be pruned, got {paths:?}"
        );
        assert!(
            !paths.contains(&"debug.log"),
            "ignored glob must be pruned, got {paths:?}"
        );
    }

    #[test]
    fn parse_skipped_band_is_hashed_but_labeled() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "big.go", &"a".repeat(4096));
        let config = ScanConfig {
            parse_cap: 16,
            ..ScanConfig::default()
        };

        let files = scan(dir.path(), &config).expect("scan");
        assert_eq!(files.len(), 1);
        // Within the read cap: the hash exists (incrementality needs it) even
        // though the file will never be parsed (ARCHITECTURE §4.1).
        assert_eq!(files[0].skip, Some(SkipReason::ParseSkipped));
        assert!(
            files[0].hash.is_some(),
            "parse-skipped files must still be hashed"
        );
        assert_eq!(files[0].size, 4096);
    }

    #[test]
    fn over_read_cap_files_are_listed_but_never_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "big.go", &"a".repeat(4096));
        let config = ScanConfig {
            read_cap: 16,
            ..ScanConfig::default()
        };

        let files = scan(dir.path(), &config).expect("scan");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].skip, Some(SkipReason::TooLarge));
        assert_eq!(files[0].hash, None);
        assert_eq!(files[0].size, 4096);
    }

    #[test]
    fn scan_is_deterministic_across_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "b/x.go", "package b\n");
        write(dir.path(), "a.go", "package a\n");

        let render = |files: &[ScannedFile]| {
            files
                .iter()
                .map(|f| format!("{} {:?} {} {}", f.path, f.lang, f.size, f.mtime))
                .collect::<Vec<_>>()
        };
        let first = render(&scan(dir.path(), &ScanConfig::default()).expect("scan"));
        let second = render(&scan(dir.path(), &ScanConfig::default()).expect("scan"));
        assert_eq!(first, second);
    }

    #[test]
    fn missing_root_is_an_error_naming_the_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope");
        let err = scan(&missing, &ScanConfig::default()).expect_err("must fail");
        assert!(matches!(err, ScanError::BadRoot { .. }));
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn file_cap_is_an_error_not_a_truncation() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a.go", "package a\n");
        write(dir.path(), "b.go", "package b\n");
        let config = ScanConfig {
            max_files: 1,
            ..ScanConfig::default()
        };

        let err = scan(dir.path(), &config).expect_err("must fail");
        match err {
            ScanError::CapExceeded { unit, limit, seen } => {
                assert_eq!(unit, "file");
                assert_eq!(limit, 1);
                assert!(seen > limit, "seen must exceed the limit");
            }
            other => panic!("expected CapExceeded, got {other:?}"),
        }
    }

    #[test]
    fn byte_cap_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a.go", "package a\n");
        let config = ScanConfig {
            max_total_bytes: 1,
            ..ScanConfig::default()
        };
        let err = scan(dir.path(), &config).expect_err("must fail");
        assert!(matches!(err, ScanError::CapExceeded { unit: "byte", .. }));
    }

    #[test]
    fn symlinks_are_not_followed_so_loops_cannot_hang() {
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().expect("tempdir");
            write(dir.path(), "real/a.go", "package a\n");
            // A self-referential directory symlink: following it would loop.
            std::os::unix::fs::symlink(dir.path(), dir.path().join("real/loop")).expect("symlink");

            let files = scan(dir.path(), &ScanConfig::default()).expect("scan");
            let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
            assert_eq!(paths, vec!["real/a.go"]);
        }
    }

    #[test]
    fn empty_directory_scans_to_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let files = scan(dir.path(), &ScanConfig::default()).expect("scan");
        assert!(files.is_empty());
    }

    /// An unreadable *subdirectory* must degrade, not abort the scan.
    ///
    /// This is a regression test for a real bug: the walker reports an unreadable
    /// subdirectory at depth 1, so an implementation that treats a shallow-depth
    /// walk error as a root failure would wrongly fail the entire scan on one
    /// bad directory. Only the root's own failure is fatal.
    #[cfg(unix)]
    #[test]
    fn unreadable_subdirectory_is_skipped_not_fatal() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "top.go", "package top\n");
        write(dir.path(), "locked/inner.go", "package inner\n");

        let locked = dir.path().join("locked");
        let mut perms = std::fs::metadata(&locked).expect("stat").permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&locked, perms).expect("chmod");

        let result = scan(dir.path(), &ScanConfig::default());

        // Restore permissions so the tempdir can be cleaned up regardless of
        // the assertion outcome.
        let mut restore = std::fs::metadata(&locked).expect("stat").permissions();
        restore.set_mode(0o755);
        std::fs::set_permissions(&locked, restore).expect("chmod restore");

        let files = result.expect("an unreadable subdirectory must not fail the scan");
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&"top.go"),
            "readable files must still be scanned, got {paths:?}"
        );
    }

    /// An unreadable file is listed with `skip = Unreadable`, not dropped.
    ///
    /// Self-calibrating: rather than probing for root with a platform binding
    /// (the crate forbids `unsafe`, and it would need `libc` for one call), this
    /// first checks whether mode `000` is actually enforced here. In a container
    /// running as root it is not, and the test says so and returns instead of
    /// asserting something the environment cannot demonstrate.
    #[cfg(unix)]
    #[test]
    fn unreadable_file_is_labeled_unreadable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "secret.go", "package secret\n");
        let target = dir.path().join("secret.go");
        let mut perms = std::fs::metadata(&target).expect("stat").permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&target, perms).expect("chmod");

        // Calibration: if the unprivileged read still succeeds, permission bits
        // are not enforced (root), so the behaviour under test is unreachable.
        let enforced = std::fs::read(&target).is_err();

        let scanned = scan(dir.path(), &ScanConfig::default());

        let mut restore = std::fs::metadata(&target).expect("stat").permissions();
        restore.set_mode(0o644);
        std::fs::set_permissions(&target, restore).expect("chmod restore");

        if !enforced {
            eprintln!("mode 000 is not enforced in this environment (running as root?); skipping");
            return;
        }

        let files = scanned.expect("scan must tolerate an unreadable file");
        assert_eq!(files.len(), 1, "the file must still be listed");
        assert_eq!(files[0].skip, Some(SkipReason::Unreadable));
        assert_eq!(files[0].hash, None);
    }

    /// An unreadable *root* must be an error, not an empty scan.
    ///
    /// This closes the silent-empty-result hole: the walker yields an `Ok` entry
    /// for the root and *then* reports that it could not read it, so an
    /// implementation that only inspects the first item, or that keys on depth,
    /// silently returns zero files and hands the user an empty slice that looks
    /// like a real answer.
    ///
    /// Self-calibrating: containers running as root do not enforce mode bits, so
    /// the test proves enforcement first and skips honestly if absent.
    #[cfg(unix)]
    #[test]
    fn unreadable_root_is_an_error_not_an_empty_scan() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "inner/a.go", "package a\n");
        let root = dir.path().join("locked");
        std::fs::create_dir_all(&root).expect("mkdir");

        let mut perms = std::fs::metadata(&root).expect("stat").permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&root, perms).expect("chmod");

        let enforced = std::fs::read_dir(&root).is_err();
        let scanned = scan(&root, &ScanConfig::default());

        let mut restore = std::fs::metadata(&root).expect("stat").permissions();
        restore.set_mode(0o755);
        std::fs::set_permissions(&root, restore).expect("chmod restore");

        if !enforced {
            eprintln!("mode 000 is not enforced in this environment (running as root?); skipping");
            return;
        }

        match scanned {
            Err(ScanError::Walk { path, .. }) => {
                assert!(
                    path.ends_with("locked"),
                    "error must name the root, got {path:?}"
                );
            }
            Err(other) => panic!("expected a Walk error, got {other:?}"),
            Ok(files) => panic!(
                "an unreadable root must not scan as empty; got {} files",
                files.len()
            ),
        }
    }
}
