//! cs-index: SQLite index storage, incremental updates, snapshots.
//!
//! Implements `cs-index` from ARCHITECTURE.md §4.4 under the streaming-first
//! architecture of ADR-021: a facts-first three-pass pipeline where SQLite is
//! the source of truth for raw extracted facts and every derived row is a
//! re-derivable projection written through per package. FTS5 is deferred to
//! cs-select (MASTER_PLAN §15 step 6, ADR-021 D10).
//!
//! # Lifecycle (ADR-021 D3)
//!
//! A fresh index starts in [`IndexState::Empty`]; a full build marks it
//! [`IndexState::Building`] before the first fact batch (slices refuse to
//! read it; `index` resumes — the hash-driven fact pass skips completed
//! batches for free) and flips it to [`IndexState::Ready`] atomically with
//! the final `snapshot_id`. Incremental runs stay `Ready` throughout: per-
//! file and per-package transaction atomicity make every committed state
//! coherent, so a crash mid-run leaves an index that is merely as-of an
//! earlier prefix.
//!
//! # Not a security boundary
//!
//! The index lives inside the repository it describes and is trusted exactly
//! as much as that repository. Opening a foreign or damaged file fails
//! loudly (version gate, state validation, SQLite's own file check) rather
//! than attempting repair-by-guesswork.

#![forbid(unsafe_code)]

mod schema;

use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, ErrorCode, Transaction};

pub use schema::{initialize, SCHEMA_SQL, SCHEMA_VERSION};

/// Everything that can go wrong at the storage layer. Every variant is
/// either actionable ("rebuild the index") or a reportable defect
/// (MASTER_PLAN §8.1: name the path, state what was attempted).
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// The index file or its directory could not be created or opened.
    #[error("index i/o at {path}: {source}")]
    Io {
        /// The index path involved.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// A SQLite failure not otherwise classified.
    #[error("sqlite error on {path}: {source}")]
    Sqlite {
        /// The index path involved.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: rusqlite::Error,
    },
    /// The file exists but is not a usable contextslice index (garbage,
    /// foreign database, or tables without meta).
    #[error("index at {path} is corrupt or foreign: {detail}")]
    Corrupt {
        /// The index path involved.
        path: PathBuf,
        /// What exactly disqualified it.
        detail: String,
    },
    /// The on-disk schema version is not the one this build speaks.
    /// The remedy is a rebuild; migrations begin only when a released
    /// format exists (ADR-021 D6).
    #[error(
        "index schema version {found} but this build requires {expected}; \
         rebuild the index (`contextslice index --rebuild` or delete it)"
    )]
    SchemaMismatch {
        /// Version found in `meta.schema_version`.
        found: i64,
        /// Version this build requires ([`SCHEMA_VERSION`]).
        expected: i64,
    },
    /// The lifecycle state does not permit the attempted transition.
    #[error("index is in state {found}; expected {expected} for this operation")]
    StateConflict {
        /// The current state.
        found: IndexState,
        /// The state the operation requires.
        expected: &'static str,
    },
    /// Another process holds the write lock (ARCHITECTURE §4.4: fail fast,
    /// no busy timeout).
    #[error("another contextslice process is using the index; wait for it to finish")]
    Locked,
}

/// The index lifecycle states (ADR-021 D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexState {
    /// No facts have been committed; nothing is queryable.
    Empty,
    /// A fresh build is in progress; slices must refuse, indexing may resume.
    Building,
    /// The index is complete and coherent; slices may read it.
    Ready,
}

impl IndexState {
    /// The canonical `meta.index_state` spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Building => "building",
            Self::Ready => "ready",
        }
    }

    /// Parse a stored `meta.index_state` value; `None` means the row is
    /// foreign or truncated — a corrupt-index condition, not a state.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "empty" => Some(Self::Empty),
            "building" => Some(Self::Building),
            "ready" => Some(Self::Ready),
            _ => None,
        }
    }
}

impl fmt::Display for IndexState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An open connection to an index database, with the lifecycle pragmas
/// applied and the schema version gate passed.
///
/// All write paths go through [`IndexDatabase::transaction`]: one
/// transaction per fact batch or per package is what makes every committed
/// state valid (ADR-021 D3).
#[derive(Debug)]
pub struct IndexDatabase {
    conn: Connection,
    path: PathBuf,
}

impl IndexDatabase {
    /// Open the index at `path`, creating it (and its parent directory)
    /// when missing.
    ///
    /// Applies the ADR-021 connection contract — WAL journaling,
    /// `synchronous=NORMAL`, `foreign_keys=ON`, no busy timeout — then the
    /// version gate: a fresh file gets the frozen schema atomically; an
    /// existing file must carry exactly [`SCHEMA_VERSION`] and a parseable
    /// `index_state`, or the open fails with a rebuild-or-report error.
    ///
    /// # Errors
    ///
    /// [`IndexError::Io`] when the path cannot be created,
    /// [`IndexError::Locked`] when another writer holds the database,
    /// [`IndexError::Corrupt`] for foreign or damaged files,
    /// [`IndexError::SchemaMismatch`] for a foreign schema version.
    pub fn open_or_create(path: &Path) -> Result<Self, IndexError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| IndexError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
            }
        }
        let mut conn = Connection::open(path).map_err(|e| Self::classify(path, e))?;

        // WAL first: it is the crash-consistency contract (D3) and a
        // filesystem that cannot provide it must fail loudly, not silently
        // downgrade to rollback-journal semantics.
        let journal: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .map_err(|e| Self::classify(path, e))?;
        if journal != "wal" {
            return Err(IndexError::Corrupt {
                path: path.to_path_buf(),
                detail: format!("cannot enable WAL journaling (filesystem returned '{journal}')"),
            });
        }
        conn.execute_batch("PRAGMA synchronous=NORMAL")
            .map_err(|e| Self::classify(path, e))?;
        conn.execute_batch("PRAGMA foreign_keys=ON")
            .map_err(|e| Self::classify(path, e))?;
        let fk_enforced: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .map_err(|e| Self::classify(path, e))?;
        if fk_enforced != 1 {
            return Err(IndexError::Corrupt {
                path: path.to_path_buf(),
                detail: "SQLite refused to enable foreign key enforcement".to_owned(),
            });
        }
        // Fail fast on writer contention (ARCHITECTURE §4.4): rusqlite
        // silently installs a 5-second busy timeout at open; the contract
        // is zero. Verified below like every other pragma, because a hung
        // second indexer is a worse failure mode than a clear error.
        conn.execute_batch("PRAGMA busy_timeout=0")
            .map_err(|e| Self::classify(path, e))?;
        let busy_timeout: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .map_err(|e| Self::classify(path, e))?;
        if busy_timeout != 0 {
            return Err(IndexError::Corrupt {
                path: path.to_path_buf(),
                detail: format!("cannot disable the busy timeout (got {busy_timeout} ms)"),
            });
        }

        let has_meta: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta')",
                [],
                |row| row.get(0),
            )
            .map_err(|e| Self::classify(path, e))?;

        if has_meta {
            Self::validate_existing(&conn, path)?;
        } else {
            // Tables without meta (or any non-SQLite furniture) is not a
            // fresh database; it is a file we must not initialize over.
            let foreign_tables: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| Self::classify(path, e))?;
            if foreign_tables > 0 {
                return Err(IndexError::Corrupt {
                    path: path.to_path_buf(),
                    detail: format!("{foreign_tables} table(s) present but no meta table"),
                });
            }
            let txn = conn.transaction().map_err(|e| Self::classify(path, e))?;
            initialize(&txn).map_err(|e| Self::classify(path, e))?;
            txn.commit().map_err(|e| Self::classify(path, e))?;
        }

        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    /// The index path this connection was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read one `meta` value.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures (busy maps to [`IndexError::Locked`]).
    pub fn meta(&self, key: &str) -> Result<Option<String>, IndexError> {
        Self::meta_value(&self.conn, &self.path, key)
    }

    /// One meta read, shared by the public getter and the open-time gate.
    fn meta_value(conn: &Connection, path: &Path, key: &str) -> Result<Option<String>, IndexError> {
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .map(Some)
        .or_else(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(Self::classify(path, other)),
        })
    }

    /// The current lifecycle state.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures; an unparsable stored state is
    /// [`IndexError::Corrupt`].
    pub fn state(&self) -> Result<IndexState, IndexError> {
        let raw = self
            .meta("index_state")?
            .ok_or_else(|| IndexError::Corrupt {
                path: self.path.clone(),
                detail: "meta.index_state is missing".to_owned(),
            })?;
        IndexState::parse(&raw).ok_or_else(|| IndexError::Corrupt {
            path: self.path.clone(),
            detail: format!("meta.index_state is '{raw}'"),
        })
    }

    /// Begin a full build: `Empty → Building`, recording the
    /// `config_fingerprint` (parse/read caps + grammar/query pins) whose
    /// change later signals that file classification may be stale.
    ///
    /// # Errors
    ///
    /// [`IndexError::StateConflict`] unless the state is `Empty`.
    pub fn begin_build(&mut self, config_fingerprint: &str) -> Result<(), IndexError> {
        let state = self.state()?;
        if state != IndexState::Empty {
            return Err(IndexError::StateConflict {
                found: state,
                expected: "empty",
            });
        }
        let path = self.path.clone();
        let txn = self.transaction()?;
        txn.execute(
            "UPDATE meta SET value = 'building' WHERE key = 'index_state'",
            [],
        )
        .map_err(|e| Self::classify(&path, e))?;
        txn.execute(
            "INSERT INTO meta (key, value) VALUES ('config_fingerprint', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [config_fingerprint],
        )
        .map_err(|e| Self::classify(&path, e))?;
        txn.commit().map_err(|e| Self::classify(&path, e))
    }

    /// Complete a full build: `Building → Ready`, committing the
    /// `snapshot_id` (blake3 over the sorted `(path, hash)` pairs) in the
    /// same transaction as the flip, so a ready index always names its
    /// content.
    ///
    /// # Errors
    ///
    /// [`IndexError::StateConflict`] unless the state is `Building`.
    pub fn mark_ready(&mut self, snapshot_id: &str) -> Result<(), IndexError> {
        let state = self.state()?;
        if state != IndexState::Building {
            return Err(IndexError::StateConflict {
                found: state,
                expected: "building",
            });
        }
        let path = self.path.clone();
        let txn = self.transaction()?;
        txn.execute(
            "UPDATE meta SET value = 'ready' WHERE key = 'index_state'",
            [],
        )
        .map_err(|e| Self::classify(&path, e))?;
        txn.execute(
            "INSERT INTO meta (key, value) VALUES ('snapshot_id', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [snapshot_id],
        )
        .map_err(|e| Self::classify(&path, e))?;
        txn.commit().map_err(|e| Self::classify(&path, e))
    }

    /// Open a write transaction (drop = rollback, `commit()` = durable
    /// under WAL). One transaction per fact batch or per package is the
    /// crash-consistency unit (ADR-021 D3).
    ///
    /// # Errors
    ///
    /// [`IndexError::Locked`] when another writer holds the database.
    pub fn transaction(&mut self) -> Result<Transaction<'_>, IndexError> {
        self.conn
            .transaction()
            .map_err(|e| Self::lock(&self.path, e))
    }

    /// Read-only access for query helpers built in later milestones.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Map a busy/locked SQLite failure to [`IndexError::Locked`], leaving
    /// everything else intact.
    fn lock(path: &Path, err: rusqlite::Error) -> IndexError {
        if matches!(
            err.sqlite_error_code(),
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        ) {
            IndexError::Locked
        } else {
            Self::classify(path, err)
        }
    }

    /// Classify a raw SQLite failure against the index path: not-a-database
    /// files are corrupt-or-foreign, everything else is reported verbatim.
    fn classify(path: &Path, err: rusqlite::Error) -> IndexError {
        if matches!(err.sqlite_error_code(), Some(ErrorCode::NotADatabase)) {
            IndexError::Corrupt {
                path: path.to_path_buf(),
                detail: "file is not an SQLite database".to_owned(),
            }
        } else {
            IndexError::Sqlite {
                path: path.to_path_buf(),
                source: err,
            }
        }
    }

    /// The version gate for an existing database: exact version match and
    /// a parseable lifecycle state, or the open fails.
    fn validate_existing(conn: &Connection, path: &Path) -> Result<(), IndexError> {
        let version: Option<String> = Self::meta_value(conn, path, "schema_version")?;
        let Some(version) = version else {
            return Err(IndexError::Corrupt {
                path: path.to_path_buf(),
                detail: "meta.schema_version is missing".to_owned(),
            });
        };
        let found: i64 = version.parse().map_err(|_| IndexError::Corrupt {
            path: path.to_path_buf(),
            detail: format!("meta.schema_version is '{version}'"),
        })?;
        if found != SCHEMA_VERSION {
            return Err(IndexError::SchemaMismatch {
                found,
                expected: SCHEMA_VERSION,
            });
        }
        let state: Option<String> = Self::meta_value(conn, path, "index_state")?;
        match state.as_deref().and_then(IndexState::parse) {
            Some(_) => Ok(()),
            None => Err(IndexError::Corrupt {
                path: path.to_path_buf(),
                detail: format!(
                    "meta.index_state is {}",
                    state.as_deref().unwrap_or("missing")
                ),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_index() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".contextslice").join("index.db");
        (dir, path)
    }

    #[test]
    fn fresh_index_is_created_with_the_full_contract() {
        let (_dir, path) = temp_index();
        let db = IndexDatabase::open_or_create(&path).expect("open");
        assert_eq!(db.state().expect("state"), IndexState::Empty);
        assert_eq!(
            db.meta("schema_version").expect("meta").as_deref(),
            Some("1")
        );

        // The pragmas are the crash-consistency and integrity contract
        // (ADR-021 D3/D7); each is verified on the live connection, not
        // assumed from the execute call's success.
        let conn = db.connection();
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("journal_mode");
        assert_eq!(journal, "wal", "WAL is normative (ADR-021)");
        let sync: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .expect("synchronous");
        assert_eq!(sync, 1, "synchronous=NORMAL");
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .expect("foreign_keys");
        assert_eq!(fk, 1, "foreign keys must be enforced");
        let busy: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .expect("busy_timeout");
        assert_eq!(busy, 0, "writer contention must fail fast, not wait");
        let user_version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("user_version");
        assert_eq!(user_version, SCHEMA_VERSION);
    }

    #[test]
    fn reopen_preserves_state_and_is_idempotent() {
        let (_dir, path) = temp_index();
        {
            let mut db = IndexDatabase::open_or_create(&path).expect("open");
            db.begin_build("caps=1MiB/50MiB;grammars=go-0.25")
                .expect("begin");
        }
        // Drop = close; reopen sees Building, not a re-initialized Empty.
        let db = IndexDatabase::open_or_create(&path).expect("reopen");
        assert_eq!(db.state().expect("state"), IndexState::Building);
        assert_eq!(
            db.meta("config_fingerprint")
                .expect("fingerprint")
                .as_deref(),
            Some("caps=1MiB/50MiB;grammars=go-0.25")
        );
    }

    #[test]
    fn lifecycle_transitions_are_gated() {
        let (_dir, path) = temp_index();
        let mut db = IndexDatabase::open_or_create(&path).expect("open");

        // Ready is unreachable from Empty: a snapshot_id without facts
        // would be a lie about content.
        let err = db.mark_ready("snap").expect_err("ready requires building");
        assert!(matches!(
            err,
            IndexError::StateConflict {
                found: IndexState::Empty,
                expected: "building"
            }
        ));

        db.begin_build("fp").expect("begin");
        let err = db.begin_build("fp2").expect_err("begin requires empty");
        assert!(matches!(
            err,
            IndexError::StateConflict {
                found: IndexState::Building,
                expected: "empty"
            }
        ));

        db.mark_ready("blake3-sorted-path-hash").expect("ready");
        assert_eq!(db.state().expect("state"), IndexState::Ready);
        assert_eq!(
            db.meta("snapshot_id").expect("snapshot").as_deref(),
            Some("blake3-sorted-path-hash")
        );
    }

    #[test]
    fn foreign_schema_version_is_a_rebuild_not_a_crash() {
        let (_dir, path) = temp_index();
        IndexDatabase::open_or_create(&path).expect("open");
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute(
                "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
                [],
            )
            .expect("bump version");
        }
        let err = IndexDatabase::open_or_create(&path).expect_err("must refuse");
        assert!(
            matches!(
                err,
                IndexError::SchemaMismatch {
                    found: 999,
                    expected: 1
                }
            ),
            "got {err:?}"
        );
        assert!(err.to_string().contains("rebuild"), "{}", err);
    }

    #[test]
    fn garbage_file_is_corrupt_not_a_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("index.db");
        std::fs::write(&path, b"this is not an sqlite database at all").expect("write");
        let err = IndexDatabase::open_or_create(&path).expect_err("must refuse");
        assert!(matches!(err, IndexError::Corrupt { .. }), "got {err:?}");
    }

    #[test]
    fn tables_without_meta_are_corrupt() {
        let (_dir, path) = temp_index();
        IndexDatabase::open_or_create(&path).expect("open");
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute("DROP TABLE meta", []).expect("drop meta");
        }
        let err = IndexDatabase::open_or_create(&path).expect_err("must refuse");
        assert!(
            matches!(err, IndexError::Corrupt { .. }),
            "tables without meta must not be re-initialized over: {err:?}"
        );
    }

    #[test]
    fn invalid_lifecycle_state_is_corrupt() {
        let (_dir, path) = temp_index();
        IndexDatabase::open_or_create(&path).expect("open");
        {
            let conn = Connection::open(&path).expect("raw open");
            conn.execute(
                "UPDATE meta SET value = 'half-built' WHERE key = 'index_state'",
                [],
            )
            .expect("poison state");
        }
        let err = IndexDatabase::open_or_create(&path).expect_err("state must validate");
        assert!(matches!(err, IndexError::Corrupt { .. }), "got {err:?}");
    }

    #[test]
    fn transactions_roll_back_on_drop_and_commit_explicitly() {
        let (_dir, path) = temp_index();
        let mut db = IndexDatabase::open_or_create(&path).expect("open");

        // Drop = rollback: the fact-batch resume story depends on it.
        {
            let txn = db.transaction().expect("txn");
            txn.execute(
                "INSERT INTO meta (key, value) VALUES ('probe', 'rolled-back')",
                [],
            )
            .expect("insert");
        } // no commit
        assert!(db.meta("probe").expect("meta").is_none());

        let txn = db.transaction().expect("txn");
        txn.execute(
            "INSERT INTO meta (key, value) VALUES ('probe', 'committed')",
            [],
        )
        .expect("insert");
        txn.commit().expect("commit");
        assert_eq!(
            db.meta("probe").expect("meta").as_deref(),
            Some("committed")
        );
    }

    #[test]
    fn foreign_keys_are_enforced_on_the_index_connection() {
        let (_dir, path) = temp_index();
        let db = IndexDatabase::open_or_create(&path).expect("open");
        let err = db.connection().execute(
            "INSERT INTO files (path, lang, package_id, hash, skip, size, mtime, parse_status)
             VALUES ('x.go', 'go', 424242, x'00', NULL, 3, 0, 'ok')",
            [],
        );
        assert!(err.is_err(), "FK to a missing package must be rejected");
    }

    #[test]
    fn second_writer_fails_fast_as_locked() {
        let (_dir, path) = temp_index();
        let mut db = IndexDatabase::open_or_create(&path).expect("open");

        // A second connection takes the write lock with no busy timeout —
        // the fail-fast contract (ARCHITECTURE §4.4). `BEGIN DEFERRED`
        // still opens (readers coexist under WAL); the conflict fires at
        // the first WRITE and must surface as SQLITE_BUSY immediately,
        // never as a retry loop.
        let blocker = Connection::open(&path).expect("second connection");
        blocker
            .execute_batch("PRAGMA busy_timeout = 0")
            .expect("no timeout");
        blocker
            .execute("BEGIN IMMEDIATE", [])
            .expect("take write lock");

        {
            let txn = db.transaction().expect("deferred txn opens");
            let err = txn
                .execute(
                    "INSERT INTO meta (key, value) VALUES ('probe', 'blocked')",
                    [],
                )
                .expect_err("write must fail fast, not wait");
            assert_eq!(
                err.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy),
                "got {err:?}"
            );
            // The mapping the pipeline will use: busy ⇒ IndexError::Locked.
            assert!(matches!(
                IndexDatabase::lock(&path, err),
                IndexError::Locked
            ));
        } // dropped = rolled back

        blocker.execute("ROLLBACK", []).expect("release");
        // The lock clears: a write succeeds again.
        let txn = db.transaction().expect("txn after release");
        txn.execute("INSERT INTO meta (key, value) VALUES ('probe', 'ok')", [])
            .expect("write after release");
        txn.commit().expect("commit");
    }

    #[test]
    fn parent_directories_are_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deep").join("nested").join("index.db");
        IndexDatabase::open_or_create(&path).expect("open");
        assert!(path.is_file(), "index file must exist");
    }
}
