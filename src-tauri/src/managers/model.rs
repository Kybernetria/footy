use crate::settings::{get_settings, write_settings};
use anyhow::Result;
use flate2::read::GzDecoder;
use futures_util::StreamExt;
use log::{debug, info, warn};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use specta::Type;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tar::Archive;
use tauri::{AppHandle, Emitter};
use tokio::sync::Notify;

const MODEL_DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MODEL_DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(30);

// These limits are intentionally pinned per bundled model rather than being
// derived from a server-supplied size or a caller-supplied value. They include
// room for the compressed archive while keeping the untrusted download finite.
const PARAKEET_V2_MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
const PARAKEET_V3_MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

fn max_download_bytes_for_model(model_id: &str) -> Result<u64> {
    match model_id {
        "parakeet-tdt-0.6b-v2" => Ok(PARAKEET_V2_MAX_DOWNLOAD_BYTES),
        "parakeet-tdt-0.6b-v3" => Ok(PARAKEET_V3_MAX_DOWNLOAD_BYTES),
        _ => Err(anyhow::anyhow!(
            "No pinned download limit for model {}",
            model_id
        )),
    }
}

fn parse_u64_header(value: Option<&str>, header_name: &str) -> Result<Option<u64>> {
    value
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("Invalid {} header: {:?}", header_name, value))
        })
        .transpose()
}

#[derive(Debug, PartialEq, Eq)]
struct ValidatedDownloadHeaders {
    total_size: Option<u64>,
    expected_body_size: Option<u64>,
}

fn parse_content_range(value: &str) -> Result<(u64, u64, u64)> {
    let value = value
        .strip_prefix("bytes ")
        .ok_or_else(|| anyhow::anyhow!("Invalid Content-Range unit"))?;
    let (range, total) = value
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("Invalid Content-Range format"))?;
    let (start, end) = range
        .split_once('-')
        .ok_or_else(|| anyhow::anyhow!("Invalid Content-Range byte range"))?;
    let start = start.parse::<u64>()?;
    let end = end.parse::<u64>()?;
    let total = total.parse::<u64>()?;

    if start > end || end >= total {
        return Err(anyhow::anyhow!(
            "Invalid Content-Range bounds: {}-{} / {}",
            start,
            end,
            total
        ));
    }

    Ok((start, end, total))
}

fn validate_download_headers(
    status: reqwest::StatusCode,
    content_length: Option<&str>,
    content_range: Option<&str>,
    resume_from: u64,
    max_download_bytes: u64,
) -> Result<ValidatedDownloadHeaders> {
    let content_length = parse_u64_header(content_length, "Content-Length")?;

    if resume_from == 0 {
        if status == reqwest::StatusCode::PARTIAL_CONTENT || content_range.is_some() {
            return Err(anyhow::anyhow!(
                "Unexpected partial response for a fresh model download"
            ));
        }
        if content_length.is_some_and(|length| length > max_download_bytes) {
            return Err(anyhow::anyhow!(
                "Content-Length exceeds the {} byte model download limit",
                max_download_bytes
            ));
        }
        return Ok(ValidatedDownloadHeaders {
            total_size: content_length,
            expected_body_size: content_length,
        });
    }

    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(anyhow::anyhow!(
            "Resume response must be HTTP 206 Partial Content, got {}",
            status
        ));
    }

    let content_range =
        content_range.ok_or_else(|| anyhow::anyhow!("Resume response is missing Content-Range"))?;
    let (range_start, range_end, total_size) = parse_content_range(content_range)?;
    if range_start != resume_from {
        return Err(anyhow::anyhow!(
            "Content-Range starts at {}, expected resume offset {}",
            range_start,
            resume_from
        ));
    }
    if total_size > max_download_bytes {
        return Err(anyhow::anyhow!(
            "Content-Range total exceeds the {} byte model download limit",
            max_download_bytes
        ));
    }
    let expected_body_size = range_end
        .checked_sub(range_start)
        .and_then(|size| size.checked_add(1))
        .ok_or_else(|| anyhow::anyhow!("Content-Range length overflow"))?;
    if content_length.is_some_and(|length| length != expected_body_size) {
        return Err(anyhow::anyhow!(
            "Content-Length does not match Content-Range"
        ));
    }

    Ok(ValidatedDownloadHeaders {
        total_size: Some(total_size),
        expected_body_size: Some(expected_body_size),
    })
}

fn next_download_size(downloaded: u64, chunk_size: usize, max_download_bytes: u64) -> Result<u64> {
    let next_size = downloaded
        .checked_add(chunk_size as u64)
        .ok_or_else(|| anyhow::anyhow!("Model download byte count overflow"))?;
    if next_size > max_download_bytes {
        return Err(anyhow::anyhow!(
            "Model download exceeds the {} byte limit",
            max_download_bytes
        ));
    }
    Ok(next_size)
}

fn partial_file_size(path: &Path) -> Result<Option<u64>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(Some(metadata.len())),
        Ok(metadata) => Err(anyhow::anyhow!(
            "Partial model path is not a regular file: {:?} ({:?})",
            path,
            metadata.file_type()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn discard_partial_file(path: &Path, reason: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(path).map_err(|error| {
                anyhow::anyhow!(
                    "Failed to remove rejected partial model {:?} ({}): {}",
                    path,
                    reason,
                    error
                )
            })?;
            warn!("Removed rejected partial model {:?}: {}", path, reason);
            Ok(())
        }
        Ok(metadata) => Err(anyhow::anyhow!(
            "Refusing to remove rejected partial path {:?} with unexpected type {:?}",
            path,
            metadata.file_type()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn reject_download_with_cleanup(path: &Path, error: anyhow::Error, reason: &str) -> anyhow::Error {
    match discard_partial_file(path, reason) {
        Ok(()) => error,
        Err(cleanup_error) => anyhow::anyhow!(
            "{}; failed to remove rejected partial: {}",
            error,
            cleanup_error
        ),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub enum EngineType {
    Parakeet,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub filename: String,
    pub url: Option<String>,
    pub sha256: Option<String>,
    pub size_mb: u64,
    pub is_downloaded: bool,
    pub is_downloading: bool,
    pub partial_size: u64,
    pub is_directory: bool,
    pub engine_type: EngineType,
    pub accuracy_score: f32,        // 0.0 to 1.0, higher is more accurate
    pub speed_score: f32,           // 0.0 to 1.0, higher is faster
    pub supports_translation: bool, // Whether the model supports translating to English
    pub is_recommended: bool,       // Whether this is the recommended model for new users
    pub supported_languages: Vec<String>, // Languages this model can transcribe
    pub supports_language_selection: bool, // Whether the user can explicitly pick a language
    pub is_custom: bool,            // Whether this is a user-provided custom model
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DownloadProgress {
    pub model_id: String,
    pub downloaded: u64,
    pub total: u64,
    pub percentage: f64,
}

/// Download ownership is claimed before any partial-file access. The cleanup
/// guard releases that claim and clears download state on every error path.
/// A per-model operation claim. Downloads also carry cancellation primitives;
/// destructive operations do not, so cancel_download cannot cancel deletion.
struct DownloadClaim {
    owner_id: u64,
    cancel_flag: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
}

struct ModelOperationClaim {
    owner_id: u64,
}

struct ActiveModelOperation {
    owner_id: u64,
    cancel_flag: Option<Arc<AtomicBool>>,
    cancel_notify: Option<Arc<Notify>>,
}

struct DownloadClaimRegistry {
    claims: Mutex<HashMap<String, ActiveModelOperation>>,
    next_owner_id: AtomicU64,
}

impl DownloadClaimRegistry {
    fn new() -> Self {
        Self {
            claims: Mutex::new(HashMap::new()),
            next_owner_id: AtomicU64::new(1),
        }
    }

    fn claim_download(&self, model_id: &str) -> Result<DownloadClaim> {
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(Notify::new());
        let owner_id = self.claim(
            model_id,
            Some(cancel_flag.clone()),
            Some(cancel_notify.clone()),
        )?;
        Ok(DownloadClaim {
            owner_id,
            cancel_flag,
            cancel_notify,
        })
    }

    fn claim_operation(&self, model_id: &str) -> Result<ModelOperationClaim> {
        let owner_id = self.claim(model_id, None, None)?;
        Ok(ModelOperationClaim { owner_id })
    }

    fn claim(
        &self,
        model_id: &str,
        cancel_flag: Option<Arc<AtomicBool>>,
        cancel_notify: Option<Arc<Notify>>,
    ) -> Result<u64> {
        let mut claims = self.claims.lock().unwrap();
        if claims.contains_key(model_id) {
            return Err(anyhow::anyhow!(
                "A model operation for {} is already in progress",
                model_id
            ));
        }

        let owner_id = self.next_owner_id.fetch_add(1, Ordering::Relaxed);
        claims.insert(
            model_id.to_string(),
            ActiveModelOperation {
                owner_id,
                cancel_flag,
                cancel_notify,
            },
        );
        Ok(owner_id)
    }

    fn cancel(&self, model_id: &str) -> bool {
        let claims = self.claims.lock().unwrap();
        let Some(claim) = claims.get(model_id) else {
            return false;
        };
        let (Some(cancel_flag), Some(cancel_notify)) = (&claim.cancel_flag, &claim.cancel_notify)
        else {
            return false;
        };
        cancel_flag.store(true, Ordering::Release);
        // notify_one retains a permit when cancellation wins the race before
        // the request/stream future begins waiting.
        cancel_notify.notify_one();
        true
    }

    fn release(&self, model_id: &str, owner_id: u64) {
        let mut claims = self.claims.lock().unwrap();
        if claims
            .get(model_id)
            .is_some_and(|claim| claim.owner_id == owner_id)
        {
            claims.remove(model_id);
        }
    }

    /// Linearizes cancellation against the narrow final install operation. A
    /// cancellation that acquires this lock first wins; otherwise the install
    /// completes before cancellation can set its flag.
    fn commit_if_not_cancelled<T>(
        &self,
        model_id: &str,
        owner_id: u64,
        commit: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        let claims = self.claims.lock().unwrap();
        let Some(active) = claims.get(model_id) else {
            return Err(anyhow::anyhow!("Model operation ownership was lost"));
        };
        if active.owner_id != owner_id {
            return Err(anyhow::anyhow!("Model operation ownership was superseded"));
        }
        if active
            .cancel_flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Ok(None);
        }

        commit().map(Some)
    }
}

struct CancellableReader<R> {
    reader: R,
    cancel_flag: Arc<AtomicBool>,
}

impl<R: Read> Read for CancellableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancel_flag.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "model download cancelled",
            ));
        }
        self.reader.read(buffer)
    }
}

fn extract_archive<R: Read>(
    reader: R,
    destination: &Path,
    cancel_flag: Arc<AtomicBool>,
) -> Result<bool> {
    let decoder = GzDecoder::new(reader);
    let cancellable_decoder = CancellableReader {
        reader: decoder,
        cancel_flag: cancel_flag.clone(),
    };
    let mut archive = Archive::new(cancellable_decoder);
    let unpack_result = archive.unpack(destination);

    // Check after unpack as well: cancellation can arrive after the final
    // read, before the archive iterator returns. The caller removes the
    // staging directory when this returns false.
    if cancel_flag.load(Ordering::Acquire) {
        return Ok(false);
    }

    unpack_result.map(|_| true).map_err(Into::into)
}

/// Clears download state and releases only the claim owned by this download.
/// The owner check prevents an older task from cancelling or clearing a newer
/// attempt for the same model.
struct DownloadCleanup<'a> {
    available_models: &'a Mutex<HashMap<String, ModelInfo>>,
    claims: &'a DownloadClaimRegistry,
    model_id: String,
    owner_id: u64,
}

impl<'a> Drop for DownloadCleanup<'a> {
    fn drop(&mut self) {
        {
            let mut models = self.available_models.lock().unwrap();
            if let Some(model) = models.get_mut(self.model_id.as_str()) {
                model.is_downloading = false;
            }
        }
        self.claims.release(&self.model_id, self.owner_id);
    }
}

struct OperationCleanup<'a> {
    claims: &'a DownloadClaimRegistry,
    model_id: String,
    owner_id: u64,
}

impl Drop for OperationCleanup<'_> {
    fn drop(&mut self) {
        self.claims.release(&self.model_id, self.owner_id);
    }
}

struct ExtractionCleanup {
    extracting_models: Arc<Mutex<HashSet<String>>>,
    model_id: String,
}

impl Drop for ExtractionCleanup {
    fn drop(&mut self) {
        let mut extracting = self.extracting_models.lock().unwrap();
        extracting.remove(&self.model_id);
    }
}

struct StagingDirectoryCleanup {
    path: PathBuf,
}

impl Drop for StagingDirectoryCleanup {
    fn drop(&mut self) {
        if self.path.exists() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

type RenameOperation = Arc<dyn Fn(&Path, &Path) -> io::Result<()> + Send + Sync>;

#[derive(Default)]
struct ModelLifecycleLock(Mutex<()>);

impl ModelLifecycleLock {
    fn execute<T>(&self, operation: impl FnOnce() -> T) -> T {
        let _guard = self.0.lock().unwrap_or_else(|poisoned| {
            warn!("Recovering poisoned model lifecycle mutex");
            poisoned.into_inner()
        });
        operation()
    }
}

/// A reversible filesystem transaction for model deletion. Files are moved
/// into a private quarantine first; they are only removed after the caller has
/// completed all other fallible work. Quarantine cleanup is deliberately
/// post-commit and best-effort so a cleanup fault cannot turn a successful
/// logical deletion into a partially applied failed operation.
struct QuarantineTransaction {
    quarantine_dir: PathBuf,
    moved_paths: Vec<(PathBuf, PathBuf)>,
    rename: RenameOperation,
    active: bool,
}

impl QuarantineTransaction {
    fn start(
        paths: Vec<PathBuf>,
        quarantine_dir: PathBuf,
        rename: RenameOperation,
    ) -> Result<Self> {
        fs::create_dir(&quarantine_dir)?;
        let mut transaction = Self {
            quarantine_dir,
            moved_paths: Vec::with_capacity(paths.len()),
            rename,
            active: true,
        };

        for (index, original_path) in paths.into_iter().enumerate() {
            let quarantine_path = transaction.quarantine_dir.join(index.to_string());
            if let Err(error) = (transaction.rename)(&original_path, &quarantine_path) {
                let rollback_result = transaction.rollback();
                return match rollback_result {
                    Ok(()) => Err(anyhow::anyhow!(
                        "Failed to quarantine model file {:?}: {}",
                        original_path,
                        error
                    )),
                    Err(rollback_error) => Err(anyhow::anyhow!(
                        "Failed to quarantine model file {:?}: {}; rollback also failed: {}",
                        original_path,
                        error,
                        rollback_error
                    )),
                };
            }
            transaction
                .moved_paths
                .push((original_path, quarantine_path));
        }

        Ok(transaction)
    }

    fn rollback(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }

        let mut rollback_error = None;
        for (original_path, quarantine_path) in self.moved_paths.iter().rev() {
            if let Err(error) = (self.rename)(quarantine_path, original_path) {
                rollback_error = Some(error);
                break;
            }
        }

        if rollback_error.is_none() {
            if let Err(error) = fs::remove_dir(&self.quarantine_dir) {
                rollback_error = Some(error);
            }
        }

        if rollback_error.is_none() {
            self.active = false;
        }

        rollback_error.map_or(Ok(()), |error| Err(error.into()))
    }

    /// Finalizes the logical delete. A failure here is not returned because
    /// the model state has already committed; the retained quarantine is safe
    /// recovery state and can be retried by a later maintenance pass.
    fn commit(mut self) {
        self.active = false;
        if let Err(error) = fs::remove_dir_all(&self.quarantine_dir) {
            warn!(
                "Model delete committed but quarantine cleanup failed at {:?}: {}",
                self.quarantine_dir, error
            );
        }
    }
}

impl Drop for QuarantineTransaction {
    fn drop(&mut self) {
        if self.active {
            if let Err(error) = self.rollback() {
                warn!(
                    "Model delete quarantine rollback failed at {:?}: {}",
                    self.quarantine_dir, error
                );
            }
        }
    }
}

async fn wait_for_download_cancellation(flag: &AtomicBool, notify: &Notify) {
    if flag.load(Ordering::Acquire) {
        return;
    }
    notify.notified().await;
}

async fn send_download_request(
    request: reqwest::RequestBuilder,
    cancel_flag: &AtomicBool,
    cancel_notify: &Notify,
) -> Result<Option<reqwest::Response>> {
    tokio::select! {
        response = request.send() => Ok(Some(response?)),
        _ = wait_for_download_cancellation(cancel_flag, cancel_notify) => Ok(None),
    }
}

pub struct ModelManager {
    app_handle: AppHandle,
    models_dir: PathBuf,
    available_models: Mutex<HashMap<String, ModelInfo>>,
    model_operations: DownloadClaimRegistry,
    extracting_models: Arc<Mutex<HashSet<String>>>,
    lifecycle_lock: ModelLifecycleLock,
    next_quarantine_id: AtomicU64,
}

impl ModelManager {
    pub fn new(app_handle: &AppHandle) -> Result<Self> {
        // Create models directory in app data
        let models_dir = crate::portable::app_data_dir(app_handle)
            .map_err(|e| anyhow::anyhow!("Failed to get app data dir: {}", e))?
            .join("models");

        if !models_dir.exists() {
            fs::create_dir_all(&models_dir)?;
        }

        let mut available_models = HashMap::new();

        // Add NVIDIA Parakeet models (directory-based)
        available_models.insert(
            "parakeet-tdt-0.6b-v2".to_string(),
            ModelInfo {
                id: "parakeet-tdt-0.6b-v2".to_string(),
                name: "Parakeet V2".to_string(),
                description: "English only. The best model for English speakers.".to_string(),
                filename: "parakeet-tdt-0.6b-v2-int8".to_string(), // Directory name
                url: Some("https://blob.handy.computer/parakeet-v2-int8.tar.gz".to_string()),
                sha256: Some(
                    "ac9b9429984dd565b25097337a887bb7f0f8ac393573661c651f0e7d31563991".to_string(),
                ),
                size_mb: 473, // Approximate size for int8 quantized model
                is_downloaded: false,
                is_downloading: false,
                partial_size: 0,
                is_directory: true,
                engine_type: EngineType::Parakeet,
                accuracy_score: 0.85,
                speed_score: 0.85,
                supports_translation: false,
                is_recommended: false,
                supported_languages: vec!["en".to_string()],
                supports_language_selection: false,
                is_custom: false,
            },
        );

        // Parakeet V3 supported languages (25 EU languages + Russian/Ukrainian):
        // bg, hr, cs, da, nl, en, et, fi, fr, de, el, hu, it, lv, lt, mt, pl, pt, ro, sk, sl, es, sv, ru, uk
        let parakeet_v3_languages: Vec<String> = vec![
            "bg", "hr", "cs", "da", "nl", "en", "et", "fi", "fr", "de", "el", "hu", "it", "lv",
            "lt", "mt", "pl", "pt", "ro", "sk", "sl", "es", "sv", "ru", "uk",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        available_models.insert(
            "parakeet-tdt-0.6b-v3".to_string(),
            ModelInfo {
                id: "parakeet-tdt-0.6b-v3".to_string(),
                name: "Parakeet V3".to_string(),
                description: "Fast and accurate. Supports 25 European languages.".to_string(),
                filename: "parakeet-tdt-0.6b-v3-int8".to_string(), // Directory name
                url: Some("https://blob.handy.computer/parakeet-v3-int8.tar.gz".to_string()),
                sha256: Some(
                    "43d37191602727524a7d8c6da0eef11c4ba24320f5b4730f1a2497befc2efa77".to_string(),
                ),
                size_mb: 478, // Approximate size for int8 quantized model
                is_downloaded: false,
                is_downloading: false,
                partial_size: 0,
                is_directory: true,
                engine_type: EngineType::Parakeet,
                accuracy_score: 0.80,
                speed_score: 0.85,
                supports_translation: false,
                is_recommended: true,
                supported_languages: parakeet_v3_languages,
                supports_language_selection: false,
                is_custom: false,
            },
        );

        let manager = Self {
            app_handle: app_handle.clone(),
            models_dir,
            available_models: Mutex::new(available_models),
            model_operations: DownloadClaimRegistry::new(),
            extracting_models: Arc::new(Mutex::new(HashSet::new())),
            lifecycle_lock: ModelLifecycleLock::default(),
            next_quarantine_id: AtomicU64::new(1),
        };

        // Check which models are already downloaded
        manager.update_download_status()?;

        // Auto-select a model if none is currently selected
        manager.auto_select_model_if_needed()?;

        Ok(manager)
    }

    pub fn get_available_models(&self) -> Vec<ModelInfo> {
        let models = self.available_models.lock().unwrap();
        models.values().cloned().collect()
    }

    pub fn get_model_info(&self, model_id: &str) -> Option<ModelInfo> {
        let models = self.available_models.lock().unwrap();
        models.get(model_id).cloned()
    }

    fn update_download_status(&self) -> Result<()> {
        let mut models = self.available_models.lock().unwrap();

        for model in models.values_mut() {
            if model.is_directory {
                // For directory-based models, check if the directory exists
                let model_path = self.models_dir.join(&model.filename);
                let partial_path = self.models_dir.join(format!("{}.partial", &model.filename));
                let extracting_path = self
                    .models_dir
                    .join(format!("{}.extracting", &model.filename));

                // Clean up any leftover .extracting directories from interrupted extractions
                // But only if this model is NOT currently being extracted
                let is_currently_extracting = {
                    let extracting = self.extracting_models.lock().unwrap();
                    extracting.contains(&model.id)
                };
                if extracting_path.exists() && !is_currently_extracting {
                    warn!("Cleaning up interrupted extraction for model: {}", model.id);
                    let _ = fs::remove_dir_all(&extracting_path);
                }

                model.is_downloaded = model_path.exists() && model_path.is_dir();
                model.is_downloading = false;

                // Get partial file size if it exists (for the .tar.gz being downloaded)
                if partial_path.exists() {
                    model.partial_size = partial_path.metadata().map(|m| m.len()).unwrap_or(0);
                } else {
                    model.partial_size = 0;
                }
            } else {
                // For file-based models (existing logic)
                let model_path = self.models_dir.join(&model.filename);
                let partial_path = self.models_dir.join(format!("{}.partial", &model.filename));

                model.is_downloaded = model_path.exists();
                model.is_downloading = false;

                // Get partial file size if it exists
                if partial_path.exists() {
                    model.partial_size = partial_path.metadata().map(|m| m.len()).unwrap_or(0);
                } else {
                    model.partial_size = 0;
                }
            }
        }

        Ok(())
    }

    fn auto_select_model_if_needed(&self) -> Result<()> {
        let mut settings = get_settings(&self.app_handle);

        // Clear stale selection: selected model is set but doesn't exist
        // in available_models (e.g. deleted custom model file)
        if !settings.selected_model.is_empty() {
            let models = self.available_models.lock().unwrap();
            let exists = models.contains_key(&settings.selected_model);
            drop(models);

            if !exists {
                info!(
                    "Selected model '{}' not found in available models, clearing selection",
                    settings.selected_model
                );
                settings.selected_model = String::new();
                write_settings(&self.app_handle, settings.clone());
            }
        }

        // If no model is selected, pick the first downloaded one
        if settings.selected_model.is_empty() {
            // Find the first available (downloaded) model
            let models = self.available_models.lock().unwrap();
            if let Some(available_model) = models.values().find(|model| model.is_downloaded) {
                info!(
                    "Auto-selecting model: {} ({})",
                    available_model.id, available_model.name
                );

                // Update settings with the selected model
                let mut updated_settings = settings;
                updated_settings.selected_model = available_model.id.clone();
                write_settings(&self.app_handle, updated_settings);

                info!("Successfully auto-selected model: {}", available_model.id);
            }
        }

        Ok(())
    }

    /// Verifies the SHA256 of `path` against `expected_sha256` (if provided).
    /// On mismatch or read error the partial file is deleted and an error is returned,
    /// so the next download attempt always starts from a clean state.
    /// When `expected_sha256` is `None` (custom user models) verification is skipped.
    fn verify_sha256(path: &Path, expected_sha256: Option<&str>, model_id: &str) -> Result<()> {
        Self::verify_sha256_cancellable(path, expected_sha256, model_id, None).map(|_| ())
    }

    /// Verifies a downloaded file, returning `false` when cancellation wins
    /// before verification completes. Cancellation deliberately preserves the
    /// partial file so a later attempt can resume it.
    fn verify_sha256_cancellable(
        path: &Path,
        expected_sha256: Option<&str>,
        model_id: &str,
        cancel_flag: Option<&AtomicBool>,
    ) -> Result<bool> {
        if cancel_flag.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Ok(false);
        }

        let Some(expected) = expected_sha256 else {
            return Ok(true);
        };
        let actual = match Self::compute_sha256_cancellable(path, cancel_flag) {
            Ok(actual) => actual,
            Err(e) => {
                let _ = fs::remove_file(path);
                return Err(anyhow::anyhow!(
                    "Failed to verify download for model {}: {}. Please retry.",
                    model_id,
                    e
                ));
            }
        };
        match actual {
            None => Ok(false),
            Some(actual) if actual == expected => {
                info!("SHA256 verified for model {}", model_id);
                Ok(true)
            }
            Some(actual) => {
                warn!(
                    "SHA256 mismatch for model {}: expected {}, got {}",
                    model_id, expected, actual
                );
                let _ = fs::remove_file(path);
                Err(anyhow::anyhow!(
                    "Download verification failed for model {}: file is corrupt. Please retry.",
                    model_id
                ))
            }
        }
    }

    /// Computes the SHA256 hex digest of a file, reading in 64KB chunks to handle large models.
    fn compute_sha256(path: &Path) -> Result<String> {
        Self::compute_sha256_cancellable(path, None)?.ok_or_else(|| {
            anyhow::anyhow!("SHA256 computation was cancelled without a cancellation source")
        })
    }

    fn compute_sha256_cancellable(
        path: &Path,
        cancel_flag: Option<&AtomicBool>,
    ) -> Result<Option<String>> {
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            if cancel_flag.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                return Ok(None);
            }
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        if cancel_flag.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Ok(None);
        }
        Ok(Some(format!("{:x}", hasher.finalize())))
    }

    pub async fn download_model(&self, model_id: &str) -> Result<()> {
        let model_info = {
            let models = self.available_models.lock().unwrap();
            models.get(model_id).cloned()
        };

        let model_info =
            model_info.ok_or_else(|| anyhow::anyhow!("Model not found: {}", model_id))?;

        let url = model_info
            .url
            .ok_or_else(|| anyhow::anyhow!("No download URL for model"))?;
        let max_download_bytes = max_download_bytes_for_model(model_id)?;
        // Claim before inspecting or opening either path. This makes the
        // complete/partial checks and the subsequent writes one per-model
        // operation, so duplicate callers cannot share a partial file.
        let download_claim = self.model_operations.claim_download(model_id)?;
        let model_path = self.models_dir.join(&model_info.filename);
        let partial_path = self
            .models_dir
            .join(format!("{}.partial", &model_info.filename));
        let cancel_flag = download_claim.cancel_flag.clone();
        let cancel_notify = download_claim.cancel_notify.clone();
        let _cleanup = DownloadCleanup {
            available_models: &self.available_models,
            claims: &self.model_operations,
            model_id: model_id.to_string(),
            owner_id: download_claim.owner_id,
        };

        // Don't download if complete version already exists
        if model_path.exists() {
            // Clean up any partial file that might exist.
            if partial_file_size(&partial_path)?.is_some() {
                discard_partial_file(&partial_path, "complete model already exists")?;
            }
            self.update_download_status()?;
            return Ok(());
        }

        // Check if we have a partial download to resume. A partial at the
        // ceiling cannot safely accept another byte, so discard it and start
        // fresh rather than issuing an invalid range request.
        let mut resume_from = partial_file_size(&partial_path)?.unwrap_or(0);
        if resume_from >= max_download_bytes {
            discard_partial_file(
                &partial_path,
                "partial model reached the pinned download limit",
            )?;
            resume_from = 0;
        }
        if resume_from > 0 {
            info!(
                "Resuming download of model {} from byte {}",
                model_id, resume_from
            );
        } else {
            info!("Starting fresh download of model {} from {}", model_id, url);
        }

        // Mark as downloading
        {
            let mut models = self.available_models.lock().unwrap();
            if let Some(model) = models.get_mut(model_id) {
                model.is_downloading = true;
            }
        }

        // Bound connection setup and each stalled read. Cancellation is also
        // selected alongside request/stream futures so it does not wait for a
        // server that never produces another byte.
        let client = reqwest::Client::builder()
            .connect_timeout(MODEL_DOWNLOAD_CONNECT_TIMEOUT)
            .read_timeout(MODEL_DOWNLOAD_READ_TIMEOUT)
            .build()?;
        let mut request = client.get(&url);

        if resume_from > 0 {
            request = request.header("Range", format!("bytes={}-", resume_from));
        }

        let Some(mut response) =
            send_download_request(request, &cancel_flag, &cancel_notify).await?
        else {
            info!("Download cancelled while connecting for: {}", model_id);
            return Ok(());
        };

        // If we tried to resume but server returned 200 (not 206 Partial Content),
        // the server doesn't support range requests. Delete partial file and restart
        // fresh to avoid file corruption (appending full file to partial).
        if resume_from > 0 && response.status() == reqwest::StatusCode::OK {
            warn!(
                "Server doesn't support range requests for model {}, restarting download",
                model_id
            );
            drop(response);
            discard_partial_file(&partial_path, "server does not support range requests")?;

            // Reset resume_from since we're starting fresh
            resume_from = 0;

            // Restart download without range header
            let Some(restarted_response) =
                send_download_request(client.get(&url), &cancel_flag, &cancel_notify).await?
            else {
                info!("Download cancelled while reconnecting for: {}", model_id);
                return Ok(());
            };
            response = restarted_response;
        }

        // Check for success or partial content status
        if !response.status().is_success()
            && response.status() != reqwest::StatusCode::PARTIAL_CONTENT
        {
            return Err(anyhow::anyhow!(
                "Failed to download model: HTTP {}",
                response.status()
            ));
        }

        let content_length = match response
            .headers()
            .get(CONTENT_LENGTH)
            .map(|value| value.to_str())
            .transpose()
        {
            Ok(value) => value,
            Err(error) => {
                drop(response);
                return Err(reject_download_with_cleanup(
                    &partial_path,
                    error.into(),
                    "invalid Content-Length header encoding",
                ));
            }
        };
        let content_range = match response
            .headers()
            .get(CONTENT_RANGE)
            .map(|value| value.to_str())
            .transpose()
        {
            Ok(value) => value,
            Err(error) => {
                drop(response);
                return Err(reject_download_with_cleanup(
                    &partial_path,
                    error.into(),
                    "invalid Content-Range header encoding",
                ));
            }
        };
        let validated_headers = match validate_download_headers(
            response.status(),
            content_length,
            content_range,
            resume_from,
            max_download_bytes,
        ) {
            Ok(headers) => headers,
            Err(error) => {
                // A response that cannot describe a safe bounded write must
                // not leave a stale resume point to be trusted on retry.
                drop(response);
                return Err(reject_download_with_cleanup(
                    &partial_path,
                    error,
                    "invalid download headers",
                ));
            }
        };
        let total_size = validated_headers.total_size.unwrap_or(0);

        let mut downloaded = resume_from;
        let mut stream = response.bytes_stream();

        // Open file for appending if resuming, or create new if starting fresh
        let mut file = if resume_from > 0 {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&partial_path)?
        } else {
            std::fs::File::create(&partial_path)?
        };

        // Emit initial progress
        let initial_progress = DownloadProgress {
            model_id: model_id.to_string(),
            downloaded,
            total: total_size,
            percentage: if total_size > 0 {
                (downloaded as f64 / total_size as f64) * 100.0
            } else {
                0.0
            },
        };
        let _ = self
            .app_handle
            .emit("model-download-progress", &initial_progress);

        // Throttle progress events to max 10/sec (100ms intervals)
        let mut last_emit = Instant::now();
        let throttle_duration = Duration::from_millis(100);

        // Download with progress
        loop {
            let next_chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = wait_for_download_cancellation(&cancel_flag, &cancel_notify) => {
                    info!("Download cancelled while reading for: {}", model_id);
                    return Ok(());
                },
            };
            let Some(chunk) = next_chunk else {
                break;
            };

            // Do not write a chunk selected concurrently with cancellation.
            if cancel_flag.load(Ordering::Acquire) {
                info!("Download cancelled for: {}", model_id);
                return Ok(());
            }

            let chunk = chunk?;

            let next_downloaded =
                match next_download_size(downloaded, chunk.len(), max_download_bytes) {
                    Ok(next_downloaded) => next_downloaded,
                    Err(error) => {
                        drop(file);
                        return Err(reject_download_with_cleanup(
                            &partial_path,
                            error,
                            "stream exceeded the pinned download limit",
                        ));
                    }
                };

            // The cumulative check is deliberately before write_all: a
            // chunk that crosses the ceiling never reaches the filesystem.
            file.write_all(&chunk)?;
            downloaded = next_downloaded;

            let percentage = if total_size > 0 {
                (downloaded as f64 / total_size as f64) * 100.0
            } else {
                0.0
            };

            // Emit progress event (throttled to avoid UI freeze)
            if last_emit.elapsed() >= throttle_duration {
                let progress = DownloadProgress {
                    model_id: model_id.to_string(),
                    downloaded,
                    total: total_size,
                    percentage,
                };
                let _ = self.app_handle.emit("model-download-progress", &progress);
                last_emit = Instant::now();
            }
        }

        if cancel_flag.load(Ordering::Acquire) {
            info!(
                "Download cancelled after receiving final chunk for: {}",
                model_id
            );
            return Ok(());
        }

        // Emit final progress to ensure 100% is shown
        let final_progress = DownloadProgress {
            model_id: model_id.to_string(),
            downloaded,
            total: total_size,
            percentage: if total_size > 0 {
                (downloaded as f64 / total_size as f64) * 100.0
            } else {
                100.0
            },
        };
        let _ = self
            .app_handle
            .emit("model-download-progress", &final_progress);

        file.flush()?;
        drop(file); // Ensure file is closed before moving

        // Verify downloaded file size matches the validated response metadata.
        let actual_size = partial_file_size(&partial_path)?.ok_or_else(|| {
            anyhow::anyhow!("Downloaded partial model disappeared before verification")
        })?;
        if let Some(expected_body_size) = validated_headers.expected_body_size {
            let received_body_size = downloaded.checked_sub(resume_from).ok_or_else(|| {
                anyhow::anyhow!("Downloaded byte count is below the resume offset")
            })?;
            if received_body_size != expected_body_size {
                discard_partial_file(&partial_path, "download body length mismatch")?;
                return Err(anyhow::anyhow!(
                    "Download body length mismatch: expected {} bytes, got {} bytes",
                    expected_body_size,
                    received_body_size
                ));
            }
        }
        if validated_headers
            .total_size
            .is_some_and(|expected| actual_size != expected)
        {
            discard_partial_file(&partial_path, "download size mismatch")?;
            return Err(anyhow::anyhow!(
                "Download incomplete: expected {} bytes, got {} bytes",
                total_size,
                actual_size
            ));
        }

        // Verify SHA256 checksum. Runs in a blocking thread so the async executor is not
        // stalled while hashing large model files (up to 1.6 GB). On failure the partial
        // is deleted inside verify_sha256 so the next attempt always starts fresh.
        let _ = self.app_handle.emit("model-verification-started", model_id);
        info!("Verifying SHA256 for model {}...", model_id);
        let verify_path = partial_path.clone();
        let verify_expected = model_info.sha256.clone();
        let verify_model_id = model_id.to_string();
        let verify_cancel_flag = cancel_flag.clone();
        let verify_result = tokio::task::spawn_blocking(move || {
            Self::verify_sha256_cancellable(
                &verify_path,
                verify_expected.as_deref(),
                &verify_model_id,
                Some(&verify_cancel_flag),
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("SHA256 task panicked: {}", e))??;
        if !verify_result {
            info!(
                "Download cancelled during SHA256 verification: {}",
                model_id
            );
            return Ok(());
        }
        let _ = self
            .app_handle
            .emit("model-verification-completed", model_id);

        // Handle directory-based models (extract tar.gz) vs file-based models.
        // Extraction is staged and cancellation-aware; nothing is installed
        // until the per-model claim linearizes the final rename.
        let mut extraction_cleanup: Option<ExtractionCleanup> = None;
        let mut staging_cleanup: Option<StagingDirectoryCleanup> = None;
        let staged_model_path = if model_info.is_directory {
            if cancel_flag.load(Ordering::Acquire) {
                return Ok(());
            }

            {
                let mut extracting = self.extracting_models.lock().unwrap();
                extracting.insert(model_id.to_string());
            }
            extraction_cleanup = Some(ExtractionCleanup {
                extracting_models: self.extracting_models.clone(),
                model_id: model_id.to_string(),
            });

            let _ = self.app_handle.emit("model-extraction-started", model_id);
            info!("Extracting archive for directory-based model: {}", model_id);

            let temp_extract_dir = self
                .models_dir
                .join(format!("{}.extracting", &model_info.filename));
            let final_model_dir = self.models_dir.join(&model_info.filename);
            if temp_extract_dir.exists() {
                fs::remove_dir_all(&temp_extract_dir)?;
            }
            fs::create_dir_all(&temp_extract_dir)?;
            staging_cleanup = Some(StagingDirectoryCleanup {
                path: temp_extract_dir.clone(),
            });

            let archive_path = partial_path.clone();
            let extraction_destination = temp_extract_dir.clone();
            let extraction_cancel_flag = cancel_flag.clone();
            let extracted = tokio::task::spawn_blocking(move || {
                if extraction_cancel_flag.load(Ordering::Acquire) {
                    return Ok(false);
                }
                let tar_gz = File::open(archive_path)?;
                extract_archive(tar_gz, &extraction_destination, extraction_cancel_flag)
            })
            .await
            .map_err(|e| anyhow::anyhow!("Extraction task panicked: {}", e))?;
            match extracted {
                Ok(true) => {}
                Ok(false) => {
                    info!("Download cancelled during extraction: {}", model_id);
                    return Ok(());
                }
                Err(e) => {
                    let error_msg = format!("Failed to extract archive: {}", e);
                    let _ = fs::remove_file(&partial_path);
                    let _ = self.app_handle.emit(
                        "model-extraction-failed",
                        &serde_json::json!({
                            "model_id": model_id,
                            "error": error_msg
                        }),
                    );
                    return Err(anyhow::anyhow!(error_msg));
                }
            }

            if cancel_flag.load(Ordering::Acquire) {
                info!("Download cancelled after extraction: {}", model_id);
                return Ok(());
            }

            let mut extracted_dirs = Vec::new();
            for entry in fs::read_dir(&temp_extract_dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    extracted_dirs.push(entry.path());
                }
            }

            // Keep the staged path in place until the final commit. This makes
            // cancellation during extraction unable to expose a model path.
            if final_model_dir.exists() {
                return Err(anyhow::anyhow!(
                    "Model path already exists during extraction: {}",
                    model_id
                ));
            }
            if extracted_dirs.len() == 1 {
                extracted_dirs.remove(0)
            } else {
                temp_extract_dir.clone()
            }
        } else {
            partial_path.clone()
        };

        if cancel_flag.load(Ordering::Acquire) {
            info!("Download cancelled before model install: {}", model_id);
            return Ok(());
        }

        let final_path = if model_info.is_directory {
            self.models_dir.join(&model_info.filename)
        } else {
            model_path.clone()
        };
        let partial_path_for_commit = partial_path.clone();
        let staging_path_for_commit = staged_model_path.clone();
        let temp_extract_path_for_commit = if model_info.is_directory {
            Some(
                self.models_dir
                    .join(format!("{}.extracting", &model_info.filename)),
            )
        } else {
            None
        };
        let committed = self.model_operations.commit_if_not_cancelled(
            model_id,
            download_claim.owner_id,
            || {
                if final_path.exists() {
                    return Err(anyhow::anyhow!(
                        "Model path already exists during final install: {}",
                        model_id
                    ));
                }
                fs::rename(&staging_path_for_commit, &final_path)?;
                if model_info.is_directory {
                    fs::remove_file(&partial_path_for_commit).map_err(|e| {
                        let _ = fs::rename(&final_path, &staging_path_for_commit);
                        anyhow::anyhow!("Failed to remove verified archive after install: {}", e)
                    })?;
                    if let Some(temp_path) = &temp_extract_path_for_commit {
                        if temp_path.exists() {
                            fs::remove_dir_all(temp_path).map_err(|e| {
                                let _ = fs::rename(&final_path, &staging_path_for_commit);
                                anyhow::anyhow!(
                                    "Failed to clean extraction staging directory: {}",
                                    e
                                )
                            })?;
                        }
                    }
                }
                Ok(())
            },
        )?;
        if committed.is_none() {
            info!(
                "Download cancelled before final model install: {}",
                model_id
            );
            return Ok(());
        }

        if model_info.is_directory {
            let _ = self.app_handle.emit("model-extraction-completed", model_id);
        }
        drop(staging_cleanup);
        drop(extraction_cleanup);

        {
            let mut models = self.available_models.lock().unwrap();
            if let Some(model) = models.get_mut(model_id) {
                model.is_downloading = false;
                model.is_downloaded = true;
                model.partial_size = 0;
            }
        }
        // The cleanup guard releases the claim after this owner returns.

        // Emit completion event
        let _ = self.app_handle.emit("model-download-complete", model_id);

        info!(
            "Successfully downloaded model {} to {:?}",
            model_id, model_path
        );

        Ok(())
    }

    /// Serializes model lifecycle mutations which also touch application
    /// selection or the loaded transcription engine. Downloads remain
    /// independently scoped by `model_operations`.
    pub fn with_lifecycle_operation<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        self.lifecycle_lock.execute(operation)
    }

    /// Runs a model mutation while holding exclusive ownership for that model.
    /// Command-layer state changes can safely be placed inside this closure:
    /// rejected concurrent operations return before the closure is entered.
    pub fn with_model_operation<T>(
        &self,
        model_id: &str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let operation_claim = self.model_operations.claim_operation(model_id)?;
        let _operation_cleanup = OperationCleanup {
            claims: &self.model_operations,
            model_id: model_id.to_string(),
            owner_id: operation_claim.owner_id,
        };
        operation()
    }

    pub fn delete_model(&self, model_id: &str) -> Result<()> {
        self.with_lifecycle_operation(|| {
            self.with_model_operation(model_id, || {
                self.delete_model_claimed_for_command(model_id, || Ok(()))
            })
        })
    }

    /// Quarantines all model files before running the caller's state changes.
    /// If the callback fails, the files are restored and no model metadata is
    /// changed. The callback is used by the command layer to unload/clear an
    /// active model inside the same lifecycle transaction.
    pub(crate) fn delete_model_claimed_for_command<T>(
        &self,
        model_id: &str,
        before_commit: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        debug!("ModelManager: delete_model called for: {}", model_id);

        let model_info = {
            let models = self.available_models.lock().unwrap();
            models.get(model_id).cloned()
        }
        .ok_or_else(|| anyhow::anyhow!("Model not found: {}", model_id))?;

        let model_path = self.models_dir.join(&model_info.filename);
        let partial_path = self
            .models_dir
            .join(format!("{}.partial", &model_info.filename));
        let mut paths = Vec::new();

        let model_metadata = fs::symlink_metadata(&model_path);
        match model_metadata {
            Ok(metadata) => {
                let expected_directory = model_info.is_directory;
                if metadata.file_type().is_dir() != expected_directory {
                    return Err(anyhow::anyhow!(
                        "Model path has unexpected type: {:?}",
                        model_path
                    ));
                }
                paths.push(model_path.clone());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        match fs::symlink_metadata(&partial_path) {
            Ok(metadata) if metadata.file_type().is_file() => paths.push(partial_path.clone()),
            Ok(_) => {
                return Err(anyhow::anyhow!(
                    "Partial model path has unexpected type: {:?}",
                    partial_path
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        if paths.is_empty() {
            return Err(anyhow::anyhow!("No model files found to delete"));
        }

        let quarantine_id = self.next_quarantine_id.fetch_add(1, Ordering::Relaxed);
        let quarantine_dir = self
            .models_dir
            .join(format!(".delete-{}-{}", model_info.filename, quarantine_id));
        let rename: RenameOperation = Arc::new(|from: &Path, to: &Path| fs::rename(from, to));
        let mut quarantine = QuarantineTransaction::start(paths, quarantine_dir, rename)?;

        let callback_result = before_commit();
        let callback_value = match callback_result {
            Ok(value) => value,
            Err(error) => {
                quarantine.rollback().map_err(|rollback_error| {
                    anyhow::anyhow!(
                        "Model delete failed: {}; rollback failed: {}",
                        error,
                        rollback_error
                    )
                })?;
                return Err(error);
            }
        };

        // These in-memory updates cannot fail and are performed only after all
        // fallible pre-commit work completed. Avoid rescanning the filesystem:
        // a scan would add a new failure point after quarantine.
        if model_info.is_custom {
            let mut models = self.available_models.lock().unwrap();
            if models.remove(model_id).is_none() {
                quarantine.rollback()?;
                return Err(anyhow::anyhow!(
                    "Model metadata disappeared during deletion: {}",
                    model_id
                ));
            }
        } else {
            let mut models = self.available_models.lock().unwrap();
            let Some(model) = models.get_mut(model_id) else {
                quarantine.rollback()?;
                return Err(anyhow::anyhow!(
                    "Model metadata disappeared during deletion: {}",
                    model_id
                ));
            };
            model.is_downloaded = false;
            model.is_downloading = false;
            model.partial_size = 0;
        }

        quarantine.commit();
        let _ = self.app_handle.emit("model-deleted", model_id);
        info!("Model deletion committed for {}", model_id);
        Ok(callback_value)
    }

    pub fn get_model_path(&self, model_id: &str) -> Result<PathBuf> {
        let model_info = self
            .get_model_info(model_id)
            .ok_or_else(|| anyhow::anyhow!("Model not found: {}", model_id))?;

        if !model_info.is_downloaded {
            return Err(anyhow::anyhow!("Model not available: {}", model_id));
        }

        // Ensure we don't return partial files/directories
        if model_info.is_downloading {
            return Err(anyhow::anyhow!(
                "Model is currently downloading: {}",
                model_id
            ));
        }

        let model_path = self.models_dir.join(&model_info.filename);
        let partial_path = self
            .models_dir
            .join(format!("{}.partial", &model_info.filename));

        if model_info.is_directory {
            // For directory-based models, ensure the directory exists and is complete
            if model_path.exists() && model_path.is_dir() && !partial_path.exists() {
                Ok(model_path)
            } else {
                Err(anyhow::anyhow!(
                    "Complete model directory not found: {}",
                    model_id
                ))
            }
        } else {
            // For file-based models (existing logic)
            if model_path.exists() && !partial_path.exists() {
                Ok(model_path)
            } else {
                Err(anyhow::anyhow!(
                    "Complete model file not found: {}",
                    model_id
                ))
            }
        }
    }

    pub fn cancel_download(&self, model_id: &str) -> Result<()> {
        debug!("ModelManager: cancel_download called for: {}", model_id);

        // Only the currently claimed download can observe this cancellation.
        // Cleanup uses the claim owner as well, so a stale task cannot clear a
        // newer attempt's state.
        if self.model_operations.cancel(model_id) {
            info!("Cancellation flag set for: {}", model_id);
        } else {
            warn!("No active download found for: {}", model_id);
        }

        // Emit cancellation event so all UI components can clear their state
        let _ = self.app_handle.emit("model-download-cancelled", model_id);

        info!("Download cancellation initiated for: {}", model_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn chunked_stream_rejects_oversize_before_write_and_removes_partial() {
        let (_dir, path) = write_temp_file(b"1234");
        let max_download_bytes = 6;
        let mut downloaded = 4;

        for chunk in [b"56".as_slice(), b"7".as_slice()] {
            let result = next_download_size(downloaded, chunk.len(), max_download_bytes);
            if let Err(error) = result {
                discard_partial_file(&path, "test stream exceeded limit").unwrap();
                assert!(error.to_string().contains("exceeds"));
                break;
            }
            downloaded += chunk.len() as u64;
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(chunk)
                .unwrap();
        }

        assert_eq!(downloaded, max_download_bytes);
        assert!(!path.exists(), "oversized partial must be removed");
    }

    #[test]
    fn endless_chunked_stream_is_bounded_without_content_length() {
        let max_download_bytes = 8;
        let mut downloaded = 0;
        for _ in 0..2 {
            downloaded = next_download_size(downloaded, 3, max_download_bytes).unwrap();
        }
        assert_eq!(downloaded, 6);
        assert!(next_download_size(downloaded, 3, max_download_bytes).is_err());
    }

    #[test]
    fn content_length_over_limit_is_rejected_before_write() {
        assert!(
            validate_download_headers(reqwest::StatusCode::OK, Some("9"), None, 0, 8,).is_err()
        );
        assert!(validate_download_headers(
            reqwest::StatusCode::OK,
            Some("not-a-number"),
            None,
            0,
            8,
        )
        .is_err());
    }

    #[test]
    fn resume_content_range_must_match_offset_and_fit_limit() {
        let valid = validate_download_headers(
            reqwest::StatusCode::PARTIAL_CONTENT,
            Some("4"),
            Some("bytes 6-9/10"),
            6,
            10,
        )
        .unwrap();
        assert_eq!(valid.total_size, Some(10));
        assert_eq!(valid.expected_body_size, Some(4));

        assert!(validate_download_headers(
            reqwest::StatusCode::PARTIAL_CONTENT,
            Some("4"),
            Some("bytes 5-8/10"),
            6,
            10,
        )
        .is_err());
        assert!(validate_download_headers(
            reqwest::StatusCode::PARTIAL_CONTENT,
            Some("4"),
            Some("bytes 6-9/11"),
            6,
            10,
        )
        .is_err());
    }

    #[test]
    fn oversized_partial_is_discarded_before_resume() {
        let (_dir, path) = write_temp_file(b"123456789");
        let max_download_bytes = 8;
        let size = partial_file_size(&path).unwrap().unwrap();
        assert!(size >= max_download_bytes);
        discard_partial_file(&path, "test resume offset exceeds limit").unwrap();
        assert!(!path.exists());
    }

    // ── SHA256 verification tests ─────────────────────────────────────────────

    /// Helper: write `data` to a temp file and return (TempDir, path).
    /// TempDir must be kept alive for the duration of the test.
    fn write_temp_file(data: &[u8]) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("model.partial");
        let mut f = File::create(&path).unwrap();
        f.write_all(data).unwrap();
        (dir, path)
    }

    #[test]
    fn test_verify_sha256_skipped_when_none() {
        // Custom models have no expected hash — verification must be a no-op.
        let (_dir, path) = write_temp_file(b"anything");
        assert!(ModelManager::verify_sha256(&path, None, "custom").is_ok());
        assert!(
            path.exists(),
            "file must be untouched when verification is skipped"
        );
    }

    #[test]
    fn test_verify_sha256_passes_on_correct_hash() {
        // Compute the real hash so the test is self-consistent.
        let (_dir, path) = write_temp_file(b"hello world");
        let actual = ModelManager::compute_sha256(&path).unwrap();
        assert!(
            ModelManager::verify_sha256(&path, Some(&actual), "test_model").is_ok(),
            "should pass when hash matches"
        );
        assert!(
            path.exists(),
            "file must be kept on successful verification"
        );
    }

    #[test]
    fn test_verify_sha256_fails_and_deletes_partial_on_mismatch() {
        let (_dir, path) = write_temp_file(b"this is not the real model");
        let wrong_hash = "0000000000000000000000000000000000000000000000000000000000000000";

        let result = ModelManager::verify_sha256(&path, Some(wrong_hash), "bad_model");

        assert!(result.is_err(), "mismatch must return an error");
        assert!(
            result.unwrap_err().to_string().contains("corrupt"),
            "error message should mention corruption"
        );
        assert!(
            !path.exists(),
            "partial file must be deleted after hash mismatch"
        );
    }

    #[test]
    fn test_verify_sha256_fails_and_deletes_partial_when_file_missing() {
        // Simulate a partial file that was already removed (e.g. disk full mid-download).
        let dir = TempDir::new().unwrap();
        let missing_path = dir.path().join("gone.partial");
        // Don't create the file — it should not exist.

        let result =
            ModelManager::verify_sha256(&missing_path, Some("anyexpectedhash"), "missing_model");

        assert!(result.is_err(), "missing file must return an error");
    }

    #[test]
    fn concurrent_download_claims_allow_only_one_owner() {
        let registry = Arc::new(DownloadClaimRegistry::new());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut workers = Vec::new();

        for _ in 0..2 {
            let registry = registry.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                registry.claim_download("same-model").is_ok()
            }));
        }

        let successful_claims = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|claimed| *claimed)
            .count();
        assert_eq!(successful_claims, 1);
    }

    #[test]
    fn stale_download_owner_cannot_release_new_claim() {
        let registry = DownloadClaimRegistry::new();
        let old_claim = registry.claim_download("same-model").unwrap();
        registry.release("same-model", old_claim.owner_id);
        let new_claim = registry.claim_download("same-model").unwrap();

        registry.release("same-model", old_claim.owner_id);
        assert!(registry.claim_download("same-model").is_err());

        registry.release("same-model", new_claim.owner_id);
        assert!(registry.claim_download("same-model").is_ok());
    }

    #[test]
    fn delete_claim_excludes_download_claims_until_owner_exits() {
        let registry = DownloadClaimRegistry::new();
        {
            let delete_claim = registry.claim_operation("same-model").unwrap();
            let _cleanup = OperationCleanup {
                claims: &registry,
                model_id: "same-model".to_string(),
                owner_id: delete_claim.owner_id,
            };
            assert!(registry.claim_download("same-model").is_err());
        }
        assert!(registry.claim_download("same-model").is_ok());
    }

    #[test]
    fn selected_model_concurrent_delete_rejection_has_no_mutation() {
        let registry = Arc::new(DownloadClaimRegistry::new());
        let active_download = registry.claim_download("selected-model").unwrap();
        let delete_started = Arc::new(std::sync::Barrier::new(2));
        let delete_mutations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let delete_registry = registry.clone();
        let delete_barrier = delete_started.clone();
        let delete_mutations_for_thread = delete_mutations.clone();

        let delete_thread = std::thread::spawn(move || {
            delete_barrier.wait();
            if delete_registry.claim_operation("selected-model").is_ok() {
                delete_mutations_for_thread.fetch_add(1, Ordering::Relaxed);
            }
        });
        delete_started.wait();
        delete_thread.join().unwrap();

        // The claim is rejected before the command can unload the engine or
        // clear the selected-model setting.
        assert_eq!(delete_mutations.load(Ordering::Relaxed), 0);
        registry.release("selected-model", active_download.owner_id);
        assert!(registry.claim_operation("selected-model").is_ok());
    }

    #[test]
    fn filesystem_fault_during_quarantine_restores_everything() {
        let directory = TempDir::new().unwrap();
        let model_path = directory.path().join("model");
        let partial_path = directory.path().join("model.partial");
        fs::write(&model_path, b"complete model").unwrap();
        fs::write(&partial_path, b"partial model").unwrap();
        let quarantine_path = directory.path().join(".delete-model-1");
        let rename_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_rename = rename_calls.clone();
        let rename: RenameOperation = Arc::new(move |from, to| {
            let call = calls_for_rename.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 2 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected rename failure",
                ));
            }
            fs::rename(from, to)
        });

        let result = QuarantineTransaction::start(
            vec![model_path.clone(), partial_path.clone()],
            quarantine_path.clone(),
            rename,
        );

        assert!(result.is_err());
        assert_eq!(fs::read(&model_path).unwrap(), b"complete model");
        assert_eq!(fs::read(&partial_path).unwrap(), b"partial model");
        assert!(!quarantine_path.exists());
        assert_eq!(rename_calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn lifecycle_lock_prevents_delete_from_clearing_newer_switch() {
        let lifecycle_lock = Arc::new(ModelLifecycleLock::default());
        let operation_started = Arc::new(std::sync::Barrier::new(2));
        let selected_model = Arc::new(Mutex::new("old-model".to_string()));
        let mut workers = Vec::new();

        {
            let lifecycle_lock = lifecycle_lock.clone();
            let operation_started = operation_started.clone();
            let selected_model = selected_model.clone();
            workers.push(std::thread::spawn(move || {
                operation_started.wait();
                lifecycle_lock.execute(|| {
                    std::thread::sleep(Duration::from_millis(10));
                    let mut selected = selected_model.lock().unwrap();
                    if *selected == "old-model" {
                        *selected = String::new();
                    }
                });
            }));
        }
        {
            let lifecycle_lock = lifecycle_lock.clone();
            let operation_started = operation_started.clone();
            let selected_model = selected_model.clone();
            workers.push(std::thread::spawn(move || {
                operation_started.wait();
                lifecycle_lock.execute(|| {
                    *selected_model.lock().unwrap() = "new-model".to_string();
                });
            }));
        }

        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(*selected_model.lock().unwrap(), "new-model");
    }

    #[test]
    fn cancellation_before_final_commit_does_not_run_install() {
        let registry = DownloadClaimRegistry::new();
        let download = registry.claim_download("model").unwrap();
        download.cancel_flag.store(true, Ordering::Release);
        let installed = Arc::new(AtomicBool::new(false));
        let installed_in_commit = installed.clone();

        let result = registry
            .commit_if_not_cancelled("model", download.owner_id, || {
                installed_in_commit.store(true, Ordering::Release);
                Ok(())
            })
            .unwrap();

        assert!(result.is_none());
        assert!(!installed.load(Ordering::Acquire));
    }

    #[test]
    fn cancellation_during_extraction_does_not_install_staged_files() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Cursor;
        use tar::Builder;

        let directory = TempDir::new().unwrap();
        let destination = directory.path().join("model.extracting");
        fs::create_dir_all(&destination).unwrap();

        let mut compressed = Vec::new();
        {
            let encoder = GzEncoder::new(&mut compressed, Compression::fast());
            let mut builder = Builder::new(encoder);
            let contents = vec![b'x'; 2 * 1024 * 1024];
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, "model/model.bin", contents.as_slice())
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }

        struct CancelAfterFirstRead {
            reader: Cursor<Vec<u8>>,
            cancel_flag: Arc<AtomicBool>,
            cancelled: bool,
        }

        impl Read for CancelAfterFirstRead {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let result = self.reader.read(buffer);
                if !self.cancelled {
                    self.cancelled = true;
                    self.cancel_flag.store(true, Ordering::Release);
                }
                result
            }
        }

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let result = extract_archive(
            CancelAfterFirstRead {
                reader: Cursor::new(compressed),
                cancel_flag: cancel_flag.clone(),
                cancelled: false,
            },
            &destination,
            cancel_flag,
        )
        .unwrap();

        assert!(!result, "cancellation during extraction must abort staging");
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn cancellation_interrupts_a_stalled_download_request() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = std::sync::mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            accepted_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(Notify::new());
        let flag_for_canceller = cancel_flag.clone();
        let notify_for_canceller = cancel_notify.clone();
        let canceller = std::thread::spawn(move || {
            accepted_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("download request should reach the stalled server");
            flag_for_canceller.store(true, Ordering::Release);
            notify_for_canceller.notify_one();
        });

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(1))
            .read_timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(send_download_request(
            client.get(format!("http://{}", address)),
            &cancel_flag,
            &cancel_notify,
        ));

        canceller.join().unwrap();
        server.join().unwrap();
        assert!(
            result.unwrap().is_none(),
            "cancellation must abort the request"
        );
    }
}
