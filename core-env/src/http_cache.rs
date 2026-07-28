use std::{
    collections::HashSet,
    net::IpAddr,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use base64::Engine as _;
use http::{HeaderMap, Method, request::Parts};
use serde::Deserialize;
use stremio_core::runtime::EnvError;

use crate::{get_db_conn, sanitized_url_for_log, spawn_on_runtime};

const MAX_CACHE_BYTES: i64 = 256 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
const MAX_AGE_SECONDS: i64 = 30 * 24 * 60 * 60;
const REVALIDATE_AFTER_SECONDS: i64 = 60;
const MAINTENANCE_WRITE_INTERVAL: usize = 64;

static REVALIDATING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static WRITES_SINCE_MAINTENANCE: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResourceKind {
    Manifest,
    Catalog,
    AddonCatalog,
    Meta,
    LegacyMeta,
}

impl ResourceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Catalog => "catalog",
            Self::AddonCatalog => "addon_catalog",
            Self::Meta => "meta",
            Self::LegacyMeta => "legacy_meta",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CacheRequest {
    cache_key: String,
    resource_kind: ResourceKind,
    url: String,
    headers: HeaderMap,
}

impl CacheRequest {
    pub(crate) fn classify(parts: &Parts) -> Option<Self> {
        if parts.method != Method::GET
            || parts.headers.contains_key(http::header::AUTHORIZATION)
            || parts.headers.contains_key(http::header::COOKIE)
        {
            return None;
        }

        let mut url = url::Url::parse(&parts.uri.to_string()).ok()?;
        if !matches!(url.scheme(), "http" | "https") || is_loopback(&url) {
            return None;
        }
        url.set_fragment(None);

        let resource_kind = classify_path(&url)?;
        let canonical_url = url.to_string();
        let cache_key = cache_key(&canonical_url, &parts.headers);
        Some(Self {
            cache_key,
            resource_kind,
            url: canonical_url,
            headers: parts.headers.clone(),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HttpCacheEntry {
    resource_kind: String,
    status: i64,
    body: Vec<u8>,
    etag: Option<String>,
    last_modified: Option<String>,
    stored_at: i64,
    validated_at: i64,
}

impl HttpCacheEntry {
    fn validation_time(&self) -> i64 {
        if self.validated_at == 0 {
            self.stored_at
        } else {
            self.validated_at
        }
    }

    fn is_expired(&self, now: i64) -> bool {
        now.saturating_sub(self.validation_time()) > MAX_AGE_SECONDS
    }

    fn should_revalidate(&self, now: i64) -> bool {
        self.validated_at == 0 || now.saturating_sub(self.validated_at) >= REVALIDATE_AFTER_SECONDS
    }
}

#[derive(Debug)]
struct NetworkResponse {
    status: u16,
    body: Vec<u8>,
    etag: Option<String>,
    last_modified: Option<String>,
    no_store: bool,
    no_cache: bool,
    vary_star: bool,
}

pub(crate) async fn fetch<OUT>(
    request: CacheRequest,
    client: reqwest::Client,
) -> Result<OUT, EnvError>
where
    for<'de> OUT: Deserialize<'de> + Send + 'static,
{
    let now = chrono::Utc::now().timestamp();
    match read_entry(&request.cache_key).await {
        Ok(Some(entry))
            if entry.resource_kind == request.resource_kind.as_str()
                && (200..300).contains(&entry.status)
                && !entry.is_expired(now) =>
        {
            match serde_json::from_slice::<OUT>(&entry.body) {
                Ok(value) => {
                    schedule_access_touch(request.cache_key.clone(), now);
                    if entry.should_revalidate(now) {
                        schedule_revalidation::<OUT>(request, entry, client);
                    }
                    return Ok(value);
                }
                Err(error) => {
                    tracing::debug!(%error, "discarding corrupt cached JSON response");
                    if let Err(error) = delete_entry(&request.cache_key).await {
                        tracing::debug!(%error, "corrupt HTTP cache entry deletion failed");
                    }
                }
            }
        }
        Ok(Some(_)) => {
            if let Err(error) = delete_entry(&request.cache_key).await {
                tracing::debug!(%error, "expired HTTP cache entry deletion failed");
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::debug!(%error, "HTTP cache read unavailable; using network");
        }
    }

    let response = send_request(&client, &request, None).await?;
    let value = serde_json::from_slice::<OUT>(&response.body)
        .map_err(|error| EnvError::Fetch(error.to_string()))?;

    if response_is_storable(&response) {
        let stored_at = chrono::Utc::now().timestamp();
        spawn_on_runtime(async move {
            if let Err(error) = write_response(&request, response, stored_at).await {
                tracing::debug!(%error, "HTTP cache write failed");
            }
        });
    }

    Ok(value)
}

fn schedule_revalidation<OUT>(request: CacheRequest, entry: HttpCacheEntry, client: reqwest::Client)
where
    for<'de> OUT: Deserialize<'de> + Send + 'static,
{
    let Some(guard) = RevalidationGuard::acquire(request.cache_key.clone()) else {
        return;
    };

    spawn_on_runtime(async move {
        let _guard = guard;
        let validators = Validators {
            etag: entry.etag.clone(),
            last_modified: entry.last_modified.clone(),
        };
        let response = match send_request(&client, &request, Some(&validators)).await {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(%error, "HTTP cache background revalidation failed");
                return;
            }
        };
        let now = chrono::Utc::now().timestamp();

        if response.no_store || response.vary_star {
            if let Err(error) = delete_entry(&request.cache_key).await {
                tracing::debug!(%error, "HTTP cache removal after cache-control failed");
            }
            return;
        }

        if response.status == reqwest::StatusCode::NOT_MODIFIED.as_u16() {
            let validated_at = if entry.validated_at == 0 || response.no_cache {
                0
            } else {
                now
            };
            if let Err(error) = mark_validated(
                &request.cache_key,
                validated_at,
                now,
                response.etag.as_deref(),
                response.last_modified.as_deref(),
            )
            .await
            {
                tracing::debug!(%error, "HTTP cache validation timestamp update failed");
            }
            return;
        }

        if !response_is_storable(&response)
            || serde_json::from_slice::<OUT>(&response.body).is_err()
        {
            return;
        }

        if let Err(error) = write_response(&request, response, now).await {
            tracing::debug!(%error, "HTTP cache revalidation write failed");
        }
    });
}

#[derive(Debug)]
struct Validators {
    etag: Option<String>,
    last_modified: Option<String>,
}

async fn send_request(
    client: &reqwest::Client,
    request: &CacheRequest,
    validators: Option<&Validators>,
) -> Result<NetworkResponse, EnvError> {
    let log_url = sanitized_url_for_log(&request.url);
    let mut builder = client.get(&request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    if let Some(validators) = validators {
        if let Some(etag) = validators.etag.as_deref() {
            builder = builder.header(reqwest::header::IF_NONE_MATCH, etag);
        } else if let Some(last_modified) = validators.last_modified.as_deref() {
            builder = builder.header(reqwest::header::IF_MODIFIED_SINCE, last_modified);
        }
    }

    tracing::debug!(method = "GET", url = %log_url, "Sending Core API request");
    let start = std::time::Instant::now();
    let response = builder.send().await.map_err(|error| {
        let error = error.without_url();
        tracing::error!(url = %log_url, error = %error, "Core API request failed");
        EnvError::Fetch(error.to_string())
    })?;
    let elapsed = start.elapsed().as_millis();
    let status = response.status();
    if elapsed > 300 {
        tracing::warn!(url = %log_url, %status, elapsed_ms = elapsed, "Core API request took longer than threshold");
    } else {
        tracing::debug!(url = %log_url, %status, elapsed_ms = elapsed, "Core API request completed");
    }

    let headers = response.headers();
    let etag = header_string(headers, reqwest::header::ETAG);
    let last_modified = header_string(headers, reqwest::header::LAST_MODIFIED);
    let cache_control = header_string(headers, reqwest::header::CACHE_CONTROL)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let vary_star = headers
        .get_all(reqwest::header::VARY)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim() == "*");
    let body = response
        .bytes()
        .await
        .map_err(|error| EnvError::Fetch(error.without_url().to_string()))?
        .to_vec();

    Ok(NetworkResponse {
        status: status.as_u16(),
        body,
        etag,
        last_modified,
        no_store: directive_present(&cache_control, "no-store"),
        no_cache: directive_present(&cache_control, "no-cache"),
        vary_star,
    })
}

fn response_is_storable(response: &NetworkResponse) -> bool {
    (200..300).contains(&response.status)
        && response.body.len() <= MAX_ENTRY_BYTES
        && !response.no_store
        && !response.vary_star
}

fn header_string(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn directive_present(value: &str, directive: &str) -> bool {
    value
        .split(',')
        .filter_map(|part| part.split(';').next())
        .filter_map(|part| part.trim().split('=').next())
        .any(|part| part == directive)
}

async fn read_entry(cache_key: &str) -> Result<Option<HttpCacheEntry>, String> {
    let conn = get_db_conn().await.map_err(|error| error.to_string())?;
    let mut rows = conn
        .query(
            "SELECT resource_kind, status, body, etag, last_modified, stored_at, validated_at
             FROM http_cache WHERE cache_key = ?",
            [cache_key],
        )
        .await
        .map_err(|error| error.to_string())?;
    let Some(row) = rows.next().await.map_err(|error| error.to_string())? else {
        return Ok(None);
    };

    Ok(Some(HttpCacheEntry {
        resource_kind: row.get(0).map_err(|error| error.to_string())?,
        status: row.get(1).map_err(|error| error.to_string())?,
        body: row.get(2).map_err(|error| error.to_string())?,
        etag: row.get(3).map_err(|error| error.to_string())?,
        last_modified: row.get(4).map_err(|error| error.to_string())?,
        stored_at: row.get(5).map_err(|error| error.to_string())?,
        validated_at: row.get(6).map_err(|error| error.to_string())?,
    }))
}

async fn write_response(
    request: &CacheRequest,
    response: NetworkResponse,
    now: i64,
) -> Result<(), String> {
    let conn = get_db_conn().await.map_err(|error| error.to_string())?;
    let size_bytes = i64::try_from(response.body.len()).map_err(|error| error.to_string())?;
    let validated_at = if response.no_cache { 0 } else { now };
    conn.execute(
        "INSERT INTO http_cache (
            cache_key, resource_kind, status, body, etag, last_modified,
            stored_at, validated_at, accessed_at, size_bytes
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(cache_key) DO UPDATE SET
            resource_kind = excluded.resource_kind,
            status = excluded.status,
            body = excluded.body,
            etag = excluded.etag,
            last_modified = excluded.last_modified,
            stored_at = excluded.stored_at,
            validated_at = excluded.validated_at,
            accessed_at = excluded.accessed_at,
            size_bytes = excluded.size_bytes",
        (
            request.cache_key.clone(),
            request.resource_kind.as_str().to_owned(),
            i64::from(response.status),
            response.body,
            response.etag,
            response.last_modified,
            now,
            validated_at,
            now,
            size_bytes,
        ),
    )
    .await
    .map_err(|error| error.to_string())?;

    if WRITES_SINCE_MAINTENANCE.fetch_add(1, Ordering::Relaxed) % MAINTENANCE_WRITE_INTERVAL
        == MAINTENANCE_WRITE_INTERVAL - 1
        && let Err(error) = maintain().await
    {
        tracing::debug!(%error, "periodic HTTP cache maintenance failed");
    }
    Ok(())
}

fn schedule_access_touch(cache_key: String, now: i64) {
    spawn_on_runtime(async move {
        let Ok(conn) = get_db_conn().await else {
            return;
        };
        if let Err(error) = conn
            .execute(
                "UPDATE http_cache SET accessed_at = ? WHERE cache_key = ?",
                (now, cache_key),
            )
            .await
        {
            tracing::debug!(%error, "HTTP cache access timestamp update failed");
        }
    });
}

async fn delete_entry(cache_key: &str) -> Result<(), String> {
    let conn = get_db_conn().await.map_err(|error| error.to_string())?;
    conn.execute("DELETE FROM http_cache WHERE cache_key = ?", [cache_key])
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn mark_validated(
    cache_key: &str,
    validated_at: i64,
    accessed_at: i64,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<(), String> {
    let conn = get_db_conn().await.map_err(|error| error.to_string())?;
    conn.execute(
        "UPDATE http_cache SET
            validated_at = ?,
            accessed_at = ?,
            stored_at = CASE WHEN ? = 0 THEN ? ELSE stored_at END,
            etag = COALESCE(?, etag),
            last_modified = COALESCE(?, last_modified)
         WHERE cache_key = ?",
        (
            validated_at,
            accessed_at,
            validated_at,
            accessed_at,
            etag,
            last_modified,
            cache_key,
        ),
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub(crate) async fn maintain() -> Result<(), String> {
    let mut conn = get_db_conn().await.map_err(|error| error.to_string())?;
    let cutoff = chrono::Utc::now().timestamp() - MAX_AGE_SECONDS;
    conn.execute(
        "DELETE FROM http_cache
         WHERE (CASE WHEN validated_at = 0 THEN stored_at ELSE validated_at END) < ?",
        [cutoff],
    )
    .await
    .map_err(|error| error.to_string())?;

    let mut total_rows = conn
        .query("SELECT COALESCE(SUM(size_bytes), 0) FROM http_cache", ())
        .await
        .map_err(|error| error.to_string())?;
    let total = total_rows
        .next()
        .await
        .map_err(|error| error.to_string())?
        .map(|row| row.get::<i64>(0))
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    drop(total_rows);
    if total <= MAX_CACHE_BYTES {
        return Ok(());
    }

    let mut rows = conn
        .query(
            "SELECT cache_key, size_bytes FROM http_cache ORDER BY accessed_at ASC",
            (),
        )
        .await
        .map_err(|error| error.to_string())?;
    let mut remaining = total;
    let mut evictions = Vec::new();
    while remaining > MAX_CACHE_BYTES {
        let Some(row) = rows.next().await.map_err(|error| error.to_string())? else {
            break;
        };
        let cache_key: String = row.get(0).map_err(|error| error.to_string())?;
        let size_bytes: i64 = row.get(1).map_err(|error| error.to_string())?;
        remaining = remaining.saturating_sub(size_bytes.max(0));
        evictions.push(cache_key);
    }
    drop(rows);

    let transaction = conn
        .transaction()
        .await
        .map_err(|error| error.to_string())?;
    for cache_key in evictions {
        transaction
            .execute("DELETE FROM http_cache WHERE cache_key = ?", [cache_key])
            .await
            .map_err(|error| error.to_string())?;
    }
    transaction
        .commit()
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn cache_key(canonical_url: &str, headers: &HeaderMap) -> String {
    let mut identity = String::with_capacity(canonical_url.len() + 96);
    identity.push_str("GET\n");
    identity.push_str(canonical_url);
    for name in [http::header::ACCEPT, http::header::ACCEPT_LANGUAGE] {
        identity.push('\n');
        identity.push_str(name.as_str());
        identity.push(':');
        if let Some(value) = headers.get(&name).and_then(|value| value.to_str().ok()) {
            identity.push_str(value.trim());
        }
    }
    blake3::hash(identity.as_bytes()).to_hex().to_string()
}

fn is_loopback(url: &url::Url) -> bool {
    let Some(host) = url.host_str() else {
        return true;
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn classify_path(url: &url::Url) -> Option<ResourceKind> {
    let path = url.path();
    if path.ends_with("/manifest.json") || path == "/manifest.json" {
        return Some(ResourceKind::Manifest);
    }
    if path.ends_with("/q.json") || path == "/q.json" {
        return classify_legacy(url);
    }
    let segments = path.split('/').filter(|segment| !segment.is_empty());
    for segment in segments {
        match segment {
            "catalog" => return Some(ResourceKind::Catalog),
            "addon_catalog" => return Some(ResourceKind::AddonCatalog),
            "meta" => return Some(ResourceKind::Meta),
            _ => {}
        }
    }
    None
}

fn classify_legacy(url: &url::Url) -> Option<ResourceKind> {
    let encoded = url
        .query_pairs()
        .find_map(|(name, value)| (name == "b").then(|| value.into_owned()))?
        .replace(' ', "+");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    match value.get("method").and_then(serde_json::Value::as_str) {
        Some("meta" | "meta.find" | "meta.get") => Some(ResourceKind::LegacyMeta),
        _ => None,
    }
}

struct RevalidationGuard {
    cache_key: String,
}

impl RevalidationGuard {
    fn acquire(cache_key: String) -> Option<Self> {
        let revalidating = REVALIDATING.get_or_init(|| Mutex::new(HashSet::new()));
        let mut revalidating = revalidating
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        revalidating
            .insert(cache_key.clone())
            .then_some(Self { cache_key })
    }
}

impl Drop for RevalidationGuard {
    fn drop(&mut self) {
        let Some(revalidating) = REVALIDATING.get() else {
            return;
        };
        revalidating
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.cache_key);
    }
}

#[cfg(test)]
mod tests {
    use super::{CacheRequest, HttpCacheEntry, ResourceKind, cache_key};
    use base64::Engine as _;
    use http::Request;

    fn classify(url: &str) -> Option<ResourceKind> {
        let (parts, ()) = Request::get(url)
            .body(())
            .expect("valid request")
            .into_parts();
        CacheRequest::classify(&parts).map(|request| request.resource_kind)
    }

    #[test]
    fn classifies_only_the_selected_remote_resources() {
        assert_eq!(
            classify("https://example.com/manifest.json"),
            Some(ResourceKind::Manifest)
        );
        assert_eq!(
            classify("https://example.com/catalog/movie/top.json"),
            Some(ResourceKind::Catalog)
        );
        assert_eq!(
            classify("https://example.com/meta/series/tt1.json"),
            Some(ResourceKind::Meta)
        );
        assert_eq!(classify("https://example.com/stream/movie/tt1.json"), None);
        assert_eq!(
            classify("https://example.com/subtitles/movie/tt1.json"),
            None
        );
        assert_eq!(
            classify("http://127.0.0.1:11470/catalog/movie/top.json"),
            None
        );
    }

    #[test]
    fn authenticated_requests_bypass_cache() {
        let (parts, ()) = Request::get("https://example.com/catalog/movie/top.json")
            .header(http::header::AUTHORIZATION, "Bearer private")
            .body(())
            .expect("valid request")
            .into_parts();

        assert!(CacheRequest::classify(&parts).is_none());
    }

    #[test]
    fn legacy_classifier_accepts_meta_and_rejects_streams() {
        let meta = base64::engine::general_purpose::STANDARD
            .encode(r#"{"jsonrpc":"2.0","method":"meta.find","params":[]}"#);
        let stream = base64::engine::general_purpose::STANDARD
            .encode(r#"{"jsonrpc":"2.0","method":"stream.find","params":[]}"#);

        assert_eq!(
            classify(&format!("https://example.com/q.json?b={meta}")),
            Some(ResourceKind::LegacyMeta)
        );
        assert_eq!(
            classify(&format!("https://example.com/q.json?b={stream}")),
            None
        );
    }

    #[test]
    fn cache_identity_is_hashed_and_representation_sensitive() {
        let mut headers = http::HeaderMap::new();
        let first = cache_key("https://example.com/meta/movie/tt1.json", &headers);
        headers.insert(http::header::ACCEPT_LANGUAGE, "fr".parse().expect("header"));
        let second = cache_key("https://example.com/meta/movie/tt1.json", &headers);

        assert_eq!(first.len(), 64);
        assert_ne!(first, second);
        assert!(!first.contains("example.com"));
    }

    #[test]
    fn no_cache_entries_revalidate_on_every_hit_without_immediate_expiry() {
        let entry = HttpCacheEntry {
            resource_kind: "meta".to_owned(),
            status: 200,
            body: Vec::new(),
            etag: None,
            last_modified: None,
            stored_at: 10_000,
            validated_at: 0,
        };

        assert!(entry.should_revalidate(10_001));
        assert!(!entry.is_expired(10_001));
    }
}
