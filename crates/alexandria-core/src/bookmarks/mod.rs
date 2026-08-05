//! Bookmarks (F-06 / FR-BM-01..06): browser bookmarks, optionally grouped
//! into a `kind = 'bookmark'` collection and sharing the same two-phase
//! soft/hard deletion model as files.
//!
//! Laid out like `catalog` and `collections`: a `model` of the domain types,
//! a `repos` port with its Sqlite implementation, and one Command/Query
//! handler per use case.
pub mod commands;
pub mod model;
pub mod queries;
pub mod repos;
