# Project Overview — Alexandria

## What This Is

Alexandria is a back-end API that indexes, organizes, and surfaces the user's
personal library of media and documents living on disk. It is written in Rust
and exposes a JSON-over-HTTP API that any client in any language can call, while
keeping its domain logic in a reusable core library that a Flutter desktop
front-end can also consume directly via FFI.

## The Problem

Files accumulate across a single user's machine — music, movies and series, HTML
pages, Markdown and text notes, PDFs and e-books, images, and browser bookmarks
— with no unified way to browse, search, edit metadata, or track consumption
progress. Scattered files and disconnected browser-managed bookmarks make
organization slow, lossy, and hard to automate.

Alexandria gives that user a single, fast, type-aware catalog over everything
already on disk, without re-encoding, duplicating, or relocating the files.

## Who It's For

- **The owner** — a single user whose personal collection the library catalogs.
  There is no multi-tenant model; all data belongs to this one user.
- **The Flutter desktop front-end** — the first-party client, calling the API
  over HTTP and (optionally) the core library over FFI for tight integration.
- **Other systems** — any external client in any language that talks the HTTP
  API in JSON to read, organize, and edit the catalog.

## What It Does

- Indexes thousands of files from disk and keeps catalog metadata up to date.
- Lets the user view, edit metadata of, rename, and delete:
  - Music (audio files)
  - Movies and series (video files)
  - HTML pages
  - Markdown and text files
  - PDF and e-book formats (books)
  - Comic books
  - Images
  - Browser bookmarks
- Lets the user create, update, delete, and organize bookmarks and files in
  folders (collections).
- Lets the user build watchlists for movies and series and track watched
  progress.
- Lets the user build reading lists for books and comic books and track reading
  progress.
- Edits the content of Markdown and text files, writing changes back to the file
  on disk.
- Exposes the same domain operations over HTTP/REST-JSON and via a Rust core
  library callable through Flutter FFI.
- Performs indexing and other heavy work asynchronously so responses stay fast.

## What It Doesn't Do

- No multi-user accounts or shared libraries — single owner only.
- No complex media editing — no audio or video re-encoding, no image
  manipulation. The API manages metadata, names, organization, and (for text)
  content; it does not transform media.
- No duplicating or relocating source files — files stay on disk where they are.
  The API stores metadata plus a path/content-hash reference, never the bytes.
- No automatic removal of files from disk. Deletion of a catalog record never
  touches the on-disk file unless the user runs a separate, explicit purge.
- No self-issued authentication for the external mode. When external auth is
  configured, JWTs are issued by an external authentication service; Alexandria
  validates them. As an alternative, a local-login mode validates encrypted
  credentials stored on the local machine. Only one mode is active at runtime.

## How Success Is Measured

- The library indexes thousands of files on a personal machine without blocking
  the caller, and subsequent reads return fast.
- The owner can find, open metadata for, rename, edit (where applicable), and
  organize any supported file type from a single client.
- Watchlists accurately track which movies and series the owner has watched and
  what remains pending.
- Reading lists accurately track which books and comic books the owner has read
  and what remains pending.
- The HTTP API and the FFI core produce identical results for the same
  operation, so the Flutter front-end can choose either transport without
  behavioral drift.
- Deletion is safe: removed items can be restored until their retention window
  expires, and on-disk files are only removed by an explicit, intentional
  command.
