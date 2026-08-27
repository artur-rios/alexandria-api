# The album artist

**Date:** 2026-08-27
**Status:** approved
**Issue:** [#120](https://github.com/artur-rios/alexandria-api/issues/120)

## Problem

An audio file's extracted metadata carries the artist of the *track*. It
carries nothing about the artist of the *record*.

For most albums those are the same and nobody notices. For the two cases where
they differ they are the only thing that matters: a compilation scatters one
record across a dozen artists, and a guest feature files "A feat. B" apart from
the rest of the album it belongs to. A client grouping a library by artist —
which is the first thing any music interface does — gets both wrong, and has
nothing to group by instead.

`ALBUMARTIST` (Vorbis comments), `TPE2` (ID3v2) and `aART` (MP4 atoms) are the
tags that answer it. `lofty`, already the crate behind `audio_tags.rs`, reads
all three.

## Design

### 1. A seventh field

`album_artist` joins the six fields an audio file already carries, through
every layer they pass:

- a column on `audio_files`, added by a migration in the shape of
  `00000000000010_video_duration.sql`;
- a field on `AudioTags`, read by `LoftyAudioMetadataReader` at first index;
- a field on `SubtypeMetadata::Audio`, so it reaches both transports;
- an editable field under FR-FC-14, because every other thing extracted from a
  file's tags is one and an owner correcting a mis-tagged compilation is the
  case this feature exists for.

Nothing about how extraction works changes. It is best-effort, it runs at first
index only, and a file that carries no such tag simply has none — exactly as
with the other six.

### 2. What the core does not decide

The core does not fall back. A file with no album artist has
`album_artist: null`, not a copy of its track artist.

That is deliberate and it is the same rule the catalog already follows
everywhere: what the core reports is what the file says. A client that wants to
group by "album artist, or the track artist when there is none" can apply that
rule itself and knows it is applying it; a core that folded the fallback in
would make an absent tag indistinguishable from a present one that happens to
match, and would take the choice away from a client that wanted to tell them
apart — a compilation tagged with an explicit "Various Artists" and one with no
album artist at all are different things.

## The consequence for existing libraries

FR-FC-25 prefills at first index only, and re-index deliberately never re-runs
extraction so that an owner's edits are never overwritten. So a library that is
already indexed gains no album artists until it is indexed afresh.

That is not a new limitation invented here — it is what happened when
`duration_seconds` was added, and it is the price of the rule that protects
edits. It is stated rather than worked around, and the client-side fallback to
the track artist is what makes an un-re-indexed library keep working meanwhile.

## Requirements impact

- **FR-FC-01** lists what an audio file's record holds, and gains the album
  artist.
- **FR-FC-14** covers editing an audio file's metadata, and gains it too.

No new use case: UC-01 indexes, UC-04 edits, and both already cover this field's
siblings.

## Testing

Following Testing Specification §3:

- **Unit, on the reader:** a fixture carrying `ALBUMARTIST` returns it; one
  carrying none returns `None` for that field while still returning the others;
  an unparseable file still returns nothing at all rather than failing.
- **Unit, on the handler:** a file whose tags carry an album artist is recorded
  with one; extraction failure still leaves the file indexed.
- **Integration, against SQLite:** the column round-trips, and a listing carries
  it (FR-FC-12) as it carries the other six.
- **Integration, editing:** an owner can set and clear it, and clearing it is
  distinguishable from never having had one.
- **Parity:** HTTP and FFI answer identically for a file that has one and a
  file that does not (NFR-09, FR-FC-24).

## Risks

The migration adds a nullable column, so it cannot fail on existing data — but
every existing audio row will read `null` for it, and no test that indexes fresh
data will notice. The integration test that matters is the one that reads a row
written before the field existed; without it, a client's fallback path is
exercised by nothing in this repository.
