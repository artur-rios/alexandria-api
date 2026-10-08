//! The configured library root (`filesystem.root`) as a bound on what the
//! catalog may point at (FR-FC-26).
//!
//! Three places need the same answer to "is this path inside the library
//! root?", and they must not each grow their own idea of it:
//!
//! - **indexing** (UC-01) — a requested root must sit inside;
//! - **moving a library** (UC-51) — the new root must sit inside, because a
//!   move rewrites every stored path under it without walking the disk, and
//!   streaming then serves whatever the catalog holds;
//! - **serving or changing a cataloged file's bytes** (UC-05, UC-09, UC-32,
//!   UC-33, UC-38 – UC-40, the energy envelope) — the file's resolved path
//!   must sit inside, so a row that already points outside (indexed before
//!   the bound was configured, moved before the move was bounded, or reached
//!   through a symbolic link swapped in since) is refused rather than served.
//!
//! One function decides all three — [`LibraryRoot::contains`] — so the
//! canonicalisation and symbolic-link rules cannot drift apart.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use crate::errors::DomainError;

/// The client-facing rejection message for FR-FC-26 when a requested root —
/// to index, or to move a library to — is genuinely outside the configured
/// library root. Deliberately free of the configured root's absolute path:
/// the caller does not need to be told where the library lives in order to
/// learn that its request was out of bounds.
pub const OUTSIDE_LIBRARY_ROOT: &str = "root path is outside the configured library root";

/// The client-facing rejection message when a cataloged file's resolved path
/// is outside the configured library root, so its bytes are neither served
/// nor changed. Names no path, for the reason [`OUTSIDE_LIBRARY_ROOT`] does
/// not.
pub const FILE_OUTSIDE_LIBRARY_ROOT: &str = "file is outside the configured library root";

/// The client-facing rejection message for FR-FC-26 when the *server's*
/// `filesystem.root` configuration itself cannot be resolved. Deliberately
/// distinct from [`OUTSIDE_LIBRARY_ROOT`]: that message implies the caller's
/// request was wrong, which is misleading here — the caller's root may be
/// perfectly fine, and it is the server's configuration that needs fixing.
/// Still free of the configured root's absolute path — naming the failure
/// mode is not the same as naming the path.
pub const LIBRARY_ROOT_UNRESOLVABLE: &str =
    "the server's configured library root could not be resolved; contact the operator";

/// The configured `filesystem.root`, or its absence.
///
/// Built once from configuration and handed to every handler that needs the
/// bound. An empty (or whitespace-only) value means the key is unset, and
/// every check passes — the constraint is opt-in by configuration, so no
/// deployment changes behaviour on upgrade.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryRoot {
    configured: Option<String>,
}

impl LibraryRoot {
    /// From the raw `filesystem.root` value. Trimmed; empty means unset.
    pub fn new(raw: &str) -> Self {
        let trimmed = raw.trim();
        Self {
            configured: (!trimmed.is_empty()).then(|| trimmed.to_string()),
        }
    }

    /// No bound: every check passes.
    pub fn unconfigured() -> Self {
        Self::default()
    }

    /// Whether a bound is configured at all.
    pub fn is_configured(&self) -> bool {
        self.configured.is_some()
    }

    /// FR-FC-26 for a root a caller asks for — one to index, or one to move a
    /// library to. Refused with [`OUTSIDE_LIBRARY_ROOT`].
    pub fn check_root(&self, requested: &str) -> Result<(), DomainError> {
        self.check(requested, OUTSIDE_LIBRARY_ROOT)
    }

    /// The same bound for a cataloged file about to be served or changed.
    /// Refused with [`FILE_OUTSIDE_LIBRARY_ROOT`].
    pub fn check_file(&self, path: &str) -> Result<(), DomainError> {
        self.check(path, FILE_OUTSIDE_LIBRARY_ROOT)
    }

    fn check(&self, path: &str, outside: &str) -> Result<(), DomainError> {
        match self.contains(path)? {
            true => Ok(()),
            false => Err(DomainError::InvalidInput(outside.into())),
        }
    }

    /// Whether `path` is the configured library root or a descendant of it.
    /// Always `true` when no root is configured.
    ///
    /// Both sides are resolved before comparison. The library root is
    /// canonicalized outright. `path` goes through [`resolve`], which is
    /// `canonicalize` for a path that exists and, for one that does not yet,
    /// canonicalizes the longest existing ancestor and appends the rest after
    /// lexical normalisation. That is what holds the check against
    /// `<root>/../../etc` (the traversal is resolved away), against `<root>`
    /// vs `<root>/` vs `<root>/.` (all resolve to the same path), and against
    /// a symlink inside the root that points out of it (it resolves to its
    /// target) — while still accepting a destination that does not exist at
    /// the moment, such as a folder on an unplugged drive whose mount point
    /// sits inside the root. The comparison itself is `Path::starts_with`,
    /// which matches whole path components — a string prefix test would let
    /// `/library-evil` slip past a `/library` bound.
    ///
    /// A configured root that cannot be resolved is a misconfiguration, not a
    /// caller error, and is an `Err` with [`LIBRARY_ROOT_UNRESOLVABLE`]
    /// rather than a silent degradation to "unconstrained": a security bound
    /// that disappears when its configuration is wrong is worse than no bound
    /// at all, because the operator believes it is there.
    ///
    /// A `path` that cannot be resolved — a dangling symbolic link, a
    /// component the process may not look into, a link loop — is `false`:
    /// whatever it would turn out to name cannot be shown to be inside.
    ///
    /// The filesystem calls here are blocking, and short: a handful of
    /// `stat`/`readlink` calls per request.
    pub fn contains(&self, path: &str) -> Result<bool, DomainError> {
        let Some(library_root) = self.configured.as_deref() else {
            return Ok(true);
        };
        let canonical_library_root = match std::fs::canonicalize(library_root) {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::error!(
                    root = %library_root,
                    error = %err,
                    "configured filesystem.root cannot be resolved; refusing until it is fixed"
                );
                return Err(DomainError::InvalidInput(LIBRARY_ROOT_UNRESOLVABLE.into()));
            }
        };
        Ok(match resolve(Path::new(path)) {
            Some(resolved) => resolved.starts_with(&canonical_library_root),
            None => false,
        })
    }
}

/// Where `path` resolves to, whether or not it exists yet.
///
/// A path that exists is `canonicalize`d. Otherwise the path is walked from
/// its first component, canonicalizing each prefix, until a prefix fails:
///
/// - if that prefix is genuinely absent (`NotFound`, and not even a dangling
///   symbolic link), the canonical form of the prefix before it is the
///   longest existing ancestor, and the remaining components are appended
///   lexically — `..` pops, `.` is dropped. Nothing below an absent component
///   can be a symbolic link, so lexical treatment is exact there;
/// - any other failure (a dangling link, permission denied, a loop, a file
///   used as a folder) is `None`: what the path would name cannot be known.
///
/// A relative path is taken relative to the process's working directory,
/// which is what every filesystem call on it would do.
pub fn resolve(path: &Path) -> Option<PathBuf> {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Some(resolved);
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let components: Vec<Component<'_>> = absolute.components().collect();

    let mut prefix = PathBuf::new();
    let mut resolved: Option<PathBuf> = None;
    let mut rest = components.len();
    for (index, component) in components.iter().enumerate() {
        prefix.push(component.as_os_str());
        match std::fs::canonicalize(&prefix) {
            Ok(canonical) => resolved = Some(canonical),
            Err(err) if err.kind() == ErrorKind::NotFound && is_absent(&prefix) => {
                rest = index;
                break;
            }
            Err(_) => return None,
        }
    }

    let mut out = resolved?;
    for component in &components[rest..] {
        match component {
            Component::Normal(name) => out.push(name),
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            // Only the leading components can be these, and they always exist.
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(out)
}

/// `true` only when nothing at all is at `path` — not even a dangling
/// symbolic link, which `canonicalize` also reports as `NotFound`.
fn is_absent(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(err) if err.kind() == ErrorKind::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(path: &Path) -> &str {
        path.to_str().expect("utf-8 temp path")
    }

    #[test]
    fn given_an_existing_path_when_resolved_then_it_is_canonical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inner = dir.path().join("a");
        std::fs::create_dir(&inner).expect("mkdir");

        let resolved = resolve(&dir.path().join("a").join("..").join("a").join("."));

        assert_eq!(resolved, Some(std::fs::canonicalize(&inner).unwrap()));
    }

    #[test]
    fn given_a_path_that_does_not_exist_when_resolved_then_the_rest_is_appended() {
        let dir = tempfile::tempdir().expect("tempdir");

        let resolved = resolve(&dir.path().join("gone").join("deeper").join("..").join("x"));

        assert_eq!(
            resolved,
            Some(
                std::fs::canonicalize(dir.path())
                    .unwrap()
                    .join("gone")
                    .join("x")
            )
        );
    }

    #[test]
    fn given_a_missing_folder_that_climbs_out_when_resolved_then_the_climb_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        std::fs::create_dir(&root).expect("mkdir");

        let resolved = resolve(&root.join("gone").join("..").join("..").join("etc"));

        assert_eq!(
            resolved,
            Some(std::fs::canonicalize(dir.path()).unwrap().join("etc"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn given_a_dangling_symlink_when_resolved_then_it_cannot_be_resolved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink("/nonexistent-target-for-alexandria", &link).expect("link");

        assert_eq!(resolve(&link.join("music")), None);
    }

    #[test]
    fn given_no_configured_root_when_checked_then_everything_is_inside() {
        let root = LibraryRoot::new("   ");

        assert!(!root.is_configured());
        assert!(root.check_root("/etc").is_ok());
        assert!(root.check_file("/etc/passwd").is_ok());
    }

    #[test]
    fn given_a_sibling_sharing_a_prefix_when_checked_then_it_is_outside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let library = dir.path().join("lib");
        std::fs::create_dir(&library).expect("mkdir");
        let root = LibraryRoot::new(s(&library));

        assert!(!root.contains(s(&dir.path().join("lib-evil"))).unwrap());
        assert!(root.contains(s(&library.join("x"))).unwrap());
    }

    #[test]
    fn given_an_unresolvable_configured_root_when_checked_then_it_fails_closed() {
        let root = LibraryRoot::new("/nonexistent-library-root-for-alexandria");

        let result = root.check_file("/nonexistent-library-root-for-alexandria/a.mp3");

        assert!(
            matches!(result, Err(DomainError::InvalidInput(ref m)) if m == LIBRARY_ROOT_UNRESOLVABLE)
        );
    }
}
