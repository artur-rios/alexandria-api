# Metadata in the catalog listing

**Date:** 2026-08-26
**Status:** approved
**Issue:** [#116](https://github.com/artur-rios/alexandria-api/issues/116)

## Problem

`GET /v1/files` and `alexandria_files_list` answer an array of `File` records:
uuid, path, name, type, state, the stat pair, the timestamps. No metadata. The
title of a track, the artist, the album, the author of a book, the resolution of
a video — all of it is on the subtype row, and all of it needs a second call per
file to reach.

Two consequences, and they are the same missing capability seen from opposite
sides.

**Search cannot match what a file is called.** `FileFilter` carries a type, a
state and a collection, and nothing else; the desktop client therefore matches
client-side over the records it has loaded, against the file's name and path,
because that is all the listing gives it. So an owner types `Airbag` and finds
nothing, while typing the ripper's `DISKNAME-01` finds the track — in a client
whose music area has just spent a whole feature refusing to show them that name.

**Naming a library costs one call per file.** That same client draws its music
area by artist, album and title, and to know those it lists the audio files and
then reads each one's detail individually. A few thousand tracks is a few
thousand sequential calls before the area can group anything. It caches the
result, so the cost is paid once per run — but it is paid at the moment the
owner first opens the area, which is the worst moment available.

Neither is a defect in the client. Both are the shape of the listing.

## Design

### 1. The listing answers what the detail call answers

`GET /v1/files` and `alexandria_files_list` return an array of **`FileView`** —
the same record `GET /v1/files/{uuid}` already returns for one file: the `File`,
its `SubtypeMetadata`, and the extracted scalars that live outside it (image
width and height, document page count, video duration, comic page count).

One shape for a file, everywhere. The alternative — a lighter record carrying
some of what the detail call carries, under a different name — invites exactly
the question this design exists to remove: why does the list know less than the
detail, and which call do I need?

The listing gets heavier, deliberately. A thousand audio files now carry a
thousand titles and artists. That is the payload the client was making a
thousand calls to assemble.

### 2. Batched, not per row

The obvious implementation — call `find_metadata_by_uuid` for each row — would
move the N+1 from the client into the core and call it fixed. It is not what
this does.

The listing runs its existing query, then **one further query per subtype table
the result actually contains**, each fetching every matching row at once:

```txt
files matching the filter        1 query
audio metadata for those ids     1 query   (only when the result has audio)
video, document, comic, image    1 each    (same condition)
the extracted scalars            1 each    (same condition)
```

A listing filtered to one type — which is what the client always asks for —
costs two or three queries whatever its size. An unfiltered listing across a
mixed library costs a bounded handful. Neither grows with the number of files.

The subtype rows are keyed by the integer `file_id`, so each batch is a single
`WHERE file_id IN (…)` against an indexed column, and the results are stitched
back onto their files in memory.

### 3. Both transports, identically

FR-FC-24 and NFR-09 require the HTTP and FFI surfaces to return identical
results, and this changes what both return. The FFI surface's
`alexandria_files_list` answers the same JSON array the HTTP route does, as it
does today — the change is in the shape of the elements, not in how either
transport carries them.

This is a **breaking change** to both. The desktop client is the only consumer
and is updated in the same sitting; there is no deprecation window to keep,
because there is nobody to keep it for.

### 4. What this does not do

It adds no search query. The client's search stays client-side, matching over
the records it has loaded, which is what its own use case describes — it simply
gains metadata to match against. A core-side search endpoint is a different
feature, and this one removes the reason to want it.

It changes no filter. `FileFilter` is untouched.

It extracts nothing new. Every value in the response is already in the database,
put there by first-index prefill (FR-FC-25).

## Requirements impact

- **FR-FC-12** describes browsing the catalog and what a listing answers, and
  becomes the `FileView` shape rather than the `File` shape.
- **FR-FC-24** and **NFR-09** are unchanged in substance; the parity assertion
  simply now covers a richer record.

No new use case: UC-03 already covers browsing files, and this widens what its
answer carries.

## Testing

Following Testing Specification §3:

- **Unit,** against fakes: a listing of one type carries that type's metadata; a
  mixed listing carries each file's own; a file whose subtype row is absent
  carries no metadata rather than failing; the type, state and collection
  filters behave exactly as before.
- **Integration,** against SQLite: the batching holds — a listing of many files
  issues a bounded number of queries rather than one per file. This is the
  claim the whole design rests on, and it is the one a later change could
  silently break; assert it rather than trusting it.
- **Integration,** through the real HTTP surface: the array's elements carry
  metadata, and the existing filter behaviour is unchanged.
- **Parity:** the same filter over HTTP and FFI returns identical arrays
  (NFR-09, FR-FC-24).

## Risks

The payload grows, and the growth is unbounded in the number of files. A library
of a hundred thousand documents now returns a hundred thousand titles and
authors on one call. That is the deliberate trade — the client was fetching them
anyway, one at a time — but it is the thing to watch, and the honest answer if it
stops fitting is pagination, which the listing does not have today and which
would be a feature of its own rather than a patch to this one.
