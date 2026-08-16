use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use async_trait::async_trait;
use credential_store::{CredentialError, CredentialRef, CredentialStore};
use moka::future::Cache;
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const CACHE_CAPACITY: u64 = 4_096;
const MAX_IDLE_CONNECTIONS_PER_HOST: usize = 2;
const IDLE_POOL_TIMEOUT: Duration = Duration::from_secs(30);

static HTTP_CLIENT: LazyLock<Result<Client, DebridError>> = LazyLock::new(|| {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .pool_max_idle_per_host(MAX_IDLE_CONNECTIONS_PER_HOST)
        .pool_idle_timeout(IDLE_POOL_TIMEOUT)
        .user_agent("Stremio-Native/1")
        .build()
        .map_err(|_| DebridError::Unavailable)
});
static REQUEST_PERMITS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(4));
static AVAILABILITY_CACHE: LazyLock<Cache<AvailabilityCacheKey, DebridAvailability>> =
    LazyLock::new(|| {
        Cache::builder()
            .max_capacity(CACHE_CAPACITY)
            .time_to_live(CACHE_TTL)
            .build()
    });

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct AvailabilityCacheKey {
    provider: ProviderKind,
    credential_ref: CredentialRef,
    hash: String,
}

/// Invalidates all cached debrid availability annotations.
///
/// Call this after credentials are changed or their owning profile is deleted.
pub fn invalidate_availability_cache() {
    AVAILABILITY_CACHE.invalidate_all();
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    RealDebrid,
    AllDebrid,
    Premiumize,
    DebridLink,
    TorBox,
}

impl ProviderKind {
    pub const ALL: [Self; 5] = [
        Self::RealDebrid,
        Self::AllDebrid,
        Self::Premiumize,
        Self::DebridLink,
        Self::TorBox,
    ];

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::RealDebrid => "Real-Debrid",
            Self::AllDebrid => "AllDebrid",
            Self::Premiumize => "Premiumize",
            Self::DebridLink => "Debrid-Link",
            Self::TorBox => "TorBox",
        }
    }

    const fn base_url(self) -> &'static str {
        match self {
            Self::RealDebrid => "https://api.real-debrid.com/rest/1.0",
            Self::AllDebrid => "https://api.alldebrid.com/v4",
            Self::Premiumize => "https://www.premiumize.me/api",
            Self::DebridLink => "https://debrid-link.com/api/v2",
            Self::TorBox => "https://api.torbox.app/v1/api",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DebridAvailability {
    Cached,
    Uncached,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AccountStatus {
    pub username: Option<String>,
    pub premium: bool,
    pub expires_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolvedLink {
    pub url: String,
    pub filename: Option<String>,
    pub size_bytes: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DebridError {
    #[error("credential vault operation failed: {0}")]
    Credential(#[from] CredentialError),
    #[error("provider request timed out")]
    Timeout,
    #[error("provider rejected the credential")]
    Unauthorized,
    #[error("provider is temporarily unavailable")]
    Unavailable,
    #[error("provider returned an invalid response")]
    InvalidResponse,
    #[error("source cannot be resolved by this provider")]
    Unsupported,
}

#[async_trait]
pub trait DebridProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    async fn account_status(&self) -> Result<AccountStatus, DebridError>;
    async fn availability(&self, hash: &str) -> Result<DebridAvailability, DebridError>;
    async fn resolve(&self, source: &str) -> Result<ResolvedLink, DebridError>;
}

#[derive(Clone)]
pub struct HttpDebridProvider {
    kind: ProviderKind,
    credential_ref: CredentialRef,
    credentials: Arc<dyn CredentialStore>,
    client: Client,
}

impl HttpDebridProvider {
    pub fn new(
        kind: ProviderKind,
        credential_ref: CredentialRef,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, DebridError> {
        let client = HTTP_CLIENT.as_ref().map_err(Clone::clone)?.clone();
        Ok(Self {
            kind,
            credential_ref,
            credentials,
            client,
        })
    }

    async fn token(&self) -> Result<String, DebridError> {
        let secret = self.credentials.get(&self.credential_ref).await?;
        String::from_utf8(secret.expose().to_vec()).map_err(|_| DebridError::InvalidResponse)
    }

    async fn send(&self, request: RequestBuilder) -> Result<serde_json::Value, DebridError> {
        let _permit = REQUEST_PERMITS
            .acquire()
            .await
            .map_err(|_| DebridError::Unavailable)?;
        let response = tokio::time::timeout(REQUEST_TIMEOUT, request.send())
            .await
            .map_err(|_| DebridError::Timeout)?
            .map_err(map_transport_error)?;
        match response.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(DebridError::Unauthorized),
            status if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS => {
                Err(DebridError::Unavailable)
            }
            status if !status.is_success() => Err(DebridError::InvalidResponse),
            _ => response
                .json()
                .await
                .map_err(|_| DebridError::InvalidResponse),
        }
    }

    fn authorized_request(&self, method: Method, path: &str, token: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.kind.base_url()))
            .bearer_auth(token)
    }
}

#[async_trait]
impl DebridProvider for HttpDebridProvider {
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    async fn account_status(&self) -> Result<AccountStatus, DebridError> {
        let token = self.token().await?;
        let path = match self.kind {
            ProviderKind::RealDebrid => "/user",
            ProviderKind::AllDebrid => "/user",
            ProviderKind::Premiumize => "/account/info",
            ProviderKind::DebridLink => "/account/infos",
            ProviderKind::TorBox => "/user/me",
        };
        let value = self
            .send(self.authorized_request(Method::GET, path, &token))
            .await?;
        Ok(normalize_account(&value))
    }

    async fn availability(&self, hash: &str) -> Result<DebridAvailability, DebridError> {
        let hash = normalized_hash(hash)?;
        let key = AvailabilityCacheKey {
            provider: self.kind,
            credential_ref: self.credential_ref.clone(),
            hash: hash.clone(),
        };
        AVAILABILITY_CACHE
            .try_get_with(key, async {
                let token = self.token().await?;
                let request = match self.kind {
                    ProviderKind::RealDebrid => self.authorized_request(
                        Method::GET,
                        &format!("/torrents/instantAvailability/{hash}"),
                        &token,
                    ),
                    ProviderKind::AllDebrid => self
                        .authorized_request(Method::GET, "/magnet/instant", &token)
                        .query(&[("agent", "StremioNative"), ("magnets[]", hash.as_str())]),
                    ProviderKind::Premiumize => self
                        .authorized_request(Method::GET, "/cache/check", &token)
                        .query(&[("items[]", hash.as_str())]),
                    ProviderKind::DebridLink => self
                        .authorized_request(Method::POST, "/seedbox/cached", &token)
                        .form(&[("url", format!("magnet:?xt=urn:btih:{hash}"))]),
                    ProviderKind::TorBox => self
                        .authorized_request(Method::GET, "/torrents/checkcached", &token)
                        .query(&[("hash", hash.as_str())]),
                };
                let value = self.send(request).await?;
                Ok::<DebridAvailability, DebridError>(parse_availability(self.kind, &hash, &value))
            })
            .await
            .map_err(|error| (*error).clone())
    }

    async fn resolve(&self, source: &str) -> Result<ResolvedLink, DebridError> {
        if source.trim().is_empty() {
            return Err(DebridError::Unsupported);
        }
        let token = self.token().await?;
        let (path, field) = match self.kind {
            ProviderKind::RealDebrid => ("/unrestrict/link", "link"),
            ProviderKind::AllDebrid => ("/link/unlock", "link"),
            ProviderKind::Premiumize => ("/transfer/directdl", "src"),
            ProviderKind::DebridLink => ("/downloader/add", "url"),
            ProviderKind::TorBox => ("/torrents/requestdl", "token"),
        };
        let value = self
            .send(
                self.authorized_request(Method::POST, path, &token)
                    .form(&[(field, source)]),
            )
            .await?;
        parse_resolved_link(&value).ok_or(DebridError::InvalidResponse)
    }
}

fn normalized_hash(hash: &str) -> Result<String, DebridError> {
    let hash = hash.trim().to_ascii_lowercase();
    if matches!(hash.len(), 40 | 64) && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(hash)
    } else {
        Err(DebridError::Unsupported)
    }
}

fn map_transport_error(error: reqwest::Error) -> DebridError {
    if error.is_timeout() {
        DebridError::Timeout
    } else {
        DebridError::Unavailable
    }
}

fn normalize_account(value: &serde_json::Value) -> AccountStatus {
    let data = value.get("data").unwrap_or(value);
    AccountStatus {
        username: ["username", "email", "user"]
            .into_iter()
            .find_map(|key| data.get(key).and_then(serde_json::Value::as_str))
            .map(ToOwned::to_owned),
        premium: data
            .get("premium")
            .or_else(|| data.get("isPremium"))
            .and_then(serde_json::Value::as_bool)
            .or_else(|| {
                data.get("type")
                    .and_then(serde_json::Value::as_str)
                    .map(|value| value.eq_ignore_ascii_case("premium"))
            })
            .unwrap_or(false),
        expires_at: data
            .get("expiration")
            .or_else(|| data.get("premium_until"))
            .and_then(serde_json::Value::as_i64),
    }
}

fn parse_availability(
    kind: ProviderKind,
    hash: &str,
    value: &serde_json::Value,
) -> DebridAvailability {
    let data = value.get("data").unwrap_or(value);
    let cached = match kind {
        ProviderKind::RealDebrid => data
            .get(hash)
            .and_then(serde_json::Value::as_object)
            .is_some_and(|entry| !entry.is_empty()),
        ProviderKind::Premiumize => data
            .get("response")
            .and_then(serde_json::Value::as_array)
            .and_then(|items| items.first())
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        _ => data
            .get("cached")
            .or_else(|| data.get("instant"))
            .or_else(|| data.get("available"))
            .and_then(serde_json::Value::as_bool)
            .or_else(|| {
                data.as_array()
                    .and_then(|items| items.first())
                    .and_then(|item| item.get("instant").or_else(|| item.get("cached")))
                    .and_then(serde_json::Value::as_bool)
            })
            .unwrap_or(false),
    };
    if cached {
        DebridAvailability::Cached
    } else {
        DebridAvailability::Uncached
    }
}

fn parse_resolved_link(value: &serde_json::Value) -> Option<ResolvedLink> {
    let data = value.get("data").unwrap_or(value);
    let url = ["download", "link", "url", "direct_link"]
        .into_iter()
        .find_map(|key| data.get(key).and_then(serde_json::Value::as_str))?;
    url::Url::parse(url).ok()?;
    Some(ResolvedLink {
        url: url.to_owned(),
        filename: data
            .get("filename")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned),
        size_bytes: data
            .get("filesize")
            .or_else(|| data.get("size"))
            .and_then(serde_json::Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn hashes_are_validated_without_exposing_them_in_errors() {
        assert!(normalized_hash(&"a".repeat(40)).is_ok());
        assert_eq!(
            normalized_hash("secret=not-a-hash"),
            Err(DebridError::Unsupported)
        );
    }

    #[test]
    fn parses_common_cached_responses() {
        assert_eq!(
            parse_availability(
                ProviderKind::Premiumize,
                &"a".repeat(40),
                &serde_json::json!({"response": [true]})
            ),
            DebridAvailability::Cached
        );
    }

    #[test]
    fn availability_cache_key_isolates_providers_and_credentials() {
        let hash = "a".repeat(40);
        let first = AvailabilityCacheKey {
            provider: ProviderKind::RealDebrid,
            credential_ref: CredentialRef::new("debrid/profile-a").expect("valid reference"),
            hash: hash.clone(),
        };
        let other_provider = AvailabilityCacheKey {
            provider: ProviderKind::AllDebrid,
            ..first.clone()
        };
        let other_credential = AvailabilityCacheKey {
            credential_ref: CredentialRef::new("debrid/profile-b").expect("valid reference"),
            ..first.clone()
        };

        assert_ne!(first, other_provider);
        assert_ne!(first, other_credential);
    }

    #[test]
    fn availability_cache_uses_balanced_policy() {
        let policy = AVAILABILITY_CACHE.policy();

        assert_eq!(policy.max_capacity(), Some(CACHE_CAPACITY));
        assert_eq!(policy.time_to_live(), Some(CACHE_TTL));
    }

    #[tokio::test]
    async fn identical_availability_lookups_are_single_flight() {
        let cache = Cache::builder().max_capacity(16).build();
        let requests = Arc::new(AtomicUsize::new(0));
        let key = AvailabilityCacheKey {
            provider: ProviderKind::TorBox,
            credential_ref: CredentialRef::new("debrid/single-flight").expect("valid ref"),
            hash: "a".repeat(40),
        };
        let first_requests = requests.clone();
        let second_requests = requests.clone();
        let first = cache.try_get_with(key.clone(), async move {
            first_requests.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(25)).await;
            Ok::<_, DebridError>(DebridAvailability::Cached)
        });
        let second = cache.try_get_with(key, async move {
            second_requests.fetch_add(1, Ordering::SeqCst);
            Ok::<_, DebridError>(DebridAvailability::Uncached)
        });

        let (first, second) = tokio::join!(first, second);
        assert_eq!(first.expect("first"), second.expect("second"));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_availability_lookup_is_not_cached() {
        let cache = Cache::builder().max_capacity(16).build();
        let key = AvailabilityCacheKey {
            provider: ProviderKind::Premiumize,
            credential_ref: CredentialRef::new("debrid/error-retry").expect("valid ref"),
            hash: "b".repeat(40),
        };
        let first = cache
            .try_get_with(key.clone(), async {
                Err::<DebridAvailability, _>(DebridError::Unavailable)
            })
            .await;
        let second = cache
            .try_get_with(key, async {
                Ok::<_, DebridError>(DebridAvailability::Cached)
            })
            .await;

        assert!(first.is_err());
        assert_eq!(second.expect("retry result"), DebridAvailability::Cached);
    }

    #[tokio::test]
    async fn availability_cache_can_be_invalidated_after_credential_changes() {
        let key = AvailabilityCacheKey {
            provider: ProviderKind::DebridLink,
            credential_ref: CredentialRef::new("debrid/invalidation").expect("valid ref"),
            hash: "c".repeat(40),
        };
        AVAILABILITY_CACHE
            .insert(key.clone(), DebridAvailability::Cached)
            .await;

        invalidate_availability_cache();

        assert!(AVAILABILITY_CACHE.get(&key).await.is_none());
    }
}
