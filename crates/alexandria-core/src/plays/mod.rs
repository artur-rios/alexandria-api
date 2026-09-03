//! Play history: what was played, and when — the collection mechanism the
//! music statistics are aggregated from (play history design).
//!
//! Laid out like `playlists`: a `model` of the domain types, a `repos` port
//! with its Sqlite implementation, and one Command/Query handler per
//! operation. Two of them, and only two: a play is recorded, and the
//! rankings are read. Nothing edits or deletes one.
pub mod commands;
pub mod model;
pub mod queries;
pub mod repos;
