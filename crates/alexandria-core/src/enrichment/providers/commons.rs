//! Artist photography, by way of Wikidata and Wikimedia Commons.
//!
//! MusicBrainz stores no images, so this is the route to one that needs no
//! API key: Wikidata records a MusicBrainz artist id (property P434) against
//! its own entity, and that entity's P18 names a file on Commons. Two
//! lookups plus a download, none of them against MusicBrainz — so none of
//! them spends the one-per-second budget the identity lookup needs.
//!
//! Everything Commons serves is licensed, and most of it requires
//! attribution. `source_url` is stored for exactly that reason: an image
//! whose provenance has been lost cannot be credited, and an uncredited
//! image cannot lawfully be shown.

use reqwest::Client;
use serde::Deserialize;

use super::{user_agent, ArtistImageAsset, ArtistImageProvider, ProviderError};

const WIKIDATA_API: &str = "https://www.wikidata.org/w/api.php";
const COMMONS_FILE_PATH: &str = "https://commons.wikimedia.org/wiki/Special:FilePath";

/// The largest image this will download, in bytes.
///
/// Commons holds originals that run to tens of megabytes, and an artist
/// portrait rendered beside a track listing needs none of that. The cap is
/// on what is *accepted* rather than requested, because the request has no
/// way to state a size — so a file above it is skipped rather than pulled
/// down and then discarded.
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// The width requested from Commons, in pixels.
///
/// Ample for a portrait shown beside a track listing or on a now-playing
/// screen, and small enough that the byte cap above is a guard against the
/// unexpected rather than something a normal image trips.
const MAX_IMAGE_WIDTH: u32 = 1000;

#[derive(Debug, Deserialize)]
struct SearchEnvelope {
    #[serde(default)]
    query: Option<SearchQuery>,
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    #[serde(default)]
    search: Vec<SearchHit>,
}

#[derive(Debug, Deserialize)]
struct SearchHit {
    title: String,
}

#[derive(Debug, Deserialize)]
struct ClaimsEnvelope {
    #[serde(default)]
    claims: Option<Claims>,
}

#[derive(Debug, Deserialize)]
struct Claims {
    #[serde(rename = "P18", default)]
    image: Vec<Claim>,
}

#[derive(Debug, Deserialize)]
struct Claim {
    mainsnak: Snak,
}

#[derive(Debug, Deserialize)]
struct Snak {
    #[serde(default)]
    datavalue: Option<DataValue>,
}

#[derive(Debug, Deserialize)]
struct DataValue {
    /// The Commons file name, e.g. `Miles Davis 1955.jpg`.
    value: String,
}

/// Fetches artist photography from Commons via Wikidata.
pub struct CommonsImageClient {
    http: Client,
    /// The Wikidata API and the Commons file root. Only the tests ever
    /// change them — see `MusicBrainzClient::base_url`.
    wikidata_api: String,
    file_path_root: String,
}

impl CommonsImageClient {
    pub fn new(contact: &str) -> Result<Self, ProviderError> {
        let http = Client::builder()
            .user_agent(user_agent(contact))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ProviderError::Unreachable(e.to_string()))?;

        Ok(Self {
            http,
            wikidata_api: WIKIDATA_API.to_string(),
            file_path_root: COMMONS_FILE_PATH.to_string(),
        })
    }

    /// The same client against local stubs, for tests.
    #[cfg(test)]
    pub(crate) fn against(
        contact: &str,
        wikidata_api: &str,
        file_path_root: &str,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            wikidata_api: wikidata_api.to_string(),
            file_path_root: file_path_root.trim_end_matches('/').to_string(),
            ..Self::new(contact)?
        })
    }

    /// The Wikidata entity carrying `mbid` as its MusicBrainz artist id.
    async fn entity_for(&self, mbid: &str) -> Result<Option<String>, ProviderError> {
        let response = self
            .http
            .get(&self.wikidata_api)
            .query(&[
                ("action", "query"),
                ("list", "search"),
                // `haswbstatement` searches the statement itself rather than
                // the article text, so this cannot match an entity that
                // merely mentions the id somewhere in prose.
                ("srsearch", &format!("haswbstatement:P434={mbid}")),
                ("srlimit", "1"),
                ("format", "json"),
            ])
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable(e.to_string()))?;

        if !response.status().is_success() {
            return Err(ProviderError::Unreachable(format!(
                "status {}",
                response.status()
            )));
        }

        let body: SearchEnvelope = response
            .json()
            .await
            .map_err(|e| ProviderError::Unusable(e.to_string()))?;

        Ok(body
            .query
            .and_then(|query| query.search.into_iter().next())
            .map(|hit| hit.title))
    }

    /// The Commons file name in `entity`'s P18 claim.
    async fn image_name_for(&self, entity: &str) -> Result<Option<String>, ProviderError> {
        let response = self
            .http
            .get(&self.wikidata_api)
            .query(&[
                ("action", "wbgetclaims"),
                ("entity", entity),
                ("property", "P18"),
                ("format", "json"),
            ])
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable(e.to_string()))?;

        if !response.status().is_success() {
            return Err(ProviderError::Unreachable(format!(
                "status {}",
                response.status()
            )));
        }

        let body: ClaimsEnvelope = response
            .json()
            .await
            .map_err(|e| ProviderError::Unusable(e.to_string()))?;

        Ok(body
            .claims
            .and_then(|claims| claims.image.into_iter().next())
            .and_then(|claim| claim.mainsnak.datavalue)
            .map(|value| value.value))
    }
}

impl ArtistImageProvider for CommonsImageClient {
    async fn image_for(&self, mbid: &str) -> Result<Option<ArtistImageAsset>, ProviderError> {
        let Some(entity) = self.entity_for(mbid).await? else {
            return Ok(None);
        };
        let Some(file_name) = self.image_name_for(&entity).await? else {
            return Ok(None);
        };

        // Refused on the *name*, before a byte is downloaded. Wikidata's P18
        // legitimately names vector and scientific-imaging formats — `.svg`
        // for a band's logo, `.tif` for a scanned photograph — and a client
        // drawing a portrait beside a track listing can decode neither. What
        // used to happen was worse than a failed download: the file was
        // stored, the row was settled as `Found`, and the artist was never
        // looked up again — so the client fell back to its placeholder
        // forever with nothing anywhere saying why.
        //
        // `Ok(None)` rather than an error, and deliberately. "Commons has a
        // picture of this artist that nothing here can draw" is, from every
        // caller's point of view, the same fact as "Commons has no picture":
        // there is nothing to show and nothing to retry. Recording it as
        // `NotFound` settles it, which is the truth.
        let Some(extension) = drawable_extension(&file_name) else {
            tracing::info!(
                file_name = %file_name,
                "the artist's picture is in a format no client can draw; treating it as none"
            );
            return Ok(None);
        };

        // A width is requested rather than the original. Commons holds
        // originals running to tens of megabytes and an artist portrait
        // beside a track listing needs none of it; asking for a rendering is
        // both far less to download and far less of theirs to spend.
        let source_url = format!(
            "{}/{}?width={MAX_IMAGE_WIDTH}",
            self.file_path_root,
            urlencode(&file_name)
        );
        let response = self
            .http
            .get(&source_url)
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable(e.to_string()))?;

        if !response.status().is_success() {
            return Err(ProviderError::Unreachable(format!(
                "status {}",
                response.status()
            )));
        }

        // Checked before the body is read where the server declared a length,
        // so an oversized rendering is refused rather than downloaded first.
        //
        // `Unusable`, not `Ok(None)`. `None` means the service had nothing,
        // which the command settles and never re-asks — and "larger than a
        // cap this code chose" is not that. Recorded as retryable, so
        // raising the cap or a smaller rendering appearing is enough to get
        // the image, rather than the artist being permanently marked as
        // having no photograph anywhere.
        if let Some(length) = response.content_length() {
            if length as usize > MAX_IMAGE_BYTES {
                return Err(ProviderError::Unusable(format!(
                    "image is {length} bytes, over the {MAX_IMAGE_BYTES} cap"
                )));
            }
        }

        let bytes = read_capped(response, MAX_IMAGE_BYTES).await?;

        Ok(Some(ArtistImageAsset {
            source_url,
            extension,
            bytes,
        }))
    }
}

/// Read a response body, giving up the moment it passes `cap`.
///
/// The declared `Content-Length` above is a claim, and a chunked response
/// makes no claim at all — which is what Commons serves for some renderings.
/// `Response::bytes()` would buffer the whole body first and let the cap
/// judge what had already been allocated, so the cap bounded what was *kept*
/// and not what was *read*. This bounds the read: the stream is abandoned at
/// the first chunk that carries the total past the cap, so nothing larger is
/// ever held.
/// `Response::chunk` rather than `bytes_stream`, which would mean turning on
/// reqwest's `stream` feature for one call site; this needs no feature and
/// says the same thing.
async fn read_capped(
    mut response: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, ProviderError> {
    // Sized from the declared length where there is one, clamped to the cap
    // so a false claim cannot make this allocate more than it will accept.
    let mut buffer =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(cap as u64) as usize);

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| ProviderError::Unreachable(e.to_string()))?
    {
        if buffer.len() + chunk.len() > cap {
            return Err(ProviderError::Unusable(format!(
                "image is over the {cap} byte cap"
            )));
        }
        buffer.extend_from_slice(&chunk);
    }

    Ok(buffer)
}

/// The extensions a client can actually draw.
///
/// The same six raster formats the indexer's own `image` features cover, and
/// the intersection of those with what Flutter decodes. `.svg` is the one
/// deliberate absence worth naming: it is vector, it is common on Commons for
/// logos, and nothing downstream renders it.
const DRAWABLE_EXTENSIONS: [&str; 6] = ["jpg", "jpeg", "png", "webp", "gif", "bmp"];

/// The file's extension when a client can draw it, `None` when it cannot.
///
/// Taken from the Commons file name rather than the response's content type:
/// the name is what Wikidata recorded and what the licence attaches to, and a
/// redirect chain can leave the header describing something else.
///
/// A name with no extension at all is `jpg`, which is what it was before this
/// gained an opinion: Commons file names carry one in practice, and guessing
/// the commonest photograph format for the vanishing case is better than
/// refusing a picture that is almost certainly fine.
fn drawable_extension(file_name: &str) -> Option<String> {
    let extension = file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| {
            !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or_else(|| "jpg".to_string());

    DRAWABLE_EXTENSIONS
        .contains(&extension.as_str())
        .then_some(extension)
}

/// Percent-encodes a Commons file name for a path segment.
///
/// Written out rather than pulled in as a dependency: this encodes one thing,
/// in one place, and the set of characters that matter in a Commons file name
/// is small and known. Spaces become `_` first, which is the form Commons
/// itself canonicalizes to.
fn urlencode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.replace(' ', "_").bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn given_a_file_name_when_its_extension_is_read_then_it_is_lowercased() {
        assert_eq!(
            drawable_extension("Miles Davis.JPG").as_deref(),
            Some("jpg")
        );
        assert_eq!(drawable_extension("portrait.png").as_deref(), Some("png"));
    }

    #[test]
    fn given_a_name_with_no_extension_when_read_then_it_falls_back() {
        assert_eq!(drawable_extension("portrait").as_deref(), Some("jpg"));
        // A trailing dot names no extension either.
        assert_eq!(drawable_extension("portrait.").as_deref(), Some("jpg"));
    }

    #[test]
    fn given_a_dotted_name_when_read_then_a_sentence_is_not_an_extension() {
        // Only the last segment counts, and only when it looks like one.
        assert_eq!(
            drawable_extension("A portrait, 1959. Restored").as_deref(),
            Some("jpg")
        );
    }

    #[test]
    fn given_a_format_nothing_can_draw_when_read_then_there_is_no_extension() {
        // The whole point: a picture that exists and cannot be shown is
        // answered as no picture, so the row settles as `NotFound` rather
        // than as a `Found` that renders as a placeholder forever.
        assert_eq!(drawable_extension("A band logo.svg"), None);
        assert_eq!(drawable_extension("Scanned plate.tif"), None);
        assert_eq!(drawable_extension("Scanned plate.tiff"), None);
    }

    #[test]
    fn given_every_drawable_format_when_read_then_each_is_kept() {
        for name in ["a.jpg", "a.jpeg", "a.png", "a.webp", "a.gif", "a.bmp"] {
            assert!(drawable_extension(name).is_some(), "{name}");
        }
    }

    #[test]
    fn given_a_file_name_with_spaces_when_encoded_then_commons_form_is_used() {
        assert_eq!(urlencode("Miles Davis.jpg"), "Miles_Davis.jpg");
    }

    #[test]
    fn given_a_name_with_reserved_characters_when_encoded_then_they_are_escaped() {
        // A `?` or `&` left raw would end the path and turn the rest into a
        // query string, fetching the wrong file or none.
        let encoded = urlencode("Whose Album? Yes & No.jpg");

        assert!(!encoded.contains('?'), "{encoded}");
        assert!(!encoded.contains('&'), "{encoded}");
        assert!(encoded.contains("%3F"), "{encoded}");
    }
}
