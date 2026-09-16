//! End-to-end contract tests for the `contextslice` binary.
//!
//! These assert the two promises in MASTER_PLAN.md §6.1 that a caller depends on and
//! that unit tests cannot check: the **exit codes** and the **stdout/stderr split**.
//! Everything here runs the real binary, because the point is the process-level
//! contract, not the function return value.

use assert_cmd::Command;
use predicates::prelude::*;

fn contextslice() -> Command {
    Command::cargo_bin("contextslice").expect("binary builds")
}

#[test]
fn help_exits_zero_and_documents_exit_codes() {
    contextslice()
        .arg("--help")
        .assert()
        .success()
        // The exit-code table is how a caller learns to branch without parsing prose.
        .stdout(predicate::str::contains("EXIT CODES"))
        .stdout(predicate::str::contains("3  index or budget error"));
}

#[test]
fn version_prints_to_stdout_and_exits_zero() {
    contextslice()
        .arg("version")
        .assert()
        .success()
        // stdout carries the artifact; a version string is the artifact here.
        .stdout(predicate::str::starts_with("contextslice "));
}

#[test]
fn unknown_subcommand_is_a_usage_error_with_exit_code_two() {
    contextslice()
        .arg("definitely-not-a-command")
        .assert()
        .code(2);
}

#[test]
fn slice_without_an_index_exits_three_and_suggests_the_fix() {
    let dir = tempfile::tempdir().expect("tempdir");
    contextslice()
        .current_dir(dir.path())
        .args(["slice", "fix the auth timeout"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("contextslice index"));
}

#[test]
fn unimplemented_command_exits_four_and_names_the_roadmap_step() {
    // During bootstrap the pipeline stages are skeletons. The contract is that the
    // tool says so plainly and distinguishes "not built yet" (4) from "your setup is
    // wrong" (3) -- a caller cannot detect a plausible-looking wrong slice.
    let dir = tempfile::tempdir().expect("tempdir");
    contextslice()
        .current_dir(dir.path())
        .arg("index")
        .assert()
        .code(4)
        .stderr(predicate::str::contains("not implemented in this build"))
        .stderr(predicate::str::contains("MASTER_PLAN"));
}

#[test]
fn doctor_reports_index_state_on_stderr_with_empty_stdout() {
    let dir = tempfile::tempdir().expect("tempdir");
    contextslice()
        .current_dir(dir.path())
        .arg("doctor")
        .assert()
        .success()
        // doctor produces no artifact, so nothing may land on stdout.
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("index present: no"));
}

#[test]
fn slice_keeps_stdout_empty_when_it_fails() {
    // Diagnostics go to stderr, so a pipeline never receives error text as if it
    // were an artifact. This is the "stdout is sacred" rule under failure — and
    // it must hold on the *deep* failure path (index present, selection engine
    // missing), not just the trivial no-index one.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".contextslice")).expect("mkdir");
    contextslice()
        .current_dir(dir.path())
        .args(["slice", "fix the auth timeout"])
        .assert()
        .code(4)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::is_empty().not());
}

/// `--out -` is the documented spelling for "stdout" and must parse and route
/// there. During bootstrap the slice command fails before any artifact exists —
/// and a failing command must leave stdout empty rather than emit a
/// placeholder a piped consumer could mistake for a slice.
#[test]
fn out_dash_means_stdout() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".contextslice")).expect("mkdir");

    contextslice()
        .current_dir(dir.path())
        .args(["slice", "any task", "--out", "-"])
        .assert()
        .code(4) // no selection engine yet
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("not implemented"));
}

/// `--out <file>` is accepted, and a failing command must NOT create the file:
/// an artifact on disk from a failed run is the same poison as placeholder
/// stdout, just slower to notice.
#[test]
fn out_file_is_not_created_when_the_command_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".contextslice")).expect("mkdir");
    let target = dir.path().join("slice.md");

    contextslice()
        .current_dir(dir.path())
        .args(["slice", "any task", "--out", target.to_str().expect("utf8")])
        .assert()
        .code(4);

    assert!(
        !target.exists(),
        "a failed slice must not write the artifact"
    );
    assert!(
        !dir.path().join("-").exists(),
        "`-` must never be treated as a literal filename"
    );
}

/// The documented `contextslice mcp [--stdio]` spelling (MASTER_PLAN §6.1)
/// must parse. A previous flag configuration rejected the bare form with a
/// clap usage error (exit 2).
#[test]
fn mcp_stdio_bare_flag_parses() {
    let dir = tempfile::tempdir().expect("tempdir");
    contextslice()
        .current_dir(dir.path())
        .args(["mcp", "--stdio"])
        .assert()
        .code(4) // reaches dispatch: cs-mcp is a skeleton, not a usage error
        .stderr(predicate::str::contains("not implemented"));
}
