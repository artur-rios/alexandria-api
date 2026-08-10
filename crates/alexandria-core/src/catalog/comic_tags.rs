/// Tags read from a comic archive's embedded metadata (`ComicInfo.xml`,
/// the de-facto ComicRack/ComicVine standard, plus an archive-entry image
/// count — issue #44 comic slice). `page_count` is always computed by
/// counting image entries in the archive, independent of whether
/// `ComicInfo.xml` exists at all; `title`/`series`/`issue_number` come
/// only from `ComicInfo.xml` and are `None` when it's absent or
/// unparseable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComicTags {
    pub title: Option<String>,
    pub series: Option<String>,
    pub issue_number: Option<i64>,
    pub page_count: Option<i64>,
}

#[allow(async_fn_in_trait)]
pub trait ComicMetadataReader: Send + Sync {
    /// Best-effort read of embedded comic metadata. `None` covers only
    /// "couldn't open the archive at all" — a readable archive with no
    /// `ComicInfo.xml` still yields `Some` with `page_count` set and the
    /// other three fields `None`.
    async fn read(&self, path: &str) -> Option<ComicTags>;
}
