use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPayload {
    #[serde(rename = "cacheRoot", default)]
    pub cache_root: String,
    #[serde(rename = "cacheSize", default)]
    pub cache_size: f64,
    #[serde(rename = "proxyStreamsEnabled", default)]
    pub proxy_streams_enabled: bool,
    #[serde(rename = "btMaxConnections", default)]
    pub bt_max_connections: u64,
    #[serde(rename = "btHandshakeTimeout", default)]
    pub bt_handshake_timeout: u64,
    #[serde(rename = "btRequestTimeout", default)]
    pub bt_request_timeout: u64,
    #[serde(rename = "btDownloadSpeedSoftLimit", default)]
    pub bt_download_speed_soft_limit: f64,
    #[serde(rename = "btDownloadSpeedHardLimit", default)]
    pub bt_download_speed_hard_limit: f64,
    #[serde(rename = "btMinPeersForStable", default)]
    pub bt_min_peers_for_stable: u64,
    #[serde(rename = "btEnableDht", default)]
    pub bt_enable_dht: bool,
    #[serde(rename = "btEnablePex", default)]
    pub bt_enable_pex: bool,
    #[serde(rename = "btEnableLsd", default)]
    pub bt_enable_lsd: bool,
    #[serde(rename = "btEncryptionMode", default)]
    pub bt_encryption_mode: String,
    #[serde(rename = "btAnonymousMode", default)]
    pub bt_anonymous_mode: bool,
    #[serde(rename = "btAllowMultipleConnectionsPerIp", default)]
    pub bt_allow_multiple_connections_per_ip: bool,
    #[serde(rename = "btListenInterfaces", default)]
    pub bt_listen_interfaces: String,
    #[serde(rename = "btOutgoingInterfaces", default)]
    pub bt_outgoing_interfaces: String,
    #[serde(rename = "btOutgoingPort", default)]
    pub bt_outgoing_port: u16,
    #[serde(rename = "btNumOutgoingPorts", default)]
    pub bt_num_outgoing_ports: u16,
    #[serde(rename = "btProxyType", default)]
    pub bt_proxy_type: String,
    #[serde(rename = "btProxyHost", default)]
    pub bt_proxy_host: String,
    #[serde(rename = "btProxyPort", default)]
    pub bt_proxy_port: u16,
    #[serde(rename = "btProxyUsername", default)]
    pub bt_proxy_username: String,
    #[serde(rename = "btProxyPassword", default)]
    pub bt_proxy_password: String,
    #[serde(rename = "btProxyHostnames", default)]
    pub bt_proxy_hostnames: bool,
    #[serde(rename = "btProxyPeerConnections", default)]
    pub bt_proxy_peer_connections: bool,
    #[serde(rename = "btProxyTrackerConnections", default)]
    pub bt_proxy_tracker_connections: bool,
    #[serde(rename = "btProxySendHostInConnect", default)]
    pub bt_proxy_send_host_in_connect: bool,
    #[serde(rename = "btValidateHttpsTrackers", default)]
    pub bt_validate_https_trackers: bool,
    #[serde(rename = "btSsrfMitigation", default)]
    pub bt_ssrf_mitigation: bool,
    #[serde(rename = "autoUpdateEnabled", default)]
    pub auto_update_enabled: bool,
    #[serde(rename = "updateChannel", default)]
    pub update_channel: String,
    #[serde(rename = "updateCheckIntervalHours", default)]
    pub update_check_interval_hours: u64,
    #[serde(rename = "seedingEnabled", default)]
    pub seeding_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogsSnapshot {
    #[serde(default, alias = "logDir")]
    pub log_dir: String,
    #[serde(default, alias = "currentHumanLog")]
    pub current_human_log: Option<String>,
    #[serde(default, alias = "currentJsonLog")]
    pub current_json_log: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentLogTail {
    #[serde(default)]
    pub path: Option<String>,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangelogPayload {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub tag: Option<String>,
    pub release_url: Option<String>,
    pub published_at: Option<String>,
    pub channel: String,
    pub state: String,
    pub sections: Vec<ChangelogSectionPayload>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangelogSectionPayload {
    pub title: String,
    pub items: Vec<String>,
}

#[async_trait::async_trait]
pub trait ServerConnector: Send + Sync + 'static {
    async fn get_settings(&self) -> Result<SettingsPayload>;
    async fn apply_settings(&self, settings: SettingsPayload) -> Result<()>;
    async fn get_logs(&self) -> Result<LogsSnapshot>;
    async fn get_current_log(&self) -> Result<CurrentLogTail>;
    async fn get_changelog(&self, force: bool) -> Result<ChangelogPayload>;
    async fn export_diagnostics(&self) -> Result<Vec<u8>>;
}

#[cfg(feature = "in-process")]
static IN_PROCESS_ROUTER: std::sync::OnceLock<axum::Router> = std::sync::OnceLock::new();

#[cfg(feature = "in-process")]
fn in_process_router() -> Result<axum::Router> {
    if let Some(router) = IN_PROCESS_ROUTER.get() {
        return Ok(router.clone());
    }

    let app_state = {
        let guard = stream_server::GLOBAL_STATE
            .read()
            .map_err(|e| anyhow::anyhow!("Failed to read GLOBAL_STATE lock: {e}"))?;
        guard
            .clone()
            .ok_or_else(|| anyhow::anyhow!("stream-server AppState is not initialized"))?
    };
    let router = stream_server::build_router(app_state);
    let _ = IN_PROCESS_ROUTER.set(router);
    Ok(IN_PROCESS_ROUTER
        .get()
        .expect("in-process router was just initialized")
        .clone())
}

pub struct AppServerConnector {
    server_url: String,
    #[allow(unused)]
    http_client: reqwest::Client,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsSnapshot {
    #[serde(flatten)]
    pub settings: SettingsPayload,
    #[serde(default)]
    pub server_version: String,
}

impl AppServerConnector {
    pub fn new(server_url: String) -> Self {
        Self {
            server_url: server_url.trim_end_matches('/').to_string(),
            http_client: reqwest::Client::new(),
        }
    }

    pub async fn get_settings_snapshot(&self) -> Result<SettingsSnapshot> {
        if cfg!(feature = "in-process") {
            self.dispatch_in_process(http::Method::GET, "/settings", None::<()>)
                .await
        } else {
            self.dispatch_http(reqwest::Method::GET, "/settings", None::<()>)
                .await
        }
    }

    #[allow(unused)]
    async fn dispatch_in_process<IN, OUT>(
        &self,
        method: http::Method,
        path: &str,
        body: Option<IN>,
    ) -> Result<OUT>
    where
        IN: serde::Serialize + Send + 'static,
        for<'de> OUT: serde::Deserialize<'de> + Send + 'static,
    {
        #[cfg(feature = "in-process")]
        {
            use http::Request;
            use tower::ServiceExt;

            let router = in_process_router()?;

            let body_bytes = if let Some(b) = body {
                serde_json::to_vec(&b)?
            } else {
                Vec::new()
            };

            let req = Request::builder()
                .method(method)
                .uri(path)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(body_bytes))?;

            let response = router
                .oneshot(req)
                .await
                .map_err(|e| anyhow::anyhow!("Router dispatch failed: {e}"))?;

            let body_data = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to read response body: {e}"))?;

            let result: OUT = serde_json::from_slice(&body_data)?;
            Ok(result)
        }
        #[cfg(not(feature = "in-process"))]
        {
            Err(anyhow::anyhow!("in-process feature is not enabled"))
        }
    }

    async fn dispatch_http<IN, OUT>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<IN>,
    ) -> Result<OUT>
    where
        IN: serde::Serialize + Send + 'static,
        for<'de> OUT: serde::Deserialize<'de> + Send + 'static,
    {
        let url = format!("{}{}", self.server_url, path);
        let mut builder = self.http_client.request(method, &url);

        if let Some(b) = body {
            builder = builder.json(&b);
        }

        let resp = builder
            .send()
            .await
            .context("Failed to send HTTP request")?
            .error_for_status()
            .context("HTTP request failed with error status")?;

        let val: OUT = resp.json().await.context("Failed to parse JSON response")?;
        Ok(val)
    }
}

#[async_trait::async_trait]
impl ServerConnector for AppServerConnector {
    async fn get_settings(&self) -> Result<SettingsPayload> {
        self.get_settings_snapshot()
            .await
            .map(|snapshot| snapshot.settings)
    }

    async fn apply_settings(&self, settings: SettingsPayload) -> Result<()> {
        if cfg!(feature = "in-process") {
            let _: serde_json::Value = self
                .dispatch_in_process(http::Method::POST, "/settings", Some(settings))
                .await?;
            Ok(())
        } else {
            let _: serde_json::Value = self
                .dispatch_http(reqwest::Method::POST, "/settings", Some(settings))
                .await?;
            Ok(())
        }
    }

    async fn get_logs(&self) -> Result<LogsSnapshot> {
        if cfg!(feature = "in-process") {
            self.dispatch_in_process(http::Method::GET, "/diagnostics/logs", None::<()>)
                .await
        } else {
            self.dispatch_http(reqwest::Method::GET, "/diagnostics/logs", None::<()>)
                .await
        }
    }

    async fn get_current_log(&self) -> Result<CurrentLogTail> {
        if cfg!(feature = "in-process") {
            self.dispatch_in_process(
                http::Method::GET,
                "/diagnostics/logs/current?format=json&lines=500",
                None::<()>,
            )
            .await
        } else {
            self.dispatch_http(
                reqwest::Method::GET,
                "/diagnostics/logs/current?format=json&lines=500",
                None::<()>,
            )
            .await
        }
    }

    async fn get_changelog(&self, force: bool) -> Result<ChangelogPayload> {
        let path = format!("/update/changelog?force={}", force);
        if cfg!(feature = "in-process") {
            self.dispatch_in_process(http::Method::GET, &path, None::<()>)
                .await
        } else {
            self.dispatch_http(reqwest::Method::GET, &path, None::<()>)
                .await
        }
    }

    async fn export_diagnostics(&self) -> Result<Vec<u8>> {
        if cfg!(feature = "in-process") {
            #[cfg(feature = "in-process")]
            {
                use http::Request;
                use tower::ServiceExt;

                let router = in_process_router()?;

                let req = Request::builder()
                    .method(http::Method::GET)
                    .uri("/diagnostics/export")
                    .body(axum::body::Body::empty())?;

                let response = router
                    .oneshot(req)
                    .await
                    .map_err(|e| anyhow::anyhow!("Router dispatch failed: {e}"))?;

                let body_data = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to read response body: {e}"))?;

                Ok(body_data.to_vec())
            }
            #[cfg(not(feature = "in-process"))]
            {
                Err(anyhow::anyhow!("in-process feature is not enabled"))
            }
        } else {
            let url = format!("{}/diagnostics/export", self.server_url);
            let resp = self
                .http_client
                .get(&url)
                .send()
                .await?
                .error_for_status()?;
            let bytes = resp.bytes().await?;
            Ok(bytes.to_vec())
        }
    }
}
