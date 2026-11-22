# Project Overview — Alexandria

## What This Is

Alexandria is a back-end API for a personal library manager. The owner adds
folders as libraries; Alexandria indexes every file in them it can read, keeps
each library's folder structure exactly as it is on disk, and lets a client
browse that tree and read or play what is in it. A folder that is not a library
is invisible to it. Audio is played as files; managing music (playlists, play
history, lookups) lives in Orpheus, a separate project. It is written in Rust
as a core library that the Flutter desktop front-end calls in process, through
its C ABI. It only runs locally, for the person at the machine: there is no
server and no login.

## The Problem

Files accumulate across a single user's machine — movies and series, HTML
pages, Markdown and text notes, PDFs and e-books, images, and browser bookmarks
— with no unified way to browse, search, edit metadata, or track consumption
progress. Scattered files and disconnected browser-managed bookmarks make
organization slow, lossy, and hard to automate.

Alexandria gives that user a single, fast, type-aware catalog over everything
already on disk, without re-encoding, duplicating, or relocating the files.

## Who It's For

- **The owner** — a single user whose personal collection the library catalogs.
  There is no multi-tenant model; all data belongs to this one user.
- **The Flutter desktop front-end** — the first-party client, and the only
  one, calling the core library in process over FFI.

## What It Does

- Lets the user add folders as libraries — the only way files enter the
  catalog — and browses each one as the folder tree it is on disk.
- Indexes thousands of files from the libraries' folders, only the types it can
  read, and keeps catalog metadata up to date.
- Lets the user view, edit metadata of, rename, and delete:
  - Movies and series (video files)
  - Music and other audio files
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
- Exposes its domain operations through a Rust core library callable through
  Flutter FFI.
- Performs indexing and other heavy work asynchronously so responses stay fast.

## What It Doesn't Do

- No multi-user accounts or shared libraries — single owner only.
- No complex media editing — no video re-encoding, no image
  manipulation. The API manages metadata, names, organization, and (for text)
  content; it does not transform media.
- No duplicating or relocating source files — files stay on disk where they are.
  The API stores metadata plus a path, never the bytes. Change detection is the
  size and modification time the directory entry already carries (FR-FC-09,
  FR-FC-10); a content hash is recorded only where a text edit computes one.
- No automatic removal of files from disk. Deletion of a catalog record never
  touches the on-disk file unless the user runs a separate, explicit purge.
- No login and no network access. The core only runs locally, for the person
  at the machine, so it has no accounts, passwords, sessions or tokens, and no
  server for anything else to reach a catalog through.

## How Success Is Measured

- The library indexes thousands of files on a personal machine without blocking
  the caller, and subsequent reads return fast.
- The owner can find, open metadata for, rename, edit (where applicable), and
  organize any supported file type from a single client.
- Watchlists accurately track which movies and series the owner has watched and
  what remains pending.
- Reading lists accurately track which books and comic books the owner has read
  and what remains pending.
- Deletion is safe: removed items can be restored until their retention window
  expires, and on-disk files are only removed by an explicit, intentional
  command.
