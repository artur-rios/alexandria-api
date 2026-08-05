//! Reading lists (F-08 / FR-RL-01..08): named groupings for tracking book
//! and comic-book consumption, per issue for comic series.
//!
//! Laid out like `watchlists`: a `model` of the domain types, a `repos`
//! port with its Sqlite implementation, and one Command/Query handler per
//! use case.
pub mod commands;
pub mod model;
pub mod repos;
