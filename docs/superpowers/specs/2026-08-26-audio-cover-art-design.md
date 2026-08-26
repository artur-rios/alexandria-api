# Audio cover art

**Date:** 2026-08-26
**Status:** approved
**Issue:** [#117](https://github.com/artur-rios/alexandria-api/issues/117)

## Problem

`GET /v1/files/{uuid}/thumbnail` and `alexandria_file_thumbnail` answer for a
video, an image, or a comic. An audio file gets `InvalidInput`, because
`ThumbnailHandler::thumbnail` has no arm for `FileType::Audio`.

Nearly every tagged release carries its cover picture inside the file. `lofty`
— already the crate behind `audio_tags.rs` — reads it. The catalog simply never
asks.

The front end feels the gap directly. Alexandria's desktop client draws an
album's case with its sleeve facing the owner while a record plays, and has
nothing to put on it: it falls back to typesetting the album and artist on a
colour derived from the album's name. That is a deliberate design rather than a
placeholder, but it is standing in for something the file usually already
contains.

## Design

### 1. Read it when it is asked for, not at index time

`ThumbnailHandler::thumbnail` already reads its source on demand: a video
renders a frame with ffmpeg, a comic reads its archive's first page. Both are
cached afterwards by uuid and mtime, so the read happens once per file per
change.

Audio joins them as a fourth arm. Nothing is extracted during indexing, nothing
is stored, no table gains a column, and no existing library needs re-indexing.

That also keeps two existing rules intact rather than bending them:

- **FR-FC-09** — indexing reads no file bytes to identify a file. Reading a
  picture at index time would be a new read of every audio file in a library,
  paid at scan time, for something only a thumbnail request needs.
- **FR-FC-25** — first-index prefill covers the metadata *fields an owner can
  edit*. Cover art is not one: there is no field for it, no editor, and nothing
  to overwrite.

### 2. The arm

```txt
FileType::Audio  →  read the embedded front-cover picture
                 →  hand its bytes to ImageThumbnailRenderer
                 →  the same downscale every other thumbnail gets
```

A file that carries no embedded picture is `InvalidInput`, in the same shape an
SVG image and a `.cbr` comic already are: "not supported for this file", which
a caller can act on, rather than a decoder error it cannot.

The picture is whatever the tag calls the front cover. Where a file carries
several pictures, the front cover is the one a sleeve wants; where it carries
pictures but names none of them the front cover, the first is a better answer
than nothing.

### 3. The port

The reader is a trait, injected into `ThumbnailHandler` as its collaborators
already are, so the handler's decisions — no picture, wrong type, deleted
record, unauthorized — are unit-tested against a fake with no file I/O
(Testing Specification §6.2). The real implementation is `lofty`-backed and
wired in `services.rs`, beside the tag reader it sits next to.

It reads on the blocking pool through `read_blocking`, exactly as
`LoftyAudioMetadataReader` does. An embedded cover is bounded in practice but
not by the format, so the read is capped by `MAX_PLAYBACK_READ_BYTES` the way
the image arm's is — a file claiming a 3 GB picture must not cost 3 GB before
the decoder's own guard runs.

### 4. What does not change

The cache, its key, the downscale, the MIME type it reports, the authorization
and state checks, and both transports' shapes are all untouched. This is a new
arm in one `match`, a new port, and its implementation.

## Requirements impact

- **FR-MP-05** reads "a downscaled thumbnail image for a video, image, or comic
  File" and becomes "for a video, image, comic, or audio File", noting that an
  audio thumbnail is the picture embedded in the file itself.

No new use case: UC-40 already covers requesting a thumbnail, and this widens
the set of files it answers for.

## Testing

Following Testing Specification §3, in the layers it names:

- **Unit,** against fakes: an audio file whose reader returns a picture yields a
  thumbnail; one that returns none is `InvalidInput`; a deleted record and an
  unauthenticated caller are refused before the reader is consulted; the
  renderer is asked for the same `THUMBNAIL_MAX_DIM` every other arm uses.
- **Unit,** on the real reader: a fixture carrying a front cover returns its
  bytes; one carrying no picture returns `None`; an unparseable file returns
  `None` rather than failing.
- **Integration,** through the real HTTP surface: a request for an audio
  thumbnail answers with image bytes and the caching behaviour every other
  thumbnail has.
- **Parity:** the same audio uuid over HTTP and over FFI returns byte-identical
  results (NFR-09, FR-MP-06).

## Risks

The fixture is the awkward part. The suite needs an audio file that genuinely
carries an embedded picture, small enough to live in the repository. Writing
one with `lofty` at test time is better than committing a binary: the test then
states what it is testing rather than depending on a file nobody can read.
