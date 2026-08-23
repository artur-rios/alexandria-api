pub mod audio_tags;
pub mod classify;
pub mod clock;
pub mod comic_tags;
pub mod commands;
pub mod document_tags;
pub mod fs;
pub mod image_tags;
pub mod model;
pub mod queries;
pub mod repos;
pub mod run_registry;
pub mod runs;
pub mod video_tags;

/// Run a blocking best-effort metadata read on Tokio's blocking pool.
///
/// Every real `*MetadataReader` parses the file synchronously — lofty probes,
/// EXIF reads, PDF and ZIP parsing, ffmpeg container probes. Called straight
/// from `async fn read`, each one parks a runtime worker for as long as the
/// parse takes, and the indexer now has several files in flight at once
/// (`IndexHandler`'s `concurrency`), so those stalls would compound into
/// exactly the read-blocking FR-FC-08 forbids during a scan.
///
/// The signature matches what the readers need: extraction is best-effort, so
/// a task that panics is reported the same way a parse failure is — `None`,
/// logged — rather than propagating. A panic in a third-party parser must not
/// take down an indexing run over one malformed file.
pub(crate) async fn read_blocking<T, F>(path: &str, parse: F) -> Option<T>
where
    F: FnOnce(&str) -> Option<T> + Send + 'static,
    T: Send + 'static,
{
    let owned = path.to_string();
    match tokio::task::spawn_blocking(move || parse(&owned)).await {
        Ok(tags) => tags,
        Err(err) => {
            tracing::warn!(path, error = %err, "metadata extraction task failed to run");
            None
        }
    }
}
