//! Playlists: named, ORDERED groupings of audio files — audio's counterpart
//! to `watchlists` (video) and `reading_lists` (books/comics).
//!
//! Laid out like `reading_lists`: a `model` of the domain types, a `repos`
//! port with its Sqlite implementation, and one Command/Query handler per
//! use case.
pub mod commands;
pub mod model;
pub mod queries;
pub mod repos;
