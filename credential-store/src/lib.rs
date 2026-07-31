use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const SERVICE_NAME: &str = "stremio-native";
type MemoryEntry = (SecretKind, Vec<u8>);
type MemoryEntries = HashMap<CredentialRef, MemoryEntry>;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CredentialRef(String);

impl CredentialRef {
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialError> {
        let value = value.into();
        if value.trim().is_empty() || value.len() > 240 {
            return Err(CredentialError::InvalidReference);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'/' | b'.'))
        {
            return Err(CredentialError::InvalidReference);
        }
        Ok(Self(value))
    }

    pub fn stremio_auth(profile_id: &str) -> Result<Self, CredentialError> {
        Self::new(format!("stremio-auth/{profile_id}"))
    }

    pub fn expose_id(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecretKind {
    StremioAuth,
    ProviderApiKey,
    DebridCredential,
    IptvCredential,
    DownloadSource,
    WebhookCredential,
    WatchPartyRelay,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretValue(Vec<u8>);

impl SecretValue {
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CredentialError {
    #[error("credential reference is invalid")]
    InvalidReference,
    #[error("credential does not exist")]
    Missing,
    #[error("the operating-system credential vault is locked")]
    Locked,
    #[error("the operating-system credential vault is unavailable")]
    Unavailable,
    #[error("credential operation failed")]
    OperationFailed,
    #[error("credential worker stopped")]
    WorkerStopped,
}

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn put(
        &self,
        reference: &CredentialRef,
        kind: SecretKind,
        value: SecretValue,
    ) -> Result<(), CredentialError>;

    async fn get(&self, reference: &CredentialRef) -> Result<SecretValue, CredentialError>;

    async fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialError>;
}

#[derive(Clone, Default)]
pub struct PlatformCredentialStore {
    // Native stores can have ordering limitations and Secret Service calls are
    // blocking IPC. Serialize them, then perform each call off the async worker.
    gate: Arc<Mutex<()>>,
}

#[async_trait]
impl CredentialStore for PlatformCredentialStore {
    async fn put(
        &self,
        reference: &CredentialRef,
        _kind: SecretKind,
        value: SecretValue,
    ) -> Result<(), CredentialError> {
        let _guard = self.gate.lock().await;
        let reference = reference.clone();
        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(SERVICE_NAME, reference.expose_id())
                .map_err(map_keyring_error)?;
            entry.set_secret(value.expose()).map_err(map_keyring_error)
        })
        .await
        .map_err(|_| CredentialError::WorkerStopped)?
    }

    async fn get(&self, reference: &CredentialRef) -> Result<SecretValue, CredentialError> {
        let _guard = self.gate.lock().await;
        let reference = reference.clone();
        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(SERVICE_NAME, reference.expose_id())
                .map_err(map_keyring_error)?;
            entry
                .get_secret()
                .map(SecretValue::new)
                .map_err(map_keyring_error)
        })
        .await
        .map_err(|_| CredentialError::WorkerStopped)?
    }

    async fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialError> {
        let _guard = self.gate.lock().await;
        let reference = reference.clone();
        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(SERVICE_NAME, reference.expose_id())
                .map_err(map_keyring_error)?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(map_keyring_error(error)),
            }
        })
        .await
        .map_err(|_| CredentialError::WorkerStopped)?
    }
}

fn map_keyring_error(error: keyring::Error) -> CredentialError {
    match error {
        keyring::Error::NoEntry => CredentialError::Missing,
        keyring::Error::NoStorageAccess(_) => CredentialError::Locked,
        keyring::Error::PlatformFailure(_) => CredentialError::Unavailable,
        _ => CredentialError::OperationFailed,
    }
}

#[derive(Clone, Default)]
pub struct MemoryCredentialStore {
    entries: Arc<Mutex<MemoryEntries>>,
    forced_error: Arc<Mutex<Option<CredentialError>>>,
}

impl MemoryCredentialStore {
    pub async fn force_error(&self, error: Option<CredentialError>) {
        *self.forced_error.lock().await = error;
    }

    async fn check_error(&self) -> Result<(), CredentialError> {
        match self.forced_error.lock().await.clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl CredentialStore for MemoryCredentialStore {
    async fn put(
        &self,
        reference: &CredentialRef,
        kind: SecretKind,
        value: SecretValue,
    ) -> Result<(), CredentialError> {
        self.check_error().await?;
        self.entries
            .lock()
            .await
            .insert(reference.clone(), (kind, value.into_bytes()));
        Ok(())
    }

    async fn get(&self, reference: &CredentialRef) -> Result<SecretValue, CredentialError> {
        self.check_error().await?;
        self.entries
            .lock()
            .await
            .get(reference)
            .map(|(_, value)| SecretValue::new(value.clone()))
            .ok_or(CredentialError::Missing)
    }

    async fn delete(&self, reference: &CredentialRef) -> Result<(), CredentialError> {
        self.check_error().await?;
        self.entries.lock().await.remove(reference);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_round_trip_and_delete() {
        let store = MemoryCredentialStore::default();
        let reference = CredentialRef::stremio_auth("owner").expect("valid reference");
        store
            .put(
                &reference,
                SecretKind::StremioAuth,
                SecretValue::new(b"canary".to_vec()),
            )
            .await
            .expect("store secret");

        assert_eq!(
            store.get(&reference).await.expect("read secret").expose(),
            b"canary"
        );
        store.delete(&reference).await.expect("delete secret");
        assert_eq!(store.get(&reference).await, Err(CredentialError::Missing));
    }

    #[test]
    fn references_reject_path_and_log_injection_characters() {
        assert_eq!(
            CredentialRef::new("provider\nsecret"),
            Err(CredentialError::InvalidReference)
        );
        assert_eq!(
            CredentialRef::new("../provider?token=x"),
            Err(CredentialError::InvalidReference)
        );
    }
}
