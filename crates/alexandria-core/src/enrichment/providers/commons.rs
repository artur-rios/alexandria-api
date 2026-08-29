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
}

impl CommonsImageClient {
    pub fn new(contact: &str) -> Result<Self, ProviderError> {
        let http = Client::builder()
            .user_agent(user_agent(contact))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ProviderError::Unreachable(e.to_string()))?;

        Ok(Self { http })
    }

    /// The Wikidata entity carrying `mbid` as its MusicBrainz artist id.
    async fn entity_for(&self, mbid: &str) -> Result<Option<String>, ProviderError> {
        let response = self
            .http
            .get(WIKIDATA_API)
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
            .get(WIKIDATA_API)
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

        // A width is requested rather than the original. Commons holds
        // originals running to tens of megabytes and an artist portrait
        // beside a track listing needs none of it; asking for a rendering is
        // both far less to download and far less of theirs to spend.
        let source_url = format!(
            "{COMMONS_FILE_PATH}/{}?width={MAX_IMAGE_WIDTH}",
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

        let bytes = response
            .bytes()
            .await
            .map_err(|e| ProviderError::Unreachable(e.to_string()))?;

        // And again after, for a response that declared no length at all.
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(ProviderError::Unusable(format!(
                "image is {} bytes, over the {MAX_IMAGE_BYTES} cap",
                bytes.len()
            )));
        }

        Ok(Some(ArtistImageAsset {
            source_url,
            extension: extension_of(&file_name),
            bytes: bytes.to_vec(),
        }))
    }
}

/// The file's extension, lowercased, or `jpg` when it names none.
///
/// Taken from the Commons file name rather than the response's content type:
/// the name is what Wikidata recorded and what the licence attaches to, and a
/// redirect chain can leave the header describing something else.
fn extension_of(file_name: &str) -> String {
    file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| {
            !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or_else(|| "jpg".to_string())
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
        assert_eq!(extension_of("Miles Davis.JPG"), "jpg");
        assert_eq!(extension_of("portrait.png"), "png");
    }

    #[test]
    fn given_a_name_with_no_extension_when_read_then_it_falls_back() {
        assert_eq!(extension_of("portrait"), "jpg");
        // A trailing dot names no extension either.
        assert_eq!(extension_of("portrait."), "jpg");
    }

    #[test]
    fn given_a_dotted_name_when_read_then_a_sentence_is_not_an_extension() {
        // Only the last segment counts, and only when it looks like one.
        assert_eq!(extension_of("A portrait, 1959. Restored"), "jpg");
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
