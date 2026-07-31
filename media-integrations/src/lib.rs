use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use credential_store::{CredentialError, CredentialRef, CredentialStore};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};

type QueryParameters<'a> = Vec<(&'static str, &'a str)>;
type EnrichmentRequest<'a> = (String, QueryParameters<'a>);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Tmdb,
    Omdb,
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
            Self::Tmdb | Self::Omdb | Self::Fanart | Self::Rpdb | Self::Trakt
        )
    }

    const fn minimum_interval(self) -> Duration {
        match self {
            Self::Omdb | Self::Fanart => Duration::from_millis(250),
            _ => Duration::from_millis(100),
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
    permits: Arc<Semaphore>,
    last_request: Arc<Mutex<Option<Instant>>>,
    cache: Arc<Mutex<HashMap<String, (Instant, serde_json::Value)>>>,
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
        let client = Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent("Stremio-Native/1")
            .build()
            .map_err(|_| ProviderError::Unavailable)?;
        Ok(Self {
            kind,
            credential_ref,
            credentials,
            client,
            permits: Arc::new(Semaphore::new(4)),
            last_request: Arc::new(Mutex::new(None)),
            cache: Arc::new(Mutex::new(HashMap::new())),
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
        let mut last = self.last_request.lock().await;
        if let Some(previous) = *last {
            let remaining = self
                .kind
                .minimum_interval()
                .saturating_sub(previous.elapsed());
            if !remaining.is_zero() {
                tokio::time::sleep(remaining).await;
            }
        }
        *last = Some(Instant::now());
    }

    async fn fetch(
        &self,
        cache_key: String,
        url: String,
        query: &[(&str, &str)],
    ) -> Result<serde_json::Value, ProviderError> {
        if let Some((cached_at, value)) = self.cache.lock().await.get(&cache_key).cloned()
            && cached_at.elapsed() < Duration::from_secs(60 * 60)
        {
            return Ok(value);
        }
        self.wait_for_rate_limit().await;
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        let api_key = self.api_key().await?;
        let mut request = self.client.get(url).query(query);
        if let Some(key) = api_key.as_deref() {
            request = match self.kind {
                ProviderKind::Tmdb | ProviderKind::Trakt => request.bearer_auth(key),
                ProviderKind::Fanart => request.header("api-key", key),
                ProviderKind::Omdb | ProviderKind::Rpdb => request.query(&[("apikey", key)]),
                _ => request,
            };
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
        let value: serde_json::Value = response
            .json()
            .await
            .map_err(|_| ProviderError::InvalidResponse)?;
        self.cache
            .lock()
            .await
            .insert(cache_key, (Instant::now(), value.clone()));
        Ok(value)
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
        Ok(normalize_enrichment(self.kind, value, self.attribution()))
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
    value: serde_json::Value,
    attribution: Attribution,
) -> EnrichedMeta {
    let data = value
        .get("data")
        .and_then(|data| data.get("attributes"))
        .or_else(|| value.get("movie_results").and_then(|items| items.get(0)))
        .unwrap_or(&value);
    let mut external_ids = HashMap::new();
    if let Some(id) = data.get("id").and_then(serde_json::Value::as_u64) {
        external_ids.insert("tmdb".to_owned(), id.to_string());
    }
    EnrichedMeta {
        provider: Some(kind),
        overview: string_field(data, &["overview", "Plot", "synopsis", "description"]),
        rating: number_field(data, &["vote_average", "imdbRating", "averageRating"]),
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
    use super::*;

    #[test]
    fn normalizes_partial_metadata_without_requiring_every_field() {
        let meta = normalize_enrichment(
            ProviderKind::Omdb,
            serde_json::json!({"Plot": "A story", "imdbRating": "8.2"}),
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
}
