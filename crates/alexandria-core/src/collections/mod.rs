//! Collections (F-05 / FR-CO-01..07): flat groupings of files or bookmarks.
//!
//! Laid out like `catalog`: a `model` of the domain types, a `repos` port with
//! its Sqlite implementation, and one Command/Query handler per use case, so
//! the handlers stay unit-testable against trait fakes and both transports
//! call the same decision logic (FR-FC-24 / NFR-09).
pub mod commands;
pub mod model;
pub mod queries;
pub mod repos;
