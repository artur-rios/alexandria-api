use std::path::Path;

use crate::catalog::model::FileType;

/// Classify a file by its extension. Pure: the catalog mapping is decided by the
/// extension alone. Supported extensions are listed in the table below; any
/// other extension (or none) is skipped by the indexer (returns `None`).
///
/// | type     | extensions |
/// | ---     | --- |
/// | audio   | mp3, flac, wav, ogg, oga, m4a, aac, opus, wma |
/// | video   | mp4, m4v, mkv, avi, mov, webm, mpg, mpeg, wmv, flv |
/// | html    | html, htm, mhtml |
/// | text    | md, markdown, txt |
/// | document| pdf, epub, mobi, azw, azw3 |
/// | comic   | cbr, cbz |
/// | image   | jpg, jpeg, png, gif, webp, bmp, tif, tiff, svg |
pub fn classify_by_extension(name: &str) -> Option<FileType> {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())?;

    Some(match ext.as_str() {
        "mp3" | "flac" | "wav" | "ogg" | "oga" | "m4a" | "aac" | "opus" | "wma" => {
            FileType::Audio
        }
        "mp4" | "m4v" | "mkv" | "avi" | "mov" | "webm" | "mpg" | "mpeg" | "wmv" | "flv" => {
            FileType::Video
        }
        "html" | "htm" | "mhtml" => FileType::Html,
        "md" | "markdown" | "txt" => FileType::Text,
        "pdf" | "epub" | "mobi" | "azw" | "azw3" => FileType::Document,
        "cbr" | "cbz" => FileType::Comic,
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "svg" => {
            FileType::Image
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_audio_extension_when_classify_then_audio_case_insensitive() {
        assert_eq!(classify_by_extension("song.mp3"), Some(FileType::Audio));
        assert_eq!(classify_by_extension("song.FLAC"), Some(FileType::Audio));
        assert_eq!(classify_by_extension("track.WAV"), Some(FileType::Audio));
    }

    #[test]
    fn given_all_supported_types_when_classify_then_mapped() {
        assert_eq!(classify_by_extension("a.mp3"), Some(FileType::Audio));
        assert_eq!(classify_by_extension("a.mp4"), Some(FileType::Video));
        assert_eq!(classify_by_extension("a.html"), Some(FileType::Html));
        assert_eq!(classify_by_extension("a.md"), Some(FileType::Text));
        assert_eq!(classify_by_extension("a.pdf"), Some(FileType::Document));
        assert_eq!(classify_by_extension("a.cbr"), Some(FileType::Comic));
        assert_eq!(classify_by_extension("a.png"), Some(FileType::Image));
    }

    #[test]
    fn given_pdf_cbr_mapping_when_classify_then_pdf_is_document_and_cbr_is_comic() {
        assert_eq!(classify_by_extension("book.pdf"), Some(FileType::Document));
        assert_eq!(classify_by_extension("comic.cbr"), Some(FileType::Comic));
    }

    #[test]
    fn given_unsupported_extension_when_classify_then_none() {
        assert_eq!(classify_by_extension("archive.zip"), None);
        assert_eq!(classify_by_extension("noext"), None);
        assert_eq!(classify_by_extension(""), None);
        assert_eq!(classify_by_extension("README"), None);
    }
}