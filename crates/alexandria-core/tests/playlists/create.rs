//! Integration test for `CreatePlaylistHandler` against a real migrated
//! database (Testing Specification §6.4) — this is the first thing that
//! proves migration 17 applies at all, not just that the domain logic is
//! sound against a fake.

use alexandria_core::playlists::commands::create::CreatePlaylistHandler;
use alexandria_core::playlists::repos::PlaylistRepository;

use crate::common::FakeAuth;
use crate::playlists_fixtures::repo_with_pool;

#[tokio::test]
async fn given_a_valid_name_when_a_playlist_is_created_then_it_is_listed() {
    let (repo, _pool, _dir) = repo_with_pool().await;
    let handler = CreatePlaylistHandler::new(FakeAuth::Allowing, repo.clone());

    let created = handler.create("Road trip", "token").await.expect("created");

    let all = repo.list_all().await.expect("listed");
    assert_eq!(all, vec![created]);
}
