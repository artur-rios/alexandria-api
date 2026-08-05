//! The content hash is persisted on every File record and compared on each
//! re-index, so its exact encoding is part of the on-disk contract — not an
//! implementation detail. A change in hex formatting would silently make every
//! cataloged file look modified. These vectors pin the encoding against
//! published SHA-256 values rather than against our own output.

use alexandria_core::catalog::fs::{Filesystem, StdFilesystem};

async fn hash_of(bytes: &[u8]) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("subject");
    std::fs::write(&path, bytes).expect("write");
    StdFilesystem
        .content_hash(path.to_str().expect("utf-8 path"))
        .await
        .expect("hash")
}

#[tokio::test]
async fn given_known_vectors_when_hashed_then_matches_published_sha256() {
    assert_eq!(
        hash_of(b"abc").await,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hash_of(b"").await,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hash_of(b"audio bytes").await,
        "ef71589075ccf9332917b0d8d711d1a8d205560f96842f9221de70e6c29454e0"
    );
}

#[tokio::test]
async fn given_any_input_when_hashed_then_is_64_lowercase_hex_chars() {
    let hash = hash_of(b"anything at all").await;
    assert_eq!(hash.len(), 64, "SHA-256 renders as 64 hex characters");
    assert!(
        hash.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "lowercase hex only, got {hash}"
    );
}
