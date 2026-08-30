//! Libraries: registered folders browsed as a tree, whose files are shown
//! only there (libraries design).
//!
//! The catalog sorts by type, which is right for a music collection and
//! wrong for material that only means anything together — a course scatters
//! each class's recording, handout and slides across four panels, in none of
//! which they are near each other.
//!
//! Laid out like `playlists`: a `model` of the domain types, a `repos` port
//! with its Sqlite implementation, and one handler per use case.
pub mod commands;
pub mod model;
pub mod queries;
pub mod repos;
