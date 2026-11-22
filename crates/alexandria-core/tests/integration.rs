//! alexandria-core's integration tests, as one test binary.
//!
//! Each area (and a handful of single-file suites) used to be its own
//! top-level `tests/*.rs` target, so `cargo test` compiled the 3,500-line
//! `common/mod.rs` into eight separate crates and linked fifteen executables,
//! every one carrying the full dependency graph (sqlx, ffmpeg, image). As
//! modules of one crate they are compiled and linked once, and their tests
//! share one harness that runs them in parallel rather than one binary after
//! another. The tree under `tests/` still mirrors `src/`.
//!
//! Three suites stay top-level files, each in a process of its own, because
//! they need one:
//!
//! - `config.rs` sets and clears `ALEXANDRIA_*` environment variables, which
//!   are process-wide.
//! - `browse_batching.rs` installs a process-global tracing subscriber that
//!   counts every SQL statement; other tests' queries would be noise in it.
//! - `throughput.rs` is the ignored NFR-01/NFR-02 harness the `performance`
//!   workflow runs on its own (`--test throughput -- --ignored`).
//!
//! Run one area with a filter: `cargo test -p alexandria-core --test integration catalog::`.

mod common;

mod bookmarks;
mod catalog;
mod collections;
mod libraries;
mod migrations;
mod playback;
mod reading_lists;
mod settings;
mod watchlists;
