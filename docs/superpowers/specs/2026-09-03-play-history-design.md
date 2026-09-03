# Play history

**Date:** 2026-09-03
**Status:** implemented

## Problem

The owner has asked what they actually listen to — most played songs,
artists, albums and genres. The playlists design (UI,
`2026-08-28-playlists-design.md`) recorded the ask and turned it down, for a
reason that was true then and is the whole subject of this one:

> Nothing in the core records that a play happened, so they need a
> collection mechanism before they can show anything, and that is a separate
> design.

The catalog knows every track that exists and nothing about which of them
were heard. There is no statistic to compute, however clever the query.

## What a play is here

One row: this file, at this moment.

Not a duration, not a percentage, not a session. The core cannot see what
the owner is hearing, so anything richer would be a number the application
made up and the core stored as though it were a measurement. What counts as
"played" — heard to the end, or far enough into it — is the player's
judgement, applied in the player and sent here as a fact that already
happened.

The stamp is the core's, taken from its own `Clock`. A client that could
name the time could name last year's, and every ranking below is an
aggregate over exactly that column.

## Design

### 1. Append-only, and unaddressed

`play_events` has no `uuid`. Every other table here gives its rows a public
identifier because callers address them — rename this playlist, remove that
entry. Nothing addresses a play: it is never edited, never deleted on its
own, never referred to again once written. A play is a fact about a moment,
and the moment is over.

That also settles the write contract. `record` is not idempotent and carries
no uniqueness constraint: playing the same track twice is two plays, which is
the entire point of counting them.

### 2. A real foreign key, and what it buys

`playlist_entries` and `watch_progress` carry no `FOREIGN KEY` because SQLite
cannot add one to a table that already exists, which is why purging a file
has to delete their rows by hand. This table is new, so it declares one, with
`ON DELETE CASCADE`.

The cascade is what keeps a purged track's plays from outliving it. They are
counted by joining `files`, so an orphaned row would be a play nothing could
name — invisible in every ranking, still swelling the total beside them.
Nothing was added to the purge path for this: the rule the database applies
is one no future purge path can forget to apply.

### 3. The tags are read live, never snapshotted

A ranking joins `audio_files` at read time rather than copying artist, album
and genre onto the play row.

The trade runs both ways and the direction is deliberate. Correcting a
misspelled artist corrects the history too — a snapshot would leave the old
spelling ranking as a second artist forever. The cost is the mirror image:
retagging a track moves its past plays to the new artist. That is the same
"the catalog is the single source of truth" rule every other listing follows,
and the alternative is two artists where the owner sees one.

### 4. Untagged tracks count, but rank in one list only

A track with no tags still counts toward the totals, and still appears among
the top tracks under its filename — the file is a thing that was played.

It appears in none of the other three rankings. An untagged track has no
artist, album or genre, and ranking it under "unknown" would invent an artist
who does not exist and, given enough untagged files, put them at the top of
the owner's chart.

### 5. One credit expression, used twice

An artist is credited by `album_artist` where a track carries one, falling
back to `artist` — the precedence the album-artist design already set, so a
compilation's plays land on whose record it is rather than on each guest in
turn.

That expression is written once, in `plays::repos::CREDIT`, because the
artist ranking groups by it and the album ranking asks whether an album's
tracks agree on it. An album credited one way and an artist another would be
two answers to the same question on one screen.

Albums group by name alone. A compilation whose tracks each name a different
performer is one album; grouping by the pair would scatter it across as many
rows as it has guests. Its `artist` is filled in only when every played track
that names one agrees — `COUNT(DISTINCT …) = 1` skips NULLs, so silence is
not disagreement, and only a genuine conflict answers with none.

### 6. One read, five queries

The summary and the four rankings are one response, not a route per ranking:
they are read together, on one screen, and four round trips could each see a
different instant and disagree with each other.

Underneath, five queries regardless of how much has been played — the same
constant-query-count property `browse_batching.rs` pins for the listing.
Never one query per track, artist or album.

`limit` cuts each ranking (ten by default, a hundred at most) and is refused
rather than clamped when it is out of range: a caller that asked for a
thousand and silently got a hundred would report the top hundred as though it
were the whole answer.

## Components

| Component | Role |
| --- | --- |
| `migrations/…24_play_events.sql` | The table, its cascade, and the two indexes the readings use. |
| `plays::model` | `PlayEvent`, `MusicStats` and the four ranking rows. |
| `plays::repos` | The port, the Sqlite implementation, and `CREDIT`. |
| `plays::commands::record` | Auth, then the clock, then the write. |
| `plays::queries::stats` | Auth, the limit bound, then the read. |
| `routes::plays` | `POST /v1/plays`, `GET /v1/plays/stats`. |
| `alexandria_play_record` / `alexandria_music_stats` | The same two operations over FFI, `PLAY_*` codes. |

## Testing

- Recording: counted, twice counted for a repeat, stamped by the core's
  clock, `NotFound` for an unknown uuid, `InvalidInput` for a file that is
  not audio, `Unauthorized` writing nothing.
- Rankings: order by count; album-artist precedence; a compilation kept
  whole and left uncredited; an album where only some tracks name an artist
  credited to that one; an untagged track in the track list and nowhere
  else; genres; the limit cutting the rankings but not the summary; a limit
  out of range refused.
- The purge cascade, asserted through the rankings *and* against
  `COUNT(*) FROM play_events` — the ranking assertion reads through a JOIN,
  which an orphaned row would slip past.
- HTTP: every status mapping, including 401 before the body is parsed.
- HTTP ↔ FFI parity: the same plays recorded over both transports answer
  identical statistics, with each leg asserted against the expected shape on
  its own first.

## Risks

The rankings are of what the player chose to report. A core that counted
nothing is honest about knowing nothing; a core fed by a player with a
generous threshold will report skipped tracks as listening, and nothing here
can tell the difference. The threshold belongs in one place, stated in the
player's own design, rather than being half-enforced at each end.

Time windows — "this month's top artists" — are deliberately absent. Every
ranking here is all-time. The table carries `played_at` and an index on it,
so a window is a `WHERE` clause away, but the surface for asking is a
parameter on two transports and a control on a screen, and none of that is
worth adding before the all-time answer has been looked at.
