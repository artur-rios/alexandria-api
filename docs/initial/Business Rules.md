# Business Rules — Alexandria

## Domain Entities

| Entity | Represents |
| --- | --- |
| **File** | An indexed on-disk resource. Subtypes carry type-specific metadata. The API stores a path and content hash plus metadata; it never stores the bytes. |
| **AudioFile** | A music/audio **File** with editable music metadata (title, artist, album, year, genre, track, …). |
| **VideoFile** | A movie or series **File**. Marked as movie or series; carries editable video metadata (title, year, resolution, …). |
| **HtmlPage** | A saved/linked HTML page **File** (metadata + references to the page/format). |
| **TextFile** | A Markdown or plain-text **File** whose content can be read and edited in place on disk. |
| **Document** | A PDF or e-book (book) **File**. Metadata only, no content editing; eligible for reading lists. |
| **ComicBook** | A comic book **File** (CBR/CBZ). Metadata only; eligible for reading lists. Distinguished from books so reading lists can target books vs comics distinctly. A `.pdf` indexes as a Document — extension alone cannot tell a comic PDF from a book PDF. |
| **Image** | An image **File** (metadata only, no editing). |
| **Bookmark** | A browser bookmark pointing to a URL, organized in bookmark collections. Independent of on-disk files. |
| **Collection** | A named folder grouping a set of items. Applies to files (file collections) and to bookmarks (bookmark collections) by its `kind` discriminator. Nesting is out of the current scope (flat collections). |
| **Watchlist** | A named group tracking the owner's consumption of movies and series, with per-item watch state. |
| **WatchProgress** | The link between a **VideoFile** and a **Watchlist**, holding that item's watch state and progress for that list. |
| **ReadingList** | A named group tracking the owner's reading of books (Documents) and comic books, with per-item read state. |
| **ReadingProgress** | The link between a Document/ComicBook and a **ReadingList**, holding that item's read state and progress for that list. |

## Relationships

| From | Cardinality | To | Notes |
| --- | --- | --- | --- |
| Collection (kind = file) | 1 — N | File | A file collection contains zero or more files. A file may belong to a collection. |
| Collection (kind = bookmark) | 1 — N | Bookmark | A bookmark collection contains zero or more bookmarks. A bookmark may belong to a collection. |
| Watchlist | 1 — N | WatchProgress | Deleting a watchlist removes its WatchProgress entries, not its VideoFiles. |
| WatchProgress | N — 1 | VideoFile | A video may appear in multiple watchlists; progress is tracked per watchlist. |
| WatchProgress | N — 1 | Watchlist | Inverse of above. |
| ReadingList | 1 — N | ReadingProgress | Deleting a reading list removes its ReadingProgress entries, not its files. |
| ReadingProgress | N — 1 | Document or ComicBook | A book or comic may appear in multiple reading lists; progress is tracked per reading list. |
| ReadingProgress | N — 1 | ReadingList | Inverse of above. |

The owner is a single implicit principal; entities carry no per-user foreign key.

## Rules

| ID | Rule | Rationale |
| --- | --- | --- |
| **BR-01** | All data belongs to a single owner. There is no multi-tenancy, account sharing, or per-library isolation. | The product is a personal library for one user. |
| **BR-02** | A File is referenced by its on-disk path and a content hash. The API stores metadata only and never imports or duplicates file bytes. | The library catalogues files that already exist on disk. |
| **BR-03** | Editing a TextFile's content writes the new content back to the file at its path on disk. | The owner wants to author/edit Markdown and text in place. |
| **BR-04** | The API does not perform complex media editing. No audio re-encoding, no video re-encoding, no image manipulation. It manages metadata, names, organization, and (for text) content only. | Scope guard — this is a catalog, not a media editor. |
| **BR-05** | Watchlists apply only to VideoFiles (movies and series). Non-video files cannot be watchlisted. | Watchlists track media consumption, which is meaningful only for video. |
| **BR-06** | For a series, watch progress is tracked per episode; for a movie, as a single item. | Reflects how movies and series are consumed. |
| **BR-07** | The domain logic lives in a Rust core library and is exposed over HTTP/REST-JSON and via a Flutter FFI surface. Both transports produce identical results for the same operation. | The owner chooses HTTP or FFI per client without behavioral drift. |
| **BR-08** | Authorization is pluggable and configured at startup; exactly one auth mode is active at runtime. The **external mode** validates JWTs issued by an external authentication service (issuance is never self-performed; provider integration is wired later). The **local-login mode** validates credentials (email + salted/hashed password) stored in a SQLite row on the local machine. No plaintext credentials are ever stored — only the one-way password hash. The **Windows mode** treats the Windows account the server process runs as the credential: nothing is typed and nothing is stored, and the process reads its own account at startup and compares it to the configured owner. | Defer identity to a dedicated external service when available; offer a self-contained local alternative for single-user desktop use; offer a zero-setup alternative when the process itself already runs as the owner. |
| **BR-09** | Indexing runs asynchronously and must not block read/query operations. | The owner indexes thousands of files while still browsing and editing. |
| **BR-10** | Record deletion is two-phase. Soft delete hides a record and keeps it restorable; a hard purge removes it permanently only after a configurable retention period has elapsed. | Protect the owner from accidental data loss. |
| **BR-11** | A hard purge removes a catalog record without touching the on-disk file. Removing the on-disk file is a separate, explicit *purge-on-disk* operation that removes both the record and the physical file. | Disk deletions must be intentional and never a side effect of catalog cleanup. |
| **BR-12** | Deleting a Collection removes the grouping only. Its contained files and bookmarks are preserved (unlinked), not deleted. | Organizing changes should not destroy the catalogued items. |
| **BR-13** | Deleting a Watchlist removes its WatchProgress entries only. Its VideoFiles are preserved. | Removing a tracking list should not remove the media records. |
| **BR-14** | SOLID principles and a Command/Query (CQRS-style) organization are the baseline for the core library's operations. | Mandated design approach from the brainstorm. |
| **BR-15** | Reading lists apply only to book Documents and ComicBooks. Watchlists apply only to VideoFiles. Reading lists and watchlists never overlap their target file kinds. | Each kind of consumption tracking is meaningful only for its matching media. |
| **BR-16** | For a comic book series, reading progress is tracked per issue; for a single book, as a single item. | Reflects how books and comics are consumed (parallel to BR-06). |
| **BR-17** | Exactly one auth mode (external JWT, local login, or Windows account) is active at a time, selected by startup configuration. A caller authenticated by an inactive mode is rejected. | Avoid ambiguity in the trust boundary; keep the model simple per deployment. |
| **BR-18** | Local-login credentials (email and a salted/hashed password) live in a single SQLite row and are set or changed via a local setup command. The plaintext password is never stored and never logged. | Protect the single user's credentials on a local-only desktop deployment. |
| **BR-19** | Deleting a ReadingList removes its ReadingProgress entries only. Its target Documents and ComicBooks are preserved. | Removing a tracking list should not remove the catalogued items. |

## Validation Constraints

| Field | Entity | Constraint |
| --- | --- | --- |
| **path** | File | Required. Absolute on-disk path. Unique across all files. |
| **contentHash** | File | Required. Content hash computed at index time. |
| **name** | File | Required. Editable. Must be a valid file name for the host OS. |
| **type** | File | Required. One of the supported file-type subtypes (audio, video, html, text, document, image). |
| **url** | Bookmark | Required. Must be a valid URL. |
| **title** | Bookmark | Required. Non-empty. |
| **kind** | Collection | Required. One of `file` or `bookmark`. |
| **name** | Collection | Required. Non-empty. |
| **name** | Watchlist | Required. Non-empty. |
| **state** | WatchProgress | Required. One of `Pending`, `Watching`, `Watched`. |
| **mediaKind** | VideoFile | Required. One of `movie` or `series`. |
| **formatKind** | Document | Required. One of `book` or `ebook`. |
| **issueNumber** | ComicBook | Optional. Positive integer when the comic is part of a series. |
| **series** | ComicBook | Optional. Series name when the comic is part of a series. |
| **mediaKind** | ReadingProgress target | Required. One of `Document` or `ComicBook`. |
| **state** | ReadingProgress | Required. One of `Pending`, `Reading`, `Read`. |
| **email** | Local login | Required when local-login mode is active. Valid email format. Unique (single owner). |
| **passwordHash** | Local login | Required when local-login mode is active. Salted hash; never plaintext. |

## Permissions

| Role | Operations |
| --- | --- |
| **Owner (authenticated caller)** | All CRUD on files, bookmarks, collections, watchlists, watch progress, reading lists, and reading progress; initiate indexing; soft and hard delete; explicit purge-on-disk. In local-login mode, also setting/changing the local credentials via a local setup command. |
| **Unauthenticated caller** | None. Every operation requires a valid credential (a JWT in external mode, local-login credentials in local mode, or a session opened via the Windows account in Windows mode). |

There is a single role — the authenticated owner. No administrative or
read-only role is distinguished in this scope.

## Lifecycle

**File**

1. **Creation** — produced during an indexing scan when a path is first seen, or
   created explicitly when a file is added to the catalog.
2. **Update** — metadata and the on-disk `name` are editable; re-indexing
   refreshes the content hash and metadata. TextFile content edits write back to
   disk.
3. **Soft delete** — record marked deleted, hidden from views, restorable.
4. **Hard purge** — after the retention window, the record is permanently
   removed from the database. The on-disk file is untouched.
5. **Purge-on-disk** — a separate explicit operation removes the catalog record
   and deletes the physical file on disk.

**Bookmark** — created, updated, soft-deleted, hard-purged under the same
two-phase model. No disk file is associated.

**Collection** — created, renamed, and deleted. Deletion unlinks (preserves)
its items.

**Watchlist** — created, renamed, and deleted. Deletion removes its
WatchProgress entries only.

**WatchProgress** — created when a video is added to a watchlist; transitions
`Pending → Watching → Watched`; deleted when the item is removed from the
watchlist or the watchlist is deleted.

**ReadingList** — created, renamed, and deleted. Deletion removes its
ReadingProgress entries only.

**ReadingProgress** — created when a book Document or ComicBook is added to a
reading list; transitions `Pending → Reading → Read`; for a comic series,
tracked per issue; deleted when the item is removed from the reading list or the
reading list is deleted.

**Local-login credentials** — set or changed via a local setup command; the
password stored in SQLite only as a salted one-way hash; not soft/hard-deleted
through the catalog lifecycle (their
management is part of the auth module, not the catalog).

## Prohibitions

- Storing file bytes in the API's storage.
- Re-encoding audio or video, or manipulating images.
- Sharing libraries or records across multiple users.
- Removing an on-disk file as a side effect of a catalog delete (only the
  explicit purge-on-disk operation may touch the file).
- Hard-purging a record before its soft-delete retention window has elapsed.
- Issuing JWTs from within Alexandria (only the external auth service issues
  them, in external mode).
- Storing plaintext passwords, logging credentials, or accepting the inactive
  auth mode's credentials.
- Auto-advancing work through workflow stage boundaries without human approval.
