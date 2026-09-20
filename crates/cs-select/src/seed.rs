//! Stage 2 seeding, signals S1/S4 (ALGORITHM.md §5, ADR-023 D3): exact
//! symbol-name matches and BM25 over symbol names, both served by a
//! per-selection in-memory FTS5 table built from the snapshot's symbols.
//!
//! The table lives exactly as long as the selection: [`SeedIndex::build`]
//! creates it from a [`SelectionSnapshot`](cs_index::SelectionSnapshot),
//! and dropping the index drops the table. Selection never writes
//! index-adjacent state.

use cs_index::SelectionSnapshot;

/// One signal's score contribution for one file (ALGORITHM §5: seed sums
/// signal_weight × signal_value per file; the sum itself is a later stage).
#[derive(Debug, Clone, PartialEq)]
pub struct SeedScore {
    /// Repo-relative file path receiving the contribution.
    pub path: String,
    /// Signal weight × value (e.g. S1 exact: `3.0 × 1.0`).
    pub score: f64,
    /// Which signal produced this contribution.
    pub signal: SeedSignal,
}

/// The seeding signals implemented in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedSignal {
    /// S1 exact (case-sensitive) symbol-name match: value 1.0 × weight 3.0.
    S1Exact,
    /// S1 case-insensitive match: value 0.7 × weight 3.0.
    S1Folded,
}

/// The per-selection FTS5 table over snapshot symbols, plus the signal
/// queries that run against it. Owns its in-memory connection: dropping
/// the index drops the table.
pub struct SeedIndex {
    conn: rusqlite::Connection,
}

impl SeedIndex {
    /// Build the in-memory symbol table for one selection.
    ///
    /// # Errors
    ///
    /// Returns [`rusqlite::Error`] if the in-memory database or the FTS5
    /// table cannot be created.
    pub fn build(
        _snapshot: &SelectionSnapshot,
    ) -> Result<Self, rusqlite::Error> {
        let conn = rusqlite::Connection::open_in_memory()?;
        Ok(Self { conn })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cs_index::{SnapshotEdge, SnapshotFile, SnapshotSymbol};

    pub(super) fn scored_snapshot() -> SelectionSnapshot {
        SelectionSnapshot {
            files: vec![
                SnapshotFile {
                    path: "auth/session.go".to_owned(),
                    lang: "go".to_owned(),
                    size: 120,
                    package: None,
                },
                SnapshotFile {
                    path: "auth/other.go".to_owned(),
                    lang: "go".to_owned(),
                    size: 80,
                    package: None,
                },
            ],
            symbols: vec![
                SnapshotSymbol {
                    file: "auth/session.go".to_owned(),
                    name: "SessionTimeout".to_owned(),
                    kind: "func".to_owned(),
                    exported: true,
                    line: 10,
                    container: None,
                    signature: None,
                    doc: None,
                },
                SnapshotSymbol {
                    file: "auth/other.go".to_owned(),
                    name: "sessiontimeout".to_owned(),
                    kind: "func".to_owned(),
                    exported: false,
                    line: 4,
                    container: None,
                    signature: None,
                    doc: None,
                },
            ],
            edges: Vec::<SnapshotEdge>::new(),
            snapshot_id: "t".to_owned(),
            update_in_progress: false,
        }
    }

    #[test]
    fn score_type_carries_path_score_and_signal() {
        let score = SeedScore {
            path: "auth/session.go".to_owned(),
            score: 3.0,
            signal: SeedSignal::S1Exact,
        };
        assert_eq!(score.path, "auth/session.go");
        assert_eq!(score.score, 3.0);
        assert_eq!(score.signal, SeedSignal::S1Exact);
        assert_ne!(score.signal, SeedSignal::S1Folded);
    }

    #[test]
    fn index_builds_from_a_snapshot() {
        let snapshot = scored_snapshot();
        let _index = SeedIndex::build(&snapshot).expect("in-memory build");
    }
}
