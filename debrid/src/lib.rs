use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use credential_store::{CredentialError, CredentialRef, CredentialStore};
use reqwest::{Client, Method, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

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
    permits: Arc<Semaphore>,
    cache: Arc<Mutex<HashMap<String, (Instant, DebridAvailability)>>>,
}

impl HttpDebridProvider {
    pub fn new(
        kind: ProviderKind,
        credential_ref: CredentialRef,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Self, DebridError> {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent("Stremio-Native/1")
            .build()
            .map_err(|_| DebridError::Unavailable)?;
        Ok(Self {
            kind,
            credential_ref,
            credentials,
            client,
            permits: Arc::new(Semaphore::new(4)),
            cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    async fn token(&self) -> Result<String, DebridError> {
        let secret = self.credentials.get(&self.credential_ref).await?;
        String::from_utf8(secret.expose().to_vec()).map_err(|_| DebridError::InvalidResponse)
    }

    async fn send(&self, request: RequestBuilder) -> Result<serde_json::Value, DebridError> {
        let _permit = self
            .permits
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
        if let Some((cached_at, availability)) = self.cache.lock().await.get(&hash).copied()
            && cached_at.elapsed() < CACHE_TTL
        {
            return Ok(availability);
        }
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
        let availability = parse_availability(self.kind, &hash, &value);
        self.cache
            .lock()
            .await
            .insert(hash, (Instant::now(), availability));
        Ok(availability)
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
}
