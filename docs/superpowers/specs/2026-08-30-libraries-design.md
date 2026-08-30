# Libraries

**Date:** 2026-08-30
**Status:** proposed

## Problem

The catalog sorts everything by type. A file is audio, or video, or a
document, and it is browsed with every other file of that type in one flat
list. That is right for a music collection and wrong for material that only
means anything together.

A course is the clear case: a folder per class, each holding a recording, a
handout, some slides and a worksheet. Indexed today, the recording lands
among the films, the handout among the books, and the slides among the
documents — scattered across four panels, in none of which they are near
each other, and all of them cluttered by material nobody was browsing for.

The structure the owner already built on disk is the structure that carries
the meaning, and the catalog throws it away.

## What a library is here

A registered folder whose contents are browsed as a tree, and which are shown
**only** there.

Two halves, and both are needed. The tree, because the folder layout is the
organisation — flattening a course into a list loses which handout belongs to
which class. The exclusivity, because material that only means something in
context is noise everywhere else: a hundred lecture recordings in the Videos
panel bury the films it exists to show.

## Naming, and the rename this forces

"Library" already means three things in this codebase, and a fourth would be
unreadable. This design takes the word and clears the others:

| Today | Becomes | Why |
| --- | --- | --- |
| `LibraryType` (audio, video, …) | `FileType` | What the core has always called it. The UI invented a second name for one concept. |
| "library folder" (a registered root) | "source folder" | It is where files come *from*. `library_sources` is already the module's name. |
| The Library menu | unchanged | It holds library-wide tools, which stays true. |

Doing this first is not tidying. Every sentence written afterwards about
"a library's file type" is ambiguous until it is done.

## Design

### 1. Membership is recorded, not derived

A file's library is written on its row at index time, not worked out from its
path when something asks.

Deriving it would mean every listing carrying the set of library roots and
testing each path against all of them — the exclusion would live in whatever
happened to be querying, which is several places and one of them will forget.
A column means the type listing excludes with `library_id IS NULL` and cannot
accidentally not.

```
libraries    (id, uuid, name, root_path)
files        + library_id
```

`root_path` is what the tree is relative to, so a library that moves on disk
is one row to correct rather than a re-index.

### 2. The core learns about a library at index time

Registered folders are the application's, not the core's — it is told a root
and walks it. So the index command gains an optional library uuid: index this
root, and everything under it belongs to that library.

That is the smallest change that puts membership where it has to be. The
alternative — the core learning what a registered folder is — would move a
concept across the boundary for no other reason.

### 3. Exclusion is one predicate, in the queries that browse by type

`files_list` and the dashboard's recent list gain `library_id IS NULL`.

**Search does not.** Nor do playlists, watchlists, reading lists or
collections. A lecture recording is still a video the owner may want in a
watchlist, and someone who types its name should find it. What the
exclusivity is for is *browsing* — the panels are where scattering hurts,
and where a hundred lectures bury the films. Refusing the file everywhere
else would amputate features that work perfectly well on it.

### 4. The tree is answered one level at a time

A query takes a library and a folder path relative to its root, and answers
the folders and files directly inside it.

Not the whole tree in one payload: a course with two hundred classes is a
large document to build, serialize and parse so the owner can look at the
six things in one folder. One level is what a tree view actually draws, and
the path arithmetic stays in the core where the paths are.

### 5. A file is in one library or none

`library_id` is a single column, so nesting a library inside another is not
expressible, and that is deliberate. Two libraries owning one file means two
answers to "where does this appear", and every screen would need a rule for
choosing. Registering a folder beneath an existing library is refused, with
the existing one named.

## Components

| Component | Change |
| --- | --- |
| `alexandria-core/migrations/…20_libraries.sql` (new) | The table and the column. |
| `alexandria-core/src/libraries/` (new) | Model, repo, the tree query. |
| `catalog/commands/index.rs` | Carry a library through a run. |
| `catalog/queries/browse.rs`, the recent list | `library_id IS NULL`. |
| `alexandria-ffi`, `alexandria-http` | Both surfaces, at parity (FR-FC-24). |
| `alexandria-ui` | The rename; registration asks; a Libraries destination and its tree. |

## Requirements impact

- A use case for registering a source folder as a library and browsing it.
- FR-CT-01's type panels gain the exclusion, stated as a rule rather than
  left implicit in a query.
- FR-CT-06 (search) explicitly keeps finding these files, so a later reader
  does not "fix" the inconsistency.

## Testing

- A file under a library is absent from its type listing and from the recents.
- The same file is still found by search, and can still join a watchlist.
- A tree level answers only its own children, not its descendants.
- A library that moves on disk needs its root corrected, not a re-index.
- Registering a folder inside an existing library is refused, naming it.
- Removing a library returns its files to the type panels rather than
  deleting them.

## Risks

The rename touches a great deal and is where a mistake hides: `LibraryType`
appears across the UI, and a half-done rename leaves two names for one thing,
which is worse than the one bad name it started with. It goes in as its own
change, mechanical and reviewable on its own, before any of this is built on
top of it.

The second risk is that exclusivity is a decision the owner cannot see the
consequences of until after indexing. Marking a folder as a library empties
part of the Videos panel, and someone who did it by accident needs an obvious
way back — which is why removing a library restores rather than deletes.
