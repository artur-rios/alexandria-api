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
    async fn find_overlapping(&self, root_path: &str) -> Result<Option<Library>, DomainError>;

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

    /// `path` with exactly one trailing separator.
    ///
    /// Prefix comparisons are how containment is decided below, and without
    /// the separator `/library/course` would contain `/library/course-notes`
    /// — a different folder that merely starts with the same letters.
    fn as_prefix(path: &str) -> String {
        let trimmed = path.trim_end_matches(['/', '\\']);
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
            .map_err(|e| DomainError::Disk(e.to_string()))?;

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

    async fn find_overlapping(&self, root_path: &str) -> Result<Option<Library>, DomainError> {
        let candidate = Self::as_prefix(root_path);

        // Compared in SQL rather than by reading every library back, so this
        // stays one query however many there are. The `||` concatenation
        // builds each stored root's prefix the same way `as_prefix` builds
        // the candidate's.
        let row = sqlx::query(
            "SELECT uuid, name, root_path FROM libraries
             WHERE ? LIKE (rtrim(root_path, '/\\') || '/%')
                OR (rtrim(root_path, '/\\') || '/') LIKE (? || '%')
             LIMIT 1",
        )
        .bind(&candidate)
        .bind(&candidate)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DomainError::Disk(e.to_string()))?;

        row.map(|row| parse_library(&row)).transpose()
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
             WHERE library_id IS NULL AND path LIKE (? || '%')",
        )
        .bind(id)
        .bind(Self::as_prefix(&root_path))
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
