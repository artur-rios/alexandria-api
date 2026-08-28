# Playlists (core) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the core named, ordered, owner-curated lists of audio files, reachable identically over HTTP and FFI.

**Architecture:** A `playlists` module beside `reading_lists`, laid out the same way — `model.rs`, a `repos.rs` port with a Sqlite implementation, one handler per use case under `commands/` and `queries/`. Two tables: `playlists` and `playlist_entries`, the latter carrying a contiguous `position` renumbered on every mutation. Reading a playlist reuses the batched listing pattern from `catalog/queries/browse.rs`.

**Tech Stack:** Rust, sqlx (SQLite), axum (HTTP), cbindgen (FFI), tokio, uuid, serde, tracing.

**Design:** `../../../alexandria-ui/docs/superpowers/specs/2026-08-28-playlists-design.md` (in the sibling repository).

## Global Constraints

- **BR-02:** the core owns domain decisions. Validation, ordering arithmetic and uuid minting happen here, never in a caller.
- **FR-FC-24 / NFR-09:** every operation is reachable over HTTP *and* FFI, and both answer the same payload modulo key ordering. A parity test must be able to fail — asserting two payloads that both omit a field passes in silence.
- **FR-CT-13:** audio is named by metadata, never by file name. Nothing in this plan reads a file name as a title.
- **Test naming:** `given_<state>_when_<action>_then_<outcome>`, snake_case, as every existing core test.
- **Doc comments explain *why*, not *what*.** Match the surrounding density; cite the use case or requirement the code serves.
- **Handlers are generic over their collaborators** (`A: AuthService`, `R: PlaylistRepository`) so decision logic is unit-tested against fakes with no database, per Testing Specification §6.2.
- **Auth is checked before the payload is consulted** (FR-AU-07 / SRD §7) — `self.auth.authenticate(token).await?` is the first statement of every handler.
- **Verification gates, all three, exit code read, on every commit:** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Never pipe these through `tail`/`head` in a way that masks the exit code. Clippy and the test suite each take several minutes — allow up to 600000 ms and wait rather than reporting unverified work.
- **Workflow:** one issue, one branch, one PR. Do not merge, self-approve, or delete branches.

## File Structure

| File | Responsibility |
| --- | --- |
| `crates/alexandria-core/migrations/00000000000017_playlists.sql` | The two tables and their index. |
| `crates/alexandria-core/src/playlists/mod.rs` | Module declaration. |
| `crates/alexandria-core/src/playlists/model.rs` | `NewPlaylist`, `Playlist`, `PlaylistEntry`, `PlaylistView`. |
| `crates/alexandria-core/src/playlists/repos.rs` | `PlaylistRepository` port + `SqlitePlaylistRepository`. |
| `crates/alexandria-core/src/playlists/commands/create.rs` | Name validation + create. |
| `crates/alexandria-core/src/playlists/commands/rename.rs` | Rename. |
| `crates/alexandria-core/src/playlists/commands/delete.rs` | Delete a playlist and its entries. |
| `crates/alexandria-core/src/playlists/commands/add_entries.rs` | Append one or more tracks. |
| `crates/alexandria-core/src/playlists/commands/remove_entry.rs` | Remove one entry, renumber the rest. |
| `crates/alexandria-core/src/playlists/commands/reorder.rs` | Move an entry to an index. |
| `crates/alexandria-core/src/playlists/queries/browse.rs` | List playlists; read one with its tracks. |
| `crates/alexandria-core/src/catalog/repos.rs` | Purge deletes a file's playlist entries. |
| `crates/alexandria-http/src/routes/playlists.rs` | The HTTP surface. |
| `crates/alexandria-ffi/src/lib.rs` | The FFI surface. |

---

### Task 1: Schema, model, and creating a playlist

**Files:**
- Create: `crates/alexandria-core/migrations/00000000000017_playlists.sql`
- Create: `crates/alexandria-core/src/playlists/mod.rs`, `model.rs`, `repos.rs`, `commands/mod.rs`, `commands/create.rs`, `queries/mod.rs`
- Modify: `crates/alexandria-core/src/lib.rs` (declare `pub mod playlists;`), `crates/alexandria-core/src/services.rs`
- Test: `crates/alexandria-core/src/playlists/commands/create.rs` (unit, in-file `mod tests`), `crates/alexandria-core/tests/playlists/mod.rs` + `create.rs` (integration)

**Interfaces:**
- Produces: `NewPlaylist { uuid: Uuid, name: String }`; `Playlist { uuid: Uuid, name: String }` (`#[serde(rename_all = "camelCase")]`); `validate_playlist_name(&str) -> Result<String, DomainError>`; `PlaylistRepository::{insert_playlist, find_by_uuid, list_all}`; `CreatePlaylistHandler::new(auth, repo).create(name: &str, token: &str) -> Result<Playlist, DomainError>`.

- [ ] **Step 1: Write the migration**

Read `migrations/00000000000007_reading_lists.sql` and `00000000000008_reading_progress.sql` first and mirror their comment style — each states *why* the shape is what it is.

```sql
-- Playlists: named, ORDERED groupings of audio files (UC-31's shape, one
-- medium over). Mirrors `reading_lists`, with two deliberate differences
-- spelled out below.
CREATE TABLE IF NOT EXISTS playlists (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    uuid TEXT    NOT NULL UNIQUE,
    name TEXT    NOT NULL
);

-- No FOREIGN KEY, for the same reason `reading_progress` has none: SQLite
-- cannot add one via ALTER TABLE. Foreign keys are enforced in this
-- workspace, so the absence here is exactly why purging a file must delete
-- this table's rows explicitly -- nothing cascades to them.
--
-- And deliberately NO `UNIQUE (playlist_id, file_id)`, which is what
-- `reading_progress` carries. A playlist may hold the same track more than
-- once: a set can legitimately open and close with the same song. An entry's
-- identity is therefore its own `id`, not its file, and removing "that track"
-- means removing that entry.
--
-- `position` is contiguous 0..n-1 within a playlist and is renumbered on
-- every mutation, so the stored position is always the position displayed.
CREATE TABLE IF NOT EXISTS playlist_entries (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    playlist_id INTEGER NOT NULL,
    file_id     INTEGER NOT NULL,
    position    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_playlist_entries_list
    ON playlist_entries (playlist_id, position);
CREATE INDEX IF NOT EXISTS idx_playlist_entries_file
    ON playlist_entries (file_id);
```

The second index exists for the purge in Task 7, which deletes by `file_id`.

- [ ] **Step 2: Write the failing unit tests for name validation**

In `commands/create.rs`. Copy the rule set from `validate_reading_list_name` — empty, blank, untrimmed, >255 bytes, NUL — because a playlist name is refused for the same reasons and a second, subtly different rule set would be the divergence.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_blank_name_when_validated_then_invalid_input() {
        assert!(matches!(
            validate_playlist_name("   "),
            Err(DomainError::InvalidInput(_))
        ));
    }

    #[test]
    fn given_an_untrimmed_name_when_validated_then_invalid_input() {
        assert!(matches!(
            validate_playlist_name(" Road trip "),
            Err(DomainError::InvalidInput(_))
        ));
    }

    #[test]
    fn given_a_name_over_255_bytes_when_validated_then_invalid_input() {
        let long = "a".repeat(256);
        assert!(matches!(
            validate_playlist_name(&long),
            Err(DomainError::InvalidInput(_))
        ));
    }

    #[test]
    fn given_a_name_with_nul_when_validated_then_invalid_input() {
        assert!(matches!(
            validate_playlist_name("road\0trip"),
            Err(DomainError::InvalidInput(_))
        ));
    }

    #[test]
    fn given_a_valid_name_when_validated_then_it_is_returned() {
        assert_eq!(validate_playlist_name("Road trip").unwrap(), "Road trip");
    }
}
```

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test -p alexandria-core playlists::commands::create`
Expected: FAIL — `validate_playlist_name` does not exist.

- [ ] **Step 4: Write the model, the port, the Sqlite implementation and the handler**

`model.rs` mirrors `reading_lists/model.rs`: `NewPlaylist` minted by the handler (so the uuid is decided by the same code on both transports and a fake can assert it), and `Playlist` exposing only the public uuid.

`repos.rs` declares `#[allow(async_fn_in_trait)] pub trait PlaylistRepository: Send + Sync` with `insert_playlist`, `find_by_uuid`, `list_all`, and a `SqlitePlaylistRepository { pool: SqlitePool }` implementing it. Follow `reading_lists/repos.rs` for the sqlx idioms and the `WRITE_TX` constant.

`commands/create.rs` mirrors `CreateReadingListHandler` exactly: authenticate first, validate second, mint the uuid, insert.

- [ ] **Step 5: Run the unit tests to verify they pass**

Run: `cargo test -p alexandria-core playlists::commands::create`
Expected: PASS.

- [ ] **Step 6: Write the integration test against a real migrated database**

Follow `crates/alexandria-core/tests/catalog/runs.rs` for how a test gets a migrated pool (`repo_with_pool` / `migrate_database`), and register the new module in `crates/alexandria-core/tests/`.

```rust
#[tokio::test]
async fn given_a_valid_name_when_a_playlist_is_created_then_it_is_listed() {
    // A real migrated database, not a fake: this is the first thing that
    // proves migration 17 applies at all.
    let (repo, _pool) = repo_with_pool().await;
    let handler = CreatePlaylistHandler::new(always_authenticated(), repo);

    let created = handler.create("Road trip", "token").await.expect("created");

    let all = handler_repo().list_all().await.expect("listed");
    assert_eq!(all, vec![created]);
}
```

- [ ] **Step 7: Run the whole gate**

Run each, read the exit code:
`cargo fmt --all` then `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace`.
Expected: all exit 0.

- [ ] **Step 8: Commit**

```bash
git add crates/alexandria-core/migrations crates/alexandria-core/src/playlists crates/alexandria-core/src/lib.rs crates/alexandria-core/src/services.rs crates/alexandria-core/tests
git commit -m "feat: hold named playlists in the core"
```

---

### Task 2: Rename and delete

**Files:**
- Create: `crates/alexandria-core/src/playlists/commands/rename.rs`, `delete.rs`
- Modify: `crates/alexandria-core/src/playlists/repos.rs`, `commands/mod.rs`, `services.rs`
- Test: in-file `mod tests` in each command; `crates/alexandria-core/tests/playlists/rename.rs`, `delete.rs`

**Interfaces:**
- Consumes: `validate_playlist_name`, `PlaylistRepository::find_by_uuid`.
- Produces: `PlaylistRepository::{rename_playlist(uuid, name) -> Result<Playlist, DomainError>, delete_playlist(uuid) -> Result<(), DomainError>}`; `RenamePlaylistHandler::rename(uuid: Uuid, name: &str, token: &str)`; `DeletePlaylistHandler::delete(uuid: Uuid, token: &str)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn given_a_playlist_when_renamed_then_the_new_name_is_stored() {
    let (repo, _pool) = repo_with_pool().await;
    let created = create_playlist(&repo, "Raod trip").await;

    let renamed = RenamePlaylistHandler::new(always_authenticated(), repo.clone())
        .rename(created.uuid, "Road trip", "token")
        .await
        .expect("renamed");

    assert_eq!(renamed.name, "Road trip");
    assert_eq!(renamed.uuid, created.uuid, "renaming must not mint a new uuid");
}

#[tokio::test]
async fn given_a_blank_new_name_when_renamed_then_invalid_input() {
    let (repo, _pool) = repo_with_pool().await;
    let created = create_playlist(&repo, "Road trip").await;

    let outcome = RenamePlaylistHandler::new(always_authenticated(), repo)
        .rename(created.uuid, "  ", "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::InvalidInput(_))));
}

#[tokio::test]
async fn given_an_unknown_uuid_when_renamed_then_not_found() {
    let (repo, _pool) = repo_with_pool().await;

    let outcome = RenamePlaylistHandler::new(always_authenticated(), repo)
        .rename(Uuid::new_v4(), "Road trip", "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
}

#[tokio::test]
async fn given_a_playlist_with_entries_when_deleted_then_its_entries_go_too() {
    // The entries table has no foreign key, so nothing cascades. Deleting a
    // playlist without deleting its entries leaves rows pointing at a
    // playlist that no longer exists -- invisible, and permanently orphaned.
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let file_id = insert_audio_file(&pool, "song.flac").await;
    repo.add_entries(playlist.uuid, &[file_id]).await.expect("added");

    DeletePlaylistHandler::new(always_authenticated(), repo)
        .delete(playlist.uuid, "token")
        .await
        .expect("deleted");

    let orphans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playlist_entries")
        .fetch_one(&pool)
        .await
        .expect("counted");
    assert_eq!(orphans, 0, "deleting a playlist left its entries behind");
}
```

The last test depends on `add_entries` from Task 3. Write it now and expect it not to compile until Task 3 lands; if executing tasks strictly in order, move that single test to the end of Task 3 rather than weakening it into one that deletes an empty playlist.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p alexandria-core --test playlists`
Expected: FAIL — handlers do not exist.

- [ ] **Step 3: Implement**

`delete_playlist` deletes the entries and the playlist in one transaction (`WRITE_TX`), entries first, so a failure cannot leave a playlist gone and its entries behind.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p alexandria-core --test playlists`
Expected: PASS.

- [ ] **Step 5: Run the whole gate, then commit**

```bash
git add crates/alexandria-core
git commit -m "feat: rename and delete a playlist"
```

---

### Task 3: Adding tracks

**Files:**
- Create: `crates/alexandria-core/src/playlists/commands/add_entries.rs`
- Modify: `crates/alexandria-core/src/playlists/repos.rs`, `model.rs`, `commands/mod.rs`, `services.rs`
- Test: `crates/alexandria-core/tests/playlists/add_entries.rs`

**Interfaces:**
- Produces: `PlaylistEntry { id: i64, file_uuid: Uuid, position: i64 }`; `PlaylistRepository::{add_entries(playlist_uuid: Uuid, file_uuids: &[Uuid]) -> Result<Vec<PlaylistEntry>, DomainError>, list_entries(playlist_uuid: Uuid) -> Result<Vec<PlaylistEntry>, DomainError>}`; `AddEntriesHandler::add(playlist_uuid: Uuid, file_uuids: &[Uuid], token: &str) -> Result<Vec<PlaylistEntry>, DomainError>`.

Taking a slice rather than one uuid is what lets "add this whole album" be one call and one transaction, rather than N calls whose failure halfway leaves half an album added.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn given_an_empty_playlist_when_tracks_are_added_then_they_take_positions_from_zero() {
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let first = insert_audio_file(&pool, "a.flac").await;
    let second = insert_audio_file(&pool, "b.flac").await;

    let added = AddEntriesHandler::new(always_authenticated(), repo)
        .add(playlist.uuid, &[first, second], "token")
        .await
        .expect("added");

    assert_eq!(added.iter().map(|e| e.position).collect::<Vec<_>>(), vec![0, 1]);
}

#[tokio::test]
async fn given_a_playlist_holding_a_track_when_the_same_track_is_added_then_it_is_held_twice() {
    // The whole reason `playlist_entries` has no UNIQUE (playlist_id,
    // file_id): a set can open and close with the same song. This test fails
    // the moment someone copies that constraint over from reading lists.
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let song = insert_audio_file(&pool, "a.flac").await;
    let handler = AddEntriesHandler::new(always_authenticated(), repo.clone());

    handler.add(playlist.uuid, &[song], "token").await.expect("first");
    handler.add(playlist.uuid, &[song], "token").await.expect("second");

    let entries = repo.list_entries(playlist.uuid).await.expect("listed");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].file_uuid, entries[1].file_uuid);
    assert_ne!(entries[0].id, entries[1].id, "each entry is its own row");
    assert_eq!(entries.iter().map(|e| e.position).collect::<Vec<_>>(), vec![0, 1]);
}

#[tokio::test]
async fn given_a_non_audio_file_when_added_then_invalid_input() {
    // A playlist holds audio (design "What a playlist is here"). Video has
    // watchlists and books have reading lists.
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let note = insert_text_file(&pool, "note.md").await;

    let outcome = AddEntriesHandler::new(always_authenticated(), repo)
        .add(playlist.uuid, &[note], "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::InvalidInput(_))));
}

#[tokio::test]
async fn given_an_unknown_file_when_added_then_not_found_and_nothing_is_added() {
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let real = insert_audio_file(&pool, "a.flac").await;

    let outcome = AddEntriesHandler::new(always_authenticated(), repo.clone())
        .add(playlist.uuid, &[real, Uuid::new_v4()], "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
    assert!(
        repo.list_entries(playlist.uuid).await.expect("listed").is_empty(),
        "a partial add left the real track behind"
    );
}
```

That last assertion is the one worth being deliberate about: the whole slice succeeds or none of it does, in one transaction.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p alexandria-core --test playlists`
Expected: FAIL.

- [ ] **Step 3: Implement**

`add_entries` resolves each uuid to a `files.id`, checks each is `FileType::Audio`, reads `MAX(position)` for the playlist, and inserts at consecutive positions — all inside one `WRITE_TX` transaction.

- [ ] **Step 4: Run to verify they pass, then add the Task 2 deletion test**

If the "deleting a playlist takes its entries" test was deferred from Task 2, add it now and confirm it passes.

- [ ] **Step 5: Run the whole gate, then commit**

```bash
git add crates/alexandria-core
git commit -m "feat: add tracks to a playlist"
```

---

### Task 4: Removing an entry

**Files:**
- Create: `crates/alexandria-core/src/playlists/commands/remove_entry.rs`
- Modify: `crates/alexandria-core/src/playlists/repos.rs`, `commands/mod.rs`, `services.rs`
- Test: `crates/alexandria-core/tests/playlists/remove_entry.rs`

**Interfaces:**
- Produces: `PlaylistRepository::remove_entry(playlist_uuid: Uuid, entry_id: i64) -> Result<(), DomainError>`; `RemoveEntryHandler::remove(playlist_uuid: Uuid, entry_id: i64, token: &str)`.

Addressed by `entry_id`, not by file uuid — with duplicates allowed, a file uuid does not identify a row.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn given_a_track_held_twice_when_one_entry_is_removed_then_the_other_stays() {
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let song = insert_audio_file(&pool, "a.flac").await;
    let added = repo.add_entries(playlist.uuid, &[song, song]).await.expect("added");

    RemoveEntryHandler::new(always_authenticated(), repo.clone())
        .remove(playlist.uuid, added[0].id, "token")
        .await
        .expect("removed");

    let left = repo.list_entries(playlist.uuid).await.expect("listed");
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, added[1].id, "the wrong entry was removed");
}

#[tokio::test]
async fn given_a_middle_entry_when_it_is_removed_then_positions_stay_contiguous() {
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let a = insert_audio_file(&pool, "a.flac").await;
    let b = insert_audio_file(&pool, "b.flac").await;
    let c = insert_audio_file(&pool, "c.flac").await;
    let added = repo.add_entries(playlist.uuid, &[a, b, c]).await.expect("added");

    RemoveEntryHandler::new(always_authenticated(), repo.clone())
        .remove(playlist.uuid, added[1].id, "token")
        .await
        .expect("removed");

    let left = repo.list_entries(playlist.uuid).await.expect("listed");
    assert_eq!(left.iter().map(|e| e.position).collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(left.iter().map(|e| e.file_uuid).collect::<Vec<_>>(), vec![a, c]);
}

#[tokio::test]
async fn given_an_entry_of_another_playlist_when_removed_then_not_found() {
    // The entry id is global; without the playlist check, one playlist could
    // delete another's row.
    let (repo, pool) = repo_with_pool().await;
    let mine = create_playlist(&repo, "Mine").await;
    let theirs = create_playlist(&repo, "Theirs").await;
    let song = insert_audio_file(&pool, "a.flac").await;
    let added = repo.add_entries(theirs.uuid, &[song]).await.expect("added");

    let outcome = RemoveEntryHandler::new(always_authenticated(), repo.clone())
        .remove(mine.uuid, added[0].id, "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
    assert_eq!(repo.list_entries(theirs.uuid).await.expect("listed").len(), 1);
}
```

- [ ] **Step 2: Run to verify they fail; Step 3: implement; Step 4: run to verify they pass**

Delete and renumber in one transaction.

- [ ] **Step 5: Run the whole gate, then commit**

```bash
git add crates/alexandria-core
git commit -m "feat: remove one entry from a playlist"
```

---

### Task 5: Reordering

**Files:**
- Create: `crates/alexandria-core/src/playlists/commands/reorder.rs`
- Modify: `crates/alexandria-core/src/playlists/repos.rs`, `commands/mod.rs`, `services.rs`
- Test: `crates/alexandria-core/tests/playlists/reorder.rs`

**Interfaces:**
- Produces: `PlaylistRepository::move_entry(playlist_uuid: Uuid, entry_id: i64, to_index: i64) -> Result<Vec<PlaylistEntry>, DomainError>`; `ReorderPlaylistHandler::move_entry(playlist_uuid: Uuid, entry_id: i64, to_index: i64, token: &str) -> Result<Vec<PlaylistEntry>, DomainError>`.

The contract is deliberately "put entry X at index N", computed and renumbered here in one transaction — **not** a list of positions the caller believes are correct. A caller sending its own arithmetic would be a second implementation of the ordering rule, and the two would drift (design, Risks).

Returning the full new order lets a caller replace what it is showing with what the core actually did, rather than predicting it.

- [ ] **Step 1: Write the failing tests**

```rust
async fn ordered_names(repo: &SqlitePlaylistRepository, playlist: Uuid) -> Vec<Uuid> {
    repo.list_entries(playlist)
        .await
        .expect("listed")
        .into_iter()
        .map(|e| e.file_uuid)
        .collect()
}

#[tokio::test]
async fn given_four_tracks_when_the_last_moves_to_the_front_then_the_rest_shift_down() {
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let (a, b, c, d) = four_audio_files(&pool).await;
    let added = repo.add_entries(playlist.uuid, &[a, b, c, d]).await.expect("added");

    let after = ReorderPlaylistHandler::new(always_authenticated(), repo.clone())
        .move_entry(playlist.uuid, added[3].id, 0, "token")
        .await
        .expect("moved");

    assert_eq!(ordered_names(&repo, playlist.uuid).await, vec![d, a, b, c]);
    assert_eq!(after.iter().map(|e| e.position).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
}

#[tokio::test]
async fn given_four_tracks_when_the_first_moves_to_the_end_then_the_rest_shift_up() {
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let (a, b, c, d) = four_audio_files(&pool).await;
    let added = repo.add_entries(playlist.uuid, &[a, b, c, d]).await.expect("added");

    ReorderPlaylistHandler::new(always_authenticated(), repo.clone())
        .move_entry(playlist.uuid, added[0].id, 3, "token")
        .await
        .expect("moved");

    assert_eq!(ordered_names(&repo, playlist.uuid).await, vec![b, c, d, a]);
}

#[tokio::test]
async fn given_an_entry_when_moved_to_where_it_already_is_then_nothing_changes() {
    // A drag that lands on the row it started from. Cheap to get wrong: an
    // implementation that removes then re-inserts can land it off by one.
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let (a, b, c, d) = four_audio_files(&pool).await;
    let added = repo.add_entries(playlist.uuid, &[a, b, c, d]).await.expect("added");

    ReorderPlaylistHandler::new(always_authenticated(), repo.clone())
        .move_entry(playlist.uuid, added[2].id, 2, "token")
        .await
        .expect("moved");

    assert_eq!(ordered_names(&repo, playlist.uuid).await, vec![a, b, c, d]);
}

#[tokio::test]
async fn given_an_index_past_the_end_when_moved_then_invalid_input() {
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let (a, b, c, d) = four_audio_files(&pool).await;
    let added = repo.add_entries(playlist.uuid, &[a, b, c, d]).await.expect("added");

    let outcome = ReorderPlaylistHandler::new(always_authenticated(), repo.clone())
        .move_entry(playlist.uuid, added[0].id, 4, "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::InvalidInput(_))));
    assert_eq!(ordered_names(&repo, playlist.uuid).await, vec![a, b, c, d]);
}

#[tokio::test]
async fn given_a_negative_index_when_moved_then_invalid_input() {
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let (a, b, c, d) = four_audio_files(&pool).await;
    let added = repo.add_entries(playlist.uuid, &[a, b, c, d]).await.expect("added");

    let outcome = ReorderPlaylistHandler::new(always_authenticated(), repo.clone())
        .move_entry(playlist.uuid, added[0].id, -1, "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::InvalidInput(_))));
    assert_eq!(ordered_names(&repo, playlist.uuid).await, vec![a, b, c, d]);
}
```

- [ ] **Step 2: Run to verify they fail; Step 3: implement; Step 4: run to verify they pass**

Read the entries in order, move the element in memory, write every position back — one transaction. Simple and obviously correct beats clever arithmetic here.

- [ ] **Step 5: Run the whole gate, then commit**

```bash
git add crates/alexandria-core
git commit -m "feat: reorder a playlist"
```

---

### Task 6: Reading a playlist with its tracks

**Files:**
- Create: `crates/alexandria-core/src/playlists/queries/browse.rs`
- Create: `crates/alexandria-core/tests/playlist_batching.rs`
- Modify: `crates/alexandria-core/src/playlists/queries/mod.rs`, `services.rs`
- Test: `crates/alexandria-core/tests/playlists/browse.rs`

**Interfaces:**
- Produces: `PlaylistView { playlist: Playlist, entries: Vec<PlaylistTrack> }` and `PlaylistTrack { entry_id: i64, position: i64, file: FileView, missing: bool }`; `BrowsePlaylistsHandler::{list(token) -> Result<Vec<Playlist>, DomainError>, read(uuid, token) -> Result<PlaylistView, DomainError>}`.

`FileView` is the shape every other listing answers (`catalog/queries/browse.rs`), so the application parses a playlist with what it already has.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn given_a_playlist_when_read_then_its_tracks_come_back_in_position_order() {
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let (a, b, c, d) = four_audio_files(&pool).await;
    let added = repo.add_entries(playlist.uuid, &[a, b, c, d]).await.expect("added");
    repo.move_entry(playlist.uuid, added[3].id, 0).await.expect("moved");

    let view = BrowsePlaylistsHandler::new(always_authenticated(), repo)
        .read(playlist.uuid, "token")
        .await
        .expect("read");

    assert_eq!(
        view.entries.iter().map(|t| t.file.file.uuid).collect::<Vec<_>>(),
        vec![d, a, b, c],
        "a playlist must read back in the order it was arranged in"
    );
}

#[tokio::test]
async fn given_an_entry_whose_file_is_missing_when_read_then_it_is_present_and_flagged() {
    // Design section 5: a missing file stays in the list and is passed over.
    // Dropping it here would delete curation work invisibly, and would make an
    // unplugged drive look like an empty playlist.
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist(&repo, "Road trip").await;
    let song = insert_audio_file(&pool, "a.flac").await;
    repo.add_entries(playlist.uuid, &[song]).await.expect("added");
    mark_file_missing(&pool, song).await;

    let view = BrowsePlaylistsHandler::new(always_authenticated(), repo)
        .read(playlist.uuid, "token")
        .await
        .expect("read");

    assert_eq!(view.entries.len(), 1, "the entry was dropped rather than flagged");
    assert!(view.entries[0].missing);
}

#[tokio::test]
async fn given_an_unknown_uuid_when_read_then_not_found() {
    let (repo, _pool) = repo_with_pool().await;

    let outcome = BrowsePlaylistsHandler::new(always_authenticated(), repo)
        .read(Uuid::new_v4(), "token")
        .await;

    assert!(matches!(outcome, Err(DomainError::NotFound)));
}
```

- [ ] **Step 2: Write the query-count test**

Model it on `crates/alexandria-core/tests/browse_batching.rs`, which counts queries with a tracing subscriber. The point is that reading a playlist is a constant number of queries, not one per track — the defect this pins is invisible to every other test, because a per-track query returns exactly the right answer, only slowly.

```rust
#[tokio::test]
async fn given_a_large_playlist_when_read_then_the_query_count_does_not_grow_with_it() {
    let small = query_count_reading_playlist_of(5).await;
    let large = query_count_reading_playlist_of(200).await;

    assert_eq!(
        small, large,
        "reading a playlist issues a query per track: {small} for 5, {large} for 200"
    );
}
```

- [ ] **Step 3: Run to verify they fail; Step 4: implement; Step 5: run to verify they pass**

Read the entries in position order, then resolve their files with the batched pattern from `catalog/queries/browse.rs` — one query per subtype table, ids chunked at `MAX_SQLITE_PARAMS`. A track appearing twice must resolve once and be attached to both entries.

- [ ] **Step 6: Run the whole gate, then commit**

```bash
git add crates/alexandria-core
git commit -m "feat: read a playlist and its tracks"
```

---

### Task 7: Purging a file takes its playlist entries

**Files:**
- Modify: `crates/alexandria-core/src/catalog/repos.rs` (the purge transaction, beside the existing `watch_progress` and `reading_progress` deletes — around line 1667)
- Test: `crates/alexandria-core/tests/catalog/purge.rs`

**Interfaces:**
- Consumes: the `playlist_entries` table from Task 1.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn given_a_track_on_a_playlist_when_the_file_is_purged_then_its_entries_go_too() {
    // `playlist_entries` declares no foreign key -- SQLite cannot add one via
    // ALTER TABLE -- so nothing cascades. Without an explicit DELETE a purged
    // track leaves an entry pointing at a `files.id` that no longer exists:
    // invisible to the playlist query, which inner-joins `files`, and
    // permanently orphaned.
    let (repo, pool) = repo_with_pool().await;
    let playlist = create_playlist_row(&pool, "Road trip").await;
    let song = insert_audio_file(&pool, "a.flac").await;
    add_entry_row(&pool, playlist, song).await;

    repo.purge(song).await.expect("purged");

    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM playlist_entries")
        .fetch_one(&pool)
        .await
        .expect("counted");
    assert_eq!(left, 0, "a purged file left a playlist entry pointing at nothing");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p alexandria-core --test catalog purge`
Expected: FAIL — the count is 1.

- [ ] **Step 3: Add the delete to the purge transaction**

Directly after the `reading_progress` delete, inside the same transaction, and extend the comment above it to name the third table.

```rust
sqlx::query("DELETE FROM playlist_entries WHERE file_id = ?")
    .bind(id)
    .execute(&mut *tx)
    .await?;
```

- [ ] **Step 4: Run to verify it passes; Step 5: run the whole gate, then commit**

```bash
git add crates/alexandria-core
git commit -m "fix: purge a file's playlist entries with the file"
```

---

### Task 8: The HTTP surface

**Files:**
- Create: `crates/alexandria-http/src/routes/playlists.rs`
- Modify: `crates/alexandria-http/src/routes/mod.rs`, `crates/alexandria-http/src/lib.rs`
- Test: `crates/alexandria-http/tests/playlists_api.rs`

**Interfaces:**
- Consumes: every handler from Tasks 1-6.
- Produces: `POST /v1/playlists` `{"name"}`; `PATCH /v1/playlists/{uuid}` `{"name"}`; `DELETE /v1/playlists/{uuid}`; `GET /v1/playlists`; `GET /v1/playlists/{uuid}`; `POST /v1/playlists/{uuid}/entries` `{"fileUuids":[…]}`; `DELETE /v1/playlists/{uuid}/entries/{entryId}`; `POST /v1/playlists/{uuid}/entries/{entryId}/move` `{"toIndex":N}`.

Read `crates/alexandria-http/src/routes/reading_lists.rs` and follow it exactly — the router shape, the `bearer_token` extraction, and the `DomainError` to status mapping.

- [ ] **Step 1: Write the failing tests**

One per route, plus the mappings that are easy to get wrong:

```rust
#[tokio::test]
async fn given_no_bearer_when_a_playlist_is_created_then_401() { /* … */ }

#[tokio::test]
async fn given_a_blank_name_when_a_playlist_is_created_then_400() { /* … */ }

#[tokio::test]
async fn given_an_unknown_playlist_when_read_then_404() { /* … */ }

#[tokio::test]
async fn given_an_index_past_the_end_when_an_entry_is_moved_then_400() { /* … */ }

#[tokio::test]
async fn given_a_playlist_when_read_then_the_body_carries_its_tracks_in_order() { /* … */ }
```

Fill each body following `reading_lists_api.rs`; do not leave the `/* … */` in the committed test.

- [ ] **Step 2: Run to verify they fail; Step 3: implement; Step 4: run to verify they pass**

- [ ] **Step 5: Run the whole gate, then commit**

```bash
git add crates/alexandria-http
git commit -m "feat: expose playlists over HTTP"
```

---

### Task 9: The FFI surface and parity

**Files:**
- Modify: `crates/alexandria-ffi/src/lib.rs`
- Test: `crates/alexandria-ffi/tests/smoke.rs`, `crates/alexandria-ffi/tests/parity.rs`

**Interfaces:**
- Produces: `alexandria_playlist_create`, `_rename`, `_delete`, `alexandria_playlists_list`, `alexandria_playlist_read`, `alexandria_playlist_add_entries`, `alexandria_playlist_remove_entry`, `alexandria_playlist_move_entry`, and a `PLAYLIST_ERR_*` family (`INVALID_INPUT 1`, `UNAUTHORIZED 2`, `NOT_INITIALIZED 3`, `NOT_FOUND 4`, `INVALID_STATE 5`, `OTHER 9`) mirroring `READING_LIST_ERR_*`.

Follow `alexandria_reading_list_create` (around line 2917) exactly, including the order: services slot, then `authenticated()`, then the body. An unauthenticated caller must not learn whether its body parsed.

- [ ] **Step 1: Write the failing parity test**

```rust
#[tokio::test]
async fn given_a_playlist_when_read_via_http_and_ffi_then_both_answer_the_same_order() {
    // The parity test that could pass in silence is one where both sides
    // answer an empty playlist. Each leg is asserted to hold the arranged
    // order on its own BEFORE the two are compared.
    let expected = vec!["d.flac", "a.flac", "b.flac", "c.flac"];

    let over_http = read_playlist_over_http(&fixture).await;
    let over_ffi = read_playlist_over_ffi(&fixture);

    assert_eq!(track_names(&over_http), expected, "the HTTP leg");
    assert_eq!(track_names(&over_ffi), expected, "the FFI leg");
    assert_eq!(over_http, over_ffi);
}
```

- [ ] **Step 2: Write the failing smoke tests**

Cover, at minimum: creating over FFI returns a uuid; an unknown uuid answers `PLAYLIST_ERR_NOT_FOUND`; a blank name answers `PLAYLIST_ERR_INVALID_INPUT`; a bad token *and* a malformed body together answer `PLAYLIST_ERR_UNAUTHORIZED`, not `INVALID_INPUT` — the last one is what pins the auth ordering, and every other unauthorized test passes without it.

- [ ] **Step 3: Run to verify they fail; Step 4: implement; Step 5: run to verify they pass**

- [ ] **Step 6: Confirm the generated header carries every new function**

Run: `cargo build -p alexandria-ffi --release`
Then: `grep -c "alexandria_playlist" crates/alexandria-ffi/src/header.h`
Expected: 8. The header is generated by the build and is what the sibling repository vendors; a function missing here is invisible until the UI cannot find the symbol at runtime.

- [ ] **Step 7: Run the whole gate, then commit**

```bash
git add crates/alexandria-ffi
git commit -m "feat: expose playlists over the FFI"
```

---

### Task 10: Requirement documents

**Files:**
- Modify: `docs/requirements/Use Case Specification Document.md`, `docs/requirements/System Requirements Document.md`, `docs/System Behavior Document.md`

- [ ] **Step 1: Add the use case**

A new UC beside UC-31, in the established table format: actors, description, preconditions, postconditions, requirements, main flow, and alternative flows for a blank name (refused before the core is called), an unknown playlist or entry (not found), a non-audio file (refused), and an unauthorized call.

- [ ] **Step 2: Add the functional requirements**

Beside FR-TR-08..FR-TR-11: what the core holds, that a playlist may hold a track more than once, that positions are contiguous, that a purge takes the entries, and that a missing file's entry is kept and flagged.

- [ ] **Step 3: Add the data dictionary rows**

Both new tables in the SRD's data dictionary, and the routes in its API table — the same two places the index-scope change had to be corrected for missing.

- [ ] **Step 4: Commit**

```bash
git add docs
git commit -m "docs: specify playlists"
```

---

## Self-Review

**Spec coverage.** Design §1 → Tasks 1-6, 8, 9. §2 (two tables, no UNIQUE, no FK) → Task 1, with the duplicate rule pinned in Task 3 and the cascade obligation discharged in Task 7. §3 (contiguous positions) → Tasks 4 and 5. §4 (batched read) → Task 6, including the query-count test. §5 (missing stays and is flagged) → Task 6; the *skipping* half is UI behaviour and belongs to the UI plan. §6 (playback) → entirely UI. §7 (rename) → Task 2. Requirements impact → Task 10.

**Placeholders.** The HTTP test bodies in Task 8 are the one place with elided bodies; the step says explicitly to fill them from `reading_lists_api.rs` and not to commit the ellipsis. Everything else carries real content.

**Type consistency.** `PlaylistEntry { id, file_uuid, position }` is produced in Task 3 and used unchanged in 4, 5 and 6. `move_entry` has the same name and signature on the repository (Task 5) and on the handler. `PlaylistTrack.file` is a `FileView`, the same type `catalog/queries/browse.rs` answers. The `PLAYLIST_ERR_*` numbering matches `READING_LIST_ERR_*` value for value.

**Known cross-task dependency:** the "deleting a playlist takes its entries" test in Task 2 needs `add_entries` from Task 3. Task 2 Step 1 says so and gives the instruction — move it to the end of Task 3 rather than weakening it.
