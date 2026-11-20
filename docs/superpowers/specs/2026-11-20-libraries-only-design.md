# Libraries are the only way in

**Date:** 2026-11-20
**Status:** proposed

## Intent

Alexandria becomes a **library manager**. The owner hands it a folder; that
folder is a library. Alexandria indexes every file inside it that it can read,
keeps the folder structure exactly as it is on disk, and the owner browses that
tree and opens what is in it — reads text, Markdown, HTML, PDFs, e-books,
comics and images, and plays video and audio.

The owner's words: "The current setup does not make sense anymore. I want the
app to be a library manager, where the user will submit a folder structure and
can use the app to browse through and reproduce/read the files."

Today libraries are an optional extra layered on a type-sorted catalog
(`2026-08-30-libraries-design.md`): any folder can be indexed, its files land
in type panels, and only files under a registered library are pulled out into
a tree. This change inverts that. There is no catalog outside libraries: a
folder that is not a library is invisible to Alexandria, and the only way a
file enters the catalog is by being inside one.

## Goals

- A library is the only unit of indexing. Registering a library starts its
  first index run; re-indexing is per library. There is no free-form
  "index this root" any more.
- Every file in the catalog belongs to exactly one library (`files.library_id`
  is never NULL after this change).
- The folder structure is preserved and browsed one level at a time, as today
  (FR-FC-39, unchanged).
- Only file types Alexandria can read are indexed; everything else is skipped,
  as today.
- **Audio returns** as a catalog file type: indexed, tagged (title, artist,
  album, album artist, genre, track, year as before), its duration read,
  its embedded cover art served as its thumbnail, streamed with byte ranges,
  and editable through metadata editing like every other type.
- The supporting features stay and work on library files unchanged:
  collections, watchlists, reading lists, search through `GET /v1/files`,
  metadata editing, rename, text editing, and the soft-delete → restore →
  purge lifecycle. Bookmarks stay as a separate feature (they are URLs, not
  files, and libraries do not touch them).
- On upgrade, the catalog **starts clean**: every file record that belongs to
  no library is deleted, with everything hanging off it. The owner re-adds
  their folders as libraries and re-indexes.
- The requirements and behaviour documents in this repository describe
  Alexandria as a library manager.

## Non-goals

- **No music manager.** Playlists, play history and statistics, lyrics and
  artist lookups (MusicBrainz, LRCLIB, Wikimedia), the energy/visualiser data,
  and the `[metadata]` config section stay removed. Orpheus remains the home
  for managing music; Alexandria plays an audio file because it is in a folder
  the owner added, nothing more. No network lookup is made for any type.
- No per-library type filter. A library is the folder as it is: a cover image
  beside an album shows in the tree where it sits.
- No nested or overlapping libraries (already refused; unchanged).
- No filesystem watching. Changes on disk are picked up by re-indexing or
  refreshing, as today.
- No changes to alexandria-ui or the alexandria-docs site in this change. They
  follow as their own changes, in that order (below).
- No migration of existing file records into libraries. The owner chose to
  start clean.

## Anti-goals (a failure even if it works)

- **Touching a file on disk.** Removing a library, the upgrade cleanup, and
  every other deletion here remove database rows only.
- **A file with no library.** Any path by which a `files` row could be written
  with `library_id` NULL — an index walk, a refresh, a rename, a library move —
  is a bug. The invariant is enforced, not hoped for.
- **Dead half-features.** No `includeLibraries` flag that means nothing, no
  `types` scope argument that is ignored, no FFI function or route that can no
  longer succeed, no requirement or use case describing the type panels as if
  they existed.
- **Breaking an existing database.** The upgrade must run cleanly on a database
  holding files both inside and outside libraries, with collections,
  watchlists, reading lists, progress, run history and run failures pointing at
  both.
- **Reviving the music manager by accident.** Restoring audio from history must
  not bring back playlists, plays, enrichment or energy code, tables or routes.

## Constraints

- Three repositories, changed in order **api → ui → docs**, because the UI
  binds to the API's FFI header. This change is the API, including its own
  `docs/`.
- The HTTP and FFI surfaces change incompatibly: the workspace version goes
  from 0.5.0 to **0.6.0**. HTTP↔FFI parity (FR-FC-24) holds for every new or
  changed call.
- Migrations are forward-only and existing ones are never edited (sqlx
  checksums them). The audio type needs `files`' `type` CHECK widened again,
  which in SQLite means another table rebuild, the same technique migration 25
  used in reverse.
- `filesystem.root` (FR-FC-26) keeps bounding everything it bounds today. A
  library's root is now the only root that is ever walked, so **registration**
  checks it too — today only a move and a free-form index start do.
- Use case numbers stay stable. A use case that no longer applies is marked
  removed in place, the way UC-49 and UC-50 were; one that changes meaning is
  rewritten under its number.
- Historical specs, plans and reports under `docs/superpowers/` are left as
  they are; this document explains what changed.

## Approaches considered

**How the change is shaped**

- **A. Lean library manager** — keep browse, open/play, search and resume
  positions; remove type panels, collections, watchlists, reading lists and
  bookmarks. Smallest app.
- **B. Libraries plus the extras, scoped to libraries** — remove only the
  catalog outside libraries; every other feature keeps working on library
  files. Least code removed; the app stays broad.
- **C. A hand-picked middle.**

**Chosen: B** (the owner's pick).

**What happens to files indexed outside any library on upgrade**

- **Convert automatically** — every index run recorded its root and runs are
  never deleted, so the upgrade could turn each distinct root into a library
  (nested roots collapsed into the outermost), keeping collection memberships
  and progress. A trickier migration, and generated library names.
- **Start clean** — delete every file record outside a library and everything
  attached to it; the owner re-adds folders. Simple and predictable; progress
  and memberships on those files are lost.

**Chosen: start clean** (the owner's pick).

**How audio comes back** (Claude's call)

- Restore the code removed in `8867aa8` (`audio_tags.rs`, the classifier
  arm, MIME entries, the thumbnail arm, the model and repository paths, the
  extraction arm, the metadata edit arm) and its tests, then cut anything that
  existed only for enrichment or other music-manager features. Chosen: it is
  code that was working and tested a month ago.
- Rewrite audio support from scratch. Rejected: more work for the same result.

## Design

### 1. Registering a library starts indexing it

`POST /v1/libraries` (`alexandria_library_register`) registers the folder and
starts its first index run in the same call. It answers `201` with the
library and the run id, so a client can follow the run with the existing run
calls (UC-42, UC-48).

It takes the same optional `priority` an index start takes (`"low"` /
`"normal"`, lenient as today), so the first run of a large folder can be
started gently and the FFI signature does not have to change again later.

Before anything is stored it checks, in this order: name not blank, root not
blank, root inside `filesystem.root` when one is configured (the same
`LibraryRoot::check_root` an index start uses today), root exists and is a
listable folder (the check `IndexHandler::start` makes today, `InvalidInput`),
no overlap with an existing library (`Conflict`, naming it). A refusal opens
no library and no run. Registering is therefore stricter than a move, which
still accepts a folder on an unplugged drive: a move corrects a record, while
registering walks the folder now.

If the run cannot be opened after the checks pass, the library row is removed
again before the error is returned, so a failed registration leaves nothing
behind.

`POST /v1/libraries/{uuid}/index` (`alexandria_library_index_start`) re-walks
one library, with the existing optional `priority`. It replaces
`POST /v1/index` / `alexandria_index_start`, which are removed.

### 1a. One run per library at a time

A library with a run that is not finished — running, paused, or interrupted
(resumable) — refuses with `409 Conflict`, naming the run id:

- a second index run of that library;
- a move of that library (a resume walks the root frozen on the run's row,
  so moving under a paused run would send it walking the old folder);
- removing it while the run is **running** (section 3).

The owner cancels the run first (UC-48), then retries. Refusing rather than
waiting, because cancel only raises a signal on the in-process run cell and
there is no way to wait on a walk draining, least of all one in another
process (the HTTP server and the FFI host are separate).

The `types` scope argument goes away with them, along with `IndexScope`.
`catalog_runs.scope` stays in the schema (dropping a column costs a rebuild
and buys nothing) and is written NULL; a resume reads NULL as every type, as
it already does.

The existing "claim what is already indexed beneath the folder" step in
registration becomes dead — nothing outside a library exists to claim — and
is removed.

### 2. A run belongs to its library

`catalog_runs` gains a nullable `library_id`. An index run records the library
it walks; its root is that library's root at the moment the run starts. A
refresh run has no root and no library, so its `library_id` is NULL, as is the
`libraryUuid` it reports.

Every file a run writes is written with that library's id. Today `library_id`
is resolved at insert time by matching the path against registered roots; it
becomes the run's library, passed down, and the insert refuses a NULL. A
refresh (UC-02) keeps a file's existing `library_id`. A rename stays inside
the file's folder and so inside its library. A move (FR-FC-41) rewrites paths
and keeps ids.

Refresh stays catalog-wide (`POST /v1/index/refresh`, takes no root) — it
re-checks what is recorded, and everything recorded is in a library. Run
status and the active-runs listing carry the run's library uuid so a client
can show which library a run is for.

### 3. Removing a library removes its catalog records

`DELETE /v1/libraries/{uuid}` (`alexandria_library_remove`) now:

1. refuses with `409` while one of the library's runs is **running**
   (section 1a); a paused or interrupted run is not writing, so it is not in
   the way and goes with the library in step 3;
2. deletes every file record in the library and everything attached to it —
   subtype rows, collection membership, watchlist entries and watch progress,
   reading-list entries and reading progress, run failures — exactly what a
   hard purge (UC-08) removes for one file;
3. deletes the library's index runs (and their failures) and the library row.

Steps 2 and 3 are one transaction. It touches nothing on disk. Refresh runs
are kept; any of their failures that pointed at the removed files go with
those files. A refresh running at the time only updates rows that already
exist, so it cannot recreate what was removed. FR-FC-40 changes from "keeps its files and
returns them to the type listings" to this.

### 4. The listing no longer excludes anything

`GET /v1/files` / `alexandria_files_list` keeps its `type`, `state` and
`collectionUuid` filters and loses `includeLibraries` and `LibraryScope`:
with every file in a library there is nothing to exclude. It gains an optional
`libraryUuid` filter, so a client can search or review the trash within one
library. A malformed `libraryUuid` is `400`; a well-formed one matching no
library is an empty list, the same rule `collectionUuid` follows.

The type filter stays because search and the trash use it; it no longer
backs "type panels" in any requirement.

### 5. Audio

Restored from `8867aa8` and its neighbours, as in "How audio comes back":

- `FileType::Audio` and `SubtypeMetadata::Audio`; classifier extensions
  mp3, flac, wav, ogg, oga, m4a, aac, opus, wma.
- `lofty` returns to the workspace for tag, duration and cover-art reading.
- `audio_files` table recreated with the columns it had when it was dropped
  (including duration and album artist), and `files.type` CHECK widened to
  allow `audio`, in a new migration.
- Streaming (`/stream`) serves audio with the correct MIME types; the
  thumbnail endpoint answers embedded cover art, and the same "no thumbnail"
  answer as today when there is none.
- Metadata editing (UC-04) covers audio's editable fields again.
- Not restored: anything that existed for playlists, plays, enrichment,
  energy or recording identification for a lookup service.

### 6. The upgrade cleanup

One new migration (after `…26`), in a transaction:

1. delete every `files` row with `library_id` NULL, and everything attached
   to those rows (the same set as step 2 of section 3);
2. add `catalog_runs.library_id` (nullable);
3. among **index** runs (`root` NOT NULL), delete every row (and its
   failures) whose root is not a registered library's root or inside one, and
   set `library_id` on the rest by the same path match registration uses.
   Refresh runs (`root` NULL) are kept with `library_id` NULL;
4. rebuild `files` with the widened type CHECK and a `library_id NOT NULL`
   constraint, and create `audio_files`.

An index run that was paused or interrupted outside any library is
therefore gone, not left resumable against nothing. An index run kept because
its root is *inside* a library (an old index of a subfolder) keeps that
narrower root; resuming it walks only that subfolder, which is still entirely
inside its library.

### 7. Surfaces at a glance

| Change | HTTP | FFI |
| --- | --- | --- |
| Register + first run | `POST /v1/libraries` (+ optional `priority`) → library + `runId` | `alexandria_library_register` (+ `priority`) |
| Re-index one library | `POST /v1/libraries/{uuid}/index` (new) | `alexandria_library_index_start` (new) |
| Free-form index | `POST /v1/index` removed | `alexandria_index_start` removed |
| Remove a library | `DELETE /v1/libraries/{uuid}` deletes records | `alexandria_library_remove` |
| Listing | `includeLibraries` removed, `libraryUuid` added | same |
| Run status, active runs | carry `libraryUuid` | same |
| Audio | type accepted everywhere a type is | same |

## Documentation

In this repository:

- `README.md`: the introduction describes a library manager for things you
  read, watch and listen to; the feature tables lose the type-panel framing;
  F-12 Libraries becomes the entry point the others hang off; audio is back in
  the type list, with music management still pointing at Orpheus.
- `docs/requirements/*`: Vision, System Requirements (FR-FC-01, -02, -12,
  -26, -36 … -41 and the route table in §5), Use Case Specification (UC-01
  becomes "Index a library", UC-51 "Add a library", UC-03 browsing through
  libraries, removal semantics in UC-51's flows, traceability table),
  Testing Specification, Technology Stack (lofty), Operations (what
  `filesystem.root` bounds).
- `docs/initial/*` and `docs/System Behavior Document.md`: the same story.
- `config.toml.example`: `filesystem.root` wording.
- `CHANGELOG.md` under `[Unreleased]`: Changed (libraries are the only way
  in; removing a library deletes its records), Added (audio, per-library
  re-index, `libraryUuid` filter), Removed (`POST /v1/index`,
  `alexandria_index_start`, `includeLibraries`, the `types` scope), and the
  upgrade note that files outside libraries are deleted.

Follow-ups, each its own change after this merges:

- **alexandria-ui**: regenerate bindings; adding a library is the first-run
  screen and the only way to index; the type panels go; the Libraries tree is
  the main browse; audio playback returns as plain file playback.
- **alexandria-docs**: overview, usage and architecture pages describe the
  library manager.

## Testing

Failing tests first, Given-When-Then names, through `ci-check
lint-and-test` (ffmpeg is not on the dev machine).

- Registering a library starts a run that indexes its readable files and
  skips the rest; the response carries the run id.
- Registering a folder outside `filesystem.root`, a folder that does not
  exist, or an overlapping folder (naming the other library) is refused and
  opens no library and no run.
- With a run of a library running, paused or interrupted, a second run and a
  move are refused naming the run; with one running, removal is refused; with
  one paused, removal succeeds and the run is gone.
- Removing a library keeps refresh runs.
- Every file a run writes carries the run's library; a NULL `library_id`
  insert is refused.
- Re-indexing one library walks only its root.
- Removing a library deletes its file records and their collection,
  watchlist, reading-list, progress and failure rows, leaves other libraries'
  records alone, and leaves every file on disk in place; removing a library
  with a paused run cancels the run first.
- `GET /v1/files` with `libraryUuid` returns only that library's files;
  `includeLibraries` is gone (an unknown query key is ignored as today, so the
  test asserts behaviour, not a 400).
- Audio: indexed with tags and duration; cover art as thumbnail; streamed
  with ranges and the right MIME type; metadata edited; `audio` accepted as a
  type filter.
- The upgrade migration on a database with files inside and outside
  libraries, collections, watchlists, reading lists, progress, index runs
  (finished, paused, interrupted) inside and outside libraries, refresh runs,
  and run failures attached to all of them: outside rows and everything
  attached are gone, inside rows untouched, refresh runs kept, and the schema
  allows audio and refuses a NULL `library_id`.
- HTTP↔FFI parity for every changed call (FR-FC-24).
- Removed calls are gone: `POST /v1/index` answers `404`/`405`; the FFI
  header no longer declares `alexandria_index_start`.

## Risks

- **The cleanup deletes data on upgrade.** That is the owner's decision, and
  it is irreversible. The CHANGELOG states it plainly, and the PR says the
  owner should back up `alexandria.db` before the first start on 0.6.0 if
  they want the old progress.
- **The table rebuild.** Rebuilding `files` for a CHECK and a NOT NULL change
  must keep every index and every row id that other tables reference.
  Migration 25 did the same rebuild and is the template.
- **The UI breaks until its follow-up lands.** The FFI loses
  `alexandria_index_start` and changes `alexandria_library_register`'s
  answer. alexandria-ui fails to bind against the new header until it is
  updated — loudly, at the binding, rather than silently.

## Decided by the owner

- Alexandria is a library manager: libraries are the only way files enter the
  catalog, the folder structure is preserved, and only readable types are
  indexed.
- Approach B: the other features stay and work on library files.
- Audio and music files are supported again.
- Existing files outside libraries are deleted on upgrade (start clean).

## Claude's calls (owner can override)

- Audio comes back as playable, tagged files only — no playlists, play
  history, lyrics/artist lookups or visualiser.
- Registering a library starts its first index run, with an optional
  `priority`; re-indexing is per library; the free-form index call is
  removed.
- Registering refuses a folder that does not exist (a move still accepts
  one); a registration whose run cannot open leaves no library behind.
- One unfinished run per library: a second run, a move, or removing it
  while the run is running is refused with `409` naming the run, rather than
  waiting for it.
- Refresh runs carry no library and survive library removal and the upgrade.
- No per-library type scope; the `types` argument and `IndexScope` go.
- Removing a library deletes its catalog records (never the disk files).
- Refresh stays catalog-wide.
- `GET /v1/files` gains `libraryUuid` and loses `includeLibraries`.
- Version 0.6.0; UI and docs site follow as separate changes.
- Audio is restored from history rather than rewritten.

## Left to the builder

- Exact migration SQL and its order relative to `PRAGMA foreign_keys`.
- How `library_id` is threaded through the index walk and the repository
  insert.
- How removal reuses the purge path (per-file or one bulk statement).
- Which restored audio tests still apply unchanged.
