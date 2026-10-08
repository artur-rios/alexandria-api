# Changelog

All notable changes to Alexandria API are recorded in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

No release has been tagged yet. The workspace version in `Cargo.toml` (currently 0.4.0) tracks client
compatibility with the desktop front-end, not a published release. What exists so far:

### Added

- One Rust core library, `alexandria-core`, exposed over an HTTP REST/JSON server (`alexandria-http`) and a C ABI for
  the Flutter desktop front-end (`alexandria-ffi`), so both surfaces return identical results.
- Indexing and re-indexing of on-disk audio, movies and series, HTML pages, Markdown and text files, PDFs and e-books,
  comic books, and images, with each run observable, pausable, resumable, and cancellable; folders can be grouped as
  libraries and browsed as their own tree.
- Catalog browsing, metadata editing, renaming on disk, text file editing, and two-phase soft-delete, restore, and purge.
- Collections, browser bookmarks, watchlists, reading lists, and playlists.
- Pluggable authentication: an external Heimdall JWT, a local encrypted login with recovery codes, or the Windows
  account the server process runs as.
- Media playback: byte-range streaming, comic book pages, thumbnails, and a track's energy envelope.
- Lyrics and artist photography looked up from public services, and a play history with rankings of what was played
  most.
- A health check, `config.toml` configuration with `ALEXANDRIA_*` environment overrides, and SQLite migrations applied
  on startup.

### Fixed

- Moving a library whose folder name contains a non-ASCII letter (`/media/Música`) no longer corrupts the stored path
  of every file in it (`alexandria-core`).
- A run paused or cancelled before its walk began is no longer walked anyway and closed as `complete` (or, for a
  pause, raced by its own resumed walk).
- A database path containing `?` or a `%xx` sequence is opened as written instead of being read as a URL.
- `http.bind_addr` accepts an IPv6 address (`::1`); an address that is not an IP is a configuration error instead of a
  startup panic (`alexandria-http`).
- The `Authorization` scheme is matched case-insensitively (`BEARER <token>` is accepted).
- `GET /v1/files` answers an unreadable query with the usual JSON error envelope rather than plain text.
- `PUT /v1/files/{uuid}/content` saves text files larger than 2 MiB, as the FFI surface already did.
- `alexandria_index_start` authenticates before checking `root`; it and `alexandria_index_init` refuse a path that is
  not valid UTF-8 instead of decoding it lossily (`alexandria-ffi`).
- `alexandria_index_init` re-checks for a live run at the moment it swaps services, closing a window in which a run
  started on another thread could be orphaned.
- `alexandria_index_files_json` returns NULL when the catalog cannot be read, as documented, instead of `[]`.
- A run's per-file failures are stamped with the time each failed rather than the run's start time.
- Files under a folder whose name is not valid UTF-8 are skipped with a warning instead of being cataloged under a
  path that does not exist.

### Security

- Local login always verifies the password, so a wrong email no longer answers measurably faster than a wrong
  password and the account's email cannot be learned by timing.
- Correcting a library's root (`PATCH /v1/libraries/{uuid}`, `alexandria_library_move`) is now bounded by
  `filesystem.root` like an index request: a new root outside it is refused with the index's own invalid-input error
  (`400` / `LIBRARY_ERR_INVALID_INPUT`). Before, a library could be moved anywhere, and streaming then served
  out-of-root files whose paths matched the library's. A folder that does not exist yet is still accepted when it
  would sit inside the root (`alexandria-core`).
- With `filesystem.root` set, a cataloged file whose path resolves outside it (catalogued before the root was set, or
  reached through a symbolic link) is no longer streamed, paged, thumbnailed, measured, read, edited, renamed or
  deleted from disk: those calls answer `400` / `*_ERR_INVALID_INPUT` with `file is outside the configured library
  root`. Purging only the record still works. With `filesystem.root` unset nothing changes (`alexandria-core`).

[Unreleased]: https://github.com/artur-rios/alexandria-api/commits/develop
