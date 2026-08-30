//! Browsing a library, one level at a time.

use std::collections::BTreeSet;

use uuid::Uuid;

use crate::auth::AuthService;
use crate::catalog::model::FileView;
use crate::catalog::repos::CatalogRepository;
use crate::errors::DomainError;
use crate::libraries::model::{Library, LibraryFolder, LibraryListing};
use crate::libraries::repos::LibraryRepository;

/// Splits a library's files into the folders and files at one level.
///
/// Pure, and separate from the handler for that reason: the path arithmetic
/// is the part with the edge cases — a file at the root, a folder whose name
/// repeats deeper down, a path that does not belong under the root at all —
/// and none of them need a database to exercise.
///
/// `relative` is the folder being listed, relative to the root, with no
/// leading or trailing separator. Empty is the top.
pub fn level_of(
    files: Vec<(String, FileView)>,
    relative: &str,
) -> (Vec<LibraryFolder>, Vec<FileView>) {
    let prefix = if relative.is_empty() {
        String::new()
    } else {
        format!("{relative}/")
    };

    // A set, because every file in a subfolder names that subfolder and a
    // level with two hundred files in one child would otherwise report it
    // two hundred times. Ordered, so the answer does not depend on which
    // file happened to be read first.
    let mut folders: BTreeSet<String> = BTreeSet::new();
    let mut here = Vec::new();

    for (path, view) in files {
        let Some(rest) = path.strip_prefix(&prefix) else {
            continue;
        };
        // `strip_prefix` on an empty prefix matches everything, so a file
        // outside this level is ruled out by what remains rather than by the
        // prefix alone.
        if !prefix.is_empty() && rest.is_empty() {
            continue;
        }

        match rest.split_once('/') {
            // Deeper than this level: it names the child folder it is under,
            // and nothing else about it belongs here.
            Some((folder, _)) if !folder.is_empty() => {
                folders.insert(folder.to_string());
            }
            Some(_) => {}
            None => here.push(view),
        }
    }

    let folders = folders
        .into_iter()
        .map(|name| LibraryFolder {
            path: if relative.is_empty() {
                name.clone()
            } else {
                format!("{relative}/{name}")
            },
            name,
        })
        .collect();

    (folders, here)
}

/// Every registered library.
///
/// The read that makes the rest reachable: browsing addresses a uuid, and
/// this is where those uuids come from.
pub struct ListLibrariesHandler<A, L> {
    auth: A,
    libraries: L,
}

impl<A, L> ListLibrariesHandler<A, L>
where
    A: AuthService,
    L: LibraryRepository,
{
    pub fn new(auth: A, libraries: L) -> Self {
        Self { auth, libraries }
    }

    pub async fn list(&self, token: &str) -> Result<Vec<Library>, DomainError> {
        self.auth.authenticate(token).await?;

        self.libraries.list_all().await
    }
}

/// Read one level of a library's tree (libraries design section 4).
pub struct BrowseLibraryHandler<A, L, C> {
    auth: A,
    libraries: L,
    catalog: C,
}

impl<A, L, C> BrowseLibraryHandler<A, L, C>
where
    A: AuthService,
    L: LibraryRepository,
    C: CatalogRepository,
{
    pub fn new(auth: A, libraries: L, catalog: C) -> Self {
        Self {
            auth,
            libraries,
            catalog,
        }
    }

    /// The folders and files directly inside `path` within the library
    /// `uuid` identifies.
    ///
    /// `path` is relative to the library's root; empty is the top. One level
    /// rather than the whole tree: a course with two hundred classes is a
    /// large document to build, send and parse so the owner can look at the
    /// six things in one folder.
    pub async fn browse(
        &self,
        uuid: Uuid,
        path: &str,
        token: &str,
    ) -> Result<LibraryListing, DomainError> {
        self.auth.authenticate(token).await?;

        let library = self
            .libraries
            .find_by_uuid(uuid)
            .await?
            .ok_or(DomainError::NotFound)?;

        let relative = normalize(path);
        let files = self.catalog.list_in_library(uuid).await?;

        // Made relative here, once, so `level_of` works in one coordinate
        // system and never has to know what the root was.
        //
        // Both sides go through `normalize`, so a Windows catalog — whose
        // paths are `D:\course\class-01\lecture.mp4` — reaches `level_of`
        // in the separator it splits on. Stripping a backslash path against
        // a backslash root did match, but every level below the top then
        // arrived as one long name with no separator `level_of` could see,
        // and the library reported no folders at all.
        let root = normalize(&library.root_path);
        let relative_files = files
            .into_iter()
            .filter_map(|view| {
                let path = normalize(&view.file.path);
                path.strip_prefix(&root)
                    .map(|rest| (rest.trim_start_matches('/').to_string(), view))
            })
            .collect();

        let (folders, files) = level_of(relative_files, &relative);

        Ok(LibraryListing {
            library: Library { ..library },
            path: relative,
            folders,
            files,
        })
    }
}

/// A caller's folder path, with the separators this code compares on.
///
/// Backslashes become forward slashes so a Windows client and a Linux one
/// address the same folder the same way, and the leading and trailing ones
/// go because `a/b`, `/a/b` and `a/b/` are the same place.
fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_string()
}
