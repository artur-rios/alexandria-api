use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::errors::{DomainError, WRITE_TX};
use crate::libraries::commands::normalize_root;
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

    /// Store `name` as the library's name and answer the library as stored,
    /// or `None` when no library has the uuid. The name is written as given:
    /// the handler trims and validates it first.
    async fn rename(&self, uuid: Uuid, name: &str) -> Result<Option<Library>, DomainError>;

    /// Pin or unpin the library, answering whether a library matched. Setting
    /// the value it already has still matches: the answer is about the row.
    async fn set_pinned(&self, uuid: Uuid, pinned: bool) -> Result<bool, DomainError>;

    /// Mark or unmark the library as a favorite, answering whether a library
    /// matched. Independent of `set_pinned`.
    async fn set_favorite(&self, uuid: Uuid, favorite: bool) -> Result<bool, DomainError>;

    /// Delete the library and its catalog records, answering how many files
    /// went (libraries-only design, section 3).
    ///
    /// Every file belongs to a library, so a removed library's files have
    /// nowhere else to be: their rows go, with everything attached to them —
    /// subtype rows, watch and reading progress (their collection membership
    /// is a column of the row) — and so do the library's index runs and every
    /// failure recorded on a path under it. One transaction: nothing here
    /// cascades, so a delete that stopped halfway would leave rows pointing at
    /// a library that is gone. Nothing on disk is touched.
    async fn delete(&self, uuid: Uuid) -> Result<u32, DomainError>;
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
            pinned: false,
            favorite: false,
        })
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<Library>, DomainError> {
        let row = sqlx::query(
            "SELECT uuid, name, root_path, pinned, favorite FROM libraries WHERE uuid = ?",
        )
        .bind(uuid.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Disk(e.to_string()))?;

        row.map(|row| parse_library(&row)).transpose()
    }

    async fn list_all(&self) -> Result<Vec<Library>, DomainError> {
        let rows = sqlx::query(
            "SELECT uuid, name, root_path, pinned, favorite FROM libraries ORDER BY name",
        )
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

        // Compared in SQL rather than by reading every library back, so this
        // stays one query however many there are. The `replace` and the `||`
        // concatenation build each stored root's prefix the same way
        // `as_prefix` builds the candidate's — forward slashes, one trailing
        // separator — so a Windows root and a POSIX one compare alike.
        // `?3 IS NULL OR uuid <> ?3` rather than two queries: the exception
        // is part of the question, not a second pass over the answer.
        //
        // Overlap is asked in both directions, because either folder can be
        // the ancestor: is the candidate inside a stored root, and is a
        // stored root inside the candidate. Both are the same `substr`
        // comparison with the arguments swapped.
        //
        // `substr(…) = …` rather than `LIKE`, which is what this used to be.
        // SQLite's `LIKE` is case-insensitive over ASCII, so a library at
        // `/media/photos` claimed the files under `/media/Photos` — a
        // different folder on every filesystem this runs on but one — and
        // `find_overlapping` refused to register that folder at all.
        // `substr` compares under the column's own BINARY collation, which
        // is also what `BrowseLibraryHandler`'s `strip_prefix` does on the
        // Rust side; the two must agree, or a file can be claimed by a
        // library whose tree can then never list it. It also ends the
        // wildcard problem at the source: `%` and `_` are ordinary
        // characters to `=`, so nothing needs defusing.
        let row = sqlx::query(
            "SELECT uuid, name, root_path, pinned, favorite FROM libraries
             WHERE (substr(?, 1, length(rtrim(replace(root_path, '\\', '/'), '/') || '/'))
                        = (rtrim(replace(root_path, '\\', '/'), '/') || '/')
                 OR substr(rtrim(replace(root_path, '\\', '/'), '/') || '/', 1, length(?)) = ?)
               AND (? IS NULL OR uuid <> ?)
             LIMIT 1",
        )
        .bind(&candidate)
        .bind(&candidate)
        .bind(&candidate)
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

        let library: Option<(String, String, bool, bool)> = sqlx::query_as(
            "SELECT name, root_path, pinned, favorite FROM libraries WHERE uuid = ?",
        )
        .bind(uuid.to_string())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DomainError::Disk(e.to_string()))?;
        let Some((name, old_root, pinned, favorite)) = library else {
            return Err(DomainError::NotFound);
        };

        // Sliced by the old root's length rather than by matching text.
        // Normalizing separators replaces one character with one character,
        // so a normalized length indexes the stored path exactly — which is
        // what lets `D:\courses\rust\class-01\x.mp4` keep its backslashes
        // below the root while the root itself is replaced wholesale.
        //
        // Counted in characters, not bytes: SQLite's `substr` on TEXT counts
        // characters, so a byte length overshoots by one for every multi-byte
        // letter in the root (`/media/Música`) and cuts the head off every
        // path below it — the separator first.
        let old_len = Self::as_prefix(&old_root).chars().count() - 1;
        // Two spellings of the new root. The paths below it are rewritten
        // from the separator onward, so they take the root without its
        // trailing one (`E:` + `\\a.mp3`); the library itself is stored as
        // registration stores a root, separator kept where it is the root of
        // a drive or of the filesystem (`E:\\`, `/`) — `E:` alone is not the
        // root of drive E, and an empty root would overlap every library.
        let path_root = new_root.trim_end_matches(['/', '\\']).to_string();
        let new_root = normalize_root(new_root).to_string();

        let moved = sqlx::query(
            "UPDATE files SET path = ? || substr(path, ?) WHERE library_id =
             (SELECT id FROM libraries WHERE uuid = ?)",
        )
        .bind(&path_root)
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
                pinned,
                favorite,
            },
            u32::try_from(moved).unwrap_or(u32::MAX),
        ))
    }

    async fn rename(&self, uuid: Uuid, name: &str) -> Result<Option<Library>, DomainError> {
        // One statement, so the row answered is the row written and no run
        // or lock is consulted: a name is not part of anything a run reads.
        let row = sqlx::query(
            "UPDATE libraries SET name = ? WHERE uuid = ?
             RETURNING uuid, name, root_path, pinned, favorite",
        )
        .bind(name)
        .bind(uuid.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Disk(e.to_string()))?;

        row.map(|row| parse_library(&row)).transpose()
    }

    async fn set_pinned(&self, uuid: Uuid, pinned: bool) -> Result<bool, DomainError> {
        let updated = sqlx::query("UPDATE libraries SET pinned = ? WHERE uuid = ?")
            .bind(pinned)
            .bind(uuid.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        Ok(updated.rows_affected() > 0)
    }

    async fn set_favorite(&self, uuid: Uuid, favorite: bool) -> Result<bool, DomainError> {
        let updated = sqlx::query("UPDATE libraries SET favorite = ? WHERE uuid = ?")
            .bind(favorite)
            .bind(uuid.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| DomainError::Disk(e.to_string()))?;

        Ok(updated.rows_affected() > 0)
    }

    async fn delete(&self, uuid: Uuid) -> Result<u32, DomainError> {
        let disk = |e: sqlx::Error| DomainError::Disk(e.to_string());
        let mut tx = self.pool.begin_with(WRITE_TX).await.map_err(disk)?;

        let library: Option<(i64, String)> =
            sqlx::query_as("SELECT id, root_path FROM libraries WHERE uuid = ?")
                .bind(uuid.to_string())
                .fetch_optional(&mut *tx)
                .await
                .map_err(disk)?;
        let Some((id, root_path)) = library else {
            return Err(DomainError::NotFound);
        };

        // The rows that hold a file id: the progress tables have no FOREIGN
        // KEY, and the subtype tables' cascade only runs with foreign keys on,
        // which this does not rely on.
        for sql in [
            "DELETE FROM reading_progress WHERE item_file_id IN (SELECT id FROM files WHERE library_id = ?)",
            "DELETE FROM watch_progress WHERE video_file_id IN (SELECT id FROM files WHERE library_id = ?)",
            "DELETE FROM video_files WHERE file_id IN (SELECT id FROM files WHERE library_id = ?)",
            "DELETE FROM html_pages WHERE file_id IN (SELECT id FROM files WHERE library_id = ?)",
            "DELETE FROM text_files WHERE file_id IN (SELECT id FROM files WHERE library_id = ?)",
            "DELETE FROM documents WHERE file_id IN (SELECT id FROM files WHERE library_id = ?)",
            "DELETE FROM comic_books WHERE file_id IN (SELECT id FROM files WHERE library_id = ?)",
            "DELETE FROM images WHERE file_id IN (SELECT id FROM files WHERE library_id = ?)",
            "DELETE FROM audio_files WHERE file_id IN (SELECT id FROM files WHERE library_id = ?)",
        ] {
            sqlx::query(sql).bind(id).execute(&mut *tx).await.map_err(disk)?;
        }

        let removed = sqlx::query("DELETE FROM files WHERE library_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(disk)?
            .rows_affected();

        // The library's own runs and their failures, then the failures any
        // other run (a refresh) recorded under its folder: those files are
        // not in the catalog any more. The same separator-safe prefix test
        // `find_overlapping` uses, so `/media/course` does not take
        // `/media/course-notes` with it.
        sqlx::query(
            "DELETE FROM catalog_run_failures \
             WHERE run_id IN (SELECT id FROM catalog_runs WHERE library_id = ?)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(disk)?;
        let prefix = Self::as_prefix(&root_path);
        sqlx::query(
            "DELETE FROM catalog_run_failures \
             WHERE substr(replace(path, '\\', '/'), 1, length(?)) = ?",
        )
        .bind(&prefix)
        .bind(&prefix)
        .execute(&mut *tx)
        .await
        .map_err(disk)?;
        sqlx::query("DELETE FROM catalog_runs WHERE library_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(disk)?;

        sqlx::query("DELETE FROM libraries WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(disk)?;

        tx.commit().await.map_err(disk)?;
        Ok(u32::try_from(removed).unwrap_or(u32::MAX))
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
        pinned: row
            .try_get("pinned")
            .map_err(|e| DomainError::Disk(e.to_string()))?,
        favorite: row
            .try_get("favorite")
            .map_err(|e| DomainError::Disk(e.to_string()))?,
    })
}
