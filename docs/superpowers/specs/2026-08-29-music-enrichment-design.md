# Music enrichment: artist images and lyrics

**Date:** 2026-08-29
**Status:** proposed

## Problem

The catalog knows what the owner's tags say — title, artist, album artist,
album, year, genre, track number. It knows nothing a tag does not carry. A
music library therefore has no artist photography and no lyrics, and both are
things a listener expects while a record is playing.

Neither can be extracted from the files: an artist photo is not in a track's
tags (embedded art is the *release* cover, which `catalog` already reads), and
lyrics are in a tag only when whoever made the file put them there, which for a
ripped library is almost never.

## The principle this changes

Alexandria's Vision Document says, of the product as a whole:

> **In-process, offline, single-user.** No server, no network calls, no
> synchronization, no second user.

This design breaks the third clause and nothing else. It is worth stating
exactly how far, because "no network calls" is load-bearing for a product whose
pitch is that the owner's library is theirs:

- Outbound only, to three named public services, and only about music.
- Never automatic. Off unless the operator turns it on (`[metadata] enabled`),
  and the shipped default is off.
- Nothing about the owner leaves the machine. The queries carry an artist name,
  a track title, an album name and a duration — no identifier, no library
  contents, no account, no telemetry, no counts.
- Everything fetched is cached in the catalog, so a lookup happens once per
  artist and once per recording rather than per play.

What stays true: no inbound server beyond the existing local HTTP surface, no
synchronization, no second user, and no telemetry.

## Why the core and not the application

BR-02, and the same argument the playlists design made: an artist's photograph
and a track's lyrics are facts *about the catalog*, not preferences about how it
is displayed. They are looked up once and then belong to the library — they
should survive a reinstall of the interface, be identical over both transports
(FR-FC-24), and be written by the layer that owns the database.

The interface half stays exactly as offline as it is today: `alexandria-ui`
gains no HTTP client and calls no third party. It reads what the core cached,
over the FFI, the way it reads everything else.

## The three services, and why each

No single service answers both questions. MusicBrainz has no images and no
lyrics; it has *identity*, which is what makes the other two lookups possible.

| Service | Answers | Key needed |
| --- | --- | --- |
| MusicBrainz | The artist's MBID and the recording's MBID, from tags | No |
| Wikidata → Wikimedia Commons | An artist image, reached by the MBID's Wikidata relation | No |
| LRCLIB | Plain and synced lyrics, by title/artist/album/duration | No |

All three are free, require no registration, and need no key the owner would
have to obtain — which is what keeps this a feature rather than a setup chore.

**MusicBrainz's terms are a hard constraint, not a courtesy.** It requires a
descriptive `User-Agent` naming the application and a contact, and it rate-limits
to one request per second per source. Both are honoured here: the agent string
is built from the crate name, version, repository and the operator's configured
`contact`, and every MusicBrainz call passes through a single process-wide
one-per-second gate. Enrichment refuses to run at all when `contact` is unset,
because an anonymous client is one they are entitled to block.

**Lyrics are somebody's copyright.** LRCLIB is chosen partly because it does not
restrict storing what it serves, which is what makes a cache lawful here. A
provider whose terms forbid retention (Genius's do) would force a fetch on every
play and would make this design wrong rather than merely slower. Lyrics are
stored, displayed to the one local owner, and never redistributed, exported, or
included in any backup this application writes.

## Design

### 1. Two tables, keyed to what each fact is actually about

```
artist_images (id, artist_name, mbid, source_url, image_path, fetched_at, outcome)
track_lyrics  (id, file_id, mbid, plain, synced, source, fetched_at, outcome)
```

An artist image is about an **artist**, not a file: one lookup serves every
track they appear on. It is keyed by the album artist's name as the catalog
holds it, because that is the only artist identity the catalog has — there is no
`artists` table, artists are tag values.

Lyrics are about a **recording**, and the closest thing the catalog has to a
recording is a file, so `track_lyrics` is keyed by `file_id`. Two files of the
same song get two rows; that is the honest answer, since they may differ in
edit, length, or language.

`outcome` records *that a lookup happened and what it found* — including
finding nothing. Without it, "no lyrics for this track" and "never looked" are
indistinguishable and every play re-asks three services a question they have
already answered no to.

Neither table carries a `FOREIGN KEY`, for the reason `reading_progress` and
`playlist_entries` do not: SQLite cannot add one through `ALTER TABLE`. Purging
a file must therefore delete its `track_lyrics` rows explicitly, exactly as it
already deletes reading progress and playlist entries.

### 2. Images are files on disk, not blobs in the database

`image_path` points into a cache directory beside the thumbnail cache, keyed by
the artist MBID. The database holds the path and the provenance, never the
bytes — the same division `playback.thumbnail_cache_dir` already makes, and for
the same reason: a catalog that has to stream image blobs out of SQLite is one
that gets slower at everything else.

### 3. Enrichment is a command the owner runs, not a step in indexing

Indexing walks the disk and must stay fast and offline. Enrichment reaches three
services over the network at one request per second, so folding it into the
index would make a first scan take hours and would tie a network outage to a
local operation.

It is its own command, over a scope the caller names — one file, one artist, or
everything not yet looked up — and it is resumable, because `outcome` records
what has already been asked.

### 4. Providers are a trait, so the tests never touch the network

One `MetadataProvider` port per question (`ArtistImageProvider`,
`LyricsProvider`), implemented once over `reqwest` and once as a fake. Every
handler is generic over the port, as every handler here already is over its
repository, so the whole command layer is unit-tested with no network, no keys,
and no flakiness. The HTTP clients themselves get a small number of tests
against recorded payloads.

### 5. A failed lookup is a recorded outcome, never a failed command

A service being down, rate-limiting, or having nothing for a track is normal.
None of it fails the enrichment run: each is written as an `outcome` and the run
continues to the next item. Only an unauthorized caller or a database failure
fails the command.

## Components

| Component | Change |
| --- | --- |
| `alexandria-core/migrations/…18_music_enrichment.sql` (new) | The two tables. |
| `alexandria-core/src/enrichment/` (new) | Model, repos, providers, commands, queries. |
| `alexandria-core/src/config.rs` | A `[metadata]` section: `enabled`, `contact`, `image_cache_dir`. |
| `alexandria-core/src/settings/mod.rs` | Report whether enrichment is available, so a client can say why it is not. |
| `catalog/commands/purge*.rs` | Delete a purged file's lyrics rows. |
| `alexandria-ffi`, `alexandria-http` | Both surfaces, at parity (FR-FC-24). |
| `alexandria-ui` | Reads and displays; gains no HTTP client. Its requirement documents record the amended principle. |

## Testing

- An artist image is looked up once and reused for every track by that artist.
- A track with no lyrics records an outcome, and a second run does not re-ask.
- A provider that is down records an outcome and the run continues.
- The MusicBrainz gate admits at most one request per second.
- Enrichment refuses to start when `contact` is unset, naming that as the reason.
- Enrichment refuses to start when `enabled` is false.
- Purging a file removes its lyrics; no orphan row is left behind.
- A file whose tags name no artist is skipped rather than queried blindly.

## Risks

The obvious one is that this makes an offline product reach the network, and the
mitigation is that it is off by default and narrow when on.

The subtler one is match quality. Tags are what the owner's ripper wrote, and
MusicBrainz will confidently return *an* artist for a misspelled name. A wrong
photograph on an artist page is worse than no photograph, so the lookup requires
a scored match above a threshold rather than taking the first result, and it
records what it matched against so a wrong one can be explained and cleared.
