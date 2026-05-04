use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, KeyIvInit, StreamCipher};
use aes::Aes128;
use base64::{engine::general_purpose, Engine as _};
use ctr::Ctr128BE;
use reqwest::header::RANGE;
use reqwest::StatusCode;
use sanitize_filename::sanitize;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256, Sha512};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{File as StdFile, OpenOptions as StdOpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::State;
use tokio::io::AsyncReadExt;
use url::Url;
use uuid::Uuid;

const MEGA_API: &str = "https://g.api.mega.co.nz/cs";
const DEFAULT_CONNECTIONS: usize = 8;
const MAX_CONNECTIONS: usize = 32;
const DEFAULT_CHUNK_MB: u64 = 8;
const MIN_CHUNK_BYTES: u64 = 1024 * 1024;
const MAX_CHUNK_BYTES: u64 = 64 * 1024 * 1024;
const BALANCED_MAX_CONNECTIONS: usize = 16;
const BALANCED_MAX_CHUNK_MB: u64 = 16;
const LOW_CPU_MAX_CONNECTIONS: usize = 4;
const LOW_CPU_MAX_CHUNK_MB: u64 = 4;
const LOW_CPU_CHUNK_COOLDOWN_MS: u64 = 8;
const SPEED_WINDOW_MS: u128 = 5_000;
const SPEED_MIN_WINDOW_MS: u128 = 1_000;
const RESUME_MANIFEST_SAVE_EVERY_CHUNKS: usize = 8;
const RESUME_MANIFEST_SAVE_INTERVAL_MS: u64 = 1_500;
const MEGA_PASSWORD_LINK_ALGORITHM: u8 = 2;
const MEGA_PASSWORD_LINK_ITERATIONS: u32 = 100_000;
const MAX_ACTIVE_TASKS: usize = 2;
const FOLDER_FILE_RETRY_ROUNDS: usize = 3;
const MAX_FOLDER_FILES_PER_TASK: usize = 50;
const MAX_PARALLEL_FOLDER_FILES: usize = 6;
const LOW_CPU_MAX_PARALLEL_FOLDER_FILES: usize = 2;

type Aes128Ctr = Ctr128BE<Aes128>;

#[derive(Clone)]
struct AppState {
    tasks: Arc<Mutex<HashMap<String, TaskSnapshot>>>,
    controls: Arc<Mutex<HashMap<String, Arc<TaskControl>>>>,
    artifacts: Arc<Mutex<HashMap<String, BTreeSet<PathBuf>>>>,
    speed_meters: Arc<Mutex<HashMap<String, SpeedMeter>>>,
    scheduler: Arc<tokio::sync::Semaphore>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            tasks: Arc::new(Mutex::new(HashMap::new())),
            controls: Arc::new(Mutex::new(HashMap::new())),
            artifacts: Arc::new(Mutex::new(HashMap::new())),
            speed_meters: Arc::new(Mutex::new(HashMap::new())),
            scheduler: Arc::new(tokio::sync::Semaphore::new(MAX_ACTIVE_TASKS)),
        }
    }
}

struct TaskControl {
    cancelled: AtomicBool,
    paused: AtomicBool,
    notify: tokio::sync::Notify,
}

impl TaskControl {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.paused.store(false, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    async fn wait_if_paused(
        &self,
        state: &AppState,
        task_id: &str,
        resume_status: TaskStatus,
    ) -> Result<(), DownloadError> {
        loop {
            if self.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            if !self.is_paused() {
                return Ok(());
            }

            state.set_status(task_id, TaskStatus::Paused);
            self.notify.notified().await;
            if self.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            if !self.is_paused() {
                state.set_status(task_id, resume_status);
                return Ok(());
            }
        }
    }

    async fn wait_for_change(&self) {
        self.notify.notified().await;
    }
}

#[derive(Debug, Clone)]
struct SpeedMeter {
    samples: VecDeque<SpeedSample>,
    ema_bps: f64,
}

#[derive(Debug, Clone)]
struct SpeedSample {
    at: u128,
    bytes: u64,
}

impl SpeedMeter {
    fn new(bytes: u64, at: u128) -> Self {
        let mut samples = VecDeque::new();
        samples.push_back(SpeedSample { at, bytes });
        Self {
            samples,
            ema_bps: 0.0,
        }
    }

    fn record(&mut self, bytes: u64, at: u128) -> f64 {
        match self.samples.back() {
            Some(last) if bytes < last.bytes => {
                *self = Self::new(bytes, at);
                return 0.0;
            }
            Some(last) if bytes == last.bytes => return self.ema_bps,
            Some(last) if last.at == at => {
                if let Some(last) = self.samples.back_mut() {
                    last.bytes = bytes;
                }
            }
            _ => self.samples.push_back(SpeedSample { at, bytes }),
        }

        while self.samples.len() > 2
            && at.saturating_sub(self.samples.front().map(|sample| sample.at).unwrap_or(at))
                > SPEED_WINDOW_MS
        {
            self.samples.pop_front();
        }

        let Some(first) = self.samples.front() else {
            return 0.0;
        };
        let Some(last) = self.samples.back() else {
            return 0.0;
        };
        let elapsed_ms = last.at.saturating_sub(first.at);
        let delta_bytes = last.bytes.saturating_sub(first.bytes);
        if elapsed_ms < SPEED_MIN_WINDOW_MS || delta_bytes == 0 {
            return self.ema_bps;
        }

        let window_bps = (delta_bytes as f64 * 1000.0) / elapsed_ms as f64;
        self.ema_bps = if self.ema_bps > 0.0 {
            self.ema_bps * 0.7 + window_bps * 0.3
        } else {
            window_bps
        };
        self.ema_bps
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskSnapshot {
    id: String,
    url: String,
    file_name: String,
    output_dir: String,
    output_path: String,
    current_file: Option<String>,
    status: TaskStatus,
    total_bytes: u64,
    downloaded_bytes: u64,
    speed_bps: f64,
    connections: usize,
    chunk_size_bytes: u64,
    overwrite: bool,
    verify_integrity: bool,
    performance_mode: PerformanceMode,
    retry_mode: RetryMode,
    error: Option<String>,
    created_at: u128,
    updated_at: u128,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum TaskStatus {
    Queued,
    Resolving,
    Downloading,
    Paused,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadRequest {
    url: String,
    output_dir: Option<String>,
    password: Option<String>,
    connections: Option<usize>,
    chunk_size_mb: Option<u64>,
    overwrite: Option<bool>,
    verify_integrity: Option<bool>,
    performance_mode: Option<PerformanceMode>,
    retry_mode: Option<RetryMode>,
    low_cpu_mode: Option<bool>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum PerformanceMode {
    Balanced,
    Fast,
    LowImpact,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
enum RetryMode {
    Auto,
    Manual,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkPreview {
    kind: String,
    name: String,
    total_bytes: u64,
    file_count: usize,
    requires_password: bool,
}

#[derive(Debug, Clone)]
enum PublicLink {
    File(MegaFileLink),
    Folder(MegaFolderLink),
}

#[derive(Debug, Clone)]
struct MegaFileLink {
    file_id: String,
    key: String,
}

#[derive(Debug, Clone)]
struct MegaFolderLink {
    folder_id: String,
    key: String,
    selected_handle: Option<String>,
}

#[derive(Debug, Clone)]
struct FileCrypto {
    aes_key: [u8; 16],
    iv_words: [u32; 2],
    meta_mac: [u32; 2],
}

#[derive(Debug, Clone)]
struct FileMetadata {
    name: String,
    size: u64,
    direct_url: String,
    crypto: FileCrypto,
    manifest_id: String,
}

#[derive(Debug, Clone)]
struct FolderPlan {
    name: String,
    total_bytes: u64,
    files: Vec<FolderFilePlan>,
    folder_id: String,
    create_root_dir: bool,
}

#[derive(Debug, Clone)]
struct FolderFilePlan {
    name: String,
    relative_path: PathBuf,
    size: u64,
    handle: String,
    crypto: FileCrypto,
    manifest_id: String,
}

#[derive(Debug, Clone)]
struct FolderFileFailure {
    file: FolderFilePlan,
    error: String,
}

#[derive(Debug, Clone)]
struct Chunk {
    start: u64,
    end: u64,
}

impl Chunk {
    fn len(&self) -> u64 {
        self.end - self.start + 1
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResumeManifest {
    manifest_id: String,
    file_name: String,
    total_bytes: u64,
    chunk_size: u64,
    completed: BTreeSet<u64>,
}

#[derive(Debug)]
struct ManifestState {
    manifest: ResumeManifest,
    dirty_chunks: usize,
    last_saved_at: Instant,
}

impl ManifestState {
    fn new(manifest: ResumeManifest) -> Self {
        Self {
            manifest,
            dirty_chunks: 0,
            last_saved_at: Instant::now(),
        }
    }

    fn mark_completed(&mut self, chunk_start: u64) -> Option<ResumeManifest> {
        if !self.manifest.completed.insert(chunk_start) {
            return None;
        }

        self.dirty_chunks += 1;
        if self.dirty_chunks >= RESUME_MANIFEST_SAVE_EVERY_CHUNKS
            || self.last_saved_at.elapsed()
                >= Duration::from_millis(RESUME_MANIFEST_SAVE_INTERVAL_MS)
        {
            self.dirty_chunks = 0;
            self.last_saved_at = Instant::now();
            Some(self.manifest.clone())
        } else {
            None
        }
    }

    fn snapshot(&self) -> ResumeManifest {
        self.manifest.clone()
    }
}

#[derive(Debug, Deserialize)]
struct RawNode {
    h: String,
    p: Option<String>,
    t: u8,
    a: Option<String>,
    k: Option<String>,
    s: Option<u64>,
}

#[derive(Debug, Clone)]
struct FolderNode {
    name: String,
    parent: Option<String>,
    kind: u8,
}

#[derive(Debug, thiserror::Error)]
enum DownloadError {
    #[error("{0}")]
    Message(String),
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),
    #[error("base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("download cancelled")]
    Cancelled,
    #[error("password-protected MEGA link requires a password")]
    PasswordRequired,
    #[error("MEGA API returned {0}")]
    MegaApi(i64),
    #[error("HTTP status {0} while downloading")]
    HttpStatus(u16),
    #[error("downloaded chunk length mismatch at byte {start}: expected {expected}, got {actual}")]
    ChunkLength {
        start: u64,
        expected: u64,
        actual: u64,
    },
    #[error("download integrity check failed")]
    MacMismatch,
}

impl AppState {
    fn insert_task(&self, task: TaskSnapshot, control: Arc<TaskControl>) {
        let task_id = task.id.clone();
        let created_at = task.created_at;
        let downloaded_bytes = task.downloaded_bytes;
        self.tasks
            .lock()
            .expect("task lock poisoned")
            .insert(task_id.clone(), task.clone());
        self.controls
            .lock()
            .expect("control lock poisoned")
            .insert(task_id.clone(), control);
        self.speed_meters
            .lock()
            .expect("speed lock poisoned")
            .insert(task_id, SpeedMeter::new(downloaded_bytes, created_at));
    }

    fn sorted_tasks(&self) -> Vec<TaskSnapshot> {
        let mut tasks: Vec<_> = self
            .tasks
            .lock()
            .expect("task lock poisoned")
            .values()
            .cloned()
            .collect();
        tasks.sort_by_key(|task| task.created_at);
        tasks
    }

    fn task_snapshot(&self, id: &str) -> Option<TaskSnapshot> {
        self.tasks
            .lock()
            .expect("task lock poisoned")
            .get(id)
            .cloned()
    }

    fn delete_finished_task(&self, id: &str) -> Result<TaskSnapshot, String> {
        if self
            .controls
            .lock()
            .map_err(|_| "control lock poisoned".to_string())?
            .contains_key(id)
        {
            return Err("任务仍在运行或收尾，请先取消并等待状态刷新。".to_string());
        }

        let removed = {
            let mut tasks = self
                .tasks
                .lock()
                .map_err(|_| "task lock poisoned".to_string())?;
            let Some(task) = tasks.get(id).cloned() else {
                return Err("任务不存在。".to_string());
            };
            if !task.status.is_terminal() {
                return Err("只能删除已完成、失败或已取消的任务。".to_string());
            }
            tasks.remove(id)
        }
        .ok_or_else(|| "任务不存在。".to_string())?;

        self.speed_meters
            .lock()
            .map_err(|_| "speed lock poisoned".to_string())?
            .remove(id);
        self.artifacts
            .lock()
            .map_err(|_| "artifact lock poisoned".to_string())?
            .remove(id);
        Ok(removed)
    }

    fn clear_finished_tasks(&self) -> Result<usize, String> {
        let running_ids: BTreeSet<String> = self
            .controls
            .lock()
            .map_err(|_| "control lock poisoned".to_string())?
            .keys()
            .cloned()
            .collect();

        let removable_ids: Vec<String> = {
            let tasks = self
                .tasks
                .lock()
                .map_err(|_| "task lock poisoned".to_string())?;
            tasks
                .iter()
                .filter(|(id, task)| task.status.is_terminal() && !running_ids.contains(*id))
                .map(|(id, _)| id.clone())
                .collect()
        };

        if removable_ids.is_empty() {
            return Ok(0);
        }

        {
            let mut tasks = self
                .tasks
                .lock()
                .map_err(|_| "task lock poisoned".to_string())?;
            for id in &removable_ids {
                tasks.remove(id);
            }
        }
        {
            let mut meters = self
                .speed_meters
                .lock()
                .map_err(|_| "speed lock poisoned".to_string())?;
            for id in &removable_ids {
                meters.remove(id);
            }
        }
        {
            let mut artifacts = self
                .artifacts
                .lock()
                .map_err(|_| "artifact lock poisoned".to_string())?;
            for id in &removable_ids {
                artifacts.remove(id);
            }
        }

        Ok(removable_ids.len())
    }

    fn update<F>(&self, id: &str, mutate: F) -> Option<TaskSnapshot>
    where
        F: FnOnce(&mut TaskSnapshot),
    {
        let mut tasks = self.tasks.lock().expect("task lock poisoned");
        let task = tasks.get_mut(id)?;
        mutate(task);
        task.updated_at = now_millis();
        Some(task.clone())
    }

    fn set_status(&self, id: &str, status: TaskStatus) {
        let should_zero_speed = !matches!(status, TaskStatus::Downloading);
        let is_terminal = matches!(
            status,
            TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Failed
        );
        self.update(id, |task| {
            task.status = status;
            if should_zero_speed {
                task.speed_bps = 0.0;
            }
        });
        if is_terminal {
            self.speed_meters
                .lock()
                .expect("speed lock poisoned")
                .remove(id);
        }
    }

    fn set_error(&self, id: &str, status: TaskStatus, error: String) {
        let is_terminal = matches!(
            status,
            TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Failed
        );
        self.update(id, |task| {
            task.status = status;
            task.error = Some(error);
            task.speed_bps = 0.0;
        });
        if is_terminal {
            self.speed_meters
                .lock()
                .expect("speed lock poisoned")
                .remove(id);
        }
    }

    fn set_resolved(&self, id: &str, name: String, output: String, total: u64) {
        let now = now_millis();
        self.speed_meters
            .lock()
            .expect("speed lock poisoned")
            .insert(id.to_string(), SpeedMeter::new(0, now));
        self.update(id, |task| {
            task.file_name = name;
            task.output_path = output;
            task.total_bytes = total;
            task.status = TaskStatus::Downloading;
            task.error = None;
        });
    }

    fn set_current_file(&self, id: &str, current_file: Option<String>) {
        self.update(id, |task| {
            task.current_file = current_file;
        });
    }

    fn set_progress(&self, id: &str, downloaded: u64, total: u64) {
        let now = now_millis();
        let speed_bps = {
            let mut meters = self.speed_meters.lock().expect("speed lock poisoned");
            let meter = meters
                .entry(id.to_string())
                .or_insert_with(|| SpeedMeter::new(downloaded, now));
            meter.record(downloaded, now)
        };
        self.update(id, |task| {
            task.speed_bps = speed_bps;
            task.downloaded_bytes = downloaded;
            task.total_bytes = total;
        });
    }

    fn set_progress_baseline(&self, id: &str, downloaded: u64, total: u64) {
        let now = now_millis();
        self.speed_meters
            .lock()
            .expect("speed lock poisoned")
            .insert(id.to_string(), SpeedMeter::new(downloaded, now));
        self.update(id, |task| {
            task.speed_bps = 0.0;
            task.downloaded_bytes = downloaded;
            task.total_bytes = total;
        });
    }

    fn get_control(&self, id: &str) -> Result<Arc<TaskControl>, String> {
        self.controls
            .lock()
            .map_err(|_| "control lock poisoned".to_string())?
            .get(id)
            .cloned()
            .ok_or_else(|| "task is not running".to_string())
    }

    fn controls_snapshot(&self) -> Result<Vec<(String, Arc<TaskControl>)>, String> {
        Ok(self
            .controls
            .lock()
            .map_err(|_| "control lock poisoned".to_string())?
            .iter()
            .map(|(id, control)| (id.clone(), control.clone()))
            .collect())
    }

    fn remove_control(&self, id: &str) {
        self.controls
            .lock()
            .expect("control lock poisoned")
            .remove(id);
    }

    fn register_artifacts(&self, id: &str, paths: impl IntoIterator<Item = PathBuf>) {
        let mut artifacts = self.artifacts.lock().expect("artifact lock poisoned");
        let task_artifacts = artifacts.entry(id.to_string()).or_default();
        task_artifacts.extend(paths);
    }

    fn artifacts_snapshot(&self, id: &str) -> Vec<PathBuf> {
        self.artifacts
            .lock()
            .expect("artifact lock poisoned")
            .get(id)
            .map(|paths| paths.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn clear_artifacts(&self, id: &str) {
        self.artifacts
            .lock()
            .expect("artifact lock poisoned")
            .remove(id);
    }
}

#[tauri::command]
fn default_download_dir() -> Result<String, String> {
    Ok(default_download_path()
        .map_err(|err| err.to_string())?
        .display()
        .to_string())
}

#[tauri::command]
async fn choose_download_dir() -> Result<Option<String>, String> {
    let selected = tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择保存文件夹")
            .pick_folder()
    })
    .await
    .map_err(|err| err.to_string())?;

    Ok(selected.map(|path| path.display().to_string()))
}

#[tauri::command]
fn list_tasks(state: State<'_, AppState>) -> Vec<TaskSnapshot> {
    state.sorted_tasks()
}

#[tauri::command]
fn cancel_download(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let control = state.get_control(&id)?;
    control.cancel();
    state.set_status(&id, TaskStatus::Cancelled);
    Ok(())
}

#[tauri::command]
fn pause_download(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let control = state.get_control(&id)?;
    control.pause();
    state.set_status(&id, TaskStatus::Paused);
    Ok(())
}

#[tauri::command]
fn resume_download(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let control = state.get_control(&id)?;
    control.resume();
    state.set_status(&id, TaskStatus::Queued);
    Ok(())
}

#[tauri::command]
fn pause_all_downloads(state: State<'_, AppState>) -> Result<(), String> {
    for (id, control) in state.controls_snapshot()? {
        control.pause();
        state.set_status(&id, TaskStatus::Paused);
    }
    Ok(())
}

#[tauri::command]
fn resume_all_downloads(state: State<'_, AppState>) -> Result<(), String> {
    for (id, control) in state.controls_snapshot()? {
        control.resume();
        state.set_status(&id, TaskStatus::Queued);
    }
    Ok(())
}

#[tauri::command]
fn delete_task(state: State<'_, AppState>, id: String) -> Result<TaskSnapshot, String> {
    state.delete_finished_task(&id)
}

#[tauri::command]
fn clear_finished_tasks(state: State<'_, AppState>) -> Result<usize, String> {
    state.clear_finished_tasks()
}

#[tauri::command]
fn open_task_file(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let (task, path, _) = completed_task_path(state.inner(), &id)?;
    open_path(&path).map_err(|err| format!("无法打开 {}：{err}", task.file_name))
}

#[tauri::command]
fn open_task_folder(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let (_, path, metadata) = completed_task_path(state.inner(), &id)?;
    let folder = if metadata.is_dir() {
        path
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| "无法定位保存文件夹。".to_string())?
    };

    match std::fs::metadata(&folder) {
        Ok(metadata) if metadata.is_dir() => open_path(&folder),
        Ok(_) => Err("保存路径不是文件夹，可能已变更。".to_string()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err("保存文件夹不存在，可能已删除或路径已变更。".to_string())
        }
        Err(err) => Err(format!("无法读取保存文件夹：{err}")),
    }
}

fn completed_task_path(
    state: &AppState,
    id: &str,
) -> Result<(TaskSnapshot, PathBuf, std::fs::Metadata), String> {
    let task = state
        .task_snapshot(id)
        .ok_or_else(|| "任务不存在。".to_string())?;
    if !matches!(task.status, TaskStatus::Completed) {
        return Err("只有已完成任务可以打开文件或文件夹。".to_string());
    }
    if task.output_path.trim().is_empty() {
        return Err("任务没有保存路径。".to_string());
    }

    let path = PathBuf::from(&task.output_path);
    match std::fs::metadata(&path) {
        Ok(metadata) => Ok((task, path, metadata)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Err("文件不存在，可能已删除或路径已变更。".to_string())
        }
        Err(err) => Err(format!("无法读取保存路径：{err}")),
    }
}

#[cfg(target_os = "windows")]
fn open_path(path: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    std::process::Command::new("rundll32.exe")
        .arg("url.dll,FileProtocolHandler")
        .arg(path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|err| err.to_string())
}

#[cfg(target_os = "macos")]
fn open_path(path: &Path) -> Result<(), String> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    std::process::Command::new("open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| err.to_string())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_path(path: &Path) -> Result<(), String> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|err| err.to_string())
}

async fn cleanup_task_artifacts(state: &AppState, task_id: &str) -> usize {
    let mut paths = state.artifacts_snapshot(task_id);
    if paths.is_empty() {
        if let Some(task) = state.task_snapshot(task_id) {
            let output_path = PathBuf::from(task.output_path);
            paths.push(part_path_for(&output_path));
            paths.push(manifest_path_for(&output_path));
        }
    }

    let removed = tauri::async_runtime::spawn_blocking(move || remove_artifact_files(paths))
        .await
        .unwrap_or(0);
    state.clear_artifacts(task_id);
    removed
}

fn remove_artifact_files(paths: Vec<PathBuf>) -> usize {
    paths.into_iter().fold(0, |removed, path| {
        if std::fs::remove_file(&path).is_ok() {
            removed + 1
        } else {
            removed
        }
    })
}

#[tauri::command]
async fn inspect_link(url: String, password: Option<String>) -> Result<LinkPreview, String> {
    let parsed = match parse_public_link(&url, non_empty_password(password.as_deref())) {
        Ok(parsed) => parsed,
        Err(DownloadError::PasswordRequired) => {
            return Ok(LinkPreview {
                kind: "protected".to_string(),
                name: "密码保护链接".to_string(),
                total_bytes: 0,
                file_count: 0,
                requires_password: true,
            });
        }
        Err(err) => return Err(err.to_string()),
    };

    let client = make_http_client().map_err(|err| err.to_string())?;
    match parsed {
        PublicLink::File(link) => {
            let metadata = fetch_file_metadata(&client, &link)
                .await
                .map_err(|err| err.to_string())?;
            Ok(LinkPreview {
                kind: "file".to_string(),
                name: metadata.name,
                total_bytes: metadata.size,
                file_count: 1,
                requires_password: false,
            })
        }
        PublicLink::Folder(link) => {
            let plan = fetch_folder_plan(&client, &link)
                .await
                .map_err(|err| err.to_string())?;
            Ok(LinkPreview {
                kind: "folder".to_string(),
                name: plan.name,
                total_bytes: plan.total_bytes,
                file_count: plan.files.len(),
                requires_password: false,
            })
        }
    }
}

#[tauri::command]
async fn start_download(
    state: State<'_, AppState>,
    request: DownloadRequest,
) -> Result<TaskSnapshot, String> {
    let parsed = parse_public_link(
        &request.url,
        non_empty_password(request.password.as_deref()),
    )
    .map_err(|err| err.to_string())?;
    let output_dir = request
        .output_dir
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or(default_download_path().map_err(|err| err.to_string())?);

    let performance_mode = resolve_performance_mode(&request);
    let retry_mode = request.retry_mode.unwrap_or(RetryMode::Auto);
    let overwrite = request.overwrite.unwrap_or(false);
    let verify_integrity = request.verify_integrity.unwrap_or(false);
    let requested_connections = request
        .connections
        .unwrap_or(DEFAULT_CONNECTIONS)
        .clamp(1, MAX_CONNECTIONS);
    let connections = effective_connections(requested_connections, performance_mode);
    let requested_chunk_mb = request.chunk_size_mb.unwrap_or(DEFAULT_CHUNK_MB);
    let chunk_mb = effective_chunk_mb(requested_chunk_mb, performance_mode);
    let chunk_size = normalize_chunk_size(chunk_mb);
    let now = now_millis();
    let id = Uuid::new_v4().to_string();
    let control = Arc::new(TaskControl::new());
    let task = TaskSnapshot {
        id: id.clone(),
        url: request.url.clone(),
        file_name: "Resolving link".to_string(),
        output_dir: output_dir.display().to_string(),
        output_path: output_dir.display().to_string(),
        current_file: None,
        status: TaskStatus::Queued,
        total_bytes: 0,
        downloaded_bytes: 0,
        speed_bps: 0.0,
        connections,
        chunk_size_bytes: chunk_size,
        overwrite,
        verify_integrity,
        performance_mode,
        retry_mode,
        error: None,
        created_at: now,
        updated_at: now,
    };

    let app_state = state.inner().clone();
    app_state.insert_task(task.clone(), control.clone());

    tauri::async_runtime::spawn(async move {
        let task_id = id.clone();
        let result = async {
            let _permit = acquire_task_slot(&app_state, &control, &task_id).await?;
            run_download_task(
                app_state.clone(),
                task_id.clone(),
                parsed,
                request,
                output_dir,
                connections,
                chunk_size,
                control.clone(),
            )
            .await
        }
        .await;

        match result {
            Ok(()) => {
                app_state.set_status(&task_id, TaskStatus::Completed);
                app_state.clear_artifacts(&task_id);
            }
            Err(DownloadError::Cancelled) => {
                let removed = cleanup_task_artifacts(&app_state, &task_id).await;
                let message = if removed == 0 {
                    "已取消".to_string()
                } else {
                    format!("已取消，已清理 {removed} 个临时文件")
                };
                app_state.set_error(&task_id, TaskStatus::Cancelled, message);
            }
            Err(err) => app_state.set_error(&task_id, TaskStatus::Failed, err.to_string()),
        }
        app_state.remove_control(&task_id);
    });

    Ok(task)
}

async fn acquire_task_slot(
    state: &AppState,
    control: &Arc<TaskControl>,
    task_id: &str,
) -> Result<tokio::sync::OwnedSemaphorePermit, DownloadError> {
    loop {
        control
            .wait_if_paused(state, task_id, TaskStatus::Queued)
            .await?;
        if control.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }

        let permit = tokio::select! {
            permit = state.scheduler.clone().acquire_owned() => {
                permit.map_err(|_| DownloadError::Message("download scheduler closed".to_string()))?
            }
            _ = control.wait_for_change() => continue,
        };

        if control.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        if control.is_paused() {
            drop(permit);
            continue;
        }

        return Ok(permit);
    }
}

async fn run_download_task(
    state: AppState,
    task_id: String,
    parsed: PublicLink,
    request: DownloadRequest,
    output_dir: PathBuf,
    connections: usize,
    chunk_size: u64,
    control: Arc<TaskControl>,
) -> Result<(), DownloadError> {
    control
        .wait_if_paused(&state, &task_id, TaskStatus::Resolving)
        .await?;
    state.set_status(&task_id, TaskStatus::Resolving);
    let client = make_http_client()?;
    let overwrite = request.overwrite.unwrap_or(false);
    let verify_integrity = request.verify_integrity.unwrap_or(false);
    let retry_mode = request.retry_mode.unwrap_or(RetryMode::Auto);
    let low_cpu_mode = matches!(resolve_performance_mode(&request), PerformanceMode::LowImpact);

    match parsed {
        PublicLink::File(link) => {
            let metadata = fetch_file_metadata(&client, &link).await?;
            let output_path = prepare_file_output_path(&output_dir, &metadata.name, overwrite)?;
            control
                .wait_if_paused(&state, &task_id, TaskStatus::Downloading)
                .await?;
            state.set_resolved(
                &task_id,
                metadata.name.clone(),
                output_path.display().to_string(),
                metadata.size,
            );
            download_file_to_path(
                &client,
                &metadata.direct_url,
                &output_path,
                &metadata.crypto,
                metadata.size,
                chunk_size,
                connections,
                overwrite,
                verify_integrity,
                low_cpu_mode,
                control,
                &state,
                &task_id,
                0,
                metadata.size,
                None,
                true,
                true,
                metadata.manifest_id,
            )
            .await?;
        }
        PublicLink::Folder(link) => {
            let plan = fetch_folder_plan(&client, &link).await?;
            let folder_name = sanitize_component(&plan.name);
            let root = if plan.create_root_dir {
                output_dir.join(&folder_name)
            } else {
                output_dir.clone()
            };
            tokio::fs::create_dir_all(&root).await?;
            control
                .wait_if_paused(&state, &task_id, TaskStatus::Downloading)
                .await?;
            let display_path = if !plan.create_root_dir && plan.files.len() == 1 {
                root.join(&plan.files[0].relative_path)
            } else {
                root.clone()
            };
            state.set_resolved(
                &task_id,
                plan.name.clone(),
                display_path.display().to_string(),
                plan.total_bytes,
            );
            download_folder_files(
                &client,
                plan,
                root,
                chunk_size,
                connections,
                overwrite,
                verify_integrity,
                retry_mode,
                low_cpu_mode,
                control,
                &state,
                &task_id,
            )
            .await?;
        }
    }

    Ok(())
}

async fn download_folder_files(
    client: &reqwest::Client,
    plan: FolderPlan,
    root: PathBuf,
    chunk_size: u64,
    connections: usize,
    overwrite: bool,
    verify_integrity: bool,
    retry_mode: RetryMode,
    low_cpu_mode: bool,
    control: Arc<TaskControl>,
    state: &AppState,
    task_id: &str,
) -> Result<(), DownloadError> {
    let total_bytes = plan.total_bytes;
    let folder_id = plan.folder_id.clone();
    let file_parallelism = folder_file_parallelism(connections, low_cpu_mode, plan.files.len());
    let file_connections = folder_file_connections(connections, file_parallelism);
    let retry_rounds = match retry_mode {
        RetryMode::Auto => FOLDER_FILE_RETRY_ROUNDS,
        RetryMode::Manual => 0,
    };
    let shared_downloaded = Arc::new(AtomicU64::new(0));
    let mut pending_files = plan.files;

    if pending_files.is_empty() {
        state.set_progress(task_id, total_bytes, total_bytes);
        state.set_current_file(task_id, None);
        return Ok(());
    }

    for attempt in 0..=retry_rounds {
        control
            .wait_if_paused(state, task_id, TaskStatus::Downloading)
            .await?;

        let file_count = pending_files.len();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(file_parallelism.max(1)));
        let mut handles = Vec::with_capacity(file_count);

        if attempt == 0 {
            state.set_current_file(
                task_id,
                Some(format!("并行下载 {} 个文件", file_parallelism)),
            );
        } else {
            state.set_current_file(
                task_id,
                Some(format!(
                    "重试失败文件：第 {attempt}/{retry_rounds} 轮 · {file_count} 个文件"
                )),
            );
        }

        let files_this_round = std::mem::take(&mut pending_files);
        for file in files_this_round {
            let retry_file = file.clone();
            let semaphore = semaphore.clone();
            let client = client.clone();
            let folder_id = folder_id.clone();
            let root = root.clone();
            let control = control.clone();
            let state = state.clone();
            let task_id = task_id.to_string();
            let shared_downloaded = shared_downloaded.clone();
            let count_resume_progress = attempt == 0;
            let effective_overwrite = overwrite && attempt == 0;

            let handle = tokio::spawn(async move {
                control
                    .wait_if_paused(&state, &task_id, TaskStatus::Downloading)
                    .await?;
                let _permit = semaphore.acquire_owned().await.map_err(|_| {
                    DownloadError::Message("folder worker semaphore closed".to_string())
                })?;
                control
                    .wait_if_paused(&state, &task_id, TaskStatus::Downloading)
                    .await?;

                state.set_current_file(&task_id, Some(file.relative_path.display().to_string()));
                let output_path = root.join(&file.relative_path);
                let direct_url = fetch_folder_file_url(&client, &folder_id, &file.handle).await?;

                download_file_to_path(
                    &client,
                    &direct_url,
                    &output_path,
                    &file.crypto,
                    file.size,
                    chunk_size,
                    file_connections,
                    effective_overwrite,
                    verify_integrity,
                    low_cpu_mode,
                    control.clone(),
                    &state,
                    &task_id,
                    0,
                    total_bytes,
                    Some(shared_downloaded),
                    false,
                    count_resume_progress,
                    file.manifest_id,
                )
                .await
            });
            handles.push((retry_file, handle));
        }

        let mut failures = Vec::new();
        let mut cancelled = false;
        for (file, handle) in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(DownloadError::Cancelled)) => {
                    control.cancel();
                    cancelled = true;
                }
                Ok(Err(err)) => failures.push(FolderFileFailure {
                    file,
                    error: err.to_string(),
                }),
                Err(err) => failures.push(FolderFileFailure {
                    file,
                    error: format!("folder worker failed: {err}"),
                }),
            }
        }

        if cancelled {
            return Err(DownloadError::Cancelled);
        }

        if failures.is_empty() {
            state.set_progress(task_id, total_bytes, total_bytes);
            state.set_current_file(task_id, None);
            return Ok(());
        }

        if attempt == retry_rounds {
            state.set_current_file(
                task_id,
                Some(if matches!(retry_mode, RetryMode::Manual) {
                    format!("{} 个文件失败，等待手动重试", failures.len())
                } else {
                    format!("{} 个文件失败，已完成其余文件", failures.len())
                }),
            );
            return Err(folder_failures_error(&failures, retry_rounds, retry_mode));
        }

        state.set_current_file(
            task_id,
            Some(format!(
                "{} 个文件失败，等待第 {} 轮重试",
                failures.len(),
                attempt + 1
            )),
        );
        pending_files = failures.into_iter().map(|failure| failure.file).collect();
    }

    Ok(())
}

fn folder_failures_error(
    failures: &[FolderFileFailure],
    retry_rounds: usize,
    retry_mode: RetryMode,
) -> DownloadError {
    let first = failures
        .first()
        .map(|failure| {
            format!(
                "{}: {}",
                failure.file.relative_path.display(),
                failure.error
            )
        })
        .unwrap_or_else(|| "unknown folder failure".to_string());

    let message = match retry_mode {
        RetryMode::Auto => format!(
            "{} 个文件在 {} 轮重试后仍失败；首个错误：{}",
            failures.len(),
            retry_rounds,
            first
        ),
        RetryMode::Manual => format!(
            "手动重试模式：{} 个文件失败；首个错误：{}",
            failures.len(),
            first
        ),
    };
    DownloadError::Message(message)
}

async fn download_file_to_path(
    client: &reqwest::Client,
    direct_url: &str,
    output_path: &Path,
    crypto: &FileCrypto,
    total_size: u64,
    chunk_size: u64,
    connections: usize,
    overwrite: bool,
    verify_integrity: bool,
    low_cpu_mode: bool,
    control: Arc<TaskControl>,
    state: &AppState,
    task_id: &str,
    global_base: u64,
    global_total: u64,
    shared_downloaded: Option<Arc<AtomicU64>>,
    show_verify_status: bool,
    count_resume_progress: bool,
    manifest_id: String,
) -> Result<(), DownloadError> {
    control
        .wait_if_paused(state, task_id, TaskStatus::Downloading)
        .await?;

    if is_existing_complete(output_path, total_size).await? && !overwrite {
        if let Some(shared_downloaded) = &shared_downloaded {
            if !count_resume_progress {
                return Ok(());
            }
            let new_downloaded =
                shared_downloaded.fetch_add(total_size, Ordering::Relaxed) + total_size;
            state.set_progress_baseline(task_id, new_downloaded, global_total);
        } else {
            state.set_progress_baseline(task_id, global_base + total_size, global_total);
        }
        return Ok(());
    }

    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let part_path = part_path_for(output_path);
    let manifest_path = manifest_path_for(output_path);
    state.register_artifacts(task_id, [part_path.clone(), manifest_path.clone()]);

    if overwrite {
        let _ = tokio::fs::remove_file(&part_path).await;
        let _ = tokio::fs::remove_file(&manifest_path).await;
    }

    let can_resume = !overwrite && is_existing_complete(&part_path, total_size).await?;
    let manifest = if can_resume {
        load_resume_manifest(&manifest_path)
            .await?
            .filter(|manifest| {
                manifest.manifest_id == manifest_id
                    && manifest.total_bytes == total_size
                    && manifest.chunk_size == chunk_size
            })
    } else {
        None
    }
    .unwrap_or_else(|| ResumeManifest {
        manifest_id,
        file_name: output_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("download")
            .to_string(),
        total_bytes: total_size,
        chunk_size,
        completed: BTreeSet::new(),
    });

    let part_file = StdOpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&part_path)?;
    part_file.set_len(total_size)?;
    let output_file = Arc::new(part_file);

    save_resume_manifest(&manifest_path, &manifest).await?;

    let already_completed = completed_bytes(&manifest.completed, total_size, chunk_size);
    if let Some(shared_downloaded) = &shared_downloaded {
        if already_completed > 0 {
            if count_resume_progress {
                let new_downloaded =
                    shared_downloaded.fetch_add(already_completed, Ordering::Relaxed)
                        + already_completed;
                state.set_progress_baseline(task_id, new_downloaded, global_total);
            }
        }
    } else {
        state.set_progress_baseline(task_id, global_base + already_completed, global_total);
    }

    let queue = Arc::new(tokio::sync::Mutex::new(build_chunk_queue(
        total_size,
        chunk_size,
        &manifest.completed,
    )));
    let manifest = Arc::new(tokio::sync::Mutex::new(ManifestState::new(manifest)));
    let downloaded = Arc::new(AtomicU64::new(already_completed));
    let stop_workers = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();

    for _ in 0..connections {
        let queue = queue.clone();
        let client = client.clone();
        let direct_url = direct_url.to_string();
        let output = output_file.clone();
        let crypto = crypto.clone();
        let control = control.clone();
        let manifest = manifest.clone();
        let manifest_path = manifest_path.clone();
        let downloaded = downloaded.clone();
        let shared_downloaded = shared_downloaded.clone();
        let stop_workers = stop_workers.clone();
        let state = state.clone();
        let task_id = task_id.to_string();

        handles.push(tokio::spawn(async move {
            loop {
                control
                    .wait_if_paused(&state, &task_id, TaskStatus::Downloading)
                    .await?;
                if stop_workers.load(Ordering::Relaxed) {
                    break;
                }
                let chunk = {
                    let mut queue = queue.lock().await;
                    queue.pop_front()
                };
                let Some(chunk) = chunk else {
                    break;
                };

                if let Err(err) = download_chunk_with_retries(
                    &client,
                    &direct_url,
                    &output,
                    &crypto,
                    total_size,
                    &chunk,
                    &control,
                    &state,
                    &task_id,
                )
                .await
                {
                    stop_workers.store(true, Ordering::Relaxed);
                    return Err(err);
                }

                if let Some(save) = {
                    let mut manifest = manifest.lock().await;
                    manifest.mark_completed(chunk.start)
                } {
                    save_resume_manifest(&manifest_path, &save).await?;
                }

                let new_downloaded =
                    downloaded.fetch_add(chunk.len(), Ordering::Relaxed) + chunk.len();
                if let Some(shared_downloaded) = &shared_downloaded {
                    let new_global_downloaded =
                        shared_downloaded.fetch_add(chunk.len(), Ordering::Relaxed) + chunk.len();
                    state.set_progress(&task_id, new_global_downloaded, global_total);
                } else {
                    state.set_progress(&task_id, global_base + new_downloaded, global_total);
                }
                if low_cpu_mode {
                    tokio::time::sleep(Duration::from_millis(LOW_CPU_CHUNK_COOLDOWN_MS)).await;
                }
            }

            Ok::<(), DownloadError>(())
        }));
    }

    let mut first_error = None;
    for handle in handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(DownloadError::Cancelled)) => {
                if first_error.is_none() {
                    first_error = Some(DownloadError::Cancelled);
                }
            }
            Ok(Err(err)) => {
                stop_workers.store(true, Ordering::Relaxed);
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
            Err(err) => {
                stop_workers.store(true, Ordering::Relaxed);
                if first_error.is_none() {
                    first_error = Some(DownloadError::Message(format!("worker failed: {err}")));
                }
            }
        }
    }

    let manifest_snapshot = {
        let manifest = manifest.lock().await;
        manifest.snapshot()
    };
    let _ = save_resume_manifest(&manifest_path, &manifest_snapshot).await;

    if let Some(err) = first_error {
        return Err(err);
    }
    control
        .wait_if_paused(state, task_id, TaskStatus::Downloading)
        .await?;

    if verify_integrity {
        if show_verify_status {
            state.set_status(task_id, TaskStatus::Verifying);
        }
        control
            .wait_if_paused(state, task_id, TaskStatus::Verifying)
            .await?;
        verify_mac(&part_path, total_size, crypto, &control, state, task_id).await?;
    }

    if output_path.exists() {
        tokio::fs::remove_file(output_path).await?;
    }
    tokio::fs::rename(&part_path, output_path).await?;
    let _ = tokio::fs::remove_file(&manifest_path).await;
    if shared_downloaded.is_none() {
        state.set_progress(task_id, global_base + total_size, global_total);
    }
    Ok(())
}

async fn download_chunk_with_retries(
    client: &reqwest::Client,
    direct_url: &str,
    output_file: &Arc<StdFile>,
    crypto: &FileCrypto,
    total_size: u64,
    chunk: &Chunk,
    control: &Arc<TaskControl>,
    state: &AppState,
    task_id: &str,
) -> Result<(), DownloadError> {
    let mut last_error = None;
    for attempt in 0..4 {
        control
            .wait_if_paused(state, task_id, TaskStatus::Downloading)
            .await?;
        match download_chunk(
            client,
            direct_url,
            output_file.clone(),
            crypto,
            total_size,
            chunk,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_error = Some(err);
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_millis(350 * (attempt + 1) as u64)).await;
                    control
                        .wait_if_paused(state, task_id, TaskStatus::Downloading)
                        .await?;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| DownloadError::Message("chunk retry failed".to_string())))
}

async fn download_chunk(
    client: &reqwest::Client,
    direct_url: &str,
    output_file: Arc<StdFile>,
    crypto: &FileCrypto,
    total_size: u64,
    chunk: &Chunk,
) -> Result<(), DownloadError> {
    let response = client
        .get(direct_url)
        .header(RANGE, format!("bytes={}-{}", chunk.start, chunk.end))
        .send()
        .await?;

    let status = response.status();
    let is_full_file = chunk.start == 0 && chunk.end + 1 == total_size;
    if (!is_full_file && status != StatusCode::PARTIAL_CONTENT)
        || (is_full_file && !(status.is_success() || status == StatusCode::PARTIAL_CONTENT))
    {
        return Err(DownloadError::HttpStatus(status.as_u16()));
    }

    let mut data = response.bytes().await?.to_vec();
    if data.len() as u64 != chunk.len() {
        return Err(DownloadError::ChunkLength {
            start: chunk.start,
            expected: chunk.len(),
            actual: data.len() as u64,
        });
    }

    decrypt_range(&mut data, crypto, chunk.start);
    write_all_at(output_file, data, chunk.start).await?;
    Ok(())
}

async fn write_all_at(file: Arc<StdFile>, data: Vec<u8>, offset: u64) -> Result<(), DownloadError> {
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let mut written = 0;
        while written < data.len() {
            let bytes = write_at(file.as_ref(), &data[written..], offset + written as u64)?;
            if bytes == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write decrypted chunk",
                ));
            }
            written += bytes;
        }
        Ok(())
    })
    .await
    .map_err(|err| DownloadError::Message(format!("write worker failed: {err}")))?
    .map_err(DownloadError::Io)
}

#[cfg(unix)]
fn write_at(file: &StdFile, data: &[u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(data, offset)
}

#[cfg(windows)]
fn write_at(file: &StdFile, data: &[u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(data, offset)
}

#[cfg(not(any(unix, windows)))]
fn write_at(file: &StdFile, data: &[u8], offset: u64) -> std::io::Result<usize> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(offset))?;
    file.write(data)
}

fn parse_public_link(raw: &str, password: Option<&str>) -> Result<PublicLink, DownloadError> {
    let trimmed = raw.trim();
    let parsed = Url::parse(trimmed)?;

    if let Some(mut segments) = parsed.path_segments() {
        let first = segments.next();
        let second = segments.next();
        if first == Some("file") {
            let file_id = second
                .filter(|value| !value.is_empty())
                .ok_or_else(|| DownloadError::Message("missing MEGA file id".to_string()))?;
            let key = parsed
                .fragment()
                .and_then(fragment_key)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| DownloadError::Message("missing MEGA file key".to_string()))?;
            return Ok(PublicLink::File(MegaFileLink {
                file_id: file_id.to_string(),
                key: key.to_string(),
            }));
        }
        if first == Some("folder") {
            let folder_id = second
                .filter(|value| !value.is_empty())
                .ok_or_else(|| DownloadError::Message("missing MEGA folder id".to_string()))?;
            let fragment = parsed.fragment();
            let key = parsed
                .fragment()
                .and_then(fragment_key)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| DownloadError::Message("missing MEGA folder key".to_string()))?;
            return Ok(PublicLink::Folder(MegaFolderLink {
                folder_id: folder_id.to_string(),
                key: key.to_string(),
                selected_handle: fragment.and_then(fragment_selected_handle),
            }));
        }
    }

    let fragment = parsed
        .fragment()
        .ok_or_else(|| DownloadError::Message("missing MEGA link fragment".to_string()))?;
    if fragment.starts_with("P!") {
        return decrypt_password_protected_link(fragment, password);
    }
    if fragment.starts_with("F!") {
        let parts: Vec<_> = fragment.split('!').collect();
        if parts.len() >= 3 {
            return Ok(PublicLink::Folder(MegaFolderLink {
                folder_id: parts[1].to_string(),
                key: parts[2].to_string(),
                selected_handle: parts.get(3).and_then(|value| selected_handle_from_path(value)),
            }));
        }
    }
    if fragment.starts_with('!') {
        let parts: Vec<_> = fragment.split('!').collect();
        if parts.len() >= 3 {
            return Ok(PublicLink::File(MegaFileLink {
                file_id: parts[1].to_string(),
                key: parts[2].to_string(),
            }));
        }
    }

    Err(DownloadError::Message(
        "unsupported MEGA link format".to_string(),
    ))
}

fn fragment_key(fragment: &str) -> Option<&str> {
    fragment.split('/').next()
}

fn fragment_selected_handle(fragment: &str) -> Option<String> {
    let mut parts = fragment.split('/');
    let _key = parts.next()?;
    selected_handle_from_parts(parts)
}

fn selected_handle_from_path(path: &str) -> Option<String> {
    selected_handle_from_parts(path.split('/'))
}

fn selected_handle_from_parts<'a>(mut parts: impl Iterator<Item = &'a str>) -> Option<String> {
    while let Some(kind) = parts.next() {
        let handle = parts.next()?;
        if (kind == "file" || kind == "folder") && !handle.is_empty() {
            return Some(handle.to_string());
        }
    }
    None
}

fn non_empty_password(password: Option<&str>) -> Option<&str> {
    password.filter(|value| !value.is_empty())
}

fn decrypt_password_protected_link(
    fragment: &str,
    password: Option<&str>,
) -> Result<PublicLink, DownloadError> {
    let payload = fragment
        .strip_prefix("P!")
        .ok_or_else(|| DownloadError::Message("invalid password-protected link".to_string()))?;
    let password = password.ok_or(DownloadError::PasswordRequired)?;

    let decoded = decode_base64_url(payload)?;
    if decoded.len() < 88 {
        return Err(DownloadError::Message(
            "password-protected MEGA link is too short".to_string(),
        ));
    }
    if decoded[0] != MEGA_PASSWORD_LINK_ALGORITHM {
        return Err(DownloadError::Message(
            "unsupported password-protected MEGA link algorithm".to_string(),
        ));
    }

    let key_len = match decoded[1] {
        0 => 16,
        1 => 32,
        _ => {
            return Err(DownloadError::Message(
                "password-protected MEGA link has an unknown type".to_string(),
            ))
        }
    };
    let mac_start = 1 + 1 + 6 + 32 + key_len;
    let expected_len = mac_start + 32;
    if decoded.len() != expected_len {
        return Err(DownloadError::Message(
            "password-protected MEGA link has an invalid length".to_string(),
        ));
    }

    let derived_key = pbkdf2_hmac_sha512(
        password.as_bytes(),
        &decoded[8..40],
        MEGA_PASSWORD_LINK_ITERATIONS,
        64,
    );
    let actual_mac = hmac_sha256(&derived_key[32..64], &decoded[..mac_start]);
    if !constant_time_eq(&actual_mac, &decoded[mac_start..]) {
        return Err(DownloadError::Message(
            "password-protected MEGA link password is invalid".to_string(),
        ));
    }

    let encrypted_key = &decoded[40..mac_start];
    let mut key = Vec::with_capacity(key_len);
    for index in 0..key_len {
        key.push(encrypted_key[index] ^ derived_key[index]);
    }

    let handle = encode_base64_url(&decoded[2..8]);
    let key = encode_base64_url(&key);
    if decoded[1] == 0 {
        Ok(PublicLink::Folder(MegaFolderLink {
            folder_id: handle,
            key,
            selected_handle: None,
        }))
    } else {
        Ok(PublicLink::File(MegaFileLink {
            file_id: handle,
            key,
        }))
    }
}

async fn fetch_file_metadata(
    client: &reqwest::Client,
    link: &MegaFileLink,
) -> Result<FileMetadata, DownloadError> {
    let key_words = decode_key_words(&link.key)?;
    if key_words.len() != 8 {
        return Err(DownloadError::Message(
            "MEGA file key must decode to 256 bits".to_string(),
        ));
    }
    let crypto = derive_file_crypto(&key_words)?;
    let payload = json!([{ "a": "g", "g": 1, "p": link.file_id }]);
    let response = mega_api_request(client, payload, &[]).await?;
    let direct_url = response
        .get("g")
        .and_then(|value| value.as_str())
        .ok_or_else(|| DownloadError::Message("MEGA did not return a download URL".to_string()))?
        .to_string();
    let size = response
        .get("s")
        .and_then(|value| value.as_u64())
        .ok_or_else(|| DownloadError::Message("MEGA did not return file size".to_string()))?;
    let attrs = response
        .get("at")
        .and_then(|value| value.as_str())
        .ok_or_else(|| DownloadError::Message("MEGA did not return file attributes".to_string()))?;
    let name = decrypt_node_name(attrs, &crypto.aes_key)?;

    Ok(FileMetadata {
        name: sanitize_component(&name),
        size,
        direct_url,
        crypto,
        manifest_id: format!("file:{}:{}", link.file_id, link.key),
    })
}

async fn fetch_folder_plan(
    client: &reqwest::Client,
    link: &MegaFolderLink,
) -> Result<FolderPlan, DownloadError> {
    let shared_key_words = decode_key_words(&link.key)?;
    if shared_key_words.len() != 4 {
        return Err(DownloadError::Message(
            "MEGA folder key must decode to 128 bits".to_string(),
        ));
    }
    let shared_key = words_to_16_bytes(&shared_key_words)?;

    let payload = json!([{ "a": "f", "c": 1, "ca": 1, "r": 1 }]);
    let response = mega_api_request(client, payload, &[("n", &link.folder_id)]).await?;
    let raw_nodes: Vec<RawNode> =
        serde_json::from_value(response.get("f").cloned().ok_or_else(|| {
            DownloadError::Message("MEGA folder response is missing nodes".to_string())
        })?)?;

    let mut nodes = BTreeMap::new();
    let mut files = Vec::new();

    for raw in raw_nodes {
        if raw.t != 0 && raw.t != 1 {
            continue;
        }
        let encrypted_key =
            raw.k.as_deref().and_then(extract_node_key).ok_or_else(|| {
                DownloadError::Message("folder node is missing key data".to_string())
            })?;
        let decrypted_key = decrypt_key_words(&decode_key_words(encrypted_key)?, &shared_key)?;
        let attr_key = if raw.t == 0 {
            derive_file_attr_key_words(&decrypted_key)?
        } else {
            first_four_words(&decrypted_key)?
        };
        let attr_key_bytes = words_to_16_bytes(&attr_key)?;
        let attrs = raw.a.as_deref().ok_or_else(|| {
            DownloadError::Message("folder node is missing attributes".to_string())
        })?;
        let name = sanitize_component(&decrypt_node_name(attrs, &attr_key_bytes)?);

        if raw.t == 0 {
            let crypto = derive_file_crypto(&decrypted_key)?;
            let size = raw.s.unwrap_or(0);
            files.push(FolderFilePlan {
                name: name.clone(),
                relative_path: PathBuf::new(),
                size,
                handle: raw.h.clone(),
                crypto,
                manifest_id: format!("folder:{}:{}:{}", link.folder_id, raw.h, link.key),
            });
        }

        nodes.insert(
            raw.h.clone(),
            FolderNode {
                name,
                parent: raw.p,
                kind: raw.t,
            },
        );
    }

    let selected_node = match link.selected_handle.as_deref() {
        Some(handle) => Some(
            nodes
                .get(handle)
                .cloned()
                .ok_or_else(|| DownloadError::Message("selected MEGA folder item was not found".to_string()))?,
        ),
        None => None,
    };
    let path_root_handle = match (link.selected_handle.as_deref(), selected_node.as_ref()) {
        (Some(handle), Some(node)) if node.kind == 1 => handle,
        (_, Some(node)) if node.kind == 0 => node.parent.as_deref().unwrap_or(&link.folder_id),
        _ => &link.folder_id,
    };

    if let Some(handle) = link.selected_handle.as_deref() {
        match selected_node.as_ref().map(|node| node.kind) {
            Some(0) => files.retain(|file| file.handle == handle),
            Some(1) if handle != link.folder_id => {
                files.retain(|file| is_descendant_of(&file.handle, handle, &nodes));
            }
            Some(1) => {}
            _ => {
                return Err(DownloadError::Message(
                    "selected MEGA folder item is not downloadable".to_string(),
                ))
            }
        }
    }

    for file in &mut files {
        file.relative_path = build_relative_path(&file.handle, &file.name, &nodes, path_root_handle);
    }

    if files.len() > MAX_FOLDER_FILES_PER_TASK {
        return Err(DownloadError::Message(format!(
            "folder contains {} files, exceeding the current {} file limit",
            files.len(),
            MAX_FOLDER_FILES_PER_TASK
        )));
    }

    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let total_bytes = files.iter().map(|file| file.size).sum();
    let name = selected_node
        .as_ref()
        .map(|node| node.name.clone())
        .or_else(|| {
            nodes
                .get(&link.folder_id)
                .filter(|node| node.kind == 1)
                .map(|node| node.name.clone())
        })
        .unwrap_or_else(|| "MEGA Folder".to_string());
    let create_root_dir = !matches!(selected_node.as_ref().map(|node| node.kind), Some(0));

    Ok(FolderPlan {
        name,
        total_bytes,
        files,
        folder_id: link.folder_id.clone(),
        create_root_dir,
    })
}

async fn fetch_folder_file_url(
    client: &reqwest::Client,
    folder_id: &str,
    handle: &str,
) -> Result<String, DownloadError> {
    let payload = json!([{ "a": "g", "g": 1, "n": handle }]);
    let response = mega_api_request(client, payload, &[("n", folder_id)]).await?;
    response
        .get("g")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| DownloadError::Message("MEGA did not return a folder file URL".to_string()))
}

async fn mega_api_request(
    client: &reqwest::Client,
    payload: serde_json::Value,
    extra_query: &[(&str, &str)],
) -> Result<serde_json::Value, DownloadError> {
    let mut query = vec![("id", "0")];
    query.extend_from_slice(extra_query);

    let response = client
        .post(MEGA_API)
        .query(&query)
        .json(&payload)
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;

    let first = response
        .as_array()
        .and_then(|items| items.first())
        .cloned()
        .ok_or_else(|| DownloadError::Message("empty MEGA API response".to_string()))?;

    if let Some(code) = first.as_i64() {
        if code == 0 {
            return Ok(json!({}));
        }
        return Err(DownloadError::MegaApi(code));
    }

    Ok(first)
}

fn extract_node_key(value: &str) -> Option<&str> {
    value
        .split('/')
        .filter_map(|part| part.split_once(':').map(|(_, key)| key))
        .last()
        .or_else(|| (!value.is_empty()).then_some(value))
}

fn is_descendant_of(
    handle: &str,
    ancestor_handle: &str,
    nodes: &BTreeMap<String, FolderNode>,
) -> bool {
    let mut current_parent = nodes.get(handle).and_then(|node| node.parent.as_deref());
    while let Some(parent) = current_parent {
        if parent == ancestor_handle {
            return true;
        }
        current_parent = nodes.get(parent).and_then(|node| node.parent.as_deref());
    }
    false
}

fn build_relative_path(
    handle: &str,
    file_name: &str,
    nodes: &BTreeMap<String, FolderNode>,
    root_handle: &str,
) -> PathBuf {
    let mut parts = vec![sanitize_component(file_name)];
    let mut current_parent = nodes.get(handle).and_then(|node| node.parent.clone());

    while let Some(parent) = current_parent {
        if parent == root_handle {
            break;
        }
        let Some(parent_node) = nodes.get(&parent) else {
            break;
        };
        if parent_node.kind == 1 {
            parts.push(sanitize_component(&parent_node.name));
        }
        current_parent = parent_node.parent.clone();
    }

    parts.reverse();
    parts.into_iter().collect()
}

fn decode_key_words(value: &str) -> Result<Vec<u32>, DownloadError> {
    let bytes = decode_base64_url(value)?;
    if bytes.len() % 4 != 0 {
        return Err(DownloadError::Message(
            "decoded MEGA key is not word-aligned".to_string(),
        ));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

fn decode_base64_url(value: &str) -> Result<Vec<u8>, DownloadError> {
    let cleaned = value.trim().replace(',', "");
    general_purpose::URL_SAFE_NO_PAD
        .decode(cleaned.as_bytes())
        .or_else(|_| general_purpose::URL_SAFE.decode(cleaned.as_bytes()))
        .map_err(DownloadError::from)
}

fn encode_base64_url(value: &[u8]) -> String {
    general_purpose::URL_SAFE_NO_PAD.encode(value)
}

fn pbkdf2_hmac_sha512(password: &[u8], salt: &[u8], iterations: u32, output_len: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(output_len);
    let mut block_index = 1_u32;

    while output.len() < output_len {
        let mut input = Vec::with_capacity(salt.len() + 4);
        input.extend_from_slice(salt);
        input.extend_from_slice(&block_index.to_be_bytes());

        let mut u = hmac_sha512(password, &input);
        let mut block = u;
        for _ in 1..iterations {
            u = hmac_sha512(password, &u);
            for index in 0..block.len() {
                block[index] ^= u[index];
            }
        }

        output.extend_from_slice(&block);
        block_index += 1;
    }

    output.truncate(output_len);
    output
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut key_block = [0_u8; 64];
    if key.len() > key_block.len() {
        let digest = Sha256::digest(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36_u8; 64];
    let mut outer_pad = [0x5c_u8; 64];
    for index in 0..key_block.len() {
        inner_pad[index] ^= key_block[index];
        outer_pad[index] ^= key_block[index];
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(data);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    let result = outer.finalize();

    let mut output = [0_u8; 32];
    output.copy_from_slice(&result);
    output
}

fn hmac_sha512(key: &[u8], data: &[u8]) -> [u8; 64] {
    let mut key_block = [0_u8; 128];
    if key.len() > key_block.len() {
        let digest = Sha512::digest(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36_u8; 128];
    let mut outer_pad = [0x5c_u8; 128];
    for index in 0..key_block.len() {
        inner_pad[index] ^= key_block[index];
        outer_pad[index] ^= key_block[index];
    }

    let mut inner = Sha512::new();
    inner.update(inner_pad);
    inner.update(data);
    let inner = inner.finalize();

    let mut outer = Sha512::new();
    outer.update(outer_pad);
    outer.update(inner);
    let result = outer.finalize();

    let mut output = [0_u8; 64];
    output.copy_from_slice(&result);
    output
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }

    let mut diff = 0_u8;
    for index in 0..left.len() {
        diff |= left[index] ^ right[index];
    }
    diff == 0
}

fn derive_file_crypto(words: &[u32]) -> Result<FileCrypto, DownloadError> {
    if words.len() < 8 {
        return Err(DownloadError::Message(
            "file key is shorter than expected".to_string(),
        ));
    }
    let attr_words = derive_file_attr_key_words(words)?;
    Ok(FileCrypto {
        aes_key: words_to_16_bytes(&attr_words)?,
        iv_words: [words[4], words[5]],
        meta_mac: [words[6], words[7]],
    })
}

fn derive_file_attr_key_words(words: &[u32]) -> Result<Vec<u32>, DownloadError> {
    if words.len() < 8 {
        return Err(DownloadError::Message(
            "file key is shorter than expected".to_string(),
        ));
    }
    Ok(vec![
        words[0] ^ words[4],
        words[1] ^ words[5],
        words[2] ^ words[6],
        words[3] ^ words[7],
    ])
}

fn first_four_words(words: &[u32]) -> Result<Vec<u32>, DownloadError> {
    if words.len() < 4 {
        return Err(DownloadError::Message(
            "folder key is shorter than expected".to_string(),
        ));
    }
    Ok(words[..4].to_vec())
}

fn decrypt_key_words(words: &[u32], key: &[u8; 16]) -> Result<Vec<u32>, DownloadError> {
    if words.len() % 4 != 0 {
        return Err(DownloadError::Message(
            "encrypted key is not AES block-aligned".to_string(),
        ));
    }

    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut output = Vec::with_capacity(words.len());
    for block_words in words.chunks_exact(4) {
        let mut bytes = [0_u8; 16];
        for (index, word) in block_words.iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        let mut block = GenericArray::clone_from_slice(&bytes);
        cipher.decrypt_block(&mut block);
        for chunk in block.chunks_exact(4) {
            output.push(u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
    }
    Ok(output)
}

fn words_to_16_bytes(words: &[u32]) -> Result<[u8; 16], DownloadError> {
    if words.len() != 4 {
        return Err(DownloadError::Message(
            "expected exactly four 32-bit words".to_string(),
        ));
    }
    let mut bytes = [0_u8; 16];
    for (index, word) in words.iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    Ok(bytes)
}

fn decrypt_node_name(value: &str, key: &[u8; 16]) -> Result<String, DownloadError> {
    let encrypted = decode_base64_url(value)?;
    if encrypted.len() % 16 != 0 {
        return Err(DownloadError::Message(
            "encrypted attributes are not AES block-aligned".to_string(),
        ));
    }
    let mut decrypted = aes_cbc_decrypt_zero_iv(&encrypted, key)?;
    while decrypted.last() == Some(&0) {
        decrypted.pop();
    }
    if !decrypted.starts_with(b"MEGA") {
        return Err(DownloadError::Message(
            "MEGA attributes failed to decrypt".to_string(),
        ));
    }
    let attrs: serde_json::Value = serde_json::from_slice(&decrypted[4..])?;
    attrs
        .get("n")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .ok_or_else(|| DownloadError::Message("MEGA attributes do not contain a name".to_string()))
}

fn aes_cbc_decrypt_zero_iv(data: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, DownloadError> {
    if data.len() % 16 != 0 {
        return Err(DownloadError::Message(
            "AES-CBC input is not block-aligned".to_string(),
        ));
    }
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut previous = [0_u8; 16];
    let mut output = Vec::with_capacity(data.len());

    for encrypted_block in data.chunks_exact(16) {
        let mut block = GenericArray::clone_from_slice(encrypted_block);
        cipher.decrypt_block(&mut block);
        for index in 0..16 {
            block[index] ^= previous[index];
        }
        output.extend_from_slice(&block);
        previous.copy_from_slice(encrypted_block);
    }

    Ok(output)
}

fn decrypt_range(data: &mut [u8], crypto: &FileCrypto, offset: u64) {
    let block_index = offset / 16;
    let mut counter = [0_u8; 16];
    counter[0..4].copy_from_slice(&crypto.iv_words[0].to_be_bytes());
    counter[4..8].copy_from_slice(&crypto.iv_words[1].to_be_bytes());
    counter[8..16].copy_from_slice(&block_index.to_be_bytes());
    let mut cipher = Aes128Ctr::new(
        GenericArray::from_slice(&crypto.aes_key),
        GenericArray::from_slice(&counter),
    );
    cipher.apply_keystream(data);
}

async fn verify_mac(
    path: &Path,
    size: u64,
    crypto: &FileCrypto,
    control: &Arc<TaskControl>,
    state: &AppState,
    task_id: &str,
) -> Result<(), DownloadError> {
    if size == 0 {
        return Ok(());
    }

    let mut file = tokio::fs::File::open(path).await?;
    let mut global_mac = [0_u8; 16];
    let chunk_iv = mac_iv(crypto);

    for (_, chunk_size) in mega_mac_chunks(size) {
        control
            .wait_if_paused(state, task_id, TaskStatus::Verifying)
            .await?;
        let mut buffer = vec![0_u8; chunk_size as usize];
        file.read_exact(&mut buffer).await?;
        let chunk_mac = cbc_mac(&buffer, &crypto.aes_key, &chunk_iv);
        global_mac = cbc_mac_block(&chunk_mac, &crypto.aes_key, &global_mac);
    }

    let words: Vec<u32> = global_mac
        .chunks_exact(4)
        .map(|chunk| u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    let actual = [words[0] ^ words[1], words[2] ^ words[3]];
    if actual != crypto.meta_mac {
        return Err(DownloadError::MacMismatch);
    }
    Ok(())
}

fn cbc_mac(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut previous = *iv;

    for chunk in data.chunks(16) {
        let mut block = [0_u8; 16];
        block[..chunk.len()].copy_from_slice(chunk);
        for index in 0..16 {
            block[index] ^= previous[index];
        }
        let mut block = GenericArray::clone_from_slice(&block);
        cipher.encrypt_block(&mut block);
        previous.copy_from_slice(&block);
    }

    previous
}

fn cbc_mac_block(block: &[u8; 16], key: &[u8; 16], previous: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut next = *block;
    for index in 0..16 {
        next[index] ^= previous[index];
    }
    let mut next = GenericArray::clone_from_slice(&next);
    cipher.encrypt_block(&mut next);
    let mut output = [0_u8; 16];
    output.copy_from_slice(&next);
    output
}

fn mac_iv(crypto: &FileCrypto) -> [u8; 16] {
    let mut iv = [0_u8; 16];
    iv[0..4].copy_from_slice(&crypto.iv_words[0].to_be_bytes());
    iv[4..8].copy_from_slice(&crypto.iv_words[1].to_be_bytes());
    iv[8..12].copy_from_slice(&crypto.iv_words[0].to_be_bytes());
    iv[12..16].copy_from_slice(&crypto.iv_words[1].to_be_bytes());
    iv
}

fn mega_mac_chunks(size: u64) -> Vec<(u64, u64)> {
    let mut chunks = Vec::new();
    let mut position = 0;
    let mut chunk_size = 0x20000;

    while position + chunk_size < size {
        chunks.push((position, chunk_size));
        position += chunk_size;
        if chunk_size < 0x100000 {
            chunk_size += 0x20000;
        }
    }

    if position < size {
        chunks.push((position, size - position));
    }

    chunks
}

fn normalize_chunk_size(chunk_mb: u64) -> u64 {
    let bytes = (chunk_mb.max(1) * 1024 * 1024).clamp(MIN_CHUNK_BYTES, MAX_CHUNK_BYTES);
    bytes - (bytes % 16)
}

fn resolve_performance_mode(request: &DownloadRequest) -> PerformanceMode {
    request.performance_mode.unwrap_or_else(|| {
        if request.low_cpu_mode.unwrap_or(false) {
            PerformanceMode::LowImpact
        } else {
            PerformanceMode::Balanced
        }
    })
}

fn effective_connections(requested: usize, mode: PerformanceMode) -> usize {
    match mode {
        PerformanceMode::LowImpact => requested.min(LOW_CPU_MAX_CONNECTIONS),
        PerformanceMode::Balanced => requested.min(BALANCED_MAX_CONNECTIONS),
        PerformanceMode::Fast => requested.min(MAX_CONNECTIONS),
    }
    .max(1)
}

fn effective_chunk_mb(requested: u64, mode: PerformanceMode) -> u64 {
    match mode {
        PerformanceMode::LowImpact => requested.min(LOW_CPU_MAX_CHUNK_MB),
        PerformanceMode::Balanced => requested.min(BALANCED_MAX_CHUNK_MB),
        PerformanceMode::Fast => requested,
    }
    .max(1)
}

fn folder_file_parallelism(connections: usize, low_cpu_mode: bool, file_count: usize) -> usize {
    if file_count == 0 {
        return 1;
    }

    let limit = if low_cpu_mode {
        LOW_CPU_MAX_PARALLEL_FOLDER_FILES
    } else {
        MAX_PARALLEL_FOLDER_FILES
    };

    connections.max(1).min(limit).min(file_count)
}

fn folder_file_connections(connections: usize, file_parallelism: usize) -> usize {
    (connections.max(1) / file_parallelism.max(1)).max(1)
}

fn build_chunk_queue(
    total_size: u64,
    chunk_size: u64,
    completed: &BTreeSet<u64>,
) -> VecDeque<Chunk> {
    let mut queue = VecDeque::new();
    let mut start = 0;
    while start < total_size {
        let end_exclusive = (start + chunk_size).min(total_size);
        if !completed.contains(&start) {
            queue.push_back(Chunk {
                start,
                end: end_exclusive - 1,
            });
        }
        start = end_exclusive;
    }
    queue
}

fn completed_bytes(completed: &BTreeSet<u64>, total_size: u64, chunk_size: u64) -> u64 {
    completed
        .iter()
        .filter(|start| **start < total_size)
        .map(|start| (total_size - start).min(chunk_size))
        .sum()
}

async fn load_resume_manifest(path: &Path) -> Result<Option<ResumeManifest>, DownloadError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = tokio::fs::read(path).await?;
    Ok(serde_json::from_slice(&bytes).ok())
}

async fn save_resume_manifest(path: &Path, manifest: &ResumeManifest) -> Result<(), DownloadError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec(manifest)?;
    tokio::fs::write(path, bytes).await?;
    Ok(())
}

fn prepare_file_output_path(
    output_dir: &Path,
    file_name: &str,
    overwrite: bool,
) -> Result<PathBuf, DownloadError> {
    let file_name = sanitize_component(file_name);
    let output = output_dir.join(file_name);
    if overwrite || !output.exists() {
        return Ok(output);
    }
    unique_path(&output)
}

fn unique_path(path: &Path) -> Result<PathBuf, DownloadError> {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    let extension = path.extension().and_then(|value| value.to_str());

    for index in 1..10_000 {
        let candidate_name = match extension {
            Some(extension) if !extension.is_empty() => format!("{stem} ({index}).{extension}"),
            _ => format!("{stem} ({index})"),
        };
        let candidate = parent.join(candidate_name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(DownloadError::Message(
        "could not allocate a unique output path".to_string(),
    ))
}

async fn is_existing_complete(path: &Path, size: u64) -> Result<bool, DownloadError> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() == size),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(DownloadError::Io(err)),
    }
}

fn part_path_for(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.megadown.part",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("download")
    ))
}

fn manifest_path_for(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        "{}.megadown.json",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("download")
    ))
}

fn sanitize_component(value: &str) -> String {
    let sanitized = sanitize(value).trim().to_string();
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        "download".to_string()
    } else {
        sanitized
    }
}

fn make_http_client() -> Result<reqwest::Client, DownloadError> {
    reqwest::Client::builder()
        .user_agent("MegaDown/0.1")
        .pool_max_idle_per_host(MAX_CONNECTIONS)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_nodelay(true)
        .tcp_keepalive(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(DownloadError::from)
}

fn default_download_path() -> Result<PathBuf, DownloadError> {
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(profile).join("Downloads"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join("Downloads"));
    }
    Ok(std::env::current_dir()?.join("downloads"))
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_password_protected_folder_link_from_reference() {
        let parsed = parse_public_link(
            "https://mega.nz/#P!AgDJUADUNBAfILW6rGEVD0Po68-q27s0jbEYyvSDQ1fbS9EeUvFv06geiV6jv4hTl1aXNNyE--SVwYKRfvJgU_VFkmiIltBY0Z5JtmvhHJ69uyZOxSIGUA",
            Some("123.admin.30S"),
        )
        .expect("reference folder link should decrypt");

        match parsed {
            PublicLink::Folder(link) => {
                assert_eq!(link.folder_id, "yVAA1DQQ");
                assert_eq!(link.key, "XFo6ejNK7vC1VZxdSS-leQ");
            }
            PublicLink::File(_) => panic!("expected folder link"),
        }
    }

    #[test]
    fn parses_password_protected_file_link_from_reference() {
        let parsed = parse_public_link(
            "https://mega.nz/#P!AgFsUmcdCoPHOobt_DN5op-rhFtI6AF0mxDyzh7OAC_frDSVebejj8xIDciZUIb19Sg-xb-0YkpeqvrKjOKivzyGp1W8Plf3QAmgELeneVg_xmOxpck8diLiM8UnbOuYCHb4JnpyeHQ",
            Some("admin.30S"),
        )
        .expect("reference file link should decrypt");

        match parsed {
            PublicLink::File(link) => {
                assert_eq!(link.file_id, "bFJnHQqD");
                assert_eq!(link.key, "-KaiZY-vdkh6C-0WXXcaqilHsT-aBPNlH6sRpS8ZGpg");
            }
            PublicLink::Folder(_) => panic!("expected file link"),
        }
    }

    #[test]
    fn rejects_wrong_password_for_protected_link() {
        let error = parse_public_link(
            "https://mega.nz/#P!AgDJUADUNBAfILW6rGEVD0Po68-q27s0jbEYyvSDQ1fbS9EeUvFv06geiV6jv4hTl1aXNNyE--SVwYKRfvJgU_VFkmiIltBY0Z5JtmvhHJ69uyZOxSIGUA",
            Some("wrong-password"),
        )
        .expect_err("wrong password should fail");

        assert!(error.to_string().contains("password is invalid"));
    }

    #[test]
    fn detects_password_requirement_for_protected_link() {
        let error = parse_public_link(
            "https://mega.nz/#P!AgDJUADUNBAfILW6rGEVD0Po68-q27s0jbEYyvSDQ1fbS9EeUvFv06geiV6jv4hTl1aXNNyE--SVwYKRfvJgU_VFkmiIltBY0Z5JtmvhHJ69uyZOxSIGUA",
            None,
        )
        .expect_err("protected link without password should request one");

        assert!(matches!(error, DownloadError::PasswordRequired));
    }

    #[test]
    fn parses_folder_fragment_key_before_selected_child() {
        let parsed = parse_public_link(
            "https://mega.nz/folder/abcdefgh#folderkey/file/child1234",
            None,
        )
        .expect("folder link with selected child should parse");

        match parsed {
            PublicLink::Folder(link) => {
                assert_eq!(link.folder_id, "abcdefgh");
                assert_eq!(link.key, "folderkey");
                assert_eq!(link.selected_handle.as_deref(), Some("child1234"));
            }
            PublicLink::File(_) => panic!("expected folder link"),
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            default_download_dir,
            choose_download_dir,
            inspect_link,
            start_download,
            list_tasks,
            cancel_download,
            pause_download,
            resume_download,
            pause_all_downloads,
            resume_all_downloads,
            delete_task,
            clear_finished_tasks,
            open_task_file,
            open_task_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running MegaDown");
}
