use std::sync::{Arc, OnceLock, RwLock};

use credential_store::{CredentialError, CredentialRef, CredentialStore, SecretKind, SecretValue};

static STORE: OnceLock<Arc<dyn CredentialStore>> = OnceLock::new();
static ACTIVE_PROFILE: RwLock<String> = RwLock::new(String::new());
static TIDB_API_KEY: RwLock<Vec<u8>> = RwLock::new(Vec::new());

pub fn install(store: Arc<dyn CredentialStore>) {
    let _ = STORE.set(store);
}

pub async fn activate_profile(
    profile_id: &str,
    legacy_tidb_key: Option<String>,
) -> Result<(), CredentialError> {
    let reference = tidb_reference(profile_id)?;
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    if let Some(mut legacy) = legacy_tidb_key {
        store
            .put(
                &reference,
                SecretKind::ProviderApiKey,
                SecretValue::new(legacy.as_bytes().to_vec()),
            )
            .await?;
        persist_integration(profile_id, &reference, true).await?;
        // Strings cannot be guaranteed to zero their capacity, but removing it
        // from SQLite and replacing the active cache happens in this operation.
        legacy.clear();
    }
    let configured = integration_configured(profile_id).await?;
    let key = if configured {
        match store.get(&reference).await {
            Ok(value) => value.into_bytes(),
            Err(error) => return Err(error),
        }
    } else {
        Vec::new()
    };
    replace_cached_secret(key);
    *ACTIVE_PROFILE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = profile_id.to_owned();
    Ok(())
}

pub async fn set_tidb_api_key(value: &str) -> Result<(), CredentialError> {
    let profile_id = ACTIVE_PROFILE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let reference = tidb_reference(&profile_id)?;
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    if value.trim().is_empty() {
        store.delete(&reference).await?;
        persist_integration(&profile_id, &reference, false).await?;
        replace_cached_secret(Vec::new());
    } else {
        store
            .put(
                &reference,
                SecretKind::ProviderApiKey,
                SecretValue::new(value.as_bytes().to_vec()),
            )
            .await?;
        persist_integration(&profile_id, &reference, true).await?;
        replace_cached_secret(value.as_bytes().to_vec());
    }
    Ok(())
}

pub fn tidb_api_key() -> String {
    String::from_utf8_lossy(
        &TIDB_API_KEY
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
    .into_owned()
}

pub fn has_tidb_api_key() -> bool {
    !TIDB_API_KEY
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_empty()
}

pub fn clear_session_secrets() {
    replace_cached_secret(Vec::new());
}

pub async fn delete_profile_credentials(profile_id: &str) -> Result<(), CredentialError> {
    let reference = tidb_reference(profile_id)?;
    let store = STORE.get().ok_or(CredentialError::Unavailable)?;
    match store.delete(&reference).await {
        Ok(()) | Err(CredentialError::Missing) => Ok(()),
        Err(error) => Err(error),
    }
}

fn replace_cached_secret(value: Vec<u8>) {
    let mut cached = TIDB_API_KEY
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cached.fill(0);
    *cached = value;
}

fn tidb_reference(profile_id: &str) -> Result<CredentialRef, CredentialError> {
    CredentialRef::new(format!("provider/{profile_id}/theintrodb"))
}

async fn integration_configured(profile_id: &str) -> Result<bool, CredentialError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| CredentialError::OperationFailed)?;
    let mut rows = conn
        .query(
            "SELECT enabled FROM profile_integrations
             WHERE profile_id = ? AND provider = 'theintrodb'",
            [profile_id],
        )
        .await
        .map_err(|_| CredentialError::OperationFailed)?;
    Ok(rows
        .next()
        .await
        .map_err(|_| CredentialError::OperationFailed)?
        .and_then(|row| row.get::<i64>(0).ok())
        .is_some_and(|enabled| enabled != 0))
}

async fn persist_integration(
    profile_id: &str,
    reference: &CredentialRef,
    enabled: bool,
) -> Result<(), CredentialError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| CredentialError::OperationFailed)?;
    conn.execute(
        "INSERT INTO profile_integrations(
            profile_id, provider, enabled, credential_ref, metadata_json
         ) VALUES (?, 'theintrodb', ?, ?, '{}')
         ON CONFLICT(profile_id, provider) DO UPDATE SET
            enabled = excluded.enabled,
            credential_ref = excluded.credential_ref",
        (profile_id, i64::from(enabled), reference.expose_id()),
    )
    .await
    .map_err(|_| CredentialError::OperationFailed)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_references_are_profile_scoped() {
        assert_ne!(
            tidb_reference("owner").expect("reference"),
            tidb_reference("kids").expect("reference")
        );
    }
}
