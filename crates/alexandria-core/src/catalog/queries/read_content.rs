use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::fs::Filesystem;
use crate::catalog::model::{FileContent, FileType};
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;

/// UC-32 — Read text file content (FR-TX-01). Reads the current on-disk
/// content of a TextFile.
///
/// Generic over auth, catalog repository, and filesystem so the same
/// decision logic is unit-tested against trait fakes (no real DB, fs, or
/// auth in unit tests), then wired with the concrete collaborators at
/// runtime (services.rs). Both the HTTP and FFI surfaces call this handler
/// so the two stay at parity (FR-FC-24 / NFR-09).
pub struct ReadTextFileContentHandler<A, R, F> {
    auth: A,
    repo: R,
    fs: F,
}

impl<A, R, F> ReadTextFileContentHandler<A, R, F>
where
    A: AuthService,
    R: CatalogRepository,
    F: Filesystem,
{
    pub fn new(auth: A, repo: R, fs: F) -> Self {
        Self { auth, repo, fs }
    }

    /// Read the content of the TextFile identified by `uuid`.
    pub async fn read(&self, uuid: Uuid, token: &str) -> Result<FileContent, DomainError> {
        // AF-04: the caller must be authenticated.
        self.auth.authenticate(token).await?;

        // AF-03: the file must exist.
        let file = self
            .repo
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        // AF-01: the target file must be a TextFile.
        if file.file_type != FileType::Text {
            return Err(DomainError::InvalidInput(format!(
                "file {uuid} is not a text file"
            )));
        }

        // AF-02: read the bytes at the recorded path.
        let content = self.fs.read_file(&file.path).await?;

        Ok(FileContent { uuid, content })
    }
}
