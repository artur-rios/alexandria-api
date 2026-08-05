use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use uuid::Uuid;

use crate::errors::DomainError;
use crate::reading_lists::model::{
    NewReadingList, ReadingList, ReadingProgress, ReadingState, ReadingTargetKind,
};

/// Reading lists repository port. The create handler depends on this trait
/// so its decision logic (validation, uuid minting) is unit-tested against
/// an in-memory fake with no database (Testing Specification §6.2). The
/// Sqlite implementation persists `reading_lists` and `reading_progress`
/// rows.
///
/// UC-29..31 add their own methods when they ship.
#[allow(async_fn_in_trait)]
pub trait ReadingListRepository: Send + Sync {
    /// Persist a new reading list and return the stored record (UC-26 /
    /// FR-RL-01). The caller has already validated the name.
    async fn insert_reading_list(
        &self,
        new_reading_list: NewReadingList,
    ) -> Result<ReadingList, DomainError>;

    /// Look a reading list up by its public uuid (UC-28 AF-02). `None` when
    /// no such reading list exists.
    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<ReadingList>, DomainError>;

    /// Every persisted reading list, ordered by name (UC-27 / FR-RL-08).
    async fn list_all(&self) -> Result<Vec<ReadingList>, DomainError>;

    /// Every ReadingProgress row for the reading list identified by
    /// `reading_list_uuid`, ordered by item uuid (UC-27 / FR-RL-08).
    async fn list_progress(
        &self,
        reading_list_uuid: Uuid,
    ) -> Result<Vec<ReadingProgress>, DomainError>;

    /// Link the item identified by `item_uuid` (a `target_kind` of
    /// `Document` or `Comic`) to the reading list identified by
    /// `reading_list_uuid`, creating a `Pending` ReadingProgress (UC-28 /
    /// FR-RL-02), and return it. Idempotent: if the pair is already linked,
    /// the existing ReadingProgress is returned unchanged rather than reset
    /// to `Pending` — UC-29 may have already advanced it. The caller has
    /// already confirmed both exist and that the item is read-eligible.
    async fn add_item(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
        target_kind: ReadingTargetKind,
    ) -> Result<ReadingProgress, DomainError>;

    /// Look up the ReadingProgress linking `item_uuid` to
    /// `reading_list_uuid` (UC-29 AF-02). `None` when the item is not on
    /// that reading list.
    async fn find_progress(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
    ) -> Result<Option<ReadingProgress>, DomainError>;

    /// Replace the state and issue fields of the ReadingProgress linking
    /// `item_uuid` to `reading_list_uuid` (UC-29 / FR-RL-04, FR-RL-05), and
    /// return the updated record. Full replace: `current_issue` and
    /// `total_issues` are written as given, `None` writes `NULL`. The
    /// caller has already confirmed the ReadingProgress exists and that the
    /// transition to `state` is valid.
    async fn update_progress(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
        state: ReadingState,
        current_issue: Option<i64>,
        total_issues: Option<i64>,
    ) -> Result<ReadingProgress, DomainError>;

    /// Delete the ReadingProgress linking `item_uuid` to
    /// `reading_list_uuid` (UC-30 / FR-RL-06). The file itself is
    /// untouched. Returns `NotFound` when no such ReadingProgress exists
    /// (AF-01).
    async fn remove_progress(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
    ) -> Result<(), DomainError>;
}

#[derive(Clone)]
pub struct SqliteReadingListRepository {
    pool: SqlitePool,
}

impl SqliteReadingListRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl ReadingListRepository for SqliteReadingListRepository {
    async fn insert_reading_list(
        &self,
        new_reading_list: NewReadingList,
    ) -> Result<ReadingList, DomainError> {
        sqlx::query("INSERT INTO reading_lists (uuid, name) VALUES (?, ?)")
            .bind(new_reading_list.uuid.to_string())
            .bind(&new_reading_list.name)
            .execute(&self.pool)
            .await?;

        Ok(ReadingList {
            uuid: new_reading_list.uuid,
            name: new_reading_list.name,
        })
    }

    async fn find_by_uuid(&self, uuid: Uuid) -> Result<Option<ReadingList>, DomainError> {
        let row = sqlx::query("SELECT uuid, name FROM reading_lists WHERE uuid = ?")
            .bind(uuid.to_string())
            .fetch_optional(&self.pool)
            .await?;

        Ok(match row {
            Some(row) => {
                let uuid: String = row.try_get("uuid")?;
                let name: String = row.try_get("name")?;
                Some(ReadingList {
                    uuid: Uuid::parse_str(&uuid).map_err(|err| {
                        DomainError::internal(format!("corrupt reading list uuid: {err}"))
                    })?,
                    name,
                })
            }
            None => None,
        })
    }

    async fn list_all(&self) -> Result<Vec<ReadingList>, DomainError> {
        let rows = sqlx::query("SELECT uuid, name FROM reading_lists ORDER BY name")
            .fetch_all(&self.pool)
            .await?;

        rows.into_iter()
            .map(|row| {
                let uuid: String = row.try_get("uuid")?;
                let name: String = row.try_get("name")?;
                Ok(ReadingList {
                    uuid: Uuid::parse_str(&uuid).map_err(|err| {
                        DomainError::internal(format!("corrupt reading list uuid: {err}"))
                    })?,
                    name,
                })
            })
            .collect()
    }

    async fn list_progress(
        &self,
        reading_list_uuid: Uuid,
    ) -> Result<Vec<ReadingProgress>, DomainError> {
        let rows = sqlx::query(
            "SELECT f.uuid AS item_uuid, rp.target_kind, rp.state, rp.current_issue, rp.total_issues \
             FROM reading_progress rp \
             JOIN files f ON f.id = rp.item_file_id \
             WHERE rp.reading_list_id = (SELECT id FROM reading_lists WHERE uuid = ?) \
             ORDER BY f.uuid",
        )
        .bind(reading_list_uuid.to_string())
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                let item_uuid: String = row.try_get("item_uuid")?;
                let target_kind_str: String = row.try_get("target_kind")?;
                let state_str: String = row.try_get("state")?;
                let current_issue: Option<i64> = row.try_get("current_issue")?;
                let total_issues: Option<i64> = row.try_get("total_issues")?;
                Ok(ReadingProgress {
                    reading_list_uuid,
                    item_uuid: Uuid::parse_str(&item_uuid).map_err(|err| {
                        DomainError::internal(format!("corrupt item uuid: {err}"))
                    })?,
                    target_kind: ReadingTargetKind::parse(&target_kind_str).ok_or_else(|| {
                        DomainError::internal(format!(
                            "corrupt reading_progress target_kind: {target_kind_str}"
                        ))
                    })?,
                    state: ReadingState::parse(&state_str).ok_or_else(|| {
                        DomainError::internal(format!(
                            "corrupt reading_progress state: {state_str}"
                        ))
                    })?,
                    current_issue,
                    total_issues,
                })
            })
            .collect()
    }

    async fn add_item(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
        target_kind: ReadingTargetKind,
    ) -> Result<ReadingProgress, DomainError> {
        sqlx::query(
            "INSERT INTO reading_progress (reading_list_id, item_file_id, target_kind, state) \
             VALUES ( \
                (SELECT id FROM reading_lists WHERE uuid = ?), \
                (SELECT id FROM files WHERE uuid = ?), \
                ?, 'pending' \
             ) \
             ON CONFLICT (reading_list_id, item_file_id) DO NOTHING",
        )
        .bind(reading_list_uuid.to_string())
        .bind(item_uuid.to_string())
        .bind(target_kind.as_str())
        .execute(&self.pool)
        .await?;

        self.find_progress(reading_list_uuid, item_uuid)
            .await?
            .ok_or(DomainError::NotFound)
    }

    async fn find_progress(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
    ) -> Result<Option<ReadingProgress>, DomainError> {
        let row = sqlx::query(
            "SELECT rp.target_kind, rp.state, rp.current_issue, rp.total_issues \
             FROM reading_progress rp \
             JOIN reading_lists rl ON rl.id = rp.reading_list_id \
             JOIN files f ON f.id = rp.item_file_id \
             WHERE rl.uuid = ? AND f.uuid = ?",
        )
        .bind(reading_list_uuid.to_string())
        .bind(item_uuid.to_string())
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let target_kind_str: String = row.try_get("target_kind")?;
        let state_str: String = row.try_get("state")?;
        let current_issue: Option<i64> = row.try_get("current_issue")?;
        let total_issues: Option<i64> = row.try_get("total_issues")?;

        Ok(Some(ReadingProgress {
            reading_list_uuid,
            item_uuid,
            target_kind: ReadingTargetKind::parse(&target_kind_str).ok_or_else(|| {
                DomainError::internal(format!(
                    "corrupt reading_progress target_kind: {target_kind_str}"
                ))
            })?,
            state: ReadingState::parse(&state_str).ok_or_else(|| {
                DomainError::internal(format!("corrupt reading_progress state: {state_str}"))
            })?,
            current_issue,
            total_issues,
        }))
    }

    async fn update_progress(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
        state: ReadingState,
        current_issue: Option<i64>,
        total_issues: Option<i64>,
    ) -> Result<ReadingProgress, DomainError> {
        sqlx::query(
            "UPDATE reading_progress \
             SET state = ?, current_issue = ?, total_issues = ? \
             WHERE reading_list_id = (SELECT id FROM reading_lists WHERE uuid = ?) \
               AND item_file_id = (SELECT id FROM files WHERE uuid = ?)",
        )
        .bind(state.as_str())
        .bind(current_issue)
        .bind(total_issues)
        .bind(reading_list_uuid.to_string())
        .bind(item_uuid.to_string())
        .execute(&self.pool)
        .await?;

        self.find_progress(reading_list_uuid, item_uuid)
            .await?
            .ok_or(DomainError::NotFound)
    }

    async fn remove_progress(
        &self,
        reading_list_uuid: Uuid,
        item_uuid: Uuid,
    ) -> Result<(), DomainError> {
        let affected = sqlx::query(
            "DELETE FROM reading_progress \
             WHERE reading_list_id = (SELECT id FROM reading_lists WHERE uuid = ?) \
               AND item_file_id = (SELECT id FROM files WHERE uuid = ?)",
        )
        .bind(reading_list_uuid.to_string())
        .bind(item_uuid.to_string())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if affected == 0 {
            return Err(DomainError::NotFound);
        }
        Ok(())
    }
}
