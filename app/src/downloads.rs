use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use credential_store::{CredentialError, CredentialRef, CredentialStore, SecretKind, SecretValue};
use futures::StreamExt;
use reqwest::{Client, StatusCode, header::RANGE};
use serde::{Deserialize, Serialize};
use slint::ComponentHandle;
use tokio::{
    io::AsyncWriteExt,
    sync::{Mutex, Semaphore, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DownloadState {
    Queued,
    Resolving,
    Downloading,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl DownloadState {
    fn as_db(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Resolving => "resolving",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DownloadSourceKind {
    DirectHttp,
    EmbeddedServer,
}

impl DownloadSourceKind {
    fn as_db(self) -> &'static str {
        match self {
            Self::DirectHttp => "direct-http",
            Self::EmbeddedServer => "embedded-server",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DownloadSource {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub kind: DownloadSourceKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DownloadJob {
    pub id: String,
    pub profile_id: String,
    pub title: String,
    pub filename: String,
    pub destination: PathBuf,
    pub state: DownloadState,
    pub source_kind: DownloadSourceKind,
    pub credential_ref: CredentialRef,
    pub bytes_downloaded: u64,
    pub bytes_total: Option<u64>,
    pub error_code: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug)]
pub struct DownloadProgress {
    pub job_id: String,
    pub state: DownloadState,
    pub bytes_downloaded: u64,
    pub bytes_total: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DownloadError {
    #[error("download source is invalid")]
    InvalidSource,
    #[error("download destination is invalid")]
    InvalidDestination,
    #[error("credential vault operation failed: {0}")]
    Credential(#[from] CredentialError),
    #[error("download database operation failed")]
    Database,
    #[error("download network operation failed")]
    Network,
    #[error("server does not support safe download resumption")]
    RangeUnsupported,
    #[error("download filesystem operation failed")]
    Filesystem,
    #[error("download job does not exist")]
    Missing,
}

struct ActiveDownload {
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

#[derive(Clone)]
pub struct DownloadManager {
    credentials: Arc<dyn CredentialStore>,
    client: Client,
    permits: Arc<Semaphore>,
    active: Arc<Mutex<HashMap<String, ActiveDownload>>>,
    progress: watch::Sender<Option<DownloadProgress>>,
    bandwidth_limit: Arc<AtomicU64>,
    bandwidth_gate: Arc<Mutex<std::time::Instant>>,
}

pub async fn list_jobs(profile_id: &str) -> Result<Vec<DownloadJob>, DownloadError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| DownloadError::Database)?;
    let mut rows = conn
        .query(
            "SELECT id, profile_id, title, filename, destination, state, source_kind,
                    credential_ref, bytes_downloaded, bytes_total, error_code, created_at, updated_at
             FROM download_jobs WHERE profile_id = ? ORDER BY created_at DESC",
            [profile_id],
        )
        .await
        .map_err(|_| DownloadError::Database)?;
    let mut jobs = Vec::new();
    while let Some(row) = rows.next().await.map_err(|_| DownloadError::Database)? {
        jobs.push(job_from_row(&row)?);
    }
    Ok(jobs)
}

pub async fn job(job_id: &str) -> Result<DownloadJob, DownloadError> {
    load_job(job_id).await
}

pub fn setup_ui_callbacks(
    ui: &crate::MainWindow,
    manager: Arc<DownloadManager>,
    playback: Option<crate::mpv_integration::NativePlaybackBridge>,
    navigation: crate::NavigationController,
) {
    let refresh = |ui: &crate::MainWindow| {
        ui.set_downloads_loading(true);
        tokio::spawn(project_active_profile(ui.as_weak()));
    };
    refresh(ui);
    ui.on_downloads_refresh({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_downloads_loading(true);
            }
            tokio::spawn(project_active_profile(weak.clone()));
        }
    });
    ui.on_downloads_pause({
        let manager = manager.clone();
        let weak = ui.as_weak();
        move |job_id| {
            let manager = manager.clone();
            let weak = weak.clone();
            let job_id = job_id.to_string();
            tokio::spawn(async move {
                if let Err(error) = manager.pause(&job_id).await {
                    set_downloads_error(&weak, error.to_string());
                }
                project_active_profile(weak).await;
            });
        }
    });
    ui.on_downloads_resume({
        let manager = manager.clone();
        let weak = ui.as_weak();
        move |job_id| {
            let manager = manager.clone();
            let weak = weak.clone();
            let job_id = job_id.to_string();
            tokio::spawn(async move {
                if let Err(error) = manager.resume(&job_id).await {
                    set_downloads_error(&weak, error.to_string());
                }
                project_active_profile(weak).await;
            });
        }
    });
    ui.on_downloads_cancel({
        let manager = manager.clone();
        let weak = ui.as_weak();
        move |job_id| {
            let manager = manager.clone();
            let weak = weak.clone();
            let job_id = job_id.to_string();
            tokio::spawn(async move {
                if let Err(error) = manager.cancel(&job_id).await {
                    set_downloads_error(&weak, error.to_string());
                }
                project_active_profile(weak).await;
            });
        }
    });
    ui.on_downloads_reveal({
        let weak = ui.as_weak();
        move |job_id| {
            let weak = weak.clone();
            let job_id = job_id.to_string();
            tokio::spawn(async move {
                match job(&job_id).await {
                    Ok(job) => {
                        let target = if job.destination.exists() {
                            job.destination
                        } else {
                            job.destination
                                .parent()
                                .map(Path::to_path_buf)
                                .unwrap_or(job.destination)
                        };
                        if let Err(error) = open::that_detached(target) {
                            set_downloads_error(&weak, error.to_string());
                        }
                    }
                    Err(error) => {
                        set_downloads_error(&weak, error.to_string());
                    }
                }
            });
        }
    });
    let playback_for_jobs = playback.clone();
    ui.on_downloads_play({
        let weak = ui.as_weak();
        move |job_id| {
            let weak = weak.clone();
            let playback = playback_for_jobs.clone();
            let navigation = navigation.clone();
            let job_id = job_id.to_string();
            tokio::spawn(async move {
                match job(&job_id).await {
                    Ok(job) if job.state == DownloadState::Completed => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let (Some(ui), Some(playback)) = (weak.upgrade(), playback) {
                                playback.play_local_file(
                                    &ui,
                                    &navigation,
                                    &job.destination,
                                    &job.title,
                                );
                            }
                        });
                    }
                    Ok(_) => {}
                    Err(error) => {
                        set_downloads_error(&weak, error.to_string());
                    }
                }
            });
        }
    });
    if let Some(playback) = playback {
        ui.on_player_download_video({
            let manager = manager.clone();
            let playback = playback.clone();
            let weak = ui.as_weak();
            move || {
                let Some(source_url) = playback.current_source() else {
                    return;
                };
                let title = weak
                    .upgrade()
                    .map(|ui| ui.get_player_title().to_string())
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| "Stremio download".to_owned());
                enqueue_player_source(
                    manager.clone(),
                    weak.clone(),
                    source_url,
                    title,
                    DownloadSourceKind::DirectHttp,
                    None,
                );
            }
        });
        ui.on_player_download_subs({
            let manager = manager.clone();
            let playback = playback.clone();
            let weak = ui.as_weak();
            move || {
                let Some(source_url) = playback.current_external_subtitle() else {
                    return;
                };
                let title = weak
                    .upgrade()
                    .map(|ui| ui.get_player_title().to_string())
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| "Stremio subtitles".to_owned());
                enqueue_player_source(
                    manager.clone(),
                    weak.clone(),
                    source_url,
                    title,
                    DownloadSourceKind::DirectHttp,
                    Some("srt"),
                );
            }
        });
    }
}

fn enqueue_player_source(
    manager: Arc<DownloadManager>,
    ui_weak: slint::Weak<crate::MainWindow>,
    source_url: String,
    title: String,
    kind: DownloadSourceKind,
    forced_extension: Option<&'static str>,
) {
    tokio::spawn(async move {
        let result = async {
            let profile_id = crate::profiles::active_profile_id()
                .await
                .map_err(|_| DownloadError::Database)?;
            let extension = forced_extension
                .map(ToOwned::to_owned)
                .or_else(|| {
                    url::Url::parse(&source_url).ok().and_then(|url| {
                        Path::new(url.path())
                            .extension()
                            .and_then(|value| value.to_str())
                            .filter(|value| value.len() <= 8)
                            .map(ToOwned::to_owned)
                    })
                })
                .unwrap_or_else(|| "mp4".to_owned());
            manager
                .enqueue(
                    profile_id.as_str(),
                    &title,
                    &format!("{title}.{extension}"),
                    crate::paths::get().downloads(),
                    DownloadSource {
                        url: source_url,
                        headers: Vec::new(),
                        kind,
                    },
                )
                .await?;
            Ok::<(), DownloadError>(())
        }
        .await;
        if let Err(error) = result {
            let message = error.to_string();
            let weak = ui_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_error_message(message.into());
                }
            });
        }
        project_active_profile(ui_weak).await;
    });
}

fn set_downloads_error(ui_weak: &slint::Weak<crate::MainWindow>, message: String) {
    let weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_downloads_error(message.into());
        }
    });
}

pub async fn project_active_profile(ui_weak: slint::Weak<crate::MainWindow>) {
    let result = async {
        let profile_id = crate::profiles::active_profile_id()
            .await
            .map_err(|_| DownloadError::Database)?;
        list_jobs(profile_id.as_str()).await
    }
    .await;
    let (items, error) = match result {
        Ok(jobs) => (
            jobs.into_iter()
                .map(|job| {
                    let progress = job
                        .bytes_total
                        .filter(|total| *total > 0)
                        .map(|total| job.bytes_downloaded as f32 / total as f32)
                        .unwrap_or_default()
                        .clamp(0.0, 1.0);
                    let status = match job.state {
                        DownloadState::Queued => "Queued".to_owned(),
                        DownloadState::Resolving => "Resolving source…".to_owned(),
                        DownloadState::Downloading => format!(
                            "{} / {}",
                            format_bytes(job.bytes_downloaded),
                            job.bytes_total
                                .map(format_bytes)
                                .unwrap_or_else(|| "Unknown".to_owned())
                        ),
                        DownloadState::Paused => "Paused".to_owned(),
                        DownloadState::Completed => "Completed".to_owned(),
                        DownloadState::Failed => format!(
                            "Failed: {}",
                            job.error_code.as_deref().unwrap_or("unknown error")
                        ),
                        DownloadState::Cancelled => "Cancelled".to_owned(),
                    };
                    crate::DownloadItem {
                        id: job.id.into(),
                        title: job.title.into(),
                        filename: job.filename.into(),
                        destination: job.destination.to_string_lossy().into_owned().into(),
                        status: status.into(),
                        progress,
                        can_pause: matches!(
                            job.state,
                            DownloadState::Queued
                                | DownloadState::Resolving
                                | DownloadState::Downloading
                        ),
                        can_resume: matches!(
                            job.state,
                            DownloadState::Paused
                                | DownloadState::Failed
                                | DownloadState::Cancelled
                        ),
                        can_play: job.state == DownloadState::Completed,
                        can_cancel: matches!(
                            job.state,
                            DownloadState::Queued
                                | DownloadState::Resolving
                                | DownloadState::Downloading
                                | DownloadState::Paused
                        ),
                        can_delete: matches!(
                            job.state,
                            DownloadState::Completed
                                | DownloadState::Failed
                                | DownloadState::Cancelled
                        ),
                    }
                })
                .collect::<Vec<_>>(),
            None,
        ),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_download_items(slint::ModelRc::new(slint::VecModel::from(items)));
            ui.set_downloads_error(error.unwrap_or_default().into());
            ui.set_downloads_loading(false);
        }
    });
}

impl DownloadManager {
    pub fn new(credentials: Arc<dyn CredentialStore>, concurrency: usize) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            .user_agent("Stremio-Native/1")
            .build()
            .unwrap_or_default();
        let (progress, _) = watch::channel(None);
        Self {
            credentials,
            client,
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
            active: Arc::new(Mutex::new(HashMap::new())),
            progress,
            bandwidth_limit: Arc::new(AtomicU64::new(0)),
            bandwidth_gate: Arc::new(Mutex::new(std::time::Instant::now())),
        }
    }

    pub fn set_bandwidth_limit(&self, bytes_per_second: Option<u64>) {
        self.bandwidth_limit
            .store(bytes_per_second.unwrap_or_default(), Ordering::Release);
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<DownloadProgress>> {
        self.progress.subscribe()
    }

    pub async fn resume_profile(&self, profile_id: &str) -> Result<(), DownloadError> {
        for mut job in list_jobs(profile_id).await? {
            if matches!(
                job.state,
                DownloadState::Queued | DownloadState::Resolving | DownloadState::Downloading
            ) {
                job.state = DownloadState::Queued;
                update_state(&job.id, DownloadState::Queued, None).await?;
                self.start(job).await;
            }
        }
        Ok(())
    }

    pub async fn pause_profile(&self, profile_id: &str) -> Result<(), DownloadError> {
        for job in list_jobs(profile_id).await? {
            if matches!(
                job.state,
                DownloadState::Queued | DownloadState::Resolving | DownloadState::Downloading
            ) {
                self.pause(&job.id).await?;
            }
        }
        Ok(())
    }

    pub async fn enqueue(
        &self,
        profile_id: &str,
        title: &str,
        suggested_filename: &str,
        destination_dir: &Path,
        source: DownloadSource,
    ) -> Result<DownloadJob, DownloadError> {
        let url = url::Url::parse(&source.url).map_err(|_| DownloadError::InvalidSource)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(DownloadError::InvalidSource);
        }
        if !destination_dir.is_absolute() {
            return Err(DownloadError::InvalidDestination);
        }
        tokio::fs::create_dir_all(destination_dir)
            .await
            .map_err(|_| DownloadError::Filesystem)?;
        let filename = sanitize_filename(suggested_filename);
        let destination = collision_safe_path(destination_dir, &filename).await;
        let id = uuid::Uuid::new_v4().to_string();
        let credential_ref = CredentialRef::new(format!("download-source/{id}"))?;
        let source_json = serde_json::to_vec(&source).map_err(|_| DownloadError::InvalidSource)?;
        self.credentials
            .put(
                &credential_ref,
                SecretKind::DownloadSource,
                SecretValue::new(source_json),
            )
            .await?;
        let now = chrono::Utc::now().timestamp();
        let job = DownloadJob {
            id,
            profile_id: profile_id.to_owned(),
            title: title.trim().chars().take(240).collect(),
            filename,
            destination,
            state: DownloadState::Queued,
            source_kind: source.kind,
            credential_ref,
            bytes_downloaded: 0,
            bytes_total: None,
            error_code: None,
            created_at: now,
            updated_at: now,
        };
        if let Err(error) = persist_new_job(&job).await {
            let _ = self.credentials.delete(&job.credential_ref).await;
            return Err(error);
        }
        self.start(job.clone()).await;
        Ok(job)
    }

    pub async fn pause(&self, job_id: &str) -> Result<(), DownloadError> {
        if let Some(active) = self.active.lock().await.remove(job_id) {
            active.cancellation.cancel();
            let _ = active.task.await;
        }
        update_state(job_id, DownloadState::Paused, None).await
    }

    pub async fn resume(&self, job_id: &str) -> Result<(), DownloadError> {
        let mut job = load_job(job_id).await?;
        if job.state == DownloadState::Completed {
            return Ok(());
        }
        job.state = DownloadState::Queued;
        update_state(job_id, DownloadState::Queued, None).await?;
        self.start(job).await;
        Ok(())
    }

    pub async fn cancel(&self, job_id: &str) -> Result<(), DownloadError> {
        if let Some(active) = self.active.lock().await.remove(job_id) {
            active.cancellation.cancel();
            let _ = active.task.await;
        }
        update_state(job_id, DownloadState::Cancelled, None).await
    }

    async fn start(&self, job: DownloadJob) {
        if self.active.lock().await.contains_key(&job.id) {
            return;
        }
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let manager = self.clone();
        let job_id = job.id.clone();
        let task = tokio::spawn(async move {
            let result = manager.run_job(&job, task_cancellation.clone()).await;
            if task_cancellation.is_cancelled() {
                return;
            }
            if let Err(error) = result {
                let code = error_code(&error);
                let _ = update_state(&job.id, DownloadState::Failed, Some(code)).await;
                let _ = manager.progress.send(Some(DownloadProgress {
                    job_id: job.id.clone(),
                    state: DownloadState::Failed,
                    bytes_downloaded: job.bytes_downloaded,
                    bytes_total: job.bytes_total,
                }));
            }
            manager.active.lock().await.remove(&job.id);
        });
        self.active
            .lock()
            .await
            .insert(job_id, ActiveDownload { cancellation, task });
    }

    async fn run_job(
        &self,
        job: &DownloadJob,
        cancellation: CancellationToken,
    ) -> Result<(), DownloadError> {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| DownloadError::Network)?;
        update_state(&job.id, DownloadState::Resolving, None).await?;
        let secret = self.credentials.get(&job.credential_ref).await?;
        let source: DownloadSource =
            serde_json::from_slice(secret.expose()).map_err(|_| DownloadError::InvalidSource)?;
        let part_path = part_path(&job.destination);
        let resume_from = tokio::fs::metadata(&part_path)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or_default();
        let mut request = self.client.get(&source.url);
        for (name, value) in &source.headers {
            request = request.header(name, value);
        }
        if resume_from > 0 {
            request = request.header(RANGE, format!("bytes={resume_from}-"));
        }
        let response = request.send().await.map_err(|_| DownloadError::Network)?;
        if resume_from > 0 && response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(DownloadError::RangeUnsupported);
        }
        if !response.status().is_success() {
            return Err(DownloadError::Network);
        }
        let bytes_total = response
            .content_length()
            .map(|remaining| remaining.saturating_add(resume_from));
        update_progress(
            &job.id,
            DownloadState::Downloading,
            resume_from,
            bytes_total,
        )
        .await?;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part_path)
            .await
            .map_err(|_| DownloadError::Filesystem)?;
        let mut downloaded = resume_from;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            if cancellation.is_cancelled() {
                file.flush().await.map_err(|_| DownloadError::Filesystem)?;
                return Ok(());
            }
            let chunk = chunk.map_err(|_| DownloadError::Network)?;
            self.throttle(chunk.len() as u64).await;
            file.write_all(&chunk)
                .await
                .map_err(|_| DownloadError::Filesystem)?;
            downloaded = downloaded.saturating_add(chunk.len() as u64);
            update_progress(&job.id, DownloadState::Downloading, downloaded, bytes_total).await?;
            let _ = self.progress.send(Some(DownloadProgress {
                job_id: job.id.clone(),
                state: DownloadState::Downloading,
                bytes_downloaded: downloaded,
                bytes_total,
            }));
        }
        file.flush().await.map_err(|_| DownloadError::Filesystem)?;
        drop(file);
        tokio::fs::rename(&part_path, &job.destination)
            .await
            .map_err(|_| DownloadError::Filesystem)?;
        update_progress(&job.id, DownloadState::Completed, downloaded, bytes_total).await?;
        let _ = self.credentials.delete(&job.credential_ref).await;
        let _ = self.progress.send(Some(DownloadProgress {
            job_id: job.id.clone(),
            state: DownloadState::Completed,
            bytes_downloaded: downloaded,
            bytes_total,
        }));
        Ok(())
    }

    async fn throttle(&self, bytes: u64) {
        let limit = self.bandwidth_limit.load(Ordering::Acquire);
        if limit == 0 || bytes == 0 {
            return;
        }
        let duration = Duration::from_secs_f64(bytes as f64 / limit as f64);
        let sleep_until = {
            let mut next = self.bandwidth_gate.lock().await;
            let now = std::time::Instant::now();
            let start = (*next).max(now);
            *next = start + duration;
            start
        };
        tokio::time::sleep_until(tokio::time::Instant::from_std(sleep_until)).await;
    }
}

async fn persist_new_job(job: &DownloadJob) -> Result<(), DownloadError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| DownloadError::Database)?;
    conn.execute(
        "INSERT INTO download_jobs(
            id, profile_id, title, filename, destination, state, source_kind,
            credential_ref, bytes_downloaded, bytes_total, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            job.id.as_str(),
            job.profile_id.as_str(),
            job.title.as_str(),
            job.filename.as_str(),
            job.destination.to_string_lossy().as_ref(),
            job.state.as_db(),
            job.source_kind.as_db(),
            job.credential_ref.expose_id(),
            job.bytes_downloaded as i64,
            job.bytes_total.map(|value| value as i64),
            job.created_at,
            job.updated_at,
        ),
    )
    .await
    .map_err(|_| DownloadError::Database)?;
    Ok(())
}

async fn load_job(job_id: &str) -> Result<DownloadJob, DownloadError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| DownloadError::Database)?;
    let mut rows = conn
        .query(
            "SELECT id, profile_id, title, filename, destination, state, source_kind,
                    credential_ref, bytes_downloaded, bytes_total, error_code, created_at, updated_at
             FROM download_jobs WHERE id = ?",
            [job_id],
        )
        .await
        .map_err(|_| DownloadError::Database)?;
    let row = rows
        .next()
        .await
        .map_err(|_| DownloadError::Database)?
        .ok_or(DownloadError::Missing)?;
    job_from_row(&row)
}

fn job_from_row(row: &turso::Row) -> Result<DownloadJob, DownloadError> {
    Ok(DownloadJob {
        id: row.get(0).map_err(|_| DownloadError::Database)?,
        profile_id: row.get(1).map_err(|_| DownloadError::Database)?,
        title: row.get(2).map_err(|_| DownloadError::Database)?,
        filename: row.get(3).map_err(|_| DownloadError::Database)?,
        destination: PathBuf::from(row.get::<String>(4).map_err(|_| DownloadError::Database)?),
        state: parse_state(&row.get::<String>(5).map_err(|_| DownloadError::Database)?),
        source_kind: parse_source_kind(&row.get::<String>(6).map_err(|_| DownloadError::Database)?),
        credential_ref: CredentialRef::new(
            row.get::<String>(7).map_err(|_| DownloadError::Database)?,
        )?,
        bytes_downloaded: row
            .get::<i64>(8)
            .map_err(|_| DownloadError::Database)?
            .max(0) as u64,
        bytes_total: row
            .get::<Option<i64>>(9)
            .map_err(|_| DownloadError::Database)?
            .map(|value| value.max(0) as u64),
        error_code: row.get(10).map_err(|_| DownloadError::Database)?,
        created_at: row.get(11).map_err(|_| DownloadError::Database)?,
        updated_at: row.get(12).map_err(|_| DownloadError::Database)?,
    })
}

async fn update_state(
    job_id: &str,
    state: DownloadState,
    error_code: Option<&str>,
) -> Result<(), DownloadError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| DownloadError::Database)?;
    conn.execute(
        "UPDATE download_jobs SET state = ?, error_code = ?, updated_at = ? WHERE id = ?",
        (
            state.as_db(),
            error_code,
            chrono::Utc::now().timestamp(),
            job_id,
        ),
    )
    .await
    .map_err(|_| DownloadError::Database)?;
    Ok(())
}

async fn update_progress(
    job_id: &str,
    state: DownloadState,
    downloaded: u64,
    total: Option<u64>,
) -> Result<(), DownloadError> {
    let conn = crate::db::get_conn()
        .await
        .map_err(|_| DownloadError::Database)?;
    let completed_at = (state == DownloadState::Completed).then(|| chrono::Utc::now().timestamp());
    conn.execute(
        "UPDATE download_jobs
         SET state = ?, bytes_downloaded = ?, bytes_total = ?, error_code = NULL,
             updated_at = ?, completed_at = ? WHERE id = ?",
        (
            state.as_db(),
            downloaded as i64,
            total.map(|value| value as i64),
            chrono::Utc::now().timestamp(),
            completed_at,
            job_id,
        ),
    )
    .await
    .map_err(|_| DownloadError::Database)?;
    Ok(())
}

fn parse_state(value: &str) -> DownloadState {
    match value {
        "resolving" => DownloadState::Resolving,
        "downloading" => DownloadState::Downloading,
        "paused" => DownloadState::Paused,
        "completed" => DownloadState::Completed,
        "failed" => DownloadState::Failed,
        "cancelled" => DownloadState::Cancelled,
        _ => DownloadState::Queued,
    }
}

fn parse_source_kind(value: &str) -> DownloadSourceKind {
    match value {
        "embedded-server" => DownloadSourceKind::EmbeddedServer,
        _ => DownloadSourceKind::DirectHttp,
    }
}

fn sanitize_filename(value: &str) -> String {
    let sanitized = value
        .trim()
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
            {
                '_'
            } else {
                character
            }
        })
        .take(180)
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_owned();
    if sanitized.is_empty() {
        "video.mp4".to_owned()
    } else {
        sanitized
    }
}

async fn collision_safe_path(directory: &Path, filename: &str) -> PathBuf {
    let path = directory.join(filename);
    if !tokio::fs::try_exists(&path).await.unwrap_or(true) {
        return path;
    }
    let name = Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("video");
    let extension = Path::new(filename)
        .extension()
        .and_then(|value| value.to_str());
    for suffix in 2..=10_000 {
        let candidate = match extension {
            Some(extension) => directory.join(format!("{name} ({suffix}).{extension}")),
            None => directory.join(format!("{name} ({suffix})")),
        };
        if !tokio::fs::try_exists(&candidate).await.unwrap_or(true) {
            return candidate;
        }
    }
    directory.join(format!("{}-{filename}", uuid::Uuid::new_v4()))
}

fn part_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("video")
        .to_owned();
    name.push_str(".part");
    destination.with_file_name(name)
}

fn error_code(error: &DownloadError) -> &'static str {
    match error {
        DownloadError::InvalidSource => "invalid-source",
        DownloadError::InvalidDestination => "invalid-destination",
        DownloadError::Credential(_) => "credential-unavailable",
        DownloadError::Database => "database",
        DownloadError::Network => "network",
        DownloadError::RangeUnsupported => "range-unsupported",
        DownloadError::Filesystem => "filesystem",
        DownloadError::Missing => "missing",
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_sanitized_without_losing_extensions() {
        assert_eq!(sanitize_filename("A/B: Episode?.mkv"), "A_B_ Episode_.mkv");
        assert_eq!(sanitize_filename("..."), "video.mp4");
    }

    #[test]
    fn part_files_are_adjacent_to_the_destination() {
        assert_eq!(
            part_path(Path::new("C:/Media/Movie.mkv")),
            PathBuf::from("C:/Media/Movie.mkv.part")
        );
    }
}
