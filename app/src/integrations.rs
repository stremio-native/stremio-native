use std::sync::{Arc, OnceLock};

use credential_store::{CredentialError, CredentialRef, CredentialStore, SecretKind, SecretValue};
use debrid::{AccountStatus, DebridProvider, HttpDebridProvider, ProviderKind};
use media_integrations::{HttpMetadataProvider, MetadataProvider};
use stremio_core::{models::player::Selected, types::resource::StreamSource};

static STORE: OnceLock<Arc<dyn CredentialStore>> = OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum IntegrationError {
    #[error("credential operation failed: {0}")]
    Credential(#[from] CredentialError),
    #[error("integration database operation failed")]
    Database,
    #[error("provider connection test failed: {0}")]
    Provider(#[from] debrid::DebridError),
    #[error("metadata provider failed: {0}")]
    Metadata(#[from] media_integrations::ProviderError),
    #[error("notification configuration is invalid")]
    InvalidNotification,
    #[error("notification delivery failed")]
    NotificationDelivery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationKind {
    Webhook,
    Telegram,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct NotificationCredential {
    endpoint: String,
    token: Option<String>,
    chat_id: Option<String>,
}

pub async fn configure_notification(
    profile_id: &str,
    kind: NotificationKind,
    endpoint: &str,
    token: Option<&str>,
    chat_id: Option<&str>,
) -> Result<(), IntegrationError> {
    let endpoint_url =
        url::Url::parse(endpoint).map_err(|_| IntegrationError::InvalidNotification)?;
    if endpoint_url.scheme() != "https" {
        return Err(IntegrationError::InvalidNotification);
    }
    if kind == NotificationKind::Telegram
        && (token.is_none_or(str::is_empty) || chat_id.is_none_or(str::is_empty))
    {
        return Err(IntegrationError::InvalidNotification);
    }
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = notification_reference(profile_id, kind)?;
    let value = serde_json::to_vec(&NotificationCredential {
        endpoint: endpoint.to_owned(),
        token: token.map(ToOwned::to_owned),
        chat_id: chat_id.map(ToOwned::to_owned),
    })
    .map_err(|_| IntegrationError::InvalidNotification)?;
    store
        .put(
            &reference,
            SecretKind::WebhookCredential,
            SecretValue::new(value),
        )
        .await?;
    let conn = crate::db::get_conn().map_err(|_| IntegrationError::Database)?;
    conn.execute(
        "INSERT INTO profile_integrations(
            profile_id, provider, enabled, credential_ref, metadata_json
         ) VALUES (?, ?, 1, ?, '{}')
         ON CONFLICT(profile_id, provider) DO UPDATE SET
            enabled = 1, credential_ref = excluded.credential_ref",
        (
            profile_id,
            notification_db_name(kind),
            reference.expose_id(),
        ),
    )
    .await
    .map_err(|_| IntegrationError::Database)?;
    Ok(())
}

pub async fn send_notification(
    profile_id: &str,
    kind: NotificationKind,
    message: &str,
) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = notification_reference(profile_id, kind)?;
    let secret = store.get(&reference).await?;
    let credential: NotificationCredential = serde_json::from_slice(secret.expose())
        .map_err(|_| IntegrationError::InvalidNotification)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|_| IntegrationError::NotificationDelivery)?;
    let request = match kind {
        NotificationKind::Webhook => client
            .post(credential.endpoint)
            .json(&serde_json::json!({ "message": message })),
        NotificationKind::Telegram => {
            let token = credential
                .token
                .ok_or(IntegrationError::InvalidNotification)?;
            let chat_id = credential
                .chat_id
                .ok_or(IntegrationError::InvalidNotification)?;
            let mut endpoint = url::Url::parse(&credential.endpoint)
                .map_err(|_| IntegrationError::InvalidNotification)?;
            endpoint.set_path(&format!(
                "{}/bot{token}/sendMessage",
                endpoint.path().trim_end_matches('/')
            ));
            client
                .post(endpoint)
                .json(&serde_json::json!({ "chat_id": chat_id, "text": message }))
        }
    };
    let response = request
        .send()
        .await
        .map_err(|_| IntegrationError::NotificationDelivery)?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(IntegrationError::NotificationDelivery)
    }
}

pub async fn disconnect_notification(
    profile_id: &str,
    kind: NotificationKind,
) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = notification_reference(profile_id, kind)?;
    match store.delete(&reference).await {
        Ok(()) | Err(CredentialError::Missing) => {}
        Err(error) => return Err(error.into()),
    }
    let conn = crate::db::get_conn().map_err(|_| IntegrationError::Database)?;
    conn.execute(
        "UPDATE profile_integrations SET enabled = 0 WHERE profile_id = ? AND provider = ?",
        (profile_id, notification_db_name(kind)),
    )
    .await
    .map_err(|_| IntegrationError::Database)?;
    Ok(())
}

fn notification_reference(
    profile_id: &str,
    kind: NotificationKind,
) -> Result<CredentialRef, CredentialError> {
    CredentialRef::new(format!(
        "notification/{profile_id}/{}",
        notification_db_name(kind)
    ))
}

const fn notification_db_name(kind: NotificationKind) -> &'static str {
    match kind {
        NotificationKind::Webhook => "webhook",
        NotificationKind::Telegram => "telegram",
    }
}

pub fn install(store: Arc<dyn CredentialStore>) {
    let _ = STORE.set(store);
}

pub async fn delete_profile_credentials(profile_id: &str) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let conn = crate::db::get_conn().map_err(|_| IntegrationError::Database)?;
    let mut references = Vec::new();
    let mut integration_rows = conn
        .query(
            "SELECT credential_ref FROM profile_integrations
             WHERE profile_id = ? AND credential_ref IS NOT NULL",
            [profile_id],
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    while let Some(row) = integration_rows
        .next()
        .await
        .map_err(|_| IntegrationError::Database)?
    {
        references.push(
            row.get::<String>(0)
                .map_err(|_| IntegrationError::Database)?,
        );
    }
    drop(integration_rows);
    let mut download_rows = conn
        .query(
            "SELECT credential_ref FROM download_jobs WHERE profile_id = ?",
            [profile_id],
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    while let Some(row) = download_rows
        .next()
        .await
        .map_err(|_| IntegrationError::Database)?
    {
        references.push(
            row.get::<String>(0)
                .map_err(|_| IntegrationError::Database)?,
        );
    }
    references.sort();
    references.dedup();
    for reference in references {
        let reference = CredentialRef::new(reference)?;
        match store.delete(&reference).await {
            Ok(()) | Err(CredentialError::Missing) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub async fn exportable_secrets() -> Result<Vec<(String, SecretKind, Vec<u8>)>, IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let conn = crate::db::get_conn().map_err(|_| IntegrationError::Database)?;
    let mut references = Vec::<(String, SecretKind)>::new();
    let mut profiles = conn
        .query("SELECT id FROM local_profiles", ())
        .await
        .map_err(|_| IntegrationError::Database)?;
    while let Some(row) = profiles
        .next()
        .await
        .map_err(|_| IntegrationError::Database)?
    {
        let profile_id = row
            .get::<String>(0)
            .map_err(|_| IntegrationError::Database)?;
        references.push((
            CredentialRef::stremio_auth(&profile_id)?
                .expose_id()
                .to_owned(),
            SecretKind::StremioAuth,
        ));
    }
    drop(profiles);
    let mut integration_rows = conn
        .query(
            "SELECT credential_ref, provider FROM profile_integrations
             WHERE enabled = 1 AND credential_ref IS NOT NULL",
            (),
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    while let Some(row) = integration_rows
        .next()
        .await
        .map_err(|_| IntegrationError::Database)?
    {
        let reference = row
            .get::<String>(0)
            .map_err(|_| IntegrationError::Database)?;
        let provider = row
            .get::<String>(1)
            .map_err(|_| IntegrationError::Database)?;
        let kind = if provider.starts_with("debrid:") {
            SecretKind::DebridCredential
        } else if matches!(provider.as_str(), "webhook" | "telegram") {
            SecretKind::WebhookCredential
        } else {
            SecretKind::ProviderApiKey
        };
        references.push((reference, kind));
    }
    drop(integration_rows);
    let mut downloads = conn
        .query("SELECT credential_ref FROM download_jobs", ())
        .await
        .map_err(|_| IntegrationError::Database)?;
    while let Some(row) = downloads
        .next()
        .await
        .map_err(|_| IntegrationError::Database)?
    {
        references.push((
            row.get::<String>(0)
                .map_err(|_| IntegrationError::Database)?,
            SecretKind::DownloadSource,
        ));
    }
    references.sort_by(|left, right| left.0.cmp(&right.0));
    references.dedup_by(|left, right| left.0 == right.0);
    let mut secrets = Vec::new();
    for (reference, kind) in references {
        let reference_value = CredentialRef::new(reference.clone())?;
        match store.get(&reference_value).await {
            Ok(value) => secrets.push((reference, kind, value.into_bytes())),
            Err(CredentialError::Missing) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(secrets)
}

pub async fn restore_secrets(
    entries: Vec<crate::backup::SecretExportEntry>,
) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    for mut entry in entries {
        let reference = CredentialRef::new(entry.credential_ref)?;
        let value = std::mem::take(&mut entry.value);
        store
            .put(&reference, entry.kind, SecretValue::new(value))
            .await?;
    }
    Ok(())
}

pub async fn configure_debrid(
    profile_id: &str,
    provider: ProviderKind,
    api_key: &str,
) -> Result<AccountStatus, IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = debrid_reference(profile_id, provider)?;
    store
        .put(
            &reference,
            SecretKind::DebridCredential,
            SecretValue::new(api_key.as_bytes().to_vec()),
        )
        .await?;
    let client = HttpDebridProvider::new(provider, reference.clone(), store.clone())?;
    let status = match client.account_status().await {
        Ok(status) => status,
        Err(error) => {
            let _ = store.delete(&reference).await;
            return Err(error.into());
        }
    };
    let metadata = serde_json::to_string(&status).map_err(|_| IntegrationError::Database)?;
    persist_integration(profile_id, provider, true, Some(&reference), &metadata).await?;
    Ok(status)
}

pub async fn configure_metadata_provider(
    profile_id: &str,
    provider: media_integrations::ProviderKind,
    api_key: &str,
) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = metadata_reference(profile_id, provider)?;
    if api_key.trim().is_empty() {
        return Err(IntegrationError::InvalidNotification);
    }
    let previous = store.get(&reference).await.ok();
    store
        .put(
            &reference,
            SecretKind::ProviderApiKey,
            SecretValue::new(api_key.as_bytes().to_vec()),
        )
        .await?;
    let provider_client =
        HttpMetadataProvider::new(provider, Some(reference.clone()), store.clone())?;
    let validation_id = match provider {
        media_integrations::ProviderKind::Fanart => "278",
        _ => "tt0111161",
    };
    if let Err(error) = provider_client.enrich(validation_id).await {
        if let Some(previous) = previous {
            let _ = store
                .put(&reference, SecretKind::ProviderApiKey, previous)
                .await;
        } else {
            let _ = store.delete(&reference).await;
        }
        return Err(error.into());
    }
    let conn = crate::db::get_conn().map_err(|_| IntegrationError::Database)?;
    conn.execute(
        "INSERT INTO profile_integrations(profile_id, provider, enabled, credential_ref, metadata_json)
         VALUES (?, ?, 1, ?, '{}')
         ON CONFLICT(profile_id, provider) DO UPDATE SET
            enabled = 1, credential_ref = excluded.credential_ref",
        (
            profile_id,
            metadata_db_name(provider),
            reference.expose_id(),
        ),
    )
    .await
    .map_err(|_| IntegrationError::Database)?;
    Ok(())
}

pub async fn configured_integration_names(
    profile_id: &str,
) -> Result<Vec<String>, IntegrationError> {
    let conn = crate::db::get_conn().map_err(|_| IntegrationError::Database)?;
    let mut rows = conn
        .query(
            "SELECT provider FROM profile_integrations
             WHERE profile_id = ? AND enabled = 1
             ORDER BY provider",
            [profile_id],
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    let mut names = Vec::new();
    while let Some(row) = rows.next().await.map_err(|_| IntegrationError::Database)? {
        let provider = row
            .get::<String>(0)
            .map_err(|_| IntegrationError::Database)?;
        let display = match provider.as_str() {
            "debrid:real-debrid" => "Real-Debrid",
            "debrid:all-debrid" => "AllDebrid",
            "debrid:premiumize" => "Premiumize",
            "debrid:debrid-link" => "Debrid-Link",
            "debrid:torbox" => "TorBox",
            "metadata:tmdb" => "TMDB",
            "metadata:omdb" => "OMDb",
            "metadata:fanart" => "Fanart.tv",
            "metadata:rpdb" => "RPDB",
            "webhook" => "Webhook",
            "telegram" => "Telegram",
            "theintrodb" => continue,
            _ => continue,
        };
        names.push(display.to_owned());
    }
    Ok(names)
}

pub async fn disconnect_metadata_provider(
    profile_id: &str,
    provider: media_integrations::ProviderKind,
) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = metadata_reference(profile_id, provider)?;
    match store.delete(&reference).await {
        Ok(()) | Err(CredentialError::Missing) => {}
        Err(error) => return Err(error.into()),
    }
    let conn = crate::db::get_conn().map_err(|_| IntegrationError::Database)?;
    conn.execute(
        "UPDATE profile_integrations SET enabled = 0 WHERE profile_id = ? AND provider = ?",
        (profile_id, metadata_db_name(provider)),
    )
    .await
    .map_err(|_| IntegrationError::Database)?;
    Ok(())
}

pub async fn enabled_metadata_providers(
    profile_id: &str,
) -> Result<Vec<Arc<dyn MetadataProvider>>, IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let conn = crate::db::get_conn().map_err(|_| IntegrationError::Database)?;
    let mut rows = conn
        .query(
            "SELECT provider, credential_ref FROM profile_integrations
             WHERE profile_id = ? AND enabled = 1 AND provider LIKE 'metadata:%'",
            [profile_id],
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    let mut providers = Vec::<Arc<dyn MetadataProvider>>::new();
    while let Some(row) = rows.next().await.map_err(|_| IntegrationError::Database)? {
        let name = row
            .get::<String>(0)
            .map_err(|_| IntegrationError::Database)?;
        let reference = row
            .get::<Option<String>>(1)
            .map_err(|_| IntegrationError::Database)?
            .map(CredentialRef::new)
            .transpose()?;
        let Some(provider) = metadata_provider_from_db(&name) else {
            continue;
        };
        providers.push(Arc::new(HttpMetadataProvider::new(
            provider,
            reference,
            store.clone(),
        )?));
    }
    for provider in [
        media_integrations::ProviderKind::Kitsu,
        media_integrations::ProviderKind::AniZip,
    ] {
        providers.push(Arc::new(HttpMetadataProvider::new(
            provider,
            None,
            store.clone(),
        )?));
    }
    Ok(providers)
}

pub async fn disconnect_debrid(
    profile_id: &str,
    provider: ProviderKind,
) -> Result<(), IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let reference = debrid_reference(profile_id, provider)?;
    store.delete(&reference).await?;
    persist_integration(profile_id, provider, false, Some(&reference), "{}").await
}

pub async fn enabled_debrid_providers(
    profile_id: &str,
) -> Result<Vec<Arc<dyn DebridProvider>>, IntegrationError> {
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    let conn = crate::db::get_conn().map_err(|_| IntegrationError::Database)?;
    let mut rows = conn
        .query(
            "SELECT provider, credential_ref FROM profile_integrations
             WHERE profile_id = ? AND enabled = 1 AND provider LIKE 'debrid:%'",
            [profile_id],
        )
        .await
        .map_err(|_| IntegrationError::Database)?;
    let mut providers = Vec::<Arc<dyn DebridProvider>>::new();
    while let Some(row) = rows.next().await.map_err(|_| IntegrationError::Database)? {
        let provider_name: String = row.get(0).map_err(|_| IntegrationError::Database)?;
        let reference: String = row.get(1).map_err(|_| IntegrationError::Database)?;
        let Some(provider) = provider_from_db(&provider_name) else {
            continue;
        };
        let reference = CredentialRef::new(reference)?;
        providers.push(Arc::new(HttpDebridProvider::new(
            provider,
            reference,
            store.clone(),
        )?));
    }
    Ok(providers)
}

pub async fn resolve_explicit_selection(mut selected: Selected) -> Selected {
    let StreamSource::Torrent { info_hash, .. } = &selected.stream.source else {
        return selected;
    };
    let hash = hex::encode(info_hash);
    let magnet = format!("magnet:?xt=urn:btih:{hash}");
    let Ok(profile_id) = crate::profiles::active_profile_id().await else {
        return selected;
    };
    let Ok(providers) = enabled_debrid_providers(profile_id.as_str()).await else {
        return selected;
    };
    for provider in providers {
        if !matches!(
            provider.availability(&hash).await,
            Ok(debrid::DebridAvailability::Cached)
        ) {
            continue;
        }
        let Ok(resolved) = provider.resolve(&magnet).await else {
            continue;
        };
        let Ok(url) = resolved.url.parse() else {
            continue;
        };
        selected.stream.source = StreamSource::Url { url };
        selected.stream.behavior_hints.not_web_ready = false;
        return selected;
    }
    selected
}

async fn persist_integration(
    profile_id: &str,
    provider: ProviderKind,
    enabled: bool,
    reference: Option<&CredentialRef>,
    metadata: &str,
) -> Result<(), IntegrationError> {
    let conn = crate::db::get_conn().map_err(|_| IntegrationError::Database)?;
    conn.execute(
        "INSERT INTO profile_integrations(
            profile_id, provider, enabled, credential_ref, metadata_json
         ) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(profile_id, provider) DO UPDATE SET
            enabled = excluded.enabled,
            credential_ref = excluded.credential_ref,
            metadata_json = excluded.metadata_json",
        (
            profile_id,
            provider_db_name(provider),
            i64::from(enabled),
            reference.map(CredentialRef::expose_id),
            metadata,
        ),
    )
    .await
    .map_err(|_| IntegrationError::Database)?;
    Ok(())
}

fn debrid_reference(
    profile_id: &str,
    provider: ProviderKind,
) -> Result<CredentialRef, CredentialError> {
    CredentialRef::new(format!(
        "debrid/{profile_id}/{}",
        provider_db_name(provider).trim_start_matches("debrid:")
    ))
}

fn metadata_reference(
    profile_id: &str,
    provider: media_integrations::ProviderKind,
) -> Result<CredentialRef, CredentialError> {
    CredentialRef::new(format!(
        "metadata/{profile_id}/{}",
        metadata_db_name(provider).trim_start_matches("metadata:")
    ))
}

const fn metadata_db_name(provider: media_integrations::ProviderKind) -> &'static str {
    match provider {
        media_integrations::ProviderKind::Tmdb => "metadata:tmdb",
        media_integrations::ProviderKind::Omdb => "metadata:omdb",
        media_integrations::ProviderKind::Fanart => "metadata:fanart",
        media_integrations::ProviderKind::Rpdb => "metadata:rpdb",
        media_integrations::ProviderKind::Kitsu => "metadata:kitsu",
        media_integrations::ProviderKind::AniZip => "metadata:anizip",
        media_integrations::ProviderKind::Trakt => "metadata:trakt",
    }
}

fn metadata_provider_from_db(value: &str) -> Option<media_integrations::ProviderKind> {
    match value {
        "metadata:tmdb" => Some(media_integrations::ProviderKind::Tmdb),
        "metadata:omdb" => Some(media_integrations::ProviderKind::Omdb),
        "metadata:fanart" => Some(media_integrations::ProviderKind::Fanart),
        "metadata:rpdb" => Some(media_integrations::ProviderKind::Rpdb),
        "metadata:kitsu" => Some(media_integrations::ProviderKind::Kitsu),
        "metadata:anizip" => Some(media_integrations::ProviderKind::AniZip),
        "metadata:trakt" => Some(media_integrations::ProviderKind::Trakt),
        _ => None,
    }
}

const fn provider_db_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::RealDebrid => "debrid:real-debrid",
        ProviderKind::AllDebrid => "debrid:all-debrid",
        ProviderKind::Premiumize => "debrid:premiumize",
        ProviderKind::DebridLink => "debrid:debrid-link",
        ProviderKind::TorBox => "debrid:torbox",
    }
}

fn provider_from_db(value: &str) -> Option<ProviderKind> {
    match value {
        "debrid:real-debrid" => Some(ProviderKind::RealDebrid),
        "debrid:all-debrid" => Some(ProviderKind::AllDebrid),
        "debrid:premiumize" => Some(ProviderKind::Premiumize),
        "debrid:debrid-link" => Some(ProviderKind::DebridLink),
        "debrid:torbox" => Some(ProviderKind::TorBox),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_provider_has_a_stable_profile_scoped_reference() {
        for provider in ProviderKind::ALL {
            let reference = debrid_reference("owner", provider).expect("reference");
            assert!(reference.expose_id().starts_with("debrid/owner/"));
        }
    }
}
