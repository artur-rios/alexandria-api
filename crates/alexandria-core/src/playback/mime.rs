//! Extension → MIME resolution for the playback surface (FR-MP-01).
//!
//! One table, shared by UC-38's stream, UC-39's comic page, and UC-40's
//! thumbnail. It covers the formats `catalog::classify` already recognizes
//! for each of the seven file types. An extension absent from the table
//! yields `application/octet-stream` rather than an error: the bytes are
//! still perfectly streamable, and refusing to serve a file the catalog
//! happily indexed would be inconsistent.

/// MIME type for `path`, by extension, matched case-insensitively.
pub fn mime_for_path(path: &str) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        // audio
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wma" => "audio/x-ms-wma",
        // video
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "mpg" | "mpeg" => "video/mpeg",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        // html / text
        "html" | "htm" => "text/html",
        "mhtml" => "multipart/related",
        "md" | "markdown" => "text/markdown",
        "txt" => "text/plain",
        // documents
        "pdf" => "application/pdf",
        "epub" => "application/epub+zip",
        "mobi" => "application/x-mobipocket-ebook",
        "azw" | "azw3" => "application/vnd.amazon.ebook",
        // comics
        "cbz" => "application/vnd.comicbook+zip",
        "cbr" => "application/vnd.comicbook-rar",
        // images
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_known_extensions_when_resolved_then_expected_mime_returned() {
        // Arrange — one representative extension per cataloged file type.
        let cases = [
            ("/lib/song.mp3", "audio/mpeg"),
            ("/lib/song.flac", "audio/flac"),
            ("/lib/song.wma", "audio/x-ms-wma"),
            ("/lib/movie.mp4", "video/mp4"),
            ("/lib/movie.mkv", "video/x-matroska"),
            ("/lib/movie.mpg", "video/mpeg"),
            ("/lib/movie.mpeg", "video/mpeg"),
            ("/lib/movie.wmv", "video/x-ms-wmv"),
            ("/lib/movie.flv", "video/x-flv"),
            ("/lib/page.html", "text/html"),
            ("/lib/page.mhtml", "multipart/related"),
            ("/lib/notes.md", "text/markdown"),
            ("/lib/notes.txt", "text/plain"),
            ("/lib/book.pdf", "application/pdf"),
            ("/lib/book.epub", "application/epub+zip"),
            ("/lib/issue.cbz", "application/vnd.comicbook+zip"),
            ("/lib/photo.jpg", "image/jpeg"),
            ("/lib/photo.png", "image/png"),
            ("/lib/logo.svg", "image/svg+xml"),
        ];

        // Act / Assert
        for (path, expected) in cases {
            assert_eq!(mime_for_path(path), expected, "path {path}");
        }
    }

    #[test]
    fn given_unknown_or_absent_extension_when_resolved_then_octet_stream() {
        // Arrange / Act / Assert — an extension the catalog never indexes,
        // and a path with no extension at all. Neither is an error: the
        // bytes are still streamable.
        assert_eq!(mime_for_path("/lib/thing.xyz"), "application/octet-stream");
        assert_eq!(mime_for_path("/lib/README"), "application/octet-stream");
    }

    #[test]
    fn given_uppercase_extension_when_resolved_then_matched_case_insensitively() {
        // Arrange / Act / Assert
        assert_eq!(mime_for_path("/lib/PHOTO.JPG"), "image/jpeg");
    }
}
