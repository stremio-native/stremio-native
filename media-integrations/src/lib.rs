use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::Duration,
};

use async_trait::async_trait;
use credential_store::{CredentialError, CredentialRef, CredentialStore};
use moka::future::Cache;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};

type QueryParameters<'a> = Vec<(&'static str, &'a str)>;
type EnrichmentRequest<'a> = (String, QueryParameters<'a>);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const RESPONSE_CACHE_CAPACITY: u64 = 16 * 1024 * 1024;
const RESPONSE_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_IDLE_CONNECTIONS_PER_HOST: usize = 2;
const IDLE_POOL_TIMEOUT: Duration = Duration::from_secs(30);
const CACHE_ENTRY_OVERHEAD: u32 = 256;

static HTTP_CLIENT: LazyLock<Result<Client, ProviderError>> = LazyLock::new(|| {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .pool_max_idle_per_host(MAX_IDLE_CONNECTIONS_PER_HOST)
        .pool_idle_timeout(IDLE_POOL_TIMEOUT)
        .user_agent("Stremio-Native/1")
        .build()
        .map_err(|_| ProviderError::Unavailable)
});
static REQUEST_PERMITS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(8));
static RATE_LIMITERS: LazyLock<[Mutex<Option<tokio::time::Instant>>; 8]> =
    LazyLock::new(|| std::array::from_fn(|_| Mutex::new(None)));
static RESPONSE_CACHE: LazyLock<Cache<MetadataCacheKey, CachedJson>> = LazyLock::new(|| {
    Cache::builder()
        .max_capacity(RESPONSE_CACHE_CAPACITY)
        .time_to_live(RESPONSE_CACHE_TTL)
        .weigher(metadata_cache_weight)
        .build()
});

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MetadataCacheKey {
    provider: ProviderKind,
    credential_ref: Option<CredentialRef>,
    operation: String,
}

#[derive(Clone, Debug)]
struct CachedJson {
    value: Arc<serde_json::Value>,
    encoded_len: u32,
}

fn metadata_cache_weight(key: &MetadataCacheKey, value: &CachedJson) -> u32 {
    let key_bytes = key.operation.len().saturating_add(
        key.credential_ref
            .as_ref()
            .map_or(0, |reference| reference.expose_id().len()),
    );
    value
        .encoded_len
        .saturating_mul(2)
        .saturating_add(u32::try_from(key_bytes).unwrap_or(u32::MAX))
        .saturating_add(CACHE_ENTRY_OVERHEAD)
}

/// Invalidates all in-memory metadata responses.
///
/// Call this before validating a replacement key and after credentials are removed.
pub fn invalidate_response_cache() {
    RESPONSE_CACHE.invalidate_all();
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Tmdb,
    Omdb,
    Mdblist,
    Fanart,
    Rpdb,
    Kitsu,
    AniZip,
    Trakt,
}

impl ProviderKind {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Tmdb => "TMDB",
            Self::Omdb => "OMDb",
            Self::Mdblist => "MDBList",
            Self::Fanart => "Fanart.tv",
            Self::Rpdb => "RPDB",
            Self::Kitsu => "Kitsu",
            Self::AniZip => "AniZip",
            Self::Trakt => "Trakt",
        }
    }

    pub const fn attribution_url(self) -> &'static str {
        match self {
            Self::Tmdb => "https://www.themoviedb.org/",
            Self::Omdb => "https://www.omdbapi.com/",
            Self::Mdblist => "https://mdblist.com/",
            Self::Fanart => "https://fanart.tv/",
            Self::Rpdb => "https://ratingposterdb.com/",
            Self::Kitsu => "https://kitsu.io/",
            Self::AniZip => "https://anizip.net/",
            Self::Trakt => "https://trakt.tv/",
        }
    }

    const fn requires_key(self) -> bool {
        matches!(
            self,
            Self::Tmdb | Self::Omdb | Self::Mdblist | Self::Fanart | Self::Rpdb | Self::Trakt
        )
    }

    const fn minimum_interval(self) -> Duration {
        match self {
            Self::Omdb | Self::Fanart => Duration::from_millis(250),
            _ => Duration::from_millis(100),
        }
    }

    const fn rate_limit_index(self) -> usize {
        match self {
            Self::Tmdb => 0,
            Self::Omdb => 1,
            Self::Mdblist => 2,
            Self::Fanart => 3,
            Self::Rpdb => 4,
            Self::Kitsu => 5,
            Self::AniZip => 6,
            Self::Trakt => 7,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct EnrichedMeta {
    pub provider: Option<ProviderKind>,
    pub overview: Option<String>,
    pub rating: Option<f64>,
    pub votes: Option<u64>,
    pub poster: Option<String>,
    pub background: Option<String>,
    pub logo: Option<String>,
    pub cast: Vec<PersonCredit>,
    pub crew: Vec<PersonCredit>,
    pub external_ids: HashMap<String, String>,
    pub attribution: Option<Attribution>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PersonCredit {
    pub id: String,
    pub name: String,
    pub role: String,
    pub image: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WatchProviderResult {
    pub provider_name: String,
    pub logo: Option<String>,
    pub link: Option<String>,
    pub kind: WatchOfferKind,
    pub region: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WatchOfferKind {
    Subscription,
    Free,
    Ads,
    Rent,
    Buy,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Attribution {
    pub label: String,
    pub url: String,
    pub artwork_license: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProviderError {
    #[error("provider credential operation failed: {0}")]
    Credential(#[from] CredentialError),
    #[error("provider credential is required")]
    CredentialRequired,
    #[error("provider rejected the credential")]
    Unauthorized,
    #[error("provider request was rate limited")]
    RateLimited,
    #[error("provider is unavailable")]
    Unavailable,
    #[error("provider response is invalid")]
    InvalidResponse,
    #[error("provider does not support this operation")]
    Unsupported,
}

#[async_trait]
pub trait MetadataProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    async fn enrich(&self, external_id: &str) -> Result<EnrichedMeta, ProviderError>;
    async fn person(&self, person_id: &str) -> Result<PersonCredit, ProviderError>;
    async fn watch_providers(
        &self,
        external_id: &str,
        region: &str,
    ) -> Result<Vec<WatchProviderResult>, ProviderError>;
}

#[derive(Clone)]
pub struct HttpMetadataProvider {
    kind: ProviderKind,
    credential_ref: Option<CredentialRef>,
    credentials: Arc<dyn CredentialStore>,
    client: Client,
}

impl HttpMetadataProvider {
    pub fn new(
        kind: ProviderKind,
        credential_ref: Option<CredentialRef>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, ProviderError> {
        if kind.requires_key() && credential_ref.is_none() {
            return Err(ProviderError::CredentialRequired);
        }
        let client = HTTP_CLIENT.as_ref().map_err(Clone::clone)?.clone();
        Ok(Self {
            kind,
            credential_ref,
            credentials,
            client,
        })
    }

    async fn api_key(&self) -> Result<Option<String>, ProviderError> {
        let Some(reference) = &self.credential_ref else {
            return Ok(None);
        };
        let value = self.credentials.get(reference).await?;
        String::from_utf8(value.expose().to_vec())
            .map(Some)
            .map_err(|_| ProviderError::InvalidResponse)
    }

    async fn wait_for_rate_limit(&self) {
        let now = tokio::time::Instant::now();
        let delay = {
            let mut next_allowed = RATE_LIMITERS[self.kind.rate_limit_index()].lock().await;
            let reserved = next_allowed.map_or(now, |next| next.max(now));
            *next_allowed = Some(reserved + self.kind.minimum_interval());
            reserved.saturating_duration_since(now)
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    async fn fetch(
        &self,
        cache_key: String,
        url: String,
        query: &[(&str, &str)],
    ) -> Result<Arc<serde_json::Value>, ProviderError> {
        let key = MetadataCacheKey {
            provider: self.kind,
            credential_ref: self.credential_ref.clone(),
            operation: cache_key,
        };
        RESPONSE_CACHE
            .try_get_with(key, async {
                self.wait_for_rate_limit().await;
                let _permit = REQUEST_PERMITS
                    .acquire()
                    .await
                    .map_err(|_| ProviderError::Unavailable)?;
                let api_key = self.api_key().await?;
                let mut request = self.client.get(url).query(query);
                if let Some(key) = api_key.as_deref() {
                    request = apply_api_key(request, self.kind, key);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|_| ProviderError::Unavailable)?;
                match response.status() {
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                        return Err(ProviderError::Unauthorized);
                    }
                    StatusCode::TOO_MANY_REQUESTS => return Err(ProviderError::RateLimited),
                    status if !status.is_success() => return Err(ProviderError::Unavailable),
                    _ => {}
                }
                if response
                    .content_length()
                    .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
                {
                    return Err(ProviderError::InvalidResponse);
                }
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|_| ProviderError::InvalidResponse)?;
                let value = parse_metadata_json(&bytes)?;
                Ok::<CachedJson, ProviderError>(CachedJson {
                    value,
                    encoded_len: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
                })
            })
            .await
            .map(|cached| cached.value.clone())
            .map_err(|error| (*error).clone())
    }

    fn attribution(&self) -> Attribution {
        Attribution {
            label: format!("Data from {}", self.kind.display_name()),
            url: self.kind.attribution_url().to_owned(),
            artwork_license: matches!(self.kind, ProviderKind::Fanart)
                .then(|| "Artwork is subject to Fanart.tv contributor terms".to_owned()),
        }
    }
}

fn parse_metadata_json(bytes: &[u8]) -> Result<Arc<serde_json::Value>, ProviderError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(ProviderError::InvalidResponse);
    }
    serde_json::from_slice(bytes)
        .map(Arc::new)
        .map_err(|_| ProviderError::InvalidResponse)
}

fn apply_api_key(
    request: reqwest::RequestBuilder,
    kind: ProviderKind,
    key: &str,
) -> reqwest::RequestBuilder {
    match kind {
        ProviderKind::Tmdb | ProviderKind::Trakt => request.bearer_auth(key),
        ProviderKind::Fanart => request.header("api-key", key),
        ProviderKind::Omdb | ProviderKind::Mdblist | ProviderKind::Rpdb => {
            request.query(&[("apikey", key)])
        }
        ProviderKind::Kitsu | ProviderKind::AniZip => request,
    }
}

#[async_trait]
impl MetadataProvider for HttpMetadataProvider {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn enrich(&self, external_id: &str) -> Result<EnrichedMeta, ProviderError> {
        let (url, query) = enrichment_request(self.kind, external_id)?;
        let value = self
            .fetch(format!("enrich:{external_id}"), url, &query)
            .await?;
        Ok(normalize_enrichment(self.kind, &value, self.attribution()))
    }

    async fn person(&self, person_id: &str) -> Result<PersonCredit, ProviderError> {
        if self.kind != ProviderKind::Tmdb {
            return Err(ProviderError::Unsupported);
        }
        let value = self
            .fetch(
                format!("person:{person_id}"),
                format!("https://api.themoviedb.org/3/person/{person_id}"),
                &[],
            )
            .await?;
        Ok(PersonCredit {
            id: person_id.to_owned(),
            name: string_field(&value, &["name"]).unwrap_or_default(),
            role: string_field(&value, &["known_for_department"]).unwrap_or_default(),
            image: string_field(&value, &["profile_path"])
                .map(|path| format!("https://image.tmdb.org/t/p/w500{path}")),
        })
    }

    async fn watch_providers(
        &self,
        external_id: &str,
        region: &str,
    ) -> Result<Vec<WatchProviderResult>, ProviderError> {
        if self.kind != ProviderKind::Tmdb {
            return Err(ProviderError::Unsupported);
        }
        let value = self
            .fetch(
                format!("watch:{external_id}:{region}"),
                format!("https://api.themoviedb.org/3/movie/{external_id}/watch/providers"),
                &[],
            )
            .await?;
        Ok(normalize_watch_providers(&value, region))
    }
}

fn enrichment_request(
    kind: ProviderKind,
    external_id: &str,
) -> Result<EnrichmentRequest<'_>, ProviderError> {
    let request = match kind {
        ProviderKind::Tmdb => (
            format!("https://api.themoviedb.org/3/find/{external_id}"),
            vec![("external_source", "imdb_id")],
        ),
        ProviderKind::Omdb => (
            "https://www.omdbapi.com/".to_owned(),
            vec![("i", external_id)],
        ),
        ProviderKind::Mdblist => (
            format!("https://api.mdblist.com/imdb/movie/{external_id}"),
            Vec::new(),
        ),
        ProviderKind::Fanart => (
            format!("https://webservice.fanart.tv/v3/movies/{external_id}"),
            Vec::new(),
        ),
        ProviderKind::Rpdb => (
            format!("https://api.ratingposterdb.com/{external_id}"),
            Vec::new(),
        ),
        ProviderKind::Kitsu => (
            format!("https://kitsu.io/api/edge/anime/{external_id}"),
            Vec::new(),
        ),
        ProviderKind::AniZip => (
            "https://api.ani.zip/mappings".to_owned(),
            vec![("anilist_id", external_id)],
        ),
        ProviderKind::Trakt => (
            format!("https://api.trakt.tv/search/imdb/{external_id}"),
            Vec::new(),
        ),
    };
    Ok(request)
}

fn normalize_enrichment(
    kind: ProviderKind,
    value: &serde_json::Value,
    attribution: Attribution,
) -> EnrichedMeta {
    let data = value
        .get("data")
        .and_then(|data| data.get("attributes"))
        .or_else(|| value.get("movie_results").and_then(|items| items.get(0)))
        .unwrap_or(value);
    let mut external_ids = HashMap::new();
    if let Some(id) = data.get("id").and_then(serde_json::Value::as_u64) {
        external_ids.insert("tmdb".to_owned(), id.to_string());
    }
    EnrichedMeta {
        provider: Some(kind),
        overview: string_field(data, &["overview", "Plot", "synopsis", "description"]),
        rating: if kind == ProviderKind::Mdblist {
            mdblist_rating(data)
        } else {
            number_field(data, &["vote_average", "imdbRating", "averageRating"])
        },
        votes: integer_field(data, &["vote_count", "imdbVotes"]),
        poster: string_field(data, &["poster_path", "Poster", "posterImage"]),
        background: string_field(data, &["backdrop_path", "fanart", "coverImage"]),
        logo: string_field(data, &["logo", "hdmovielogo"]),
        cast: Vec::new(),
        crew: Vec::new(),
        external_ids,
        attribution: Some(attribution),
    }
}

fn mdblist_rating(value: &serde_json::Value) -> Option<f64> {
    ["score_average", "scoreaverage", "score"]
        .iter()
        .find_map(|field| number_field(value, &[*field]).filter(|score| *score > 0.0))
        .and_then(normalize_mdblist_score)
        .or_else(|| {
            let (total, count) = value
                .get("ratings")
                .and_then(serde_json::Value::as_array)?
                .iter()
                .filter_map(normalize_mdblist_rating_row)
                .fold((0.0, 0_u32), |(total, count), rating| {
                    (total + rating, count + 1)
                });
            (count > 0).then(|| total / f64::from(count))
        })
}

fn normalize_mdblist_rating_row(row: &serde_json::Value) -> Option<f64> {
    let source = row.get("source").and_then(serde_json::Value::as_str)?;
    let score = row.get("value").and_then(serde_json::Value::as_f64)?;
    if score <= 0.0 {
        return None;
    }
    match source {
        "letterboxd" if score <= 5.0 => Some(score * 2.0),
        "imdb" | "tmdb" | "trakt" | "metacritic" | "tomatoes" | "tomatometer"
        | "tomatoesaudience" | "audience" | "popcorn" | "letterboxd" | "simkl" => {
            normalize_mdblist_score(score)
        }
        _ => None,
    }
}

fn normalize_mdblist_score(score: f64) -> Option<f64> {
    let score = if score > 10.0 { score / 10.0 } else { score };
    (score <= 10.0).then_some(score)
}

fn normalize_watch_providers(value: &serde_json::Value, region: &str) -> Vec<WatchProviderResult> {
    let Some(result) = value.get("results").and_then(|results| results.get(region)) else {
        return Vec::new();
    };
    let mut providers = Vec::new();
    for (field, kind) in [
        ("flatrate", WatchOfferKind::Subscription),
        ("free", WatchOfferKind::Free),
        ("ads", WatchOfferKind::Ads),
        ("rent", WatchOfferKind::Rent),
        ("buy", WatchOfferKind::Buy),
    ] {
        for provider in result
            .get(field)
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
        {
            providers.push(WatchProviderResult {
                provider_name: string_field(provider, &["provider_name"]).unwrap_or_default(),
                logo: string_field(provider, &["logo_path"])
                    .map(|path| format!("https://image.tmdb.org/t/p/w154{path}")),
                link: string_field(result, &["link"]),
                kind,
                region: region.to_owned(),
            });
        }
    }
    providers
}

fn string_field(value: &serde_json::Value, fields: &[&str]) -> Option<String> {
    fields.iter().find_map(|field| {
        let field = value.get(*field)?;
        if let Some(value) = field.as_str() {
            Some(value.to_owned())
        } else if let Some(value) = field.get("original").and_then(serde_json::Value::as_str) {
            Some(value.to_owned())
        } else {
            field
                .as_array()
                .and_then(|items| items.first())
                .and_then(|item| item.get("url"))
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        }
    })
}

fn number_field(value: &serde_json::Value, fields: &[&str]) -> Option<f64> {
    fields.iter().find_map(|field| {
        value.get(*field).and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
    })
}

fn integer_field(value: &serde_json::Value, fields: &[&str]) -> Option<u64> {
    fields.iter().find_map(|field| {
        value.get(*field).and_then(|value| {
            value.as_u64().or_else(|| {
                value
                    .as_str()
                    .map(|value| value.replace(',', ""))
                    .and_then(|value| value.parse().ok())
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn normalizes_partial_metadata_without_requiring_every_field() {
        let meta = normalize_enrichment(
            ProviderKind::Omdb,
            &serde_json::json!({"Plot": "A story", "imdbRating": "8.2"}),
            Attribution {
                label: "OMDb".to_owned(),
                url: "https://www.omdbapi.com/".to_owned(),
                artwork_license: None,
            },
        );
        assert_eq!(meta.overview.as_deref(), Some("A story"));
        assert_eq!(meta.rating, Some(8.2));
    }

    #[test]
    fn provider_failures_are_typed_for_incremental_callers() {
        assert_eq!(
            ProviderError::Unavailable.to_string(),
            "provider is unavailable"
        );
    }

    #[test]
    fn mdblist_enrichment_request_uses_movie_endpoint() {
        let request = enrichment_request(ProviderKind::Mdblist, "tt0111161");

        assert_eq!(
            request,
            Ok((
                "https://api.mdblist.com/imdb/movie/tt0111161".to_owned(),
                Vec::new()
            ))
        );
    }

    #[test]
    fn mdblist_api_key_uses_query_parameter_authentication() {
        let request = apply_api_key(
            Client::new().get("https://api.mdblist.com/imdb/movie/tt0111161"),
            ProviderKind::Mdblist,
            "secret",
        )
        .build()
        .expect("request");

        assert_eq!(request.url().query(), Some("apikey=secret"));
    }

    #[test]
    fn mdblist_score_average_is_normalized_to_ten_point_scale() {
        let meta = normalize_enrichment(
            ProviderKind::Mdblist,
            &serde_json::json!({"score_average": 84}),
            Attribution {
                label: "MDBList".to_owned(),
                url: "https://mdblist.com/".to_owned(),
                artwork_license: None,
            },
        );

        assert_eq!(meta.rating, Some(8.4));
    }

    #[test]
    fn mdblist_rating_rows_are_aggregated_when_average_is_missing() {
        let meta = normalize_enrichment(
            ProviderKind::Mdblist,
            &serde_json::json!({
                "ratings": [
                    {"source": "imdb", "value": 8.0},
                    {"source": "trakt", "value": 80},
                    {"source": "letterboxd", "value": 4.0}
                ]
            }),
            Attribution {
                label: "MDBList".to_owned(),
                url: "https://mdblist.com/".to_owned(),
                artwork_license: None,
            },
        );

        assert_eq!(meta.rating, Some(8.0));
    }

    #[test]
    fn response_cache_key_isolates_provider_and_credential() {
        let first = MetadataCacheKey {
            provider: ProviderKind::Tmdb,
            credential_ref: Some(CredentialRef::new("metadata/profile-a").expect("valid ref")),
            operation: "enrich:tt123".to_owned(),
        };
        let other_provider = MetadataCacheKey {
            provider: ProviderKind::Omdb,
            ..first.clone()
        };
        let other_credential = MetadataCacheKey {
            credential_ref: Some(CredentialRef::new("metadata/profile-b").expect("valid ref")),
            ..first.clone()
        };

        assert_ne!(first, other_provider);
        assert_ne!(first, other_credential);
    }

    #[test]
    fn response_cache_uses_balanced_policy() {
        let policy = RESPONSE_CACHE.policy();

        assert_eq!(policy.max_capacity(), Some(RESPONSE_CACHE_CAPACITY));
        assert_eq!(policy.time_to_live(), Some(RESPONSE_CACHE_TTL));
    }

    #[test]
    fn response_weight_includes_json_and_key_storage() {
        let key = MetadataCacheKey {
            provider: ProviderKind::Kitsu,
            credential_ref: None,
            operation: "enrich:1".to_owned(),
        };
        let value = CachedJson {
            value: Arc::new(serde_json::json!({"id": 1})),
            encoded_len: 100,
        };

        assert!(metadata_cache_weight(&key, &value) > 200);
    }

    #[test]
    fn oversized_metadata_response_is_rejected_before_deserialization() {
        let bytes = vec![b' '; MAX_RESPONSE_BYTES + 1];

        assert_eq!(
            parse_metadata_json(&bytes),
            Err(ProviderError::InvalidResponse)
        );
    }

    #[tokio::test]
    async fn identical_metadata_lookups_are_single_flight_and_share_json() {
        let cache = Cache::builder().max_capacity(16).build();
        let requests = Arc::new(AtomicUsize::new(0));
        let key = MetadataCacheKey {
            provider: ProviderKind::AniZip,
            credential_ref: None,
            operation: "enrich:single-flight".to_owned(),
        };
        let first_requests = requests.clone();
        let second_requests = requests.clone();
        let first = cache.try_get_with(key.clone(), async move {
            first_requests.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(25)).await;
            Ok::<_, ProviderError>(CachedJson {
                value: Arc::new(serde_json::json!({"id": 1})),
                encoded_len: 8,
            })
        });
        let second = cache.try_get_with(key, async move {
            second_requests.fetch_add(1, Ordering::SeqCst);
            Ok::<_, ProviderError>(CachedJson {
                value: Arc::new(serde_json::json!({"id": 2})),
                encoded_len: 8,
            })
        });

        let (first, second) = tokio::join!(first, second);
        let first = first.expect("first");
        let second = second.expect("second");
        assert!(Arc::ptr_eq(&first.value, &second.value));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_metadata_lookup_is_not_cached() {
        let cache = Cache::builder().max_capacity(16).build();
        let key = MetadataCacheKey {
            provider: ProviderKind::Kitsu,
            credential_ref: None,
            operation: "enrich:error".to_owned(),
        };
        let first = cache
            .try_get_with(key.clone(), async {
                Err::<CachedJson, _>(ProviderError::Unavailable)
            })
            .await;
        let second = cache
            .try_get_with(key, async {
                Ok::<_, ProviderError>(CachedJson {
                    value: Arc::new(serde_json::json!({"ok": true})),
                    encoded_len: 11,
                })
            })
            .await;

        assert!(first.is_err());
        assert!(second.is_ok());
    }

    #[tokio::test]
    async fn metadata_cache_can_be_invalidated_after_credential_changes() {
        let key = MetadataCacheKey {
            provider: ProviderKind::Rpdb,
            credential_ref: Some(CredentialRef::new("metadata/invalidation").expect("valid ref")),
            operation: "enrich:invalidate".to_owned(),
        };
        RESPONSE_CACHE
            .insert(
                key.clone(),
                CachedJson {
                    value: Arc::new(serde_json::json!({"ok": true})),
                    encoded_len: 11,
                },
            )
            .await;

        invalidate_response_cache();

        assert!(RESPONSE_CACHE.get(&key).await.is_none());
    }

    #[tokio::test]
    async fn rate_limit_reservations_release_the_mutex_before_sleeping() {
        let limiter = &RATE_LIMITERS[ProviderKind::Omdb.rate_limit_index()];
        *limiter.lock().await = None;
        let credentials = Arc::new(credential_store::MemoryCredentialStore::default());
        let provider = HttpMetadataProvider::new(
            ProviderKind::Omdb,
            Some(CredentialRef::new("metadata/test").expect("valid ref")),
            credentials,
        )
        .expect("provider");

        provider.wait_for_rate_limit().await;
        let waiting = tokio::spawn({
            let provider = provider.clone();
            async move { provider.wait_for_rate_limit().await }
        });
        tokio::task::yield_now().await;

        assert!(limiter.try_lock().is_ok());
        waiting.await.expect("rate limit task");
    }
}
