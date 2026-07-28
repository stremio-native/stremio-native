use std::sync::{
    OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::{collections::HashMap, path::Path};
use turso::{Builder, Connection, Database};

static DB: OnceLock<Database> = OnceLock::new();
static DB_CONNECTION: OnceLock<Connection> = OnceLock::new();
static LOG_INSERTS_SINCE_CLEANUP: AtomicUsize = AtomicUsize::new(0);

const MAX_LOG_ROWS: i64 = 10_000;
const LOG_CLEANUP_INTERVAL: usize = 64;

#[tracing::instrument(skip(database_path))]
pub async fn init_db(database_path: &Path) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    let db_path = database_path.to_string_lossy().into_owned();
    tracing::info!(path = %db_path, "Initializing Turso local database...");
    let db = Builder::new_local(&db_path).build().await?;

    let conn = db.connect()?;

    // `journal_mode` returns the selected mode, so it must use the query path;
    // Turso's no-row executor rejects it with "unexpected row during execution".
    // Keep the remaining no-row pragmas and schema in one batch, and keep all
    // of this work after the first window is already being serviced.
    let mut journal_mode_rows = conn.query("PRAGMA journal_mode = WAL", ()).await?;
    let journal_mode = journal_mode_rows
        .next()
        .await?
        .map(|row| row.get::<String>(0))
        .transpose()?;
    if !matches!(journal_mode.as_deref(), Some(mode) if mode.eq_ignore_ascii_case("wal")) {
        tracing::warn!(
            journal_mode = journal_mode.as_deref().unwrap_or("unknown"),
            "database did not enable WAL journal mode"
        );
    }
    drop(journal_mode_rows);

    conn.execute_batch(
        "
        PRAGMA synchronous = NORMAL;
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -10000;
        PRAGMA busy_timeout = 5000;

        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp INTEGER NOT NULL,
            level TEXT NOT NULL,
            message TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS core_storage (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS http_cache (
            cache_key TEXT PRIMARY KEY,
            resource_kind TEXT NOT NULL,
            status INTEGER NOT NULL,
            body BLOB NOT NULL,
            etag TEXT,
            last_modified TEXT,
            stored_at INTEGER NOT NULL,
            validated_at INTEGER NOT NULL,
            accessed_at INTEGER NOT NULL,
            size_bytes INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS http_cache_accessed_idx
            ON http_cache(accessed_at);
        CREATE INDEX IF NOT EXISTS http_cache_validated_idx
            ON http_cache(validated_at);
        ",
    )
    .await?;

    let core_database = db.clone();
    DB.set(db)
        .map_err(|_| anyhow::anyhow!("DB already initialized"))?;
    DB_CONNECTION
        .set(conn)
        .map_err(|_| anyhow::anyhow!("DB connection already initialized"))?;
    if let Err(error) = core_env::install_database(core_database) {
        tracing::debug!(%error, "core storage database was initialized before app storage");
    }

    tracing::info!(
        elapsed_ms = start.elapsed().as_millis(),
        "Turso database schemas created/verified and optimizations applied"
    );

    tokio::spawn(async {
        // Keep cleanup I/O out of the cold-start and first-frame window.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if let Err(error) = run_startup_maintenance().await {
            tracing::warn!(%error, "deferred database maintenance failed");
        }
    });

    Ok(())
}

async fn run_startup_maintenance() -> anyhow::Result<()> {
    let conn = get_conn()?;
    // The active image pipeline uses the bounded memory and filesystem caches.
    conn.execute("DROP TABLE IF EXISTS image_cache", ()).await?;
    prune_logs(&conn).await?;
    core_env::maintain_http_cache()
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    insert_log("INFO", "Embedded Turso database initialized successfully.").await
}

pub fn get_conn() -> anyhow::Result<Connection> {
    DB_CONNECTION
        .get()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("DB not initialized"))
}

// === Settings Helpers ===

#[tracing::instrument(skip(key))]
pub async fn get_setting(key: &str) -> anyhow::Result<Option<String>> {
    let start = std::time::Instant::now();
    let conn = get_conn()?;
    let mut rows = conn
        .query("SELECT value FROM settings WHERE key = ?", [key])
        .await?;
    let res = if let Some(row) = rows.next().await? {
        let val: String = row.get(0)?;
        Ok(Some(val))
    } else {
        Ok(None)
    };
    tracing::info!(
        key = %key,
        elapsed_ms = start.elapsed().as_millis(),
        success = res.is_ok(),
        found = res.as_ref().map(|opt| opt.is_some()).unwrap_or(false),
        "DB: get_setting"
    );
    res
}

#[tracing::instrument(skip(keys))]
pub async fn get_settings(keys: &[&str]) -> anyhow::Result<HashMap<String, String>> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let conn = get_conn()?;
    let placeholders = std::iter::repeat_n("?", keys.len())
        .collect::<Vec<_>>()
        .join(", ");
    let query = format!("SELECT key, value FROM settings WHERE key IN ({placeholders})");
    let mut rows = conn.query(&query, keys.to_vec()).await?;
    let mut settings = HashMap::with_capacity(keys.len());
    while let Some(row) = rows.next().await? {
        settings.insert(row.get(0)?, row.get(1)?);
    }
    Ok(settings)
}

#[tracing::instrument(skip(key, value))]
pub async fn set_setting(key: &str, value: &str) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    let conn = get_conn()?;
    let res = conn
        .execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)",
            [key, value],
        )
        .await;
    tracing::info!(
        key = %key,
        elapsed_ms = start.elapsed().as_millis(),
        success = res.is_ok(),
        "DB: set_setting"
    );
    res.map(|_| ()).map_err(Into::into)
}

#[tracing::instrument(skip(values))]
pub async fn set_settings(values: &[(&str, &str)]) -> anyhow::Result<()> {
    if values.is_empty() {
        return Ok(());
    }
    let mut conn = get_conn()?;
    let transaction = conn.transaction().await?;
    for &(key, value) in values {
        transaction
            .execute(
                "INSERT INTO settings (key, value) VALUES (?, ?) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                (key, value),
            )
            .await?;
    }
    transaction.commit().await?;
    Ok(())
}

// === Logs Helpers ===

pub async fn insert_log(level: &str, message: &str) -> anyhow::Result<()> {
    let conn = get_conn()?;
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO logs (timestamp, level, message) VALUES (?, ?, ?)",
        (now, level.to_owned(), message.to_owned()),
    )
    .await?;
    let previous_insert_count = LOG_INSERTS_SINCE_CLEANUP.fetch_add(1, Ordering::Relaxed);
    if previous_insert_count % LOG_CLEANUP_INTERVAL == LOG_CLEANUP_INTERVAL - 1 {
        // Retention maintenance is best-effort and must not add a large DELETE
        // to the latency of the log write that happened to cross the threshold.
        tokio::spawn(async move {
            if let Err(error) = prune_logs(&conn).await {
                tracing::warn!(%error, "background log retention maintenance failed");
            }
        });
    }
    Ok(())
}

async fn prune_logs(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM logs
         WHERE id < COALESCE(
             (SELECT id FROM logs ORDER BY id DESC LIMIT 1 OFFSET ?),
             -1
         )",
        [MAX_LOG_ROWS - 1],
    )
    .await?;
    Ok(())
}

pub async fn get_logs(limit: usize) -> anyhow::Result<Vec<String>> {
    let conn = get_conn()?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut rows = conn
        .query(
            "SELECT timestamp, level, message FROM logs ORDER BY id DESC LIMIT ?",
            [limit],
        )
        .await?;

    let mut entries = Vec::new();
    while let Some(row) = rows.next().await? {
        let ts: i64 = row.get(0)?;
        let level: String = row.get(1)?;
        let msg: String = row.get(2)?;

        let time_str = chrono::DateTime::from_timestamp(ts, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();

        entries.push(format!("[{}] [{}] {}", time_str, level, msg));
    }
    Ok(entries)
}
