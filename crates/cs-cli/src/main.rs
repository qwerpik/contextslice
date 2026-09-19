//! The `contextslice` command-line interface.
//!
//! Commands, flags, and exit codes are specified in MASTER_PLAN.md §6.1. Two
//! contracts in that section are load-bearing and encoded here:
//!
//! * **stdout is sacred.** Only the slice artifact (and help/version text) goes
//!   to stdout; progress, diagnostics, and `--explain` output go to stderr, so
//!   `contextslice "task" | pbcopy` always pipes exactly the artifact.
//! * **Exit codes are stable.** `0` success, `2` usage error, `3` index/budget
//!   error, `4` internal error. A caller (agent, CI job, script) can branch on
//!   the code without parsing prose.
//!
//! During the bootstrap milestone the pipeline crates are skeletons, so commands
//! whose stages are not yet implemented report that plainly through
//! [`ExitCode::Internal`] with a "not implemented in this build" message rather
//! than silently succeeding or emitting an empty slice.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode as StdExitCode;

use clap::{Parser, Subcommand, ValueEnum};

/// Process exit codes (MASTER_PLAN §6.1). Stable, documented, branchable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// The command completed.
    Success,
    /// Bad flags or arguments; nothing was attempted.
    Usage,
    /// The index could not be used, or the budget is impossible.
    Index,
    /// A defect in ContextSlice. The message asks for a report.
    Internal,
}

impl ExitCode {
    /// The numeric code as seen by the shell.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Usage => 2,
            Self::Index => 3,
            Self::Internal => 4,
        }
    }
}

impl From<ExitCode> for StdExitCode {
    fn from(code: ExitCode) -> Self {
        StdExitCode::from(code.code())
    }
}

/// Output formats for a rendered slice (ARCHITECTURE §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Human- and agent-readable markdown with per-file headers (default).
    Markdown,
    /// The `SlicePlan` serialized with a stable schema, for harnesses.
    Json,
    /// Repomix-style `<file path=...>` envelope; experimental.
    Xml,
}

/// Exit-code-carrying CLI error.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// The requested stage is not implemented in this build.
    #[error("{command} is not implemented in this build: {stage} lands in {phase}")]
    NotImplemented {
        /// The command the user invoked.
        command: &'static str,
        /// The pipeline stage that is missing.
        stage: &'static str,
        /// Where the roadmap schedules it.
        phase: &'static str,
    },
    /// No index exists and the command requires one.
    #[error(
        "no index found at {path}; run `contextslice index` first \
         (indexing takes seconds to minutes on first run)"
    )]
    NoIndex {
        /// Where the index was expected.
        path: PathBuf,
    },

    /// The artifact could not be written.
    #[error("failed to write the slice to {target}: {source}")]
    Output {
        /// The destination named by `--out`, or `<stdout>`.
        target: String,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// An index operation failed.
    #[error("index error: {detail}")]
    IndexFailure {
        /// Explanation and remediation details.
        detail: String,
    },
}

impl CliError {
    /// The exit code this error maps to.
    #[must_use]
    pub const fn exit_code(&self) -> ExitCode {
        match self {
            Self::NotImplemented { .. } => ExitCode::Internal,
            // Environment/index problems with a user-actionable fix map to exit code 3 (MASTER_PLAN §6.1).
            Self::NoIndex { .. } | Self::Output { .. } | Self::IndexFailure { .. } => {
                ExitCode::Index
            }
        }
    }
}

/// Where the index lives, relative to the repository root (ARCHITECTURE §6).
pub const INDEX_DIR: &str = ".contextslice";

/// The default token budget (ALGORITHM.md §2).
pub const DEFAULT_TOKEN_BUDGET: usize = 16_000;

/// Build a slice for a task — the primary command.
#[derive(Debug, clap::Args)]
pub struct SliceArgs {
    /// The task text, in natural language. Quoting is recommended.
    ///
    /// The sugar form `contextslice "task"` (MASTER_PLAN §6.1) lands with CLI
    /// polish in step 8; this build requires the explicit `slice` subcommand.
    pub task: String,

    /// Token budget; `0` means unlimited.
    #[arg(long, default_value_t = DEFAULT_TOKEN_BUDGET)]
    pub tokens: usize,

    /// Output format.
    #[arg(long, value_enum, default_value_t = Format::Markdown)]
    pub format: Format,

    /// Write the artifact to a file instead of stdout; `-` means stdout.
    #[arg(long)]
    pub out: Option<String>,

    /// Force-include paths as maximum-weight seeds. Repeatable.
    #[arg(long = "include")]
    pub include: Vec<String>,

    /// Print the per-file scoring table to stderr.
    #[arg(long)]
    pub explain: bool,

    /// Disable git-derived signals.
    #[arg(long)]
    pub no_git: bool,

    /// Override the index location.
    #[arg(long)]
    pub index: Option<PathBuf>,
}

/// Cross-cutting options carried by every subcommand.
#[derive(Debug, clap::Args)]
pub struct CommonArgs {
    /// Disable git-derived signals.
    #[arg(long, global = true)]
    pub no_git: bool,

    /// Override the index location.
    #[arg(long, global = true)]
    pub index: Option<PathBuf>,
}

/// `contextslice` — deterministic, budget-fitted context selection for coding agents.
#[derive(Debug, Parser)]
#[command(
    name = "contextslice",
    version,
    about = "Deterministic, token-budgeted context selection for coding agents",
    long_about = "ContextSlice selects which files a coding agent should see for a given \
                  task, and at what level of detail, fitting a hard token budget. \
                  The core is deterministic and fully offline: no network, no API keys.",
    after_help = "EXIT CODES:\n  \
                  0  success\n  \
                  2  usage error\n  \
                  3  index or budget error (message explains the fix)\n  \
                  4  internal error (please report, with `contextslice doctor` output)"
)]
pub struct Cli {
    /// Subcommand to run.
    #[command(subcommand)]
    pub command: Command,

    /// Cross-cutting options.
    #[command(flatten)]
    pub common: CommonArgs,
}

/// The command surface (MASTER_PLAN §6.1).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Select context for a task and render it — the primary command.
    ///
    /// Specified as `contextslice "task"`; see `main` for why this build
    /// requires the explicit `slice` keyword.
    Slice(SliceArgs),

    /// Build or refresh the index.
    Index {
        /// Optional directory to index (defaults to current working directory).
        dir: Option<PathBuf>,
        /// Discard and rebuild from scratch.
        #[arg(long)]
        rebuild: bool,
        /// Prune rows for files that no longer exist.
        #[arg(long)]
        prune: bool,
    },

    /// Build a task-less repository map (aider-style).
    Map {
        /// Token budget; `0` means unlimited.
        #[arg(long, default_value_t = 8_000)]
        tokens: usize,
    },

    /// Show what the index knows about a path or symbol.
    Inspect {
        /// A repo-relative path or a symbol name.
        target: String,
    },

    /// Run the MCP server over stdio.
    Mcp {
        /// Serve over stdio; the only supported transport.
        ///
        /// A bare switch matching the documented `contextslice mcp [--stdio]`
        /// form (MASTER_PLAN §6.1). SECURITY.md §3 requires stdio to be the
        /// sole transport, so the meaningful operation is confirming it, and
        /// the argument exists to make the contract explicit rather than to
        /// offer a second transport — there is no value to turn off.
        #[arg(long, default_value_t = true, action = clap::ArgAction::SetTrue)]
        stdio: bool,
    },

    /// Diagnose environment and index problems.
    Doctor,

    /// Print version information.
    Version,
}

/// Result of dispatching a command.
type CliResult = Result<(), CliError>;

/// Walk up from the current directory to find an existing index directory.
///
/// Returns the expected location even when nothing exists, so the error message
/// can name a concrete path.
#[must_use]
pub fn find_index_root(start: &std::path::Path) -> (PathBuf, bool) {
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join(INDEX_DIR);
        if candidate.is_dir() {
            return (candidate, true);
        }
        current = dir.parent();
    }
    (start.join(INDEX_DIR), false)
}

/// Execute a parsed command, returning a stable exit code.
///
/// # Errors
///
/// Returns [`CliError`] for conditions that map to exit code 3 or 4; usage
/// errors are handled by clap before this point and exit 2.
pub fn run(cli: &Cli) -> CliResult {
    match &cli.command {
        Command::Slice(args) => {
            // Resolve the index relative to the invoking directory, not the
            // process's idea of a root: a slice is always requested from inside
            // the repository the user is working in.
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            run_slice(args, &cwd)
        }
        Command::Index {
            dir,
            rebuild,
            prune,
        } => run_index(dir.as_deref(), *rebuild, *prune, &cli.common),
        Command::Version => {
            // stdout: this is the artifact of the command.
            println!("contextslice {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Doctor => {
            run_doctor();
            Ok(())
        }
        not_implemented => Err(not_implemented_error(not_implemented)),
    }
}

/// Map a command whose pipeline stages are still skeletons to a precise,
/// actionable error. Kept as one table so the roadmap reference in every message
/// cannot drift apart from the others.
fn not_implemented_error(command: &Command) -> CliError {
    let (command, stage, phase) = match command {
        Command::Map { .. } => ("map", "cs-select map mode", "MASTER_PLAN §15 step 6"),
        Command::Inspect { .. } => ("inspect", "cs-index queries", "MASTER_PLAN §15 step 5"),
        Command::Mcp { .. } => ("mcp", "cs-mcp server", "MASTER_PLAN §15 step 14"),
        // `run` dispatches the implemented commands before reaching here.
        Command::Slice(_) | Command::Index { .. } | Command::Version | Command::Doctor => (
            "unknown",
            "this command",
            "the current milestone (this is a bug in cs-cli)",
        ),
    };
    CliError::NotImplemented {
        command,
        stage,
        phase,
    }
}

/// Report environment and index state.
///
/// Diagnostics go to stderr: stdout is reserved for artifacts, and `doctor` has
/// no artifact (MASTER_PLAN §6.1).
fn run_doctor() {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (index_path, exists) = find_index_root(&cwd);
    eprintln!("contextslice {}", env!("CARGO_PKG_VERSION"));
    eprintln!("index path:    {}", index_path.display());
    eprintln!(
        "index present: {}",
        if exists {
            "yes"
        } else {
            "no (run `contextslice index`)"
        }
    );
    if exists {
        let db_file = index_path.join("index.db");
        if db_file.exists() {
            match cs_index::Doctor::check_file(&db_file) {
                Ok(diags) => {
                    for diag in diags {
                        eprintln!("{diag}");
                    }
                }
                Err(e) => {
                    eprintln!("doctor inspection failed: {e}");
                }
            }
        }
    }
}

/// Run index command: cold build, rebuild, or incremental update.
fn run_index(dir: Option<&Path>, rebuild: bool, _prune: bool, common: &CommonArgs) -> CliResult {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let root = dir.unwrap_or(&cwd);
    let root = root.canonicalize().map_err(|e| CliError::IndexFailure {
        detail: format!("cannot access directory {}: {e}", root.display()),
    })?;

    let index_dir = match &common.index {
        Some(p) => p.clone(),
        None => root.join(INDEX_DIR),
    };
    let db_path = if index_dir.extension().and_then(|e| e.to_str()) == Some("db") {
        index_dir
    } else {
        index_dir.join("index.db")
    };

    if rebuild && db_path.exists() {
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }

    let scan_start = std::time::Instant::now();
    let scanned = cs_scanner::scan(&root, &cs_scanner::ScanConfig::default()).map_err(|e| {
        CliError::IndexFailure {
            detail: format!("scan failed: {e}"),
        }
    })?;
    let scan_dur = scan_start.elapsed();
    eprintln!("scanned {} files in {:?}", scanned.len(), scan_dur);

    let mut db =
        cs_index::IndexDatabase::open_or_create(&db_path).map_err(|e| CliError::IndexFailure {
            detail: format!("failed to open index at {}: {e}", db_path.display()),
        })?;

    let is_ready = matches!(db.state(), Ok(cs_index::IndexState::Ready));
    if is_ready && !rebuild {
        let inc_start = std::time::Instant::now();
        let stats = db
            .update_incremental(&root, &scanned)
            .map_err(|e| CliError::IndexFailure {
                detail: format!("incremental indexing failed: {e}"),
            })?;
        let inc_dur = inc_start.elapsed();
        eprintln!(
            "incremental index completed in {:?}: {} unchanged, {} added, {} modified, {} deleted; {} packages re-resolved",
            inc_dur,
            stats.files_unchanged,
            stats.files_added,
            stats.files_modified,
            stats.files_deleted,
            stats.packages_resolved
        );
    } else {
        // A build that never reached Ready resumes instead of restarting
        // (ADR-021 D3): the fact pass skips files whose stored hash matches,
        // and the resolve pass is idempotent, so completed batches survive.
        // `--rebuild` is the explicit way back to an empty index.
        let resuming = matches!(db.state(), Ok(cs_index::IndexState::Building));
        if !resuming {
            db.begin_build(cs_index::DEFAULT_CONFIG_FINGERPRINT)
                .map_err(|e| CliError::IndexFailure {
                    detail: format!("begin build failed: {e}"),
                })?;
        }

        let build_start = std::time::Instant::now();

        let ingest_stats =
            db.ingest_facts(&root, &scanned)
                .map_err(|e| CliError::IndexFailure {
                    detail: format!("fact ingestion failed: {e}"),
                })?;

        let resolve_stats = db.resolve_facts().map_err(|e| CliError::IndexFailure {
            detail: format!("fact resolution failed: {e}"),
        })?;
        let build_dur = build_start.elapsed();

        let snapshot_id = db.compute_snapshot_id().unwrap_or_default();
        let snap_display = if snapshot_id.len() >= 12 {
            &snapshot_id[..12]
        } else {
            &snapshot_id
        };

        let resume_note = if resuming {
            format!(
                ", {} unchanged from the interrupted build",
                ingest_stats.files_skipped_already_present
            )
        } else {
            String::new()
        };
        eprintln!(
            "{} index completed in {:?}: {} files scanned, {} inserted{}, {} symbols, {} refs, {} imports",
            if resuming {
                "resumed"
            } else {
                "cold"
            },
            build_dur,
            ingest_stats.files_scanned,
            ingest_stats.files_inserted,
            resume_note,
            ingest_stats.symbols_inserted,
            ingest_stats.refs_inserted,
            ingest_stats.imports_inserted
        );
        eprintln!(
            "resolved {} imports, {} bound refs, {} unbound refs (snapshot {})",
            resolve_stats.resolved,
            resolve_stats.refs_bound,
            resolve_stats.refs_unbound,
            snap_display
        );
    }

    Ok(())
}

/// Build a slice.
///
/// The emission contract from MASTER_PLAN §6.1 — **stdout is sacred**, so a
/// failing command must leave it untouched — is settled. This build's selection
/// and rendering stages are still skeletons: rather than emitting a placeholder
/// that a piped consumer could mistake for a slice (a caller can branch on the
/// exit code but cannot detect a plausible-looking wrong artifact), the command
/// fails cleanly with [`CliError::NotImplemented`] and writes nothing. Artifact
/// emission — stdout/`--out` routing and EPIPE-as-success semantics — is
/// introduced with those stages in MASTER_PLAN §15 steps 6–7.
///
/// # Errors
///
/// Returns [`CliError::NoIndex`] when no index exists, or
/// [`CliError::NotImplemented`] for the selection stages that are still
/// skeletons. Neither writes to stdout or `--out`.
pub fn run_slice(args: &SliceArgs, cwd: &std::path::Path) -> CliResult {
    let index_path = match &args.index {
        Some(path) => path.clone(),
        None => find_index_root(cwd).0,
    };
    if !index_path.is_dir() {
        return Err(CliError::NoIndex { path: index_path });
    }

    Err(CliError::NotImplemented {
        command: "slice",
        stage: "cs-select + cs-render",
        phase: "MASTER_PLAN §15 steps 6-7",
    })
}

fn main() -> StdExitCode {
    // MASTER_PLAN §6.1 specifies the sugar form `contextslice "task"`. clap's
    // derive cannot route a leading positional into a default subcommand without
    // `args_conflicts_with_subcommands` + a catch-all that would also swallow
    // `map` and `inspect` arguments, so a naive fallback would be ambiguous
    // rather than helpful. This build therefore requires the explicit `slice`
    // subcommand (`contextslice slice "task"`); the sugar lands with CLI polish
    // in MASTER_PLAN §15 step 8.
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::Success.into(),
        Err(err) => {
            eprintln!("error: {err}");
            err.exit_code().into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        // Catches clap misconfiguration (duplicate flags, bad defaults) at test
        // time instead of at the user's first invocation.
        Cli::command().debug_assert();
    }

    #[test]
    fn exit_codes_are_the_documented_numbers() {
        assert_eq!(ExitCode::Success.code(), 0);
        assert_eq!(ExitCode::Usage.code(), 2);
        assert_eq!(ExitCode::Index.code(), 3);
        assert_eq!(ExitCode::Internal.code(), 4);
    }

    #[test]
    fn no_index_error_maps_to_exit_three_and_names_the_path() {
        let err = CliError::NoIndex {
            path: PathBuf::from("/repo/.contextslice"),
        };
        assert_eq!(err.exit_code(), ExitCode::Index);
        assert!(err.to_string().contains("/repo/.contextslice"));
        assert!(err.to_string().contains("contextslice index"));
    }

    #[test]
    fn not_implemented_maps_to_exit_four_and_names_the_phase() {
        let err = CliError::NotImplemented {
            command: "index",
            stage: "cs-index",
            phase: "MASTER_PLAN §15 step 5",
        };
        assert_eq!(err.exit_code(), ExitCode::Internal);
        assert!(err.to_string().contains("step 5"));
    }

    #[test]
    fn find_index_root_reports_a_concrete_path_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (path, exists) = find_index_root(dir.path());
        assert!(!exists);
        assert!(path.ends_with(INDEX_DIR));
        assert!(path.starts_with(dir.path()));
    }

    #[test]
    fn find_index_root_walks_up_to_an_existing_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(INDEX_DIR)).expect("mkdir");
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).expect("mkdir");

        let (path, exists) = find_index_root(&nested);
        assert!(exists);
        assert_eq!(path, dir.path().join(INDEX_DIR));
    }

    #[test]
    fn version_command_writes_to_stdout_not_stderr() {
        let cli = Cli {
            command: Command::Version,
            common: CommonArgs {
                no_git: false,
                index: None,
            },
        };
        // Stdout is the artifact contract (MASTER_PLAN §6.1); `run` must succeed.
        assert!(run(&cli).is_ok());
    }

    #[test]
    fn slice_without_an_index_is_an_index_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = SliceArgs {
            task: "fix the auth timeout".to_string(),
            tokens: DEFAULT_TOKEN_BUDGET,
            format: Format::Markdown,
            out: None,
            include: Vec::new(),
            explain: false,
            no_git: false,
            index: None,
        };
        let err = run_slice(&args, dir.path()).expect_err("must fail without an index");
        assert_eq!(err.exit_code(), ExitCode::Index);
    }

    #[test]
    fn slice_with_an_index_reports_the_unimplemented_stage() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(INDEX_DIR)).expect("mkdir");
        let args = SliceArgs {
            task: "fix the auth timeout".to_string(),
            tokens: DEFAULT_TOKEN_BUDGET,
            format: Format::Markdown,
            out: None,
            include: Vec::new(),
            explain: false,
            no_git: false,
            index: None,
        };
        let err = run_slice(&args, dir.path()).expect_err("must fail while skeleton");
        assert_eq!(err.exit_code(), ExitCode::Internal);
    }

    #[test]
    fn index_command_resumes_a_building_index_instead_of_resetting() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            repo.path().join("go.mod"),
            b"module example.com/test\n\ngo 1.22\n",
        )
        .expect("write go.mod");
        std::fs::write(
            repo.path().join("a.go"),
            b"package test\n\nfunc Alpha() {}\n",
        )
        .expect("write a.go");
        std::fs::write(
            repo.path().join("b.go"),
            b"package test\n\nfunc Beta() {}\n",
        )
        .expect("write b.go");

        let scanned =
            cs_scanner::scan(repo.path(), &cs_scanner::ScanConfig::default()).expect("scan");

        // An interrupted build: a.go's facts are already committed, stamped
        // with a sentinel mtime no fresh scan of this tree can produce (the
        // file is never modified again, and filesystem mtimes are epoch-
        // anchored, i.e. ~1.7e9).
        let mut partial = scanned
            .iter()
            .find(|f| f.path == "a.go")
            .expect("a.go scanned")
            .clone();
        partial.mtime = 100;
        {
            let db_path = repo.path().join(".contextslice").join("index.db");
            let mut db = cs_index::IndexDatabase::open_or_create(&db_path).expect("open");
            db.begin_build(cs_index::DEFAULT_CONFIG_FINGERPRINT)
                .expect("begin");
            db.ingest_facts(repo.path(), std::slice::from_ref(&partial))
                .expect("partial ingest");
            assert_eq!(db.state().expect("state"), cs_index::IndexState::Building);
        }

        // b.go changes after the interruption.
        std::fs::write(
            repo.path().join("b.go"),
            b"package test\n\nfunc Beta() {}\n\nfunc Delta() {}\n",
        )
        .expect("rewrite b.go");

        let common = CommonArgs {
            no_git: false,
            index: None,
        };
        run_index(Some(repo.path()), false, false, &common).expect("index command resumes");

        let db_path = repo.path().join(".contextslice").join("index.db");
        let db = cs_index::IndexDatabase::open_or_create(&db_path).expect("reopen");
        assert_eq!(db.state().expect("state"), cs_index::IndexState::Ready);

        let a_mtime: i64 = db
            .connection()
            .query_row("SELECT mtime FROM files WHERE path = 'a.go'", [], |r| {
                r.get(0)
            })
            .expect("a.go row");
        assert_eq!(
            a_mtime, 100,
            "a completed batch must be resumed in place — a reset-to-empty rebuild \
             would have re-ingested a.go with its on-disk mtime"
        );

        let delta: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE name = 'Delta'",
                [],
                |r| r.get(0),
            )
            .expect("delta lookup");
        assert_eq!(
            delta, 1,
            "the post-interruption edit must be ingested by the resume"
        );
    }

    #[test]
    fn mcp_stdio_accepts_the_documented_bare_flag() {
        // `contextslice mcp [--stdio]` (MASTER_PLAN §6.1): the flag form must
        // parse. A previous configuration (ArgAction::Set + default) rejected
        // it with a usage error.
        let cli = Cli::try_parse_from(["contextslice", "mcp", "--stdio"])
            .expect("bare --stdio must parse");
        assert!(matches!(cli.command, Command::Mcp { stdio: true }));
    }

    #[test]
    fn failing_slice_writes_no_artifact() {
        // "stdout is sacred" (MASTER_PLAN §6.1): a command that fails must not
        // leave an artifact behind, whether stdout or --out.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(INDEX_DIR)).expect("mkdir");
        let out = dir.path().join("slice.md");
        let args = SliceArgs {
            task: "fix the auth timeout".to_string(),
            tokens: DEFAULT_TOKEN_BUDGET,
            format: Format::Markdown,
            out: Some(out.to_str().expect("utf8").to_string()),
            include: Vec::new(),
            explain: false,
            no_git: false,
            index: None,
        };
        let err = run_slice(&args, dir.path()).expect_err("skeleton stage fails");
        assert_eq!(err.exit_code(), ExitCode::Internal);
        assert!(!out.exists(), "a failed slice must not write the artifact");
    }
}
