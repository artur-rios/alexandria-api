//! Shared test fakes for `playback`'s unit tests.
//!
//! `CatalogRepository` is a ~23-method trait; every playback guard calls
//! exactly one of them (`find_by_uuid`). Four submodules (`mod`, `source`,
//! `comic_page`, `thumbnail`) each used to carry their own copy of a fake
//! that answers that one method and refuses every other with
//! `unimplemented!()` — so an accidental extra call fails the test loudly
//! instead of silently returning a default. This is the one copy they share.
//!
//! `pub(crate)`, not `pub`: these fakes are test-only wiring internal to
//! this crate, never part of its public API.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::auth::{AuthService, Principal};
use crate::catalog::model::{File, FileState, FileType, NewFile, StateFilter, SubtypeMetadata};
use crate::catalog::repos::CatalogRepository;
use crate::config::AuthMode;
use crate::errors::DomainError;

/// Auth fake: accepts exactly one token, rejects everything else.
///
/// `mod`'s tests exercise both branches (a good token and a bad one);
/// `source`, `comic_page`, and `thumbnail` only ever pass the good token, so
/// for them this behaves exactly like the accept-anything fake they used to
/// define separately.
#[derive(Clone)]
pub(crate) struct FakeAuth {
    pub(crate) good: &'static str,
}

impl AuthService for FakeAuth {
    async fn authenticate(&self, token: &str) -> Result<Principal, DomainError> {
        if token == self.good {
            Ok(Principal {
                user_id: "owner".to_string(),
            })
        } else {
            Err(DomainError::Unauthorized)
        }
    }

    fn mode(&self) -> AuthMode {
        AuthMode::External
    }
}

/// Catalog repo fake returning one canned file, or none. This guard calls
/// exactly one `CatalogRepository` method (`find_by_uuid`); every other
/// method is `unimplemented!()` so an accidental extra call fails the test
/// loudly instead of silently returning a default.
#[derive(Clone)]
pub(crate) struct FakeRepo {
    file: Option<File>,
}

impl FakeRepo {
    pub(crate) fn with_file(file: File) -> Self {
        Self { file: Some(file) }
    }

    pub(crate) fn none() -> Self {
        Self { file: None }
    }
}

impl CatalogRepository for FakeRepo {
    async fn find_by_path(&self, _path: &str) -> Result<Option<File>, DomainError> {
        unimplemented!()
    }

    async fn find_by_uuid(&self, _uuid: Uuid) -> Result<Option<File>, DomainError> {
        Ok(self.file.clone())
    }

    async fn insert_file(&self, _new_file: NewFile) -> Result<File, DomainError> {
        unimplemented!()
    }

    async fn list_all(&self) -> Result<Vec<File>, DomainError> {
        unimplemented!()
    }

    async fn refresh_hash(
        &self,
        _path: &str,
        _content_hash: &str,
        _indexed_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        unimplemented!()
    }

    async fn mark_missing(
        &self,
        _path: &str,
        _missing_at: DateTime<Utc>,
    ) -> Result<(), DomainError> {
        unimplemented!()
    }

    async fn update_metadata(
        &self,
        _uuid: Uuid,
        _metadata: &SubtypeMetadata,
    ) -> Result<(), DomainError> {
        unimplemented!()
    }

    async fn list_filtered(
        &self,
        _file_type: Option<FileType>,
        _state: StateFilter,
        _collection_uuid: Option<Uuid>,
    ) -> Result<Vec<File>, DomainError> {
        unimplemented!()
    }

    async fn find_metadata_by_uuid(
        &self,
        _uuid: Uuid,
    ) -> Result<Option<SubtypeMetadata>, DomainError> {
        unimplemented!()
    }

    async fn set_image_dimensions(
        &self,
        _uuid: Uuid,
        _width: i64,
        _height: i64,
    ) -> Result<(), DomainError> {
        unimplemented!()
    }

    async fn find_image_dimensions(&self, _uuid: Uuid) -> Result<Option<(i64, i64)>, DomainError> {
        unimplemented!()
    }

    async fn set_document_page_count(
        &self,
        _uuid: Uuid,
        _page_count: i64,
    ) -> Result<(), DomainError> {
        unimplemented!()
    }

    async fn find_document_page_count(&self, _uuid: Uuid) -> Result<Option<i64>, DomainError> {
        unimplemented!()
    }

    async fn set_video_duration(
        &self,
        _uuid: Uuid,
        _duration_seconds: f64,
    ) -> Result<(), DomainError> {
        unimplemented!()
    }

    async fn find_video_duration(&self, _uuid: Uuid) -> Result<Option<f64>, DomainError> {
        unimplemented!()
    }

    async fn set_comic_page_count(&self, _uuid: Uuid, _page_count: i64) -> Result<(), DomainError> {
        unimplemented!()
    }

    async fn find_comic_page_count(&self, _uuid: Uuid) -> Result<Option<i64>, DomainError> {
        unimplemented!()
    }

    async fn rename_file(
        &self,
        _uuid: Uuid,
        _new_name: &str,
        _new_path: &str,
    ) -> Result<File, DomainError> {
        unimplemented!()
    }

    async fn soft_delete(
        &self,
        _uuid: Uuid,
        _deleted_at: DateTime<Utc>,
    ) -> Result<File, DomainError> {
        unimplemented!()
    }

    async fn restore(&self, _uuid: Uuid) -> Result<File, DomainError> {
        unimplemented!()
    }

    async fn purge(&self, _uuid: Uuid) -> Result<(), DomainError> {
        unimplemented!()
    }

    async fn set_collection(&self, _uuid: Uuid, _collection_uuid: Uuid) -> Result<(), DomainError> {
        unimplemented!()
    }

    async fn clear_collection(
        &self,
        _uuid: Uuid,
        _collection_uuid: Uuid,
    ) -> Result<(), DomainError> {
        unimplemented!()
    }
}

/// A canned `File`. `content_hash` is always `"abc"` — the thumbnail
/// module's cache-key tests are written against that exact value, so it is
/// not a parameter here; `path`, `file_type`, `state`, and `missing_at` are,
/// because different tests across the four modules genuinely need different
/// values for those.
pub(crate) fn a_file(
    path: &str,
    file_type: FileType,
    state: FileState,
    missing_at: Option<DateTime<Utc>>,
) -> File {
    File {
        uuid: Uuid::nil(),
        path: path.to_string(),
        name: path
            .rsplit_once('/')
            .map_or(path, |(_, name)| name)
            .to_string(),
        file_type,
        content_hash: "abc".to_string(),
        state,
        deleted_at: None,
        indexed_at: Utc::now(),
        missing_at,
    }
}
