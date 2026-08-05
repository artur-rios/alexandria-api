//! Watchlists (F-07 / FR-WL-01..08): named groupings for tracking video
//! consumption, per episode for series.
//!
//! Laid out like `collections` and `bookmarks`: a `model` of the domain
//! types, a `repos` port with its Sqlite implementation, and one
//! Command/Query handler per use case.
pub mod commands;
pub mod model;
pub mod queries;
pub mod repos;
