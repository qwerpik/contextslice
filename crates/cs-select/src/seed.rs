//! Stage 2 seeding, signals S1/S4 (ALGORITHM.md §5, ADR-023 D3): exact
//! symbol-name matches and BM25 over symbol names, both served by a
//! per-selection in-memory FTS5 table built from the snapshot's symbols.
//!
//! The table lives exactly as long as the selection: [`SeedIndex::build`]
//! creates it from a [`SelectionSnapshot`](cs_index::SelectionSnapshot),
//! and dropping the index drops the table. Selection never writes
//! index-adjacent state.

use cs_index::SelectionSnapshot;

use crate::task::ParsedTerm;
use crate::tuning;

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
    /// S4 BM25 match: value `b/(b+3)` with `b = -bm25()`, weight 1.0.
    S4Bm25,
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
    /// One FTS5 row per snapshot symbol (`name`, `file`, `kind`); the table
    /// is the only S1/S4 index the selection reads.
    ///
    /// # Errors
    ///
    /// Returns [`rusqlite::Error`] if the in-memory database or the FTS5
    /// table cannot be created.
    pub fn build(snapshot: &SelectionSnapshot) -> Result<Self, rusqlite::Error> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE VIRTUAL TABLE seed_fts USING fts5(name, file, kind, tokenize='unicode61');",
        )?;
        {
            let mut insert = conn.prepare(
                "INSERT INTO seed_fts (name, file, kind) VALUES (?1, ?2, ?3)",
            )?;
            for symbol in &snapshot.symbols {
                insert.execute(rusqlite::params![
                    symbol.name,
                    symbol.file,
                    symbol.kind
                ])?;
            }
        }
        Ok(Self { conn })
    }

    /// S1 seed contributions for parsed task terms (ALGORITHM §5).
    ///
    /// Each distinct term runs as an FTS5 phrase query for candidates; the
    /// exact/folded classification is then decided in Rust by string
    /// comparison, never by the tokenizer: a candidate whose name equals
    /// one of the term's original-case spellings scores exact
    /// (`S1_EXACT_SYMBOL_WEIGHT × S1_CASE_SENSITIVE_VALUE`), a
    /// case-folded-only match scores folded (`× S1_CASE_INSENSITIVE_VALUE`).
    /// Output is sorted by (score desc, path asc, symbol asc) — the sort
    /// runs on candidate rows before mapping to [`SeedScore`], so equal
    /// entries can never leak insertion order.
    #[must_use]
    pub fn seed_s1(&self, terms: &[ParsedTerm]) -> Vec<SeedScore> {
        let mut rows: Vec<(f64, String, String, SeedSignal)> = Vec::new();
        for term in terms {
            // Double-quoted FTS5 phrases are literal: the only character
            // needing escape is the quote itself.
            let phrase = format!("\"{}\"", term.lower.replace('"', "\"\""));
            let mut query = match self.conn.prepare(
                "SELECT file, name FROM seed_fts WHERE seed_fts MATCH ?1",
            ) {
                Ok(query) => query,
                Err(_) => continue,
            };
            let candidates = query.query_map([phrase], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            });
            let candidates = match candidates {
                Ok(candidates) => candidates,
                Err(_) => continue,
            };
            for candidate in candidates.flatten() {
                let (file, name) = candidate;
                let (value, signal) = if term.originals.contains(&name) {
                    (
                        tuning::S1_CASE_SENSITIVE_VALUE,
                        SeedSignal::S1Exact,
                    )
                } else if name.to_lowercase() == term.lower {
                    (
                        tuning::S1_CASE_INSENSITIVE_VALUE,
                        SeedSignal::S1Folded,
                    )
                } else {
                    continue;
                };
                rows.push((tuning::S1_EXACT_SYMBOL_WEIGHT * value, file, name, signal));
            }
        }
        rows.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        rows
            .into_iter()
            .map(|(score, path, _, signal)| SeedScore {
                path,
                score,
                signal,
            })
            .collect()
    }

    /// S4 seed contributions for parsed task terms (ALGORITHM §5).
    ///
    /// Each distinct term runs as an FTS5 query; every matched row scores
    /// `S4_BM25_WEIGHT × b/(b+S4_BM25_NORM_DIVISOR)` with `b = -bm25(...)`.
    /// The negation comes first because FTS5 ranks better matches MORE
    /// negative — consuming `bm25()` unnegated would invert the ranking.
    /// Rows with `b <= 0.0` contribute nothing and are skipped. Output
    /// order is the same total (score desc, path asc, symbol asc) sort as
    /// [`SeedIndex::seed_s1`].
    #[must_use]
    pub fn seed_s4(&self, terms: &[ParsedTerm]) -> Vec<SeedScore> {
        let mut rows: Vec<(f64, String)> = Vec::new();
        for term in terms {
            let phrase = format!("\"{}\"", term.lower.replace('"', "\"\""));
            let mut query = match self.conn.prepare(
                "SELECT file, -bm25(seed_fts) FROM seed_fts WHERE seed_fts MATCH ?1",
            ) {
                Ok(query) => query,
                Err(_) => continue,
            };
            let candidates = query.query_map([phrase], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            });
            let candidates = match candidates {
                Ok(candidates) => candidates,
                Err(_) => continue,
            };
            for candidate in candidates.flatten() {
                let (file, b) = candidate;
                if b <= 0.0 {
                    continue;
                }
                let value = b / (b + tuning::S4_BM25_NORM_DIVISOR);
                rows.push((tuning::S4_BM25_WEIGHT * value, file));
            }
        }
        rows.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        rows
            .into_iter()
            .map(|(score, path)| SeedScore {
                path,
                score,
                signal: SeedSignal::S4Bm25,
            })
            .collect()
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
                    name: "Login".to_owned(),
                    kind: "func".to_owned(),
                    exported: true,
                    line: 10,
                    container: None,
                    signature: None,
                    doc: None,
                },
                SnapshotSymbol {
                    file: "auth/other.go".to_owned(),
                    name: "login".to_owned(),
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

    #[test]
    fn s1_exact_beats_folded_with_spec_values() {
        use crate::task::parse_task;
        let snapshot = scored_snapshot();
        let index = SeedIndex::build(&snapshot).expect("build");
        // "Login" stays one term (single run, single piece); the FTS phrase
        // matches both folded tokens, and Rust classifies exact vs folded.
        // NOTE (v1 scope): camelCase task text like "SessionTimeout" splits
        // into pieces before seeding, so multi-piece exact names match only
        // via their folded pieces — whole-name joining is a later stage.
        let task = parse_task("Login");
        let scores = index.seed_s1(&task.terms);
        assert_eq!(scores.len(), 2, "both symbols match, got {scores:?}");
        assert_eq!(scores[0].path, "auth/session.go");
        assert_eq!(scores[0].score, 3.0, "S1 exact: 3.0 x 1.0");
        assert_eq!(scores[0].signal, SeedSignal::S1Exact);
        assert_eq!(scores[1].path, "auth/other.go");
        assert!(
            (scores[1].score - 3.0 * 0.7).abs() < 1e-12,
            "S1 folded: 3.0 x 0.7, got {}",
            scores[1].score
        );
        assert_eq!(scores[1].signal, SeedSignal::S1Folded);
    }

    #[test]
    fn s1_no_match_yields_no_scores() {
        use crate::task::parse_task;
        let snapshot = scored_snapshot();
        let index = SeedIndex::build(&snapshot).expect("build");
        let task = parse_task("zxqvkw");
        assert!(index.seed_s1(&task.terms).is_empty());
    }

    #[test]
    fn s1_output_order_is_score_then_path_then_symbol() {        use crate::task::parse_task;
        let snapshot = scored_snapshot();
        let index = SeedIndex::build(&snapshot).expect("build");
        // Both symbols fold-match "login": tied 2.1 scores must
        // break by path asc, deterministically, on every run.
        let task = parse_task("login");
        let scores = index.seed_s1(&task.terms);
        assert_eq!(scores.len(), 2);
        assert!(scores[0].score >= scores[1].score);
        let order: Vec<&str> = scores.iter().map(|s| s.path.as_str()).collect();
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(order, sorted, "tied scores break by path asc");
        assert_eq!(index.seed_s1(&task.terms), scores, "same order twice");
    }

    #[test]
    fn s4_bm25_sign_convention_is_pinned() {
        // FTS5 ranks better matches MORE negative. S4 consumes -bm25, so
        // if SQLite ever flips the sign convention this test — not silent
        // ranking drift — tells us.
        let snapshot = scored_snapshot();
        let index = SeedIndex::build(&snapshot).expect("build");
        let raw: f64 = index
            .conn
            .query_row(
                "SELECT bm25(seed_fts) FROM seed_fts WHERE seed_fts MATCH '\"login\"'",
                [],
                |row| row.get(0),
            )
            .expect("bm25 query");
        assert!(raw < 0.0, "bm25 must be negative for a match, got {raw}");
    }

    #[test]
    fn s4_scores_follow_b_over_b_plus_3() {
        use crate::task::parse_task;
        use crate::tuning;
        let snapshot = scored_snapshot();
        let index = SeedIndex::build(&snapshot).expect("build");
        let task = parse_task("login");
        let scores = index.seed_s4(&task.terms);
        assert_eq!(scores.len(), 2, "both symbols match, got {scores:?}");
        for score in &scores {
            assert_eq!(score.signal, SeedSignal::S4Bm25);
            assert!(
                score.score > 0.0 && score.score < tuning::S4_BM25_WEIGHT,
                "value b/(b+3) in (0,1) times weight 1.0, got {}",
                score.score
            );
        }
        // Recompute from the pinned-negative bm25: score must equal
        // weight * b/(b+3) exactly (same arithmetic, no hidden terms).
        let b: f64 = index
            .conn
            .query_row(
                "SELECT -bm25(seed_fts) FROM seed_fts WHERE seed_fts MATCH '\"login\"' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("bm25 query");
        let expected = tuning::S4_BM25_WEIGHT * (b / (b + tuning::S4_BM25_NORM_DIVISOR));
        assert!(
            scores.iter().any(|s| (s.score - expected).abs() < 1e-12),
            "a score matches weight*b/(b+3) = {expected}, got {scores:?}"
        );
    }
}
