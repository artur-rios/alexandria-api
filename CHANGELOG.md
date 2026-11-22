# Changelog

All notable changes to Alexandria API are recorded in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0] - 2026-11-22

The core has no login any more. It only ever runs locally, for the person at the machine, and the desktop front-end
calls it in process through its C ABI, so authentication protected nothing and the HTTP server reached a catalog nobody
reached over a network.

### Added

- Pinned and favorite libraries: `alexandria_library_set_pinned(uuid, pinned)` and
  `alexandria_library_set_favorite(uuid, favorite)` set each flag on its own, and every library JSON (the listing,
  browse, register, rename) carries `pinned` and `favorite`. Both answer `0`, `LIBRARY_ERR_INVALID_INPUT` for a
  malformed uuid, `LIBRARY_ERR_NOT_INITIALIZED`, or `LIBRARY_ERR_NOT_FOUND`; setting the value a library already has
  changes nothing. Moving, renaming or re-indexing a library keeps its flags (`alexandria-core`, `alexandria-ffi`).
- Renaming a library: `alexandria_library_rename(uuid, {"name": "..."})` answers the updated library. The name is
  stored trimmed and a blank one is refused with `LIBRARY_ERR_INVALID_INPUT`. It is allowed at any time, including
  while the library's run is in progress or paused (`alexandria-core`, `alexandria-ffi`).
- Catalog statistics: `alexandria_catalog_stats_json()` answers `{"libraries": n, "filesByType": {...}}` with all
  seven file types, `0` for an empty one. Files count as the default file listing shows them: files marked missing on
  disk count, soft-deleted ones do not (`alexandria-core`, `alexandria-ffi`).

### Changed

- `alexandria_index_init` no longer checks the Windows account it runs as. It validates no settings any more; its
  remaining failures are a NULL or non-UTF-8 database path (`INDEX_ERR_INVALID_INPUT`), `INDEX_ERR_BUSY`, and a failed
  migration or a panic (both `INDEX_ERR_OTHER`) (`alexandria-ffi`).
- A `config.toml` that still holds `[auth]` or `[http]` tables keeps loading; they are ignored, and so are the
  `ALEXANDRIA_AUTH_*` and `ALEXANDRIA_HTTP_*` environment variables (`alexandria-core`).
- Breaking for clients: the FFI changes shape, so a client built against 0.6 cannot use this core.

### Upgrade

Starting this version deletes the stored local-login credential, every session and every recovery code (migration 30),
and adds the pinned and favorite flags to every library, both off. No catalog data is lost: libraries, files, runs,
progress, collections and bookmarks are untouched, and so are files on disk.

### Removed

- The `token` parameter of every FFI call. Every function that took one has lost it, and its other parameters keep
  their order: `alexandria_files_list(json_filters, token)` is `alexandria_files_list(json_filters)`
  (`alexandria-ffi`).
- Authentication, in all three modes: the local account with its recovery codes, external Heimdall JWTs, and the
  Windows account. The `alexandria_auth_*` FFI functions go with it (`local_login`, `windows_login`,
  `local_set_credentials`, `local_register`, `local_account`, `local_redeem_recovery_code`,
  `local_regenerate_recovery_codes`), and so do `AUTH_ERR_*` and their result struct.
- `*_ERR_UNAUTHORIZED` in every result family. Code 2 is retired and never reused; every other code keeps its value.
- The HTTP REST/JSON server, `alexandria-http`, with every `/v1/*` route and `/health`. The core is used only through
  its C ABI (`alexandria-ffi`).
- The `[auth]` and `[http]` configuration sections and their `ALEXANDRIA_AUTH_*` and `ALEXANDRIA_HTTP_*` overrides.

## [0.6.0] - 2026-11-20

The first tagged release. The workspace version in `Cargo.toml` tracks client compatibility with the desktop
front-end, which refuses a core of another minor version at startup. Everything up to this release:

### Added

- One Rust core library, `alexandria-core`, exposed over an HTTP REST/JSON server (`alexandria-http`) and a C ABI for
  the Flutter desktop front-end (`alexandria-ffi`), so both surfaces return identical results.
- Libraries: the owner adds a folder as a library, and Alexandria indexes every file in it that it can read — movies
  and series, music and other audio, HTML pages, Markdown and text files, PDFs and e-books, comic books, and images —
  keeps its folder structure, and answers it one level at a time. Each run is observable, pausable, resumable, and
  cancellable. A library can be re-indexed on its own (`POST /v1/libraries/{uuid}/index`,
  `alexandria_library_index_start`), corrected when its folder moves, and removed.
- Audio as playable files: tags (title, artist, album, album artist, year, genre, track) and duration read at index
  time, the embedded front cover as the thumbnail, byte-range streaming, and metadata editing like every other type.
  No playlists, play history, or lookups: music management is Orpheus's.
- `GET /v1/files` and `alexandria_files_list` filter by `libraryUuid`, and every listed file carries its
  `libraryUuid`; run status and the active-run listing carry the run's `libraryUuid` (none for a refresh).
- Catalog browsing, metadata editing, renaming on disk, text file editing, and two-phase soft-delete, restore, and purge.
- Collections, browser bookmarks, watchlists, and reading lists.
- Pluggable authentication: an external Heimdall JWT, a local encrypted login with recovery codes, or the Windows
  account the server process runs as.
- Media playback: byte-range streaming, comic book pages, and thumbnails.
- A health check, `config.toml` configuration with `ALEXANDRIA_*` environment overrides, and SQLite migrations applied
  on startup.

### Changed

Libraries are the only way into Alexandria (0.6.0). A file enters the catalog only by being inside a library, and
every file belongs to exactly one.

- Adding a library (`POST /v1/libraries`, `alexandria_library_register`) starts its first index run, with an optional
  `priority`, and answers the library with the run's `runId`. It is refused, storing nothing, for a folder that does not
  exist, one outside `filesystem.root`, or one overlapping another library.
- A library with a running or paused run refuses a second run and a move (`409` / `INDEX_ERR_CONFLICT`,
  `LIBRARY_ERR_CONFLICT`), naming the run; removal is refused only while the run is running.
- Removing a library (`DELETE /v1/libraries/{uuid}`, `alexandria_library_remove`) deletes its catalog records — its
  files with their progress and collection membership, its runs, and the failures recorded under its folder — instead
  of returning its files to the type listings. Files on disk are never touched.
- `GET /v1/files` answers files in every library; nothing is hidden from it any more.
- Breaking for clients: the core is version 0.6.0, and a client built against 0.5 refuses it at startup.

### Upgrade

Starting 0.6.0 deletes every catalog record outside a library, with its progress and collection membership (migration
28); index runs over roots no library holds are deleted with their failures, and refresh runs are kept. A library left
with more than one unfinished run keeps the newest and the rest are cancelled (migration 29), since a library may have
only one. Files on disk are untouched. Back up the database file (`database.path`, `alexandria.sqlite` by default) first to keep them, then add
your folders as libraries to index them again.

### Removed

- Free-form indexing: `POST /v1/index` and `alexandria_index_start` (0.6.0). Add the folder as a library instead.
- The per-run type scope (`types` / `IndexScope`) and the `includeLibraries` listing flag (0.6.0). A library is
  indexed whole, and the listing answers every library.

Music and audio moved out of Alexandria in 0.5.0, which made it a library for things you read and watch; music lives in
Orpheus, a separate project. 0.6.0 brings audio back as playable files only (see Added); everything below except the
file type itself stays removed.

- The `audio` file type, in 0.5.0: audio files were no longer catalogued and an indexed folder skipped them. 0.6.0
  restores the type, its tag reading (lofty) and its metadata as playable files (see Added); only what follows stays
  removed.
- Playlists and their entries.
- Play history and the play statistics.
- Music enrichment: lyrics and artist photography lookups, and the `[metadata]` configuration section with its
  `ALEXANDRIA_METADATA_*` environment variables. A `config.toml` that still carries `[metadata]` starts cleanly.
- The track energy envelope.
- HTTP routes: `/v1/enrichment/*`, `/v1/plays`, `/v1/plays/stats`, `/v1/playlists*`, and `/v1/files/{uuid}/energy`.
  They answer `404`.
- FFI functions: `alexandria_enrichment_run`, `alexandria_enrichment_read_track`, `alexandria_artist_image`,
  `alexandria_artist_image_fetch`, `alexandria_play_record`, `alexandria_music_stats`, every `alexandria_playlist*`
  function, `alexandria_playlists_list`, and `alexandria_track_energy`.
- Data, on upgrade: migration 25 deletes the stored music from the database. It drops the playlist, lyrics, energy,
  play history, and artist image tables, removes every audio file and its tag record, and rebuilds `files` without the
  `audio` type. Index runs scoped to audio alone are deleted, and `audio` is stripped from the scope of mixed runs.
  Migration 26 then deletes the failures that surviving runs recorded on audio files, keeping each run's failure
  count as it was. Videos, documents, and every other file keep their records, ids, and progress. Files on disk and
  cached artist images are never touched.
- Breaking for clients: the core is version 0.5.0, and a client built against 0.4 refuses it at startup.

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
- `alexandria_index_init` refuses a database path that is not valid UTF-8 instead of decoding it lossily
  (`alexandria-ffi`).
- `alexandria_index_init` re-checks for a live run at the moment it swaps services, closing a window in which a run
  started on another thread could be orphaned.
- `alexandria_index_files_json` returns NULL when the catalog cannot be read, as documented, instead of `[]`.
- A run's per-file failures are stamped with the time each failed rather than the run's start time.
- Files under a folder whose name is not valid UTF-8 are skipped with a warning instead of being cataloged under a
  path that does not exist.

### Security

- External mode verifies Heimdall's ES256 tokens against the key set Heimdall publishes, set with
  `auth.heimdall_jwks_url` (`ALEXANDRIA_AUTH_HEIMDALL_JWKS_URL`), instead of a shared HS256 secret. The set is cached
  and fetched again when a token names an unknown `kid` or the set is five minutes old, at most once a minute; a
  failed fetch keeps the known keys. The algorithm is pinned, so a token's `alg` header cannot select the key type.
  `auth.heimdall_token_secret` is now optional: HS256 tokens are accepted while it is set, for the overlap while
  Heimdall still accepts them, and refused once it is removed. Startup requires the key set URL or the secret
  (`alexandria-core`).
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

[Unreleased]: https://github.com/artur-rios/alexandria-api/compare/v0.7.0...develop
[0.7.0]: https://github.com/artur-rios/alexandria-api/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/artur-rios/alexandria-api/releases/tag/v0.6.0
