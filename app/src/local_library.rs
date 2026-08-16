use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use slint::ComponentHandle;
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

const FINGERPRINT_CHUNK_SIZE: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalRoot {
    pub id: String,
    pub path: PathBuf,
    pub enabled: bool,
    pub recursive: bool,
    pub last_scan_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalMediaType {
    Movie,
    Episode,
    Unknown,
}

impl LocalMediaType {
    fn as_db(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Episode => "episode",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalMediaItem {
    pub id: String,
    pub root_id: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified_at: i64,
    pub fingerprint: String,
    pub media_type: LocalMediaType,
    pub title: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub subtitles: Vec<PathBuf>,
    pub metadata: serde_json::Value,
}

#[derive(Clone, Debug, Default)]
pub struct ScanProgress {
    pub root_id: String,
    pub visited: u64,
    pub matched: u64,
    pub complete: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LocalLibraryError {
    #[error("local media root is invalid")]
    InvalidRoot,
    #[error("local media database operation failed")]
    Database,
    #[error("local media scan operation failed")]
    Scan,
    #[error("local media watcher operation failed")]
    Watcher,
}

#[derive(Clone)]
pub struct LocalLibraryManager {
    progress: watch::Sender<ScanProgress>,
    watchers: Arc<Mutex<Vec<LocalWatcher>>>,
}

impl Default for LocalLibraryManager {
    fn default() -> Self {
        let (progress, _) = watch::channel(ScanProgress::default());
        Self {
            progress,
            watchers: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl LocalLibraryManager {
    pub fn subscribe(&self) -> watch::Receiver<ScanProgress> {
        self.progress.subscribe()
    }

    pub async fn add_root(&self, path: &Path) -> Result<LocalRoot, LocalLibraryError> {
        let path = tokio::fs::canonicalize(path)
            .await
            .map_err(|_| LocalLibraryError::InvalidRoot)?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|_| LocalLibraryError::InvalidRoot)?;
        if !metadata.is_dir() {
            return Err(LocalLibraryError::InvalidRoot);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let conn = crate::db::get_conn()
            .await
            .map_err(|_| LocalLibraryError::Database)?;
        conn.execute(
            "INSERT INTO local_roots(id, path, enabled, recursive) VALUES (?, ?, 1, 1)
             ON CONFLICT(path) DO UPDATE SET enabled = 1",
            (id.as_str(), path.to_string_lossy().as_ref()),
        )
        .await
        .map_err(|_| LocalLibraryError::Database)?;
        drop(conn);
        let root = list_roots()
            .await?
            .into_iter()
            .find(|root| root.path == path)
            .ok_or(LocalLibraryError::Database)?;
        self.retain_watcher(self.watch_root(root.clone())?);
        Ok(root)
    }

    pub async fn scan(&self, root: LocalRoot) -> Result<Vec<LocalMediaItem>, LocalLibraryError> {
        let progress = self.progress.clone();
        let root_for_scan = root.clone();
        let items = tokio::task::spawn_blocking(move || scan_root(&root_for_scan, &progress))
            .await
            .map_err(|_| LocalLibraryError::Scan)??;
        persist_scan(&root, &items).await?;
        let _ = self.progress.send(ScanProgress {
            root_id: root.id,
            visited: items.len() as u64,
            matched: items.len() as u64,
            complete: true,
            error: None,
        });
        Ok(items)
    }

    pub fn watch_root(&self, root: LocalRoot) -> Result<LocalWatcher, LocalLibraryError> {
        let (events_tx, mut events_rx) = hotpath::channel!(
            tokio::sync::mpsc::channel::<()>(64),
            label = "local_library_events"
        );
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                if event.is_ok() {
                    let _ = events_tx.try_send(());
                }
            })
            .map_err(|_| LocalLibraryError::Watcher)?;
        watcher
            .watch(
                &root.path,
                if root.recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                },
            )
            .map_err(|_| LocalLibraryError::Watcher)?;
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let manager = self.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = task_cancellation.cancelled() => break,
                    event = events_rx.recv() => {
                        if event.is_none() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(750)).await;
                        while events_rx.try_recv().is_ok() {}
                        let _ = manager.scan(root.clone()).await;
                    }
                }
            }
        });
        Ok(LocalWatcher {
            _watcher: watcher,
            cancellation,
            task: Some(task),
        })
    }

    pub async fn watch_enabled_roots(&self) -> Result<(), LocalLibraryError> {
        for root in list_roots().await?.into_iter().filter(|root| root.enabled) {
            self.retain_watcher(self.watch_root(root)?);
        }
        Ok(())
    }

    fn retain_watcher(&self, watcher: LocalWatcher) {
        self.watchers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(watcher);
    }
}

pub struct LocalWatcher {
    _watcher: RecommendedWatcher,
    cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl LocalWatcher {
    pub async fn stop(mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for LocalWatcher {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

pub async fn list_roots() -> Result<Vec<LocalRoot>, LocalLibraryError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| LocalLibraryError::Database)?;
    let mut rows = conn
        .query(
            "SELECT id, path, enabled, recursive, last_scan_at, last_error
             FROM local_roots ORDER BY path",
            (),
        )
        .await
        .map_err(|_| LocalLibraryError::Database)?;
    let mut roots = Vec::new();
    while let Some(row) = rows.next().await.map_err(|_| LocalLibraryError::Database)? {
        roots.push(LocalRoot {
            id: row.get(0).map_err(|_| LocalLibraryError::Database)?,
            path: PathBuf::from(
                row.get::<String>(1)
                    .map_err(|_| LocalLibraryError::Database)?,
            ),
            enabled: row.get::<i64>(2).map_err(|_| LocalLibraryError::Database)? != 0,
            recursive: row.get::<i64>(3).map_err(|_| LocalLibraryError::Database)? != 0,
            last_scan_at: row.get(4).map_err(|_| LocalLibraryError::Database)?,
            last_error: row.get(5).map_err(|_| LocalLibraryError::Database)?,
        });
    }
    Ok(roots)
}

pub async fn list_items() -> Result<Vec<LocalMediaItem>, LocalLibraryError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| LocalLibraryError::Database)?;
    let mut rows = conn
        .query(
            "SELECT id, root_id, path, size_bytes, modified_at, fingerprint,
                    media_type, title, season, episode, metadata_json
             FROM local_media_items ORDER BY title COLLATE NOCASE, path",
            (),
        )
        .await
        .map_err(|_| LocalLibraryError::Database)?;
    let mut items = Vec::new();
    while let Some(row) = rows.next().await.map_err(|_| LocalLibraryError::Database)? {
        let path = PathBuf::from(
            row.get::<String>(2)
                .map_err(|_| LocalLibraryError::Database)?,
        );
        let media_type = match row
            .get::<String>(6)
            .map_err(|_| LocalLibraryError::Database)?
            .as_str()
        {
            "movie" => LocalMediaType::Movie,
            "episode" => LocalMediaType::Episode,
            _ => LocalMediaType::Unknown,
        };
        let metadata_json = row
            .get::<String>(10)
            .map_err(|_| LocalLibraryError::Database)?;
        items.push(LocalMediaItem {
            id: row.get(0).map_err(|_| LocalLibraryError::Database)?,
            root_id: row.get(1).map_err(|_| LocalLibraryError::Database)?,
            path: path.clone(),
            size_bytes: row
                .get::<i64>(3)
                .map_err(|_| LocalLibraryError::Database)?
                .max(0) as u64,
            modified_at: row.get(4).map_err(|_| LocalLibraryError::Database)?,
            fingerprint: row.get(5).map_err(|_| LocalLibraryError::Database)?,
            media_type,
            title: row.get(7).map_err(|_| LocalLibraryError::Database)?,
            season: row
                .get::<Option<i64>>(8)
                .map_err(|_| LocalLibraryError::Database)?
                .and_then(|value| u32::try_from(value).ok()),
            episode: row
                .get::<Option<i64>>(9)
                .map_err(|_| LocalLibraryError::Database)?
                .and_then(|value| u32::try_from(value).ok()),
            subtitles: discover_subtitles(&path),
            metadata: serde_json::from_str(&metadata_json).unwrap_or_default(),
        });
    }
    Ok(items)
}

pub async fn item_by_id(id: &str) -> Result<Option<LocalMediaItem>, LocalLibraryError> {
    Ok(list_items().await?.into_iter().find(|item| item.id == id))
}

pub async fn project(ui_weak: slint::Weak<crate::MainWindow>) {
    let result = list_items().await;
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        match result {
            Ok(items) => {
                let columns = usize::try_from(ui.get_library_column_count())
                    .unwrap_or(6)
                    .max(1);
                let cards = items
                    .into_iter()
                    .map(|item| {
                        let description = item
                            .metadata
                            .get("plot")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        crate::MediaCardItem {
                            id: item.id.into(),
                            media_type: match item.media_type {
                                LocalMediaType::Movie => "movie",
                                LocalMediaType::Episode => "series",
                                LocalMediaType::Unknown => "other",
                            }
                            .into(),
                            video_id: "".into(),
                            title: item.title.into(),
                            poster_url: "".into(),
                            poster: slint::Image::default(),
                            description: description.into(),
                            show_checkmark: false,
                            show_progress: false,
                            progress_value: 0.0,
                            new_videos: 0,
                            can_play: true,
                        }
                    })
                    .collect::<Vec<_>>();
                let rows = cards
                    .chunks(columns)
                    .map(|chunk| crate::LibraryRow {
                        cols: slint::ModelRc::new(slint::VecModel::from(chunk.to_vec())),
                    })
                    .collect::<Vec<_>>();
                ui.set_local_library_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
                ui.set_local_library_scan_status("".into());
            }
            Err(error) => ui.set_local_library_scan_status(error.to_string().into()),
        }
    });
}

pub fn setup(
    ui: &crate::MainWindow,
    playback: Option<crate::mpv_integration::NativePlaybackBridge>,
    navigation: crate::NavigationController,
) {
    let manager = LocalLibraryManager::default();
    let ui_weak = ui.as_weak();
    ui.on_library_local_add_root({
        let manager = manager.clone();
        let ui_weak = ui_weak.clone();
        move |path| {
            let manager = manager.clone();
            let ui_weak = ui_weak.clone();
            let path = PathBuf::from(path.as_str());
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_local_library_scan_status("Scanning local media…".into());
            }
            tokio::spawn(async move {
                let result = async {
                    let root = manager.add_root(&path).await?;
                    manager.scan(root).await.map(|_| ())
                }
                .await;
                if let Err(error) = result {
                    let message = error.to_string();
                    let weak = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.set_local_library_scan_status(message.into());
                        }
                    });
                }
                project(ui_weak).await;
            });
        }
    });
    ui.on_library_local_rescan({
        let manager = manager.clone();
        let ui_weak = ui_weak.clone();
        move || {
            let manager = manager.clone();
            let ui_weak = ui_weak.clone();
            tokio::spawn(async move {
                let roots = list_roots().await.unwrap_or_default();
                for root in roots.into_iter().filter(|root| root.enabled) {
                    let _ = manager.scan(root).await;
                }
                let _ = repair_index().await;
                project(ui_weak).await;
            });
        }
    });
    ui.on_library_local_play({
        let ui_weak = ui_weak.clone();
        move |id| {
            let playback = playback.clone();
            let navigation = navigation.clone();
            let ui_weak = ui_weak.clone();
            let id = id.to_string();
            tokio::spawn(async move {
                let Ok(Some(item)) = item_by_id(&id).await else {
                    return;
                };
                let _ = slint::invoke_from_event_loop(move || {
                    if let (Some(ui), Some(playback)) = (ui_weak.upgrade(), playback.as_ref()) {
                        playback.play_local_file(&ui, &navigation, &item.path, &item.title);
                    }
                });
            });
        }
    });
    let mut progress = manager.subscribe();
    let progress_weak = ui_weak.clone();
    tokio::spawn(async move {
        while progress.changed().await.is_ok() {
            let current = progress.borrow().clone();
            let message = if current.complete {
                format!("Local scan complete · {} items", current.matched)
            } else {
                format!("Scanning local media · {} files checked", current.visited)
            };
            let weak = progress_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_local_library_scan_status(message.into());
                }
            });
        }
    });
    let startup_weak = ui_weak;
    tokio::spawn(async move {
        let _ = manager.watch_enabled_roots().await;
        project(startup_weak).await;
    });
}

fn scan_root(
    root: &LocalRoot,
    progress: &watch::Sender<ScanProgress>,
) -> Result<Vec<LocalMediaItem>, LocalLibraryError> {
    let walker = WalkDir::new(&root.path)
        .follow_links(false)
        .max_depth(if root.recursive { usize::MAX } else { 1 });
    let mut items = Vec::new();
    let mut visited = 0_u64;
    for entry in walker
        .into_iter()
        .filter_entry(|entry| !entry.path_is_symlink())
    {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        visited = visited.saturating_add(1);
        let path = entry.path();
        if !is_video(path) {
            continue;
        }
        items.push(inspect_media_file(root, path)?);
        if visited.is_multiple_of(32) {
            let _ = progress.send(ScanProgress {
                root_id: root.id.clone(),
                visited,
                matched: items.len() as u64,
                complete: false,
                error: None,
            });
        }
    }
    Ok(items)
}

fn inspect_media_file(root: &LocalRoot, path: &Path) -> Result<LocalMediaItem, LocalLibraryError> {
    let metadata = path.metadata().map_err(|_| LocalLibraryError::Scan)?;
    let size_bytes = metadata.len();
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();
    let fingerprint = fingerprint(path, size_bytes)?;
    let parsed = parse_filename(path);
    let nfo = read_nfo(path);
    let title = nfo
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or(parsed.title);
    let media_type = if parsed.season.is_some() && parsed.episode.is_some() {
        LocalMediaType::Episode
    } else if title.is_empty() {
        LocalMediaType::Unknown
    } else {
        LocalMediaType::Movie
    };
    Ok(LocalMediaItem {
        id: blake3::hash(path.to_string_lossy().as_bytes())
            .to_hex()
            .to_string(),
        root_id: root.id.clone(),
        path: path.to_path_buf(),
        size_bytes,
        modified_at,
        fingerprint,
        media_type,
        title,
        season: parsed.season,
        episode: parsed.episode,
        subtitles: discover_subtitles(path),
        metadata: nfo,
    })
}

async fn persist_scan(root: &LocalRoot, items: &[LocalMediaItem]) -> Result<(), LocalLibraryError> {
    let mut conn = crate::db::get_conn()
        .await
        .map_err(|_| LocalLibraryError::Database)?;
    let transaction = conn
        .transaction()
        .await
        .map_err(|_| LocalLibraryError::Database)?;
    for item in items {
        let moved = transaction
            .execute(
                "UPDATE local_media_items SET
                    root_id = ?, path = ?, size_bytes = ?, modified_at = ?, media_type = ?,
                    title = ?, season = ?, episode = ?, metadata_json = ?
                 WHERE fingerprint = ? AND size_bytes = ?",
                (
                    item.root_id.as_str(),
                    item.path.to_string_lossy().as_ref(),
                    item.size_bytes as i64,
                    item.modified_at,
                    item.media_type.as_db(),
                    item.title.as_str(),
                    item.season.map(i64::from),
                    item.episode.map(i64::from),
                    item.metadata.to_string(),
                    item.fingerprint.as_str(),
                    item.size_bytes as i64,
                ),
            )
            .await
            .map_err(|_| LocalLibraryError::Database)?;
        if moved == 0 {
            transaction
                .execute(
                    "INSERT INTO local_media_items(
                    id, root_id, path, size_bytes, modified_at, fingerprint, media_type,
                    title, season, episode, metadata_json
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(path) DO UPDATE SET
                    size_bytes = excluded.size_bytes, modified_at = excluded.modified_at,
                    fingerprint = excluded.fingerprint, media_type = excluded.media_type,
                    title = excluded.title, season = excluded.season,
                    episode = excluded.episode, metadata_json = excluded.metadata_json",
                    (
                        item.id.as_str(),
                        item.root_id.as_str(),
                        item.path.to_string_lossy().as_ref(),
                        item.size_bytes as i64,
                        item.modified_at,
                        item.fingerprint.as_str(),
                        item.media_type.as_db(),
                        item.title.as_str(),
                        item.season.map(i64::from),
                        item.episode.map(i64::from),
                        item.metadata.to_string(),
                    ),
                )
                .await
                .map_err(|_| LocalLibraryError::Database)?;
        }
    }
    transaction
        .execute(
            "UPDATE local_roots SET last_scan_at = ?, last_error = NULL WHERE id = ?",
            (chrono::Utc::now().timestamp(), root.id.as_str()),
        )
        .await
        .map_err(|_| LocalLibraryError::Database)?;
    transaction
        .commit()
        .await
        .map_err(|_| LocalLibraryError::Database)?;
    Ok(())
}

pub async fn repair_index() -> Result<usize, LocalLibraryError> {
    let items = list_items().await?;
    let missing = items
        .into_iter()
        .filter(|item| !item.path.is_file())
        .map(|item| item.id)
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(0);
    }
    let mut conn = crate::db::get_conn()
        .await
        .map_err(|_| LocalLibraryError::Database)?;
    let transaction = conn
        .transaction()
        .await
        .map_err(|_| LocalLibraryError::Database)?;
    for id in &missing {
        transaction
            .execute("DELETE FROM local_media_items WHERE id = ?", [id.as_str()])
            .await
            .map_err(|_| LocalLibraryError::Database)?;
    }
    transaction
        .commit()
        .await
        .map_err(|_| LocalLibraryError::Database)?;
    Ok(missing.len())
}

struct ParsedFilename {
    title: String,
    season: Option<u32>,
    episode: Option<u32>,
}

fn parse_filename(path: &Path) -> ParsedFilename {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let normalized = stem.replace(['.', '_'], " ");
    let lower = normalized.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for index in 0..bytes.len().saturating_sub(5) {
        if bytes[index] == b's'
            && bytes[index + 1..index + 3].iter().all(u8::is_ascii_digit)
            && bytes[index + 3] == b'e'
            && bytes[index + 4..index + 6].iter().all(u8::is_ascii_digit)
        {
            return ParsedFilename {
                title: normalized[..index].trim().to_owned(),
                season: lower[index + 1..index + 3].parse().ok(),
                episode: lower[index + 4..index + 6].parse().ok(),
            };
        }
    }
    ParsedFilename {
        title: normalized.trim().to_owned(),
        season: None,
        episode: None,
    }
}

fn fingerprint(path: &Path, size: u64) -> Result<String, LocalLibraryError> {
    let mut file = File::open(path).map_err(|_| LocalLibraryError::Scan)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());
    let mut buffer = vec![0_u8; FINGERPRINT_CHUNK_SIZE];
    let read = file
        .read(&mut buffer)
        .map_err(|_| LocalLibraryError::Scan)?;
    hasher.update(&buffer[..read]);
    if size > FINGERPRINT_CHUNK_SIZE as u64 {
        file.seek(SeekFrom::End(-(FINGERPRINT_CHUNK_SIZE as i64)))
            .map_err(|_| LocalLibraryError::Scan)?;
        let read = file
            .read(&mut buffer)
            .map_err(|_| LocalLibraryError::Scan)?;
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn read_nfo(video_path: &Path) -> serde_json::Value {
    let Ok(contents) = std::fs::read_to_string(video_path.with_extension("nfo")) else {
        return serde_json::json!({});
    };
    serde_json::json!({
        "title": xml_tag(&contents, "title"),
        "plot": xml_tag(&contents, "plot")
    })
}

fn xml_tag(contents: &str, tag: &str) -> Option<String> {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let start = contents.find(&start_tag)? + start_tag.len();
    let end = contents[start..].find(&end_tag)? + start;
    Some(contents[start..end].trim().to_owned())
}

fn discover_subtitles(video_path: &Path) -> Vec<PathBuf> {
    let Some(parent) = video_path.parent() else {
        return Vec::new();
    };
    let stem = video_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let mut subtitles = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_subtitle(path))
        .filter(|path| {
            path.file_stem()
                .and_then(|value| value.to_str())
                .is_some_and(|subtitle_stem| subtitle_stem.starts_with(stem))
        })
        .collect::<Vec<_>>();
    subtitles.sort();
    subtitles
}

fn is_video(path: &Path) -> bool {
    extension_in(
        path,
        &[
            "mkv", "mp4", "avi", "mov", "wmv", "webm", "m4v", "ts", "m2ts",
        ],
    )
}

fn is_subtitle(path: &Path) -> bool {
    extension_in(path, &["srt", "ass", "ssa", "vtt", "sub"])
}

fn extension_in(path: &Path, supported: &[&str]) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            supported
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_episode_names_without_guessing_unrelated_numbers() {
        let parsed = parse_filename(Path::new("The.Show.S02E07.1080p.mkv"));
        assert_eq!(parsed.title, "The Show");
        assert_eq!(parsed.season, Some(2));
        assert_eq!(parsed.episode, Some(7));
        let movie = parse_filename(Path::new("2001 A Space Odyssey.mkv"));
        assert_eq!(movie.season, None);
        assert_eq!(movie.episode, None);
    }

    #[test]
    fn extracts_small_nfo_fields() {
        assert_eq!(
            xml_tag("<movie><title>Arrival</title></movie>", "title"),
            Some("Arrival".to_owned())
        );
    }
}
