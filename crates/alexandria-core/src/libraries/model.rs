use serde::Serialize;
use uuid::Uuid;

use crate::catalog::model::FileView;

/// A library about to be persisted. The `uuid` is minted by the handler so
/// the value is decided by the same code on both transports.
#[derive(Debug, Clone)]
pub struct NewLibrary {
    pub uuid: Uuid,
    pub name: String,
    pub root_path: String,
}

/// A registered folder browsed as a tree, whose files are shown only there
/// (libraries design).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Library {
    pub uuid: Uuid,
    /// The owner's name for it, not derived from the folder: a directory
    /// called `2024-final-v2` is a path, not a title.
    pub name: String,
    /// What every entry's position is relative to.
    pub root_path: String,
}

/// A folder directly inside the level being browsed.
///
/// Carries no contents. A tree is drawn one level at a time (design section
/// 4), and a folder is a thing to open rather than a thing to unpack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryFolder {
    /// The folder's own name, as it is on disk.
    pub name: String,
    /// Its path relative to the library root, which is what asking for the
    /// next level down takes.
    pub path: String,
}

/// One level of a library's tree.
///
/// Folders and files kept apart rather than interleaved in one list: a tree
/// view draws them differently and almost always groups them, and a caller
/// that wanted them mixed can concatenate far more easily than one that
/// wanted them separate can partition.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryListing {
    pub library: Library,
    /// The folder being listed, relative to the root. Empty at the top.
    pub path: String,
    pub folders: Vec<LibraryFolder>,
    /// The files directly in this folder, as the same `FileView` every other
    /// listing answers — so a client parses a library with what it already
    /// has for the catalog.
    pub files: Vec<FileView>,
}
