use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::errors::{DomainError, WRITE_TX};
use crate::libraries::model::{Library, NewLibrary};

/// Libraries repository port.
///
/// The handlers depend on this rather than on `SqlitePool` so their
/// decisions — what counts as nesting, what a rename may change — are unit
/// tested against a fake with no database (Testing Specification §6.2).
#[allow(async_fn_in_trait)]
pub trait LibraryRepository: Send + Sync {
    /// Persist a library and return the stored record.
    async fn insert(&self, library: NewLibrary) -> Result<Library, DomainError>;

    /// The library `uuid` identifies, or `None`.
    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Library>, DomainError>;

    /// Every library, by name.
    async fn list_all(&self) -> Result<Vec<Library>, DomainError>;

    /// The library whose root contains `path`, or whose root is beneath it.
    ///
    /// Both directions, because both are the same refusal: a library may
    /// neither be created inside an existing one nor wrapped around one. A
    /// file in two libraries means two answers to "where does this appear",
    /// and every screen would need a rule for choosing (design section 5).
    ///
    /// `except` is the library asking. Correcting a root has to ask this
    /// question about itself, and a library always overlaps where it already
    /// is — so without the exception, moving a folder one level up would be
    /// refused on the grounds that it collides with itself.
    async fn find_overlapping(
        &self,
        root_path: &str,
        except: Option<Uuid>,
    ) -> Result<Option<Library>, DomainError>;

    /// Point the library at `new_root`, taking the files it holds with it.
    ///
    /// The folder moved on disk; the files under it moved with it, and their
    /// stored paths are the part that has to follow. Every claimed path has
    /// its root replaced, so the library is browsable immediately rather than
    /// after a re-walk of a disk that has not changed (design section 1) —
    /// and the records keep their identity: a file's uuid, its hash, its
    /// place in a watchlist and its progress all survive a move, which is
    /// exactly what re-indexing at the new location would throw away.
    ///
    /// Returns the moved library and how many files came with it.
    async fn move_root(&self, uuid: Uuid, new_root: &str) -> Result<(Library, u32), DomainError>;

    /// Attach every active file under the library's root to it.
    ///
    /// Run after an index rather than during it: the walk records files
    /// through the catalog's own path, which knows nothing of libraries, and
    /// teaching it would thread a concept through every insert for the sake
    /// of one column. Returns how many files it claimed.
    async fn claim_files(&self, uuid: Uuid) -> Result<u32, DomainError>;

    /// Delete the library and return its files to the type panels.
    ///
    /// The files themselves are untouched: what goes is the grouping. There
    /// is no foreign key to cascade (SQLite cannot add one through ALTER
    /// TABLE), so clearing the column is explicit — and it is also what
    /// makes marking a folder by mistake recoverable.
    async fn delete(&self, uuid: Uuid) -> Result<(), DomainError>;
}

/// The Sqlite implementation.
pub struct SqliteLibraryRepository {
    pool: SqlitePool,
}

impl SqliteLibraryRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// `path` in forward slashes, with exactly one trailing separator.
    ///
    /// Prefix comparisons are how containment is decided below, and without
    /// the separator `/library/course` would contain `/library/course-notes`
    /// — a different folder that merely starts with the same letters.
    ///
    /// Forward slashes on both sides of every comparison, because Windows
    /// paths arrive with backslashes: appending `/` to `D:\course` produced
    /// `D:\course/`, which is a prefix of nothing the catalog holds, and a
    /// library there claimed no file it had while looking registered.
    fn as_prefix(path: &str) -> String {
        let trimmed = path.replace('\\', "/");
        let trimmed = trimmed.trim_end_matches('/');
        format!("{trimmed}/")
    }

    /// `path` with LIKE's wildcards defused, for use as a pattern.
    ///
    /// `LIKE` is how containment is decided below, and a folder name is not a
    /// pattern: `_` matches any single character and `%` matches any run of
    /// them. A library at `/media/tv_shows` was claiming every file under
    /// `/media/tv-shows` — a different folder — because the underscore in a
    /// perfectly ordinary directory name is a wildcard. Both characters are
    /// legal in a filename on Windows and Linux alike, so this is a name the
    /// owner can easily have.
    ///
    /// Escaped with a backslash, paired with an `ESCAPE '\'` clause on every
    /// comparison that uses one of these. The backslash itself needs no
    /// escaping here: every caller normalizes separators to forward slashes
    /// first, so a pattern reaching this function holds none.
    fn escape_like(path: &str) -> String {
        path.replace('%', "\\%").replace('_', "\\_")
    }
}

impl LibraryRepository for SqliteLibraryRepository {
    async fn insert(&self, library: NewLibrary) -> Result<Library, DomainError> {
        let mut tx = self
            .pool
            .begin_with(WRITE_TX)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        sqlx::query("INSERT INTO libraries (uuid, name, root_path) VALUES (?, ?, ?)")
            .bind(library.uuid.to_string())
            .bind(&library.name)
            .bind(&library.root_path)
            .execute(&mut *tx)
            .await
            // Defensive, and deliberately so. `root_path` is UNIQUE, and
            // the handler's containment check already refuses a folder that
            // is registered — a root always contains itself, so the exact
            // duplicate is caught there and this arm is unreachable through
            // one caller. Two registrations racing can still both pass that
            // check and reach the constraint, and the loser should be told
            // the same thing the first path tells it. Named as the conflict
            // it is, the way `move_root` below names its own: a disk error
            // would say their storage is failing when what happened is that
            // the folder is already a library.
            .map_err(|e| match e {
                sqlx::Error::Database(ref db) if db.is_unique_violation() => {
                    DomainError::Conflict("that folder is already a library".to_string())
                }
                other => DomainError::Disk(other.to_string()),
            })?;

        tx.commit()
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        Ok(Library {
            uuid: library.uuid,
            name: library.name,
            root_path: library.root_path,
        })
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Library>, DomainError> {
        let row = sqlx::query("SELECT uuid, name, root_path FROM libraries WHERE uuid = ?")
            .bind(uuid.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        row.map(|row| parse_library(&row)).transpose()
    }

    async fn list_all(&self) -> Result<Vec<Library>, DomainError> {
        let rows = sqlx::query("SELECT uuid, name, root_path FROM libraries ORDER BY name")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        rows.iter().map(parse_library).collect()
    }

    async fn find_overlapping(
        &self,
        root_path: &str,
        except: Option<Uuid>,
    ) -> Result<Option<Library>, DomainError> {
        let candidate = Self::as_prefix(root_path);
        let candidate_pattern = Self::escape_like(&candidate);

        // Compared in SQL rather than by reading every library back, so this
        // stays one query however many there are. The `replace` and the `||`
        // concatenation build each stored root's prefix the same way
        // `as_prefix` builds the candidate's — forward slashes, one trailing
        // separator — so a Windows root and a POSIX one compare alike.
        // `?3 IS NULL OR uuid <> ?3` rather than two queries: the exception
        // is part of the question, not a second pass over the answer.
        //
        // Each side appears twice with different treatment, which is the
        // fiddly part: a root is a *value* in one direction and a *pattern*
        // in the other, and only the pattern may carry wildcards. So the
        // candidate is bound raw where it is compared and escaped where it
        // is matched against, and the stored root gets the same two forms
        // from the nested `replace`s. Escaping the value side too would make
        // a root containing `_` fail to match itself.
        let row = sqlx::query(
            "SELECT uuid, name, root_path FROM libraries
             WHERE (? LIKE (replace(replace(rtrim(replace(root_path, '\\', '/'), '/'),
                                            '%', '\\%'), '_', '\\_') || '/%') ESCAPE '\\'
                 OR (rtrim(replace(root_path, '\\', '/'), '/') || '/')
                        LIKE (? || '%') ESCAPE '\\')
               AND (? IS NULL OR uuid <> ?)
             LIMIT 1",
        )
        .bind(&candidate)
        .bind(&candidate_pattern)
        .bind(except.map(|uuid| uuid.to_string()))
        .bind(except.map(|uuid| uuid.to_string()))
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Disk(e.to_string()))?;

        row.map(|row| parse_library(&row)).transpose()
    }

    async fn move_root(&self, uuid: Uuid, new_root: &str) -> Result<(Library, u32), DomainError> {
        let mut tx = self
            .pool
            .begin_with(WRITE_TX)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        let library: Option<(String, String)> =
            sqlx::query_as("SELECT name, root_path FROM libraries WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| DomainError::Disk(e.to_string()))?;
        let Some((name, old_root)) = library else {
            return Err(DomainError::NotFound);
        };

        // Sliced by the old root's length rather than by matching text.
        // Normalizing separators replaces one character with one character,
        // so a normalized length indexes the stored path exactly — which is
        // what lets `D:\courses\rust\class-01\x.mp4` keep its backslashes
        // below the root while the root itself is replaced wholesale.
        let old_len = Self::as_prefix(&old_root).len() - 1;
        let new_root = new_root.trim_end_matches(['/', '\\']).to_string();

        let moved = sqlx::query(
            "UPDATE files SET path = ? || substr(path, ?) WHERE library_id =
             (SELECT id FROM libraries WHERE uuid = ?)",
        )
        .bind(&new_root)
        .bind(i64::try_from(old_len).unwrap_or(i64::MAX) + 1)
        .bind(uuid.to_string())
        .execute(&mut *tx)
        .await
        // A path already taken: the owner indexed the new location before
        // correcting the root, so the catalog holds both copies. Named
        // rather than reported as a disk error, because the way out is a
        // decision — re-index and let the old records go missing — and not
        // something to retry.
        .map_err(|e| match e {
            sqlx::Error::Database(ref db) if db.is_unique_violation() => DomainError::Conflict(
                "the catalog already holds files at that folder; index it and remove this \
                 library instead of moving it"
                    .to_string(),
            ),
            other => DomainError::Disk(other.to_string()),
        })?
        .rows_affected();

        sqlx::query("UPDATE libraries SET root_path = ? WHERE uuid = ?")
            .bind(&new_root)
            .bind(uuid.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|e| match e {
                sqlx::Error::Database(ref db) if db.is_unique_violation() => {
                    DomainError::Conflict("another library already has that folder".to_string())
                }
                other => DomainError::Disk(other.to_string()),
            })?;

        tx.commit()
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        Ok((
            Library {
                uuid,
                name,
                root_path: new_root,
            },
            u32::try_from(moved).unwrap_or(u32::MAX),
        ))
    }

    async fn claim_files(&self, uuid: Uuid) -> Result<u32, DomainError> {
        let mut tx = self
            .pool
            .begin_with(WRITE_TX)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        let library: Option<(i64, String)> =
            sqlx::query_as("SELECT id, root_path FROM libraries WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| DomainError::Disk(e.to_string()))?;
        let Some((id, root_path)) = library else {
            return Err(DomainError::NotFound);
        };

        let claimed = sqlx::query(
            "UPDATE files SET library_id = ?
             WHERE library_id IS NULL
               AND replace(path, '\\', '/') LIKE (? || '%') ESCAPE '\\'",
        )
        .bind(id)
        // The root is the pattern here, so its wildcards are defused. A
        // library at `/media/tv_shows` claimed `/media/tv-shows` without it.
        .bind(Self::escape_like(&Self::as_prefix(&root_path)))
        .execute(&mut *tx)
        .await
        .map_err(|e| DomainError::Disk(e.to_string()))?
        .rows_affected();

        tx.commit()
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        Ok(u32::try_from(claimed).unwrap_or(u32::MAX))
    }

    async fn delete(&self, uuid: Uuid) -> Result<(), DomainError> {
        let mut tx = self
            .pool
            .begin_with(WRITE_TX)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        let id: Option<i64> = sqlx::query_scalar("SELECT id FROM libraries WHERE uuid = ?")
            .bind(uuid.to_string())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;
        let Some(id) = id else {
            return Err(DomainError::NotFound);
        };

        // Released before the library row goes, and in the same transaction:
        // nothing cascades here, so a delete that failed halfway would leave
        // files pointing at a library that no longer exists — invisible to
        // every listing, since they are excluded by the column being set.
        sqlx::query("UPDATE files SET library_id = NULL WHERE library_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        sqlx::query("DELETE FROM libraries WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))
    }
}

fn parse_library(row: &sqlx::sqlite::SqliteRow) -> Result<Library, DomainError> {
    let uuid: String = row
        .try_get("uuid")
        .map_err(|e| DomainError::Disk(e.to_string()))?;

    Ok(Library {
        uuid: Uuid::parse_str(&uuid).map_err(|e| DomainError::Disk(e.to_string()))?,
        name: row
            .try_get("name")
            .map_err(|e| DomainError::Disk(e.to_string()))?,
        root_path: row
            .try_get("root_path")
            .map_err(|e| DomainError::Disk(e.to_string()))?,
    })
}
