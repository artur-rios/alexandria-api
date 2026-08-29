//! Music enrichment: artist photography and lyrics fetched from public
//! services (music enrichment design).
//!
//! The only part of this workspace that reaches the network outbound, and
//! the reason the Vision Document's "no network calls" now reads "no network
//! calls except this, off by default". Everything here is shaped to keep
//! that exception narrow: three named services, music-only queries carrying
//! nothing about the owner, off unless switched on, and cached so a lookup
//! happens once per artist and once per recording rather than once per play.
//!
//! Laid out like `playlists`: a `model` of the domain types, a `repos` port
//! with its Sqlite implementation, `providers` for the outside world, and
//! one handler per use case under `commands` and `queries`.
pub mod commands;
pub mod model;
pub mod providers;
pub mod queries;
pub mod repos;
