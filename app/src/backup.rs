use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Write},
    path::Path,
};

use argon2::Argon2;
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit},
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

const BACKUP_VERSION: u32 = 1;
const EXPORT_TABLES: [&str; 8] = [
    "local_profiles",
    "app_state",
    "profile_settings",
    "profile_core_storage",
    "profile_integrations",
    "download_jobs",
    "local_roots",
    "local_media_items",
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackupManifestV1 {
    pub version: u32,
    pub application_version: String,
    pub created_at: i64,
    pub includes_secrets: bool,
    pub payload_checksum: String,
    pub table_rows: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct DatabasePayload {
    tables: BTreeMap<String, Vec<Vec<serde_json::Value>>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RestorePreview {
    pub manifest: BackupManifestV1,
    pub profile_count: usize,
    pub download_count: usize,
    pub local_media_count: usize,
}

#[derive(Deserialize, Serialize)]
pub struct SecretExportEntry {
    pub credential_ref: String,
    pub kind: credential_store::SecretKind,
    pub value: Vec<u8>,
}

pub struct SecretExport {
    pub passphrase: String,
    pub entries: Vec<SecretExportEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BackupError {
    #[error("backup database operation failed")]
    Database,
    #[error("backup file operation failed")]
    Filesystem,
    #[error("backup archive is invalid")]
    InvalidArchive,
    #[error("backup schema version is unsupported")]
    UnsupportedVersion,
    #[error("backup integrity validation failed")]
    Integrity,
    #[error("secret export passphrase is too short")]
    WeakPassphrase,
    #[error("secret encryption operation failed")]
    Encryption,
    #[error("backup worker stopped unexpectedly")]
    WorkerStopped,
}

pub async fn create_backup(
    destination: &Path,
    secrets: Option<SecretExport>,
) -> Result<BackupManifestV1, BackupError> {
    let payload = export_database().await?;
    let payload_json = serde_json::to_vec(&payload).map_err(|_| BackupError::InvalidArchive)?;
    let includes_secrets = secrets.is_some();
    let manifest = BackupManifestV1 {
        version: BACKUP_VERSION,
        application_version: env!("CARGO_PKG_VERSION").to_owned(),
        created_at: chrono::Utc::now().timestamp(),
        includes_secrets,
        payload_checksum: blake3::hash(&payload_json).to_hex().to_string(),
        table_rows: payload
            .tables
            .iter()
            .map(|(table, rows)| (table.clone(), rows.len()))
            .collect(),
    };
    let encrypted_secrets = secrets.map(encrypt_secrets).transpose()?;
    let manifest_json =
        serde_json::to_vec_pretty(&manifest).map_err(|_| BackupError::InvalidArchive)?;
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|_| BackupError::Filesystem)?;
        }
        let temporary = destination.with_extension("stremio-backup.tmp");
        let file = File::create(&temporary).map_err(|_| BackupError::Filesystem)?;
        let mut archive = ZipWriter::new(file);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .unix_permissions(0o600);
        write_archive_file(&mut archive, "manifest.json", &manifest_json, options)?;
        write_archive_file(&mut archive, "database.json", &payload_json, options)?;
        if let Some(encrypted_secrets) = encrypted_secrets {
            write_archive_file(&mut archive, "secrets.enc", &encrypted_secrets, options)?;
        }
        archive.finish().map_err(|_| BackupError::Filesystem)?;
        std::fs::rename(&temporary, &destination).map_err(|_| BackupError::Filesystem)
    })
    .await
    .map_err(|_| BackupError::Filesystem)??;
    Ok(manifest)
}

pub async fn preview_restore(path: &Path) -> Result<RestorePreview, BackupError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let (manifest, payload) = read_and_validate(&path)?;
        Ok(preview(manifest, &payload))
    })
    .await
    .map_err(|_| BackupError::WorkerStopped)?
}

pub async fn restore(path: &Path, expected_checksum: &str) -> Result<RestorePreview, BackupError> {
    let path = path.to_path_buf();
    let expected_checksum = expected_checksum.to_owned();
    let (manifest, mut payload) = tokio::task::spawn_blocking(move || {
        let (manifest, payload) = read_and_validate(&path)?;
        verify_expected_checksum(&manifest, &expected_checksum)?;
        Ok((manifest, payload))
    })
    .await
    .map_err(|_| BackupError::WorkerStopped)??;
    let result_preview = preview(manifest, &payload);
    let mut conn = crate::db::get_conn()
        .await
        .map_err(|_| BackupError::Database)?;
    let transaction = conn
        .transaction()
        .await
        .map_err(|_| BackupError::Database)?;
    for table in EXPORT_TABLES.iter().rev() {
        transaction
            .execute(&format!("DELETE FROM {table}"), ())
            .await
            .map_err(|_| BackupError::Database)?;
    }
    for table in EXPORT_TABLES {
        let rows = payload.tables.remove(table).unwrap_or_default();
        restore_table(&transaction, table, rows).await?;
    }
    transaction
        .commit()
        .await
        .map_err(|_| BackupError::Database)?;
    Ok(result_preview)
}

fn verify_expected_checksum(
    manifest: &BackupManifestV1,
    expected_checksum: &str,
) -> Result<(), BackupError> {
    if manifest.payload_checksum == expected_checksum {
        Ok(())
    } else {
        Err(BackupError::Integrity)
    }
}

pub fn decrypt_secret_payload(
    encrypted: &[u8],
    passphrase: &str,
) -> Result<Vec<SecretExportEntry>, BackupError> {
    if encrypted.len() < 44 || &encrypted[..4] != b"SNV1" {
        return Err(BackupError::InvalidArchive);
    }
    let salt = &encrypted[4..20];
    let nonce = <&XNonce>::try_from(&encrypted[20..44]).map_err(|_| BackupError::InvalidArchive)?;
    let ciphertext = &encrypted[44..];
    let mut key = derive_key(passphrase, salt)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| BackupError::Encryption)?;
    key.fill(0);
    serde_json::from_slice(&plaintext).map_err(|_| BackupError::InvalidArchive)
}

pub async fn read_secret_export(
    path: &Path,
    passphrase: &str,
) -> Result<Vec<SecretExportEntry>, BackupError> {
    let path = path.to_path_buf();
    let passphrase = passphrase.to_owned();
    tokio::task::spawn_blocking(move || read_secret_export_blocking(&path, &passphrase))
        .await
        .map_err(|_| BackupError::WorkerStopped)?
}

fn read_secret_export_blocking(
    path: &Path,
    passphrase: &str,
) -> Result<Vec<SecretExportEntry>, BackupError> {
    let file = File::open(path).map_err(|_| BackupError::Filesystem)?;
    let mut archive = ZipArchive::new(file).map_err(|_| BackupError::InvalidArchive)?;
    let mut encrypted = Vec::new();
    archive
        .by_name("secrets.enc")
        .map_err(|_| BackupError::InvalidArchive)?
        .read_to_end(&mut encrypted)
        .map_err(|_| BackupError::Filesystem)?;
    decrypt_secret_payload(&encrypted, passphrase)
}

async fn export_database() -> Result<DatabasePayload, BackupError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| BackupError::Database)?;
    let mut tables = BTreeMap::new();
    for table in EXPORT_TABLES {
        let columns = table_columns(table);
        let sql = format!("SELECT {} FROM {table}", columns.join(", "));
        let mut cursor = conn
            .query(&sql, ())
            .await
            .map_err(|_| BackupError::Database)?;
        let mut rows = Vec::new();
        while let Some(row) = cursor.next().await.map_err(|_| BackupError::Database)? {
            rows.push(
                (0..columns.len())
                    .map(|index| sql_value(&row, index))
                    .collect(),
            );
        }
        tables.insert(table.to_owned(), rows);
    }
    Ok(DatabasePayload { tables })
}

fn sql_value(row: &turso::Row, index: usize) -> serde_json::Value {
    if let Ok(value) = row.get::<String>(index) {
        serde_json::Value::String(value)
    } else if let Ok(value) = row.get::<i64>(index) {
        serde_json::Value::Number(value.into())
    } else if let Ok(value) = row.get::<f64>(index) {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    } else {
        serde_json::Value::Null
    }
}

async fn restore_table(
    transaction: &turso::transaction::Transaction<'_>,
    table: &str,
    rows: Vec<Vec<serde_json::Value>>,
) -> Result<(), BackupError> {
    let columns = table_columns(table);
    let placeholders = std::iter::repeat_n("?", columns.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO {table} ({}) VALUES ({placeholders})",
        columns.join(", ")
    );
    for row in rows {
        let values = row.into_iter().map(json_to_sql).collect::<Vec<_>>();
        transaction
            .execute(&sql, values)
            .await
            .map_err(|_| BackupError::Database)?;
    }
    Ok(())
}

fn json_to_sql(value: serde_json::Value) -> turso::Value {
    match value {
        serde_json::Value::Null => turso::Value::Null,
        serde_json::Value::Bool(value) => turso::Value::Integer(i64::from(value)),
        serde_json::Value::Number(value) => value
            .as_i64()
            .map(turso::Value::Integer)
            .or_else(|| value.as_f64().map(turso::Value::Real))
            .unwrap_or(turso::Value::Null),
        serde_json::Value::String(value) => turso::Value::Text(value),
        value => turso::Value::Text(value.to_string()),
    }
}

fn read_and_validate(path: &Path) -> Result<(BackupManifestV1, DatabasePayload), BackupError> {
    let file = File::open(path).map_err(|_| BackupError::Filesystem)?;
    let mut archive = ZipArchive::new(file).map_err(|_| BackupError::InvalidArchive)?;
    let manifest: BackupManifestV1 = {
        let mut file = archive
            .by_name("manifest.json")
            .map_err(|_| BackupError::InvalidArchive)?;
        serde_json::from_reader(&mut file).map_err(|_| BackupError::InvalidArchive)?
    };
    if manifest.version != BACKUP_VERSION {
        return Err(BackupError::UnsupportedVersion);
    }
    let (payload, checksum) = {
        let file = archive
            .by_name("database.json")
            .map_err(|_| BackupError::InvalidArchive)?;
        let mut reader = HashingReader::new(file);
        let payload =
            serde_json::from_reader(&mut reader).map_err(|_| BackupError::InvalidArchive)?;
        std::io::copy(&mut reader, &mut std::io::sink()).map_err(|_| BackupError::Filesystem)?;
        (payload, reader.finalize().to_hex().to_string())
    };
    if checksum != manifest.payload_checksum {
        return Err(BackupError::Integrity);
    }
    validate_payload(&payload)?;
    Ok((manifest, payload))
}

struct HashingReader<R> {
    inner: R,
    hasher: blake3::Hasher,
}

impl<R> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
        }
    }

    fn finalize(self) -> blake3::Hash {
        self.hasher.finalize()
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        Ok(read)
    }
}

fn validate_payload(payload: &DatabasePayload) -> Result<(), BackupError> {
    if payload
        .tables
        .keys()
        .any(|table| !EXPORT_TABLES.contains(&table.as_str()))
    {
        return Err(BackupError::InvalidArchive);
    }
    for (table, rows) in &payload.tables {
        let expected = table_columns(table).len();
        if expected == 0 || rows.iter().any(|row| row.len() != expected) {
            return Err(BackupError::InvalidArchive);
        }
    }
    Ok(())
}

fn preview(manifest: BackupManifestV1, payload: &DatabasePayload) -> RestorePreview {
    RestorePreview {
        profile_count: payload.tables.get("local_profiles").map_or(0, Vec::len),
        download_count: payload.tables.get("download_jobs").map_or(0, Vec::len),
        local_media_count: payload.tables.get("local_media_items").map_or(0, Vec::len),
        manifest,
    }
}

fn encrypt_secrets(mut export: SecretExport) -> Result<Vec<u8>, BackupError> {
    if export.passphrase.chars().count() < 12 {
        return Err(BackupError::WeakPassphrase);
    }
    let plaintext = serde_json::to_vec(&export.entries).map_err(|_| BackupError::Encryption)?;
    for entry in &mut export.entries {
        entry.value.fill(0);
    }
    let mut salt = [0_u8; 16];
    let mut nonce = [0_u8; 24];
    let mut rng = rand::rng();
    rng.fill_bytes(&mut salt);
    rng.fill_bytes(&mut nonce);
    let mut key = derive_key(&export.passphrase, &salt)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let cipher_nonce = XNonce::from(nonce);
    let ciphertext = cipher
        .encrypt(&cipher_nonce, plaintext.as_ref())
        .map_err(|_| BackupError::Encryption)?;
    key.fill(0);
    let mut output = Vec::with_capacity(44 + ciphertext.len());
    output.extend_from_slice(b"SNV1");
    output.extend_from_slice(&salt);
    output.extend_from_slice(&nonce);
    output.extend_from_slice(&ciphertext);
    Ok(output)
}

fn derive_key(passphrase: &str, salt: &[u8]) -> Result<[u8; 32], BackupError> {
    let mut key = [0_u8; 32];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|_| BackupError::Encryption)?;
    Ok(key)
}

fn write_archive_file(
    archive: &mut ZipWriter<File>,
    name: &str,
    bytes: &[u8],
    options: SimpleFileOptions,
) -> Result<(), BackupError> {
    archive
        .start_file(name, options)
        .map_err(|_| BackupError::Filesystem)?;
    archive
        .write_all(bytes)
        .map_err(|_| BackupError::Filesystem)
}

fn table_columns(table: &str) -> &'static [&'static str] {
    match table {
        "local_profiles" => &[
            "id",
            "name",
            "avatar",
            "role",
            "pin_hash",
            "created_at",
            "updated_at",
        ],
        "app_state" => &["key", "value"],
        "profile_settings" => &["profile_id", "key", "value"],
        "profile_core_storage" => &["profile_id", "key", "value"],
        "profile_integrations" => &[
            "profile_id",
            "provider",
            "enabled",
            "credential_ref",
            "metadata_json",
        ],
        "download_jobs" => &[
            "id",
            "profile_id",
            "title",
            "filename",
            "destination",
            "state",
            "source_kind",
            "credential_ref",
            "bytes_downloaded",
            "bytes_total",
            "error_code",
            "created_at",
            "updated_at",
            "completed_at",
        ],
        "local_roots" => &[
            "id",
            "path",
            "enabled",
            "recursive",
            "last_scan_at",
            "last_error",
        ],
        "local_media_items" => &[
            "id",
            "root_id",
            "path",
            "size_bytes",
            "modified_at",
            "fingerprint",
            "media_type",
            "title",
            "season",
            "episode",
            "metadata_json",
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_secret_export_passphrases_are_rejected() {
        assert_eq!(
            encrypt_secrets(SecretExport {
                passphrase: "short".to_owned(),
                entries: Vec::new(),
            }),
            Err(BackupError::WeakPassphrase)
        );
    }

    #[test]
    fn encrypted_secret_payload_round_trips_without_plaintext_leakage() {
        let encrypted = encrypt_secrets(SecretExport {
            passphrase: "correct horse battery staple".to_owned(),
            entries: vec![SecretExportEntry {
                credential_ref: "provider/test".to_owned(),
                kind: credential_store::SecretKind::ProviderApiKey,
                value: b"canary-secret".to_vec(),
            }],
        })
        .expect("encrypt");
        assert!(
            !encrypted
                .windows(13)
                .any(|window| window == b"canary-secret")
        );
        let decrypted =
            decrypt_secret_payload(&encrypted, "correct horse battery staple").expect("decrypt");
        assert_eq!(decrypted[0].value, b"canary-secret");
    }

    #[test]
    fn streaming_checksum_matches_the_original_json() {
        let source = br#"{"tables":{"app_state":[["key","value"]]}}"#;
        let mut reader = HashingReader::new(source.as_slice());
        let _: DatabasePayload = serde_json::from_reader(&mut reader).expect("payload");

        assert_eq!(reader.finalize(), blake3::hash(source));
    }

    #[test]
    fn restore_rejects_a_checksum_other_than_the_previewed_archive() {
        let manifest = BackupManifestV1 {
            version: BACKUP_VERSION,
            application_version: "test".to_owned(),
            created_at: 0,
            includes_secrets: false,
            payload_checksum: "confirmed".to_owned(),
            table_rows: BTreeMap::new(),
        };

        assert_eq!(
            verify_expected_checksum(&manifest, "replacement"),
            Err(BackupError::Integrity)
        );
    }
}
