use chrono::{DateTime, Utc};
use futures::{FutureExt, future};
use http::Request;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use tokio::runtime::Handle;
use tokio::sync::{OnceCell, mpsc};

use stremio_core::{
    models::{ctx::Ctx, streaming_server::StreamingServer},
    runtime::{Env, EnvError, EnvFuture, TryEnvFuture},
};

mod http_cache;

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
static TOKIO_HANDLE: OnceLock<Handle> = OnceLock::new();
#[cfg(feature = "in-process")]
static IN_PROCESS_ROUTER: OnceLock<axum::Router> = OnceLock::new();
type SequentialFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
static SEQUENTIAL_EXECUTOR: OnceLock<mpsc::UnboundedSender<SequentialFuture>> = OnceLock::new();

/// Registers the application runtime so core work can be scheduled safely from
/// native callback threads such as the libmpv actor.
pub fn install_runtime_handle(handle: Handle) {
    let _ = TOKIO_HANDLE.set(handle.clone());
    let _ = SEQUENTIAL_EXECUTOR.get_or_init(|| start_sequential_executor(&handle));
}

pub fn spawn_on_runtime(future: impl Future<Output = ()> + Send + 'static) {
    if let Some(handle) = TOKIO_HANDLE.get() {
        drop(handle.spawn(future));
    } else if let Ok(handle) = Handle::try_current() {
        drop(handle.spawn(future));
    } else {
        tracing::error!("cannot schedule core future because no Tokio runtime is registered");
    }
}

fn start_sequential_executor(handle: &Handle) -> mpsc::UnboundedSender<SequentialFuture> {
    let (sender, receiver) = mpsc::unbounded_channel();
    drop(handle.spawn(run_sequential(receiver)));
    sender
}

fn sequential_executor() -> Option<&'static mpsc::UnboundedSender<SequentialFuture>> {
    if let Some(sender) = SEQUENTIAL_EXECUTOR.get() {
        return Some(sender);
    }

    let handle = TOKIO_HANDLE
        .get()
        .cloned()
        .or_else(|| Handle::try_current().ok())?;
    Some(SEQUENTIAL_EXECUTOR.get_or_init(|| start_sequential_executor(&handle)))
}

async fn run_sequential(mut receiver: mpsc::UnboundedReceiver<SequentialFuture>) {
    while let Some(future) = receiver.recv().await {
        future.await;
    }
}

fn get_http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Origin", "https://app.strem.io".parse().unwrap());
        headers.insert("Referer", "https://app.strem.io/".parse().unwrap());

        reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Stremio/4.4.168 Chrome/110.0.0.0 Safari/537.36")
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(15))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client")
    })
}

pub(crate) fn sanitized_url_for_log(raw: &str) -> String {
    let Ok(parsed) = url::Url::parse(raw) else {
        return "<invalid-url>".to_owned();
    };
    let origin = parsed.origin().ascii_serialization();
    if parsed.path() == "/" {
        format!("{origin}/")
    } else {
        format!("{origin}/<redacted>")
    }
}

pub struct DesktopEnv;

impl DesktopEnv {
    #[allow(unused)]
    async fn fetch_in_process<IN, OUT>(request: Request<IN>) -> Result<OUT, EnvError>
    where
        IN: Serialize + Send + 'static,
        for<'de> OUT: Deserialize<'de> + Send + 'static,
    {
        #[cfg(feature = "in-process")]
        {
            use tower::ServiceExt;

            let router = in_process_router()?;

            // Construct the Tower request
            let (parts, body) = request.into_parts();
            let body_bytes = if matches!(parts.method, http::Method::GET | http::Method::HEAD) {
                Vec::new()
            } else {
                serde_json::to_vec(&body).map_err(|e| EnvError::Serde(e.to_string()))?
            };
            let axum_req = Request::from_parts(parts, axum::body::Body::from(body_bytes));

            // Call the router in-memory
            let response = router
                .oneshot(axum_req)
                .await
                .map_err(|e| EnvError::Fetch(e.to_string()))?;

            // Extract the body
            let body_data = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .map_err(|e| EnvError::Fetch(e.to_string()))?;

            let result: OUT =
                serde_json::from_slice(&body_data).map_err(|e| EnvError::Serde(e.to_string()))?;

            Ok(result)
        }
        #[cfg(not(feature = "in-process"))]
        {
            Err(EnvError::Other(
                "in-process feature is not enabled".to_string(),
            ))
        }
    }

    async fn fetch_http<IN, OUT>(request: Request<IN>) -> Result<OUT, EnvError>
    where
        IN: Serialize + Send + 'static,
        for<'de> OUT: Deserialize<'de> + Send + 'static,
    {
        let (parts, body) = request.into_parts();
        let client = get_http_client();
        if let Some(cache_request) = http_cache::CacheRequest::classify(&parts) {
            return http_cache::fetch(cache_request, client.clone()).await;
        }
        let method = match parts.method {
            http::Method::GET => reqwest::Method::GET,
            http::Method::POST => reqwest::Method::POST,
            http::Method::PUT => reqwest::Method::PUT,
            http::Method::DELETE => reqwest::Method::DELETE,
            http::Method::HEAD => reqwest::Method::HEAD,
            _ => reqwest::Method::GET,
        };

        let url_str = parts.uri.to_string();
        let log_url = sanitized_url_for_log(&url_str);
        tracing::debug!(method = ?parts.method, url = %log_url, "Sending Core API request");

        let mut req_builder = client.request(method, &url_str);

        for (key, val) in parts.headers.iter() {
            req_builder = req_builder.header(key.as_str(), val.as_bytes());
        }

        if parts.method != http::Method::GET {
            req_builder = req_builder.json(&body);
        }

        let start = std::time::Instant::now();
        let resp = req_builder.send().await.map_err(|error| {
            let error = error.without_url();
            tracing::error!(url = %log_url, error = %error, "Core API request failed");
            EnvError::Fetch(error.to_string())
        })?;

        let elapsed = start.elapsed().as_millis();
        if elapsed > 300 {
            tracing::warn!(
                url = %log_url,
                status = %resp.status(),
                elapsed_ms = elapsed,
                "Core API request took longer than threshold"
            );
        } else {
            tracing::debug!(
                url = %log_url,
                status = %resp.status(),
                elapsed_ms = elapsed,
                "Core API request completed"
            );
        }

        let val: OUT = resp
            .json()
            .await
            .map_err(|error| EnvError::Fetch(error.without_url().to_string()))?;

        Ok(val)
    }
}

static DB: OnceLock<turso::Database> = OnceLock::new();
static DB_CONNECTION: OnceCell<turso::Connection> = OnceCell::const_new();

#[cfg(feature = "in-process")]
fn in_process_router() -> Result<axum::Router, EnvError> {
    if let Some(router) = IN_PROCESS_ROUTER.get() {
        return Ok(router.clone());
    }

    let app_state = {
        let guard = stream_server::GLOBAL_STATE
            .read()
            .map_err(|e| EnvError::Other(format!("Failed to read GLOBAL_STATE lock: {e}")))?;
        guard.clone().ok_or_else(|| {
            EnvError::Other("stream-server AppState is not initialized".to_owned())
        })?
    };
    let router = stream_server::build_router(app_state);
    let _ = IN_PROCESS_ROUTER.set(router);
    Ok(IN_PROCESS_ROUTER
        .get()
        .expect("in-process router was just initialized")
        .clone())
}

/// Returned when a database is installed after core storage has already
/// initialized its database handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseAlreadyInstalled;

impl std::fmt::Display for DatabaseAlreadyInstalled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("core database is already installed")
    }
}

impl std::error::Error for DatabaseAlreadyInstalled {}

/// Shares the application's Turso database and its internal connection pool
/// with the Stremio core storage environment.
pub fn install_database(database: turso::Database) -> Result<(), DatabaseAlreadyInstalled> {
    DB.set(database).map_err(|_| DatabaseAlreadyInstalled)
}

pub(crate) async fn get_db_conn() -> Result<turso::Connection, EnvError> {
    let conn = DB_CONNECTION
        .get_or_try_init(|| async {
            let db = DB.get().ok_or_else(|| {
                EnvError::Other(
                    "application database must be installed before core storage is used".to_owned(),
                )
            })?;
            let conn = db.connect().map_err(|e| EnvError::Other(e.to_string()))?;
            conn.execute_batch(
                "PRAGMA synchronous = NORMAL;
                 PRAGMA temp_store = MEMORY;
                 PRAGMA cache_size = -10000;
                 PRAGMA busy_timeout = 5000;",
            )
            .await
            .map_err(|e| EnvError::Other(e.to_string()))?;
            Ok::<turso::Connection, EnvError>(conn)
        })
        .await?;

    Ok(conn.clone())
}

/// Runs bounded age/LRU maintenance for the shared HTTP response cache.
pub async fn maintain_http_cache() -> Result<(), EnvError> {
    http_cache::maintain().await.map_err(EnvError::Other)
}

impl Env for DesktopEnv {
    fn fetch<IN, OUT>(request: Request<IN>) -> TryEnvFuture<OUT>
    where
        IN: Serialize + Send + 'static,
        for<'de> OUT: Deserialize<'de> + Send + 'static,
    {
        let uri = request.uri().clone();
        let is_local = uri.host() == Some("127.0.0.1") || uri.host() == Some("localhost");

        if is_local && cfg!(feature = "in-process") {
            Self::fetch_in_process(request).boxed()
        } else {
            Self::fetch_http(request).boxed()
        }
    }

    fn get_storage<T: for<'de> Deserialize<'de> + Send + 'static>(
        key: &str,
    ) -> TryEnvFuture<Option<T>> {
        let key = key.to_owned();
        async move {
            let conn = get_db_conn().await?;
            let mut rows = conn
                .query("SELECT value FROM core_storage WHERE key = ?", [key])
                .await
                .map_err(|e| EnvError::StorageReadError(e.to_string()))?;

            if let Some(row) = rows
                .next()
                .await
                .map_err(|e| EnvError::StorageReadError(e.to_string()))?
            {
                let value_str: String = row
                    .get(0)
                    .map_err(|e| EnvError::StorageReadError(e.to_string()))?;
                let val: T =
                    serde_json::from_str(&value_str).map_err(|e| EnvError::Serde(e.to_string()))?;
                Ok(Some(val))
            } else {
                Ok(None)
            }
        }
        .boxed()
    }

    fn set_storage<T: Serialize>(key: &str, value: Option<&T>) -> TryEnvFuture<()> {
        let key = key.to_owned();
        let value_str = match value {
            Some(v) => match serde_json::to_string(v) {
                Ok(s) => Some(s),
                Err(e) => return future::ready(Err(EnvError::Serde(e.to_string()))).boxed(),
            },
            None => None,
        };
        async move {
            let conn = get_db_conn().await?;
            if let Some(val) = value_str {
                conn.execute(
                    "INSERT OR REPLACE INTO core_storage (key, value) VALUES (?, ?)",
                    (key, val),
                )
                .await
                .map_err(|e| EnvError::StorageWriteError(e.to_string()))?;
            } else {
                conn.execute("DELETE FROM core_storage WHERE key = ?", [key])
                    .await
                    .map_err(|e| EnvError::StorageWriteError(e.to_string()))?;
            }
            Ok(())
        }
        .boxed()
    }

    fn exec_concurrent<F: Future<Output = ()> + Send + 'static>(future: F) {
        spawn_on_runtime(future);
    }

    fn exec_sequential<F: Future<Output = ()> + Send + 'static>(future: F) {
        let future: SequentialFuture = Box::pin(future);
        match sequential_executor() {
            Some(sender) => {
                if let Err(error) = sender.send(future) {
                    tracing::error!(
                        "sequential core executor stopped; scheduling the pending effect directly"
                    );
                    spawn_on_runtime(error.0);
                }
            }
            None => {
                tracing::error!(
                    "cannot schedule sequential core future because no Tokio runtime is registered"
                );
            }
        }
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn flush_analytics() -> EnvFuture<'static, ()> {
        future::ready(()).boxed()
    }

    fn analytics_context(
        _ctx: &Ctx,
        _streaming_server: &StreamingServer,
        _path: &str,
    ) -> serde_json::Value {
        serde_json::Value::Null
    }

    #[cfg(debug_assertions)]
    fn log(message: String) {
        tracing::info!("{}", message);
    }
}

#[cfg(test)]
mod tests {
    use super::{SequentialFuture, run_sequential, sanitized_url_for_log};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn sequential_executor_awaits_each_effect_in_submission_order() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<SequentialFuture>();
        let order = Arc::new(Mutex::new(Vec::new()));
        let worker = tokio::spawn(run_sequential(receiver));

        let first_order = order.clone();
        assert!(
            sender
                .send(Box::pin(async move {
                    tokio::task::yield_now().await;
                    first_order.lock().unwrap().push(1);
                }))
                .is_ok()
        );
        let second_order = order.clone();
        assert!(
            sender
                .send(Box::pin(async move {
                    second_order.lock().unwrap().push(2);
                }))
                .is_ok()
        );
        drop(sender);

        worker.await.unwrap();
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    }

    #[test]
    fn logged_urls_hide_credentials_paths_and_queries() {
        let logged = sanitized_url_for_log(
            "https://user:secret@example.com/addon/token/manifest.json?auth=private",
        );

        assert_eq!(logged, "https://example.com/<redacted>");
        assert!(!logged.contains("secret") && !logged.contains("private"));
    }

    #[test]
    fn invalid_logged_urls_do_not_echo_input() {
        assert_eq!(
            sanitized_url_for_log("definitely not a URL"),
            "<invalid-url>"
        );
    }
}
