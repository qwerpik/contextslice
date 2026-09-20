//! cs-select: deterministic task parsing, tuning table, and the select-side
//! snapshot gate (ARCHITECTURE.md §4.6; ALGORITHM.md §4–§8).
//!
//! Stage 1 ([`task`]) and the tuning table ([`tuning`]) are wired and tested
//! here. Later stages (seeding, propagation, levels, budget fitting) land in
//! the phases of `docs/MASTER_PLAN.md` §15.
//!
//! [`require_ready_snapshot`] is the select-side policy the index loader
//! defers to: a snapshot with no committed id never enters selection.

pub mod seed;
pub mod task;
pub mod tuning;

pub use seed::{SeedScore, SeedSignal};
pub use task::{Hints, ParsedTask, ParsedTerm, TaskFlags, parse_task};

use cs_index::SelectionSnapshot;

/// Selection cannot start: the snapshot carries no committed build id.
///
/// A missing id means the index never finalized (fresh or torn build), so
/// there is nothing deterministic to select from. The remedy is always the
/// same — build the index first — which is why this is one variant, not a
/// matchable taxonomy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    /// The snapshot's id is empty: build the index to a `Ready` state
    /// first (`contextslice index`), then reselect.
    #[error(
        "snapshot has no committed id (fresh or torn index); \
         build the index first, then reselect"
    )]
    EmptySnapshotId,
    /// The snapshot names a build, but a torn incremental update left
    /// facts and derived rows disagreeing: an id alone is not `Ready`.
    /// Repair with the next incremental run (or rebuild), then reselect.
    #[error(
        "snapshot {0} is torn by an unfinished incremental update; \
         run the next update (or rebuild the index), then reselect"
    )]
    StaleSnapshot(String),
}

/// The select-side readiness gate: accept only snapshots whose id names a
/// committed, untorn build.
///
/// [`cs_index`] loading is deliberately state-agnostic: it returns empty
/// vectors with the empty id on a fresh index, and whatever the tables hold
/// mid-update. This function is where both outputs are refused, loudly,
/// before any stage can treat "nothing indexed" — or "torn index" — as a
/// result. A non-empty id alone is not `Ready`: only an id with no pending
/// update enters selection.
#[must_use = "a rejected snapshot must not enter selection"]
pub fn require_ready_snapshot(snapshot: &SelectionSnapshot) -> Result<(), SelectError> {
    if snapshot.update_in_progress {
        return Err(SelectError::StaleSnapshot(snapshot.snapshot_id.clone()));
    }
    if snapshot.snapshot_id.is_empty() {
        return Err(SelectError::EmptySnapshotId);
    }
    Ok(())
}

#[cfg(test)]
mod linkage_tests {
    use super::*;
    use cs_index::{SnapshotEdge, SnapshotFile, SnapshotSymbol};

    /// Linkage guard: this test only compiles while `task` and `tuning`
    /// stay declared modules, so the gates can never pass vacuously on an
    /// unwired crate again — execution count can only be zero if the test
    /// itself is deleted, which review will see.
    #[test]
    fn stages_stay_wired_and_executed() {
        let parsed = parse_task("Fix the authentication timeout");
        assert!(
            parsed.terms.iter().any(|t| t.lower == "authentication"),
            "task stage parses: {parsed:?}"
        );
        assert!(
            (tuning::saturate(2.0) - 0.5).abs() < 1e-12,
            "tuning stage evaluates"
        );
    }

    /// Snapshot and task compose deterministically on the select side: the
    /// same snapshot is accepted twice and the same task parses identically.
    #[test]
    fn snapshot_and_task_compose_deterministically() {
        let snapshot = SelectionSnapshot {
            files: vec![SnapshotFile {
                path: "auth/session.go".to_owned(),
                lang: "go".to_owned(),
                size: 120,
                package: None,
            }],
            symbols: vec![SnapshotSymbol {
                file: "auth/session.go".to_owned(),
                name: "Login".to_owned(),
                kind: "func".to_owned(),
                exported: true,
                line: 10,
                container: None,
                signature: None,
                doc: None,
            }],
            edges: vec![SnapshotEdge {
                src: "auth/session.go".to_owned(),
                dst: "auth/store.go".to_owned(),
                kind: "import_out".to_owned(),
                weight: 0.6,
            }],
            snapshot_id: "test-build".to_owned(),
            update_in_progress: false,
        };
        let task_text = "Fix the `Session.Validate` timeout in auth/session.go";

        let first = (
            require_ready_snapshot(&snapshot).is_ok(),
            parse_task(task_text),
        );
        let second = (
            require_ready_snapshot(&snapshot).is_ok(),
            parse_task(task_text),
        );
        assert!(first.0 && second.0, "a named snapshot enters selection");
        assert_eq!(first.1, second.1, "same task parses byte-identically");
    }

    #[test]
    fn empty_snapshot_id_is_refused_loudly() {
        let snapshot = SelectionSnapshot {
            files: Vec::new(),
            symbols: Vec::new(),
            edges: Vec::new(),
            snapshot_id: String::new(),
            update_in_progress: false,
        };
        let err = require_ready_snapshot(&snapshot).expect_err("empty id must not enter");
        assert_eq!(err, SelectError::EmptySnapshotId);
        assert!(
            err.to_string().contains("build the index first"),
            "the message must say what to do: {err}"
        );
    }

    #[test]
    fn torn_snapshot_is_refused_even_with_a_stale_id() {
        // A crashed incremental update leaves facts committed with the old
        // id still in meta: the id is non-empty, so only the torn flag can
        // catch it. An id alone is not `Ready`.
        let snapshot = SelectionSnapshot {
            files: Vec::new(),
            symbols: Vec::new(),
            edges: Vec::new(),
            snapshot_id: "stale-build".to_owned(),
            update_in_progress: true,
        };
        let err = require_ready_snapshot(&snapshot).expect_err("torn must not enter");
        assert_eq!(err, SelectError::StaleSnapshot("stale-build".to_owned()));
        assert!(
            err.to_string().contains("unfinished incremental update"),
            "the message must name the cause: {err}"
        );
    }
}
