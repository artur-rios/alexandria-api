use serde::Serialize;

use crate::errors::DomainError;

#[derive(Debug, Clone, Serialize)]
pub struct FileRecord {
    pub id: String,
    pub path: String,
    pub content_hash: String,
}

#[allow(async_fn_in_trait)]
pub trait CatalogRepository: Send + Sync {
    async fn find_by_id(&self, id: &str) -> Result<Option<FileRecord>, DomainError>;

    async fn save(&self, record: &FileRecord) -> Result<(), DomainError>;
}
