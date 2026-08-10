//! Media playback (F-10 — UC-38, UC-39, UC-40).
//!
//! Alexandria never modifies or re-encodes the bytes it serves (FR-MP-03).
//! This module resolves a catalog record to on-disk bytes and, for two
//! types, to a bounded derived artifact — a comic page or a thumbnail.

pub mod mime;
