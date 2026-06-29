use crate::audio_toolkit::audio::decode_and_resample;
#[cfg(test)]
use crate::audio_toolkit::save_wav_file;
use crate::audio_toolkit::vad::{SileroVad, VoiceActivityDetector};
use crate::managers::history::HistoryManager;
use crate::managers::transcription::TranscriptionManager;
use log::{debug, info, warn};
use serde::Serialize;
use specta::Type;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_clipboard_manager::ClipboardExt;

/// Maximum file size for import (10 GB)
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024 * 1024;

/// Sample rate expected by all transcription engines
const SAMPLE_RATE: usize = 16000;

/// VAD frame size in samples (30ms at 16kHz = 480 samples)
const VAD_FRAME_SAMPLES: usize = SAMPLE_RATE * 30 / 1000; // 480

/// Maximum chunk duration in seconds. Chunks are kept under this limit
/// by splitting at the best silence boundary.
const MAX_CHUNK_SECS: f32 = 30.0;

/// Maximum chunk size in samples
const MAX_CHUNK_SAMPLES: usize = (MAX_CHUNK_SECS as usize) * SAMPLE_RATE;

/// Minimum silence gap required before we treat it as a preferred
/// chunk boundary. Shorter gaps are usually natural intra-sentence pauses.
const MIN_SPLIT_GAP_SECS: f32 = 0.25;

/// Overlap between chunk windows. This gives the model extra context
/// at chunk boundaries without materially changing overall coverage.
const CHUNK_OVERLAP_SAMPLES: usize = SAMPLE_RATE;

/// Audio shorter than this (in seconds) is transcribed in one shot.
const CHUNK_THRESHOLD_SECS: f64 = 35.0;

/// VAD speech detection threshold (0.0–1.0). Lower = more sensitive.
const VAD_THRESHOLD: f32 = 0.35;

/// Progress event emitted during import
#[derive(Serialize, Clone, Debug, Type)]
pub struct ImportProgress {
    pub stage: String,
    pub percent: u8,
    pub message: String,
}

/// Cancellation tokens for active imports
pub struct ImportCancellationTokens {
    tokens: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    /// Guard to prevent concurrent imports.
    import_active: Arc<AtomicBool>,
}

impl ImportCancellationTokens {
    pub fn new() -> Self {
        Self {
            tokens: Arc::new(Mutex::new(HashMap::new())),
            import_active: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Try to acquire the import lock. Returns false if an import is already running.
    pub fn try_start(&self) -> bool {
        self.import_active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Release the import lock.
    pub fn finish(&self) {
        self.import_active.store(false, Ordering::SeqCst);
    }

    pub fn create_token(&self, import_id: &str) -> Arc<AtomicBool> {
        let token = Arc::new(AtomicBool::new(false));
        if let Ok(mut tokens) = self.tokens.lock() {
            tokens.insert(import_id.to_string(), token.clone());
        }
        token
    }

    pub fn is_cancelled(&self, import_id: &str) -> bool {
        if let Ok(tokens) = self.tokens.lock() {
            if let Some(token) = tokens.get(import_id) {
                return token.load(Ordering::Relaxed);
            }
        }
        false
    }

    pub fn cancel(&self, import_id: &str) {
        if let Ok(tokens) = self.tokens.lock() {
            if let Some(token) = tokens.get(import_id) {
                token.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Cancel all active imports. Used when the frontend doesn't know the import ID.
    pub fn cancel_all(&self) {
        if let Ok(tokens) = self.tokens.lock() {
            for token in tokens.values() {
                token.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn cleanup(&self, import_id: &str) {
        if let Ok(mut tokens) = self.tokens.lock() {
            tokens.remove(import_id);
        }
    }
}

impl Default for ImportCancellationTokens {
    fn default() -> Self {
        Self::new()
    }
}

struct ActiveImportGuard {
    cancellation_tokens: Arc<ImportCancellationTokens>,
    import_id: String,
}

impl ActiveImportGuard {
    fn new(cancellation_tokens: Arc<ImportCancellationTokens>, import_id: String) -> Self {
        Self {
            cancellation_tokens,
            import_id,
        }
    }
}

impl Drop for ActiveImportGuard {
    fn drop(&mut self) {
        self.cancellation_tokens.cleanup(&self.import_id);
        self.cancellation_tokens.finish();
    }
}

// ── File validation ──────────────────────────────────────────────

fn validate_audio_file(path: &std::path::Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("File not found: {}", path.display()));
    }

    let file_size = fs::metadata(path)
        .map_err(|e| format!("Cannot read file metadata: {}", e))?
        .len();

    if file_size == 0 {
        return Err("File is empty".to_string());
    }

    if file_size > MAX_FILE_SIZE {
        return Err(format!(
            "File is too large ({:.1}GB, max 10GB).",
            file_size as f64 / (1024.0 * 1024.0 * 1024.0)
        ));
    }

    let file = fs::File::open(path).map_err(|e| format!("Cannot open file: {}", e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Not a valid audio file: {}", e))?;

    Ok(())
}

// ── Progress helpers ─────────────────────────────────────────────

fn emit_progress(app_handle: &AppHandle, stage: &str, percent: u8, message: &str) {
    let _ = app_handle.emit(
        "import-progress",
        ImportProgress {
            stage: stage.to_string(),
            percent,
            message: message.to_string(),
        },
    );
}

fn check_cancelled(
    app_handle: &AppHandle,
    cancellation_tokens: &ImportCancellationTokens,
    import_id: &str,
) -> Result<(), String> {
    if cancellation_tokens.is_cancelled(import_id) {
        emit_progress(app_handle, "cancelled", 0, "Import cancelled");
        return Err("Import was cancelled".into());
    }
    Ok(())
}

// ── VAD-aware chunking ───────────────────────────────────────────

/// A speech region detected by VAD (start/end in sample indices).
#[derive(Debug, Clone)]
struct SpeechSegment {
    start: usize,
    end: usize,
}

/// Run VAD over the audio and return speech segments.
fn detect_speech_segments(
    app_handle: &AppHandle,
    samples: &[f32],
) -> Result<Vec<SpeechSegment>, String> {
    // Resolve the VAD model path from bundled resources
    let vad_path = app_handle
        .path()
        .resolve(
            "resources/models/silero_vad_v4.onnx",
            tauri::path::BaseDirectory::Resource,
        )
        .map_err(|e| format!("Cannot resolve VAD model path: {}", e))?;

    let mut vad = SileroVad::new(&vad_path, VAD_THRESHOLD)
        .map_err(|e| format!("Failed to load VAD model: {}", e))?;

    let mut segments: Vec<SpeechSegment> = Vec::new();
    let mut in_speech = false;
    let mut seg_start = 0usize;

    for (i, frame) in samples.chunks_exact(VAD_FRAME_SAMPLES).enumerate() {
        let is_speech = vad
            .is_voice(frame)
            .map_err(|e| format!("VAD error: {}", e))?;

        let sample_pos = i * VAD_FRAME_SAMPLES;

        if is_speech && !in_speech {
            seg_start = sample_pos;
            in_speech = true;
        } else if !is_speech && in_speech {
            segments.push(SpeechSegment {
                start: seg_start,
                end: sample_pos,
            });
            in_speech = false;
        }
    }

    // Close any trailing speech segment
    if in_speech {
        segments.push(SpeechSegment {
            start: seg_start,
            end: samples.len(),
        });
    }

    Ok(segments)
}

fn preferred_split_points(segments: &[SpeechSegment]) -> Vec<usize> {
    let min_gap_samples = (MIN_SPLIT_GAP_SECS * SAMPLE_RATE as f32) as usize;

    segments
        .windows(2)
        .filter_map(|pair| {
            let left = &pair[0];
            let right = &pair[1];
            let gap = right.start.saturating_sub(left.end);

            if gap >= min_gap_samples {
                Some(left.end + gap / 2)
            } else {
                None
            }
        })
        .collect()
}

fn fixed_size_chunks(total_samples: usize) -> Vec<(usize, usize)> {
    let overlap = CHUNK_OVERLAP_SAMPLES.min(MAX_CHUNK_SAMPLES / 3);
    let step = MAX_CHUNK_SAMPLES.saturating_sub(overlap).max(1);
    let mut chunks = Vec::new();
    let mut pos = 0usize;

    while pos < total_samples {
        let end = (pos + MAX_CHUNK_SAMPLES).min(total_samples);
        chunks.push((pos, end));
        if end >= total_samples {
            break;
        }
        pos += step;
    }

    chunks
}

/// Build chunk windows that continuously cover the full audio while preferring
/// VAD-detected silence gaps as boundaries.
fn chunks_from_split_points(total_samples: usize, split_points: &[usize]) -> Vec<(usize, usize)> {
    if total_samples == 0 {
        return vec![];
    }

    let overlap = CHUNK_OVERLAP_SAMPLES.min(MAX_CHUNK_SAMPLES / 3);
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut split_idx = 0usize;

    while start < total_samples {
        let max_end = (start + MAX_CHUNK_SAMPLES).min(total_samples);
        let mut chosen_end: Option<usize> = None;

        while split_idx < split_points.len() && split_points[split_idx] <= max_end {
            if split_points[split_idx] > start {
                chosen_end = Some(split_points[split_idx]);
            }
            split_idx += 1;
        }

        let end = chosen_end.unwrap_or(max_end);
        chunks.push((start, end));

        if end >= total_samples {
            break;
        }

        let next_start = end.saturating_sub(overlap);
        if next_start <= start {
            // Safety fallback if overlap/window sizes ever produce no forward progress.
            let forced_end = max_end;
            if forced_end >= total_samples {
                break;
            }
            start = forced_end;
        } else {
            start = next_start;
        }
    }

    chunks
}

/// Build chunks for the entire audio. Uses VAD when possible, falls
/// back to fixed-size chunks if VAD fails or detects no speech.
fn build_chunks(app_handle: &AppHandle, samples: &[f32]) -> Vec<(usize, usize)> {
    // Try VAD-based segmentation to find better split points, while still
    // keeping chunk coverage continuous over the original audio timeline.
    match detect_speech_segments(app_handle, samples) {
        Ok(raw_segments) if !raw_segments.is_empty() => {
            info!("VAD detected {} speech regions", raw_segments.len());
            let split_points = preferred_split_points(&raw_segments);
            info!("Derived {} preferred split points", split_points.len());
            let chunks = chunks_from_split_points(samples.len(), &split_points);
            if !chunks.is_empty() {
                info!("Created {} VAD-guided chunks", chunks.len());
                return chunks;
            }
        }
        Ok(_) => {
            warn!("VAD detected no speech, falling back to fixed chunks");
        }
        Err(e) => {
            warn!("VAD failed ({}), falling back to fixed chunks", e);
        }
    }

    let chunks = fixed_size_chunks(samples.len());
    info!("Created {} fixed-size fallback chunks", chunks.len());
    chunks
}

// ── Transcription ────────────────────────────────────────────────

/// Transcribe audio, using VAD-aware chunking for long files.
///
/// Short audio (< CHUNK_THRESHOLD_SECS) is transcribed in a single call.
/// Long audio is split at silence boundaries (via Silero VAD), then each
/// chunk is transcribed separately via TranscriptionManager::transcribe().
///
/// This works with ALL upstream engine types without modifying
/// TranscriptionManager.
async fn transcribe_with_chunking(
    app_handle: &AppHandle,
    transcription_state: &Arc<TranscriptionManager>,
    cancellation_tokens: &ImportCancellationTokens,
    import_id: &str,
    samples: &[f32],
) -> Result<String, String> {
    let duration_secs = samples.len() as f64 / SAMPLE_RATE as f64;

    // Short audio: single-shot transcription
    if duration_secs <= CHUNK_THRESHOLD_SECS {
        info!(
            "Short audio ({:.1}s), transcribing in one shot",
            duration_secs
        );
        emit_progress(app_handle, "transcribing", 30, "Transcribing...");

        let tm = Arc::clone(transcription_state);
        let audio = samples.to_vec();
        return match tauri::async_runtime::spawn_blocking(move || tm.transcribe(audio)).await {
            Ok(Ok(text)) => Ok(text),
            Ok(Err(e)) => Err(format!("Transcription failed: {}", e)),
            Err(e) => Err(format!("Transcription task panicked: {}", e)),
        };
    }

    // Long audio: VAD-aware chunking
    emit_progress(
        app_handle,
        "transcribing",
        18,
        "Detecting speech segments...",
    );

    let chunks = build_chunks(app_handle, samples);
    let total_chunks = chunks.len();

    info!(
        "Transcribing {:.1}s audio in {} chunks",
        duration_secs, total_chunks
    );

    let mut results: Vec<String> = Vec::new();

    for (i, (start, end)) in chunks.iter().enumerate() {
        // Check cancellation between chunks
        check_cancelled(app_handle, cancellation_tokens, import_id)?;

        let chunk_duration = (*end - *start) as f32 / SAMPLE_RATE as f32;

        // Map chunk progress to 20-85% range
        let progress = 20.0 + (i as f32 / total_chunks as f32) * 65.0;

        emit_progress(
            app_handle,
            "transcribing",
            (progress as u8).min(85),
            &format!(
                "Transcribing chunk {}/{} ({:.0}s)...",
                i + 1,
                total_chunks,
                chunk_duration
            ),
        );

        let chunk_audio = samples[*start..*end].to_vec();
        let tm = Arc::clone(transcription_state);

        let chunk_text =
            match tauri::async_runtime::spawn_blocking(move || tm.transcribe(chunk_audio)).await {
                Ok(Ok(text)) => text,
                Ok(Err(e)) => {
                    warn!("Chunk {}/{} failed: {}, skipping", i + 1, total_chunks, e);
                    continue;
                }
                Err(e) => {
                    warn!("Chunk {}/{} panicked: {}, skipping", i + 1, total_chunks, e);
                    continue;
                }
            };

        if !chunk_text.trim().is_empty() {
            results.push(chunk_text.trim().to_string());
        }

        debug!(
            "Chunk {}/{} done ({:.0}s audio) → {} chars",
            i + 1,
            total_chunks,
            chunk_duration,
            chunk_text.len()
        );
    }

    if results.is_empty() {
        return Err("No speech detected in any chunk".to_string());
    }

    let combined = results.join(" ");
    info!(
        "Chunked transcription complete: {} chunks → {} chars",
        total_chunks,
        combined.len()
    );
    Ok(combined)
}

// ── Commands ─────────────────────────────────────────────────────

#[tauri::command]
#[specta::specta]
pub async fn import_audio_file(
    app_handle: AppHandle,
    transcription_state: State<'_, Arc<TranscriptionManager>>,
    _history_state: State<'_, Arc<HistoryManager>>,
    cancellation_tokens: State<'_, Arc<ImportCancellationTokens>>,
    file_path: String,
) -> Result<(), String> {
    use uuid::Uuid;

    let cancellation_tokens = Arc::clone(&*cancellation_tokens);
    if !cancellation_tokens.try_start() {
        return Err("An import is already in progress".to_string());
    }

    let import_id = Uuid::new_v4().to_string();
    let _cancel_token = cancellation_tokens.create_token(&import_id);
    let _import_guard = ActiveImportGuard::new(Arc::clone(&cancellation_tokens), import_id.clone());

    info!("Importing audio file: {} (ID: {})", file_path, import_id);
    emit_progress(&app_handle, "starting", 0, "Starting import...");

    let source_path = PathBuf::from(&file_path);

    validate_audio_file(&source_path).inspect_err(|error| {
        emit_progress(&app_handle, "failed", 0, error);
    })?;

    check_cancelled(&app_handle, cancellation_tokens.as_ref(), &import_id)?;

    emit_progress(&app_handle, "decoding", 5, "Decoding audio file...");
    let samples = decode_and_resample(&source_path).map_err(|e| {
        let message = format!("Failed to decode audio: {}", e);
        emit_progress(&app_handle, "failed", 0, &message);
        message
    })?;

    check_cancelled(&app_handle, cancellation_tokens.as_ref(), &import_id)?;

    let duration = samples.len() as f64 / SAMPLE_RATE as f64;
    debug!(
        "Audio duration: {:.2}s ({} samples)",
        duration,
        samples.len()
    );

    emit_progress(
        &app_handle,
        "transcribing",
        15,
        "Loading transcription model...",
    );
    transcription_state.initiate_model_load();

    let transcription_text = transcribe_with_chunking(
        &app_handle,
        &transcription_state,
        cancellation_tokens.as_ref(),
        &import_id,
        &samples,
    )
    .await
    .inspect_err(|error| {
        emit_progress(&app_handle, "failed", 0, error);
    })?;

    check_cancelled(&app_handle, cancellation_tokens.as_ref(), &import_id)?;

    emit_progress(
        &app_handle,
        "copying",
        90,
        "Copying transcript to clipboard...",
    );

    app_handle
        .clipboard()
        .write_text(transcription_text.clone())
        .map_err(|e| {
            let message = format!("Failed to copy transcript to clipboard: {}", e);
            emit_progress(&app_handle, "failed", 0, &message);
            message
        })?;

    let _ = app_handle.emit("transcription-completed", transcription_text);

    emit_progress(
        &app_handle,
        "completed",
        100,
        "Import completed and copied to clipboard",
    );
    info!("Import completed successfully ({:.1}s audio)", duration);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_import(
    cancellation_tokens: State<'_, Arc<ImportCancellationTokens>>,
    import_id: String,
) -> Result<(), String> {
    if import_id.is_empty() {
        cancellation_tokens.cancel_all();
        info!("All active imports cancelled by user");
    } else {
        cancellation_tokens.cancel(&import_id);
        info!("Import {} cancelled by user", import_id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Cancellation token tests ─────────────────────────────────

    #[test]
    fn cancellation_token_lifecycle() {
        let tokens = ImportCancellationTokens::new();

        let id = "test-import-1";
        let _token = tokens.create_token(id);

        assert!(
            !tokens.is_cancelled(id),
            "freshly created token should not be cancelled"
        );

        tokens.cancel(id);
        assert!(
            tokens.is_cancelled(id),
            "token should be cancelled after cancel()"
        );

        tokens.cleanup(id);
        assert!(
            !tokens.is_cancelled(id),
            "cleaned up token should return false"
        );
    }

    #[test]
    fn cancellation_tokens_independent() {
        let tokens = ImportCancellationTokens::new();

        tokens.create_token("a");
        tokens.create_token("b");

        tokens.cancel("a");
        assert!(tokens.is_cancelled("a"));
        assert!(
            !tokens.is_cancelled("b"),
            "cancelling 'a' should not affect 'b'"
        );
    }

    #[test]
    fn cancellation_cancel_all() {
        let tokens = ImportCancellationTokens::new();

        tokens.create_token("a");
        tokens.create_token("b");
        tokens.create_token("c");

        tokens.cancel_all();
        assert!(tokens.is_cancelled("a"));
        assert!(tokens.is_cancelled("b"));
        assert!(tokens.is_cancelled("c"));
    }

    #[test]
    fn concurrent_import_guard() {
        let tokens = ImportCancellationTokens::new();

        assert!(tokens.try_start(), "first import should succeed");
        assert!(!tokens.try_start(), "second import should be rejected");

        tokens.finish();
        assert!(tokens.try_start(), "import after finish should succeed");

        tokens.finish();
    }

    #[test]
    fn cancellation_nonexistent_id_is_safe() {
        let tokens = ImportCancellationTokens::new();
        assert!(!tokens.is_cancelled("does-not-exist"));
        tokens.cancel("does-not-exist"); // should not panic
        tokens.cleanup("does-not-exist"); // should not panic
    }

    // ── Chunking logic tests ─────────────────────────────────────

    #[test]
    fn preferred_split_points_empty() {
        let result = preferred_split_points(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn preferred_split_points_ignore_tiny_gaps() {
        let segs = vec![
            SpeechSegment {
                start: 10_000,
                end: 20_000,
            },
            SpeechSegment {
                start: 22_000,
                end: 30_000,
            },
        ];

        let result = preferred_split_points(&segs);
        assert!(result.is_empty());
    }

    #[test]
    fn preferred_split_points_use_gap_midpoint() {
        let segs = vec![
            SpeechSegment {
                start: 10_000,
                end: 20_000,
            },
            SpeechSegment {
                start: 28_000,
                end: 40_000,
            },
        ];

        let result = preferred_split_points(&segs);
        assert_eq!(result, vec![24_000]);
    }

    #[test]
    fn chunks_from_split_points_covers_entire_timeline() {
        let chunks = chunks_from_split_points(400_000, &[120_000, 260_000]);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], (0, 260_000));
        assert_eq!(chunks[1], (244_000, 400_000));
    }

    #[test]
    fn chunks_from_split_points_prefers_natural_boundaries() {
        let split_points = vec![210_000, 420_000];
        let chunks = chunks_from_split_points(700_000, &split_points);

        assert_eq!(
            chunks.len(),
            2,
            "should split on preferred boundary before forcing a hard cut"
        );
        assert_eq!(chunks[0].1, 420_000);
        assert_eq!(chunks[1].0, 404_000);
        assert_eq!(chunks[1].1, 700_000);
    }

    #[test]
    fn chunks_from_split_points_splits_large_ranges_with_overlap() {
        let chunks = chunks_from_split_points(1_440_000, &[]);

        assert!(
            chunks.len() >= 3,
            "90s audio should produce at least 3 chunks, got {}",
            chunks.len()
        );

        for (i, (start, end)) in chunks.iter().enumerate() {
            let len = end - start;
            assert!(
                len <= MAX_CHUNK_SAMPLES,
                "chunk {} has {} samples, exceeds max {}",
                i,
                len,
                MAX_CHUNK_SAMPLES
            );
        }

        assert_eq!(chunks[0].0, 0, "first chunk should start at 0");
        assert_eq!(
            chunks.last().unwrap().1,
            1_440_000,
            "last chunk should end at total"
        );

        for pair in chunks.windows(2) {
            let (_, end_a) = pair[0];
            let (start_b, _) = pair[1];
            assert!(
                start_b < end_a,
                "chunks should overlap: chunk ends at {} but next starts at {}",
                end_a,
                start_b
            );
        }
    }

    #[test]
    fn fixed_size_chunks_exact_max_size() {
        let chunks = fixed_size_chunks(MAX_CHUNK_SAMPLES);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], (0, MAX_CHUNK_SAMPLES));
    }

    // ── Decode and resample tests ────────────────────────────────

    #[test]
    fn decode_short_wav() {
        let path = PathBuf::from("test_fixtures/short_10s.wav");
        if !path.exists() {
            println!("Skipping: {:?} not found", path);
            return;
        }
        let samples = decode_and_resample(path).expect("should decode short wav");
        let duration = samples.len() as f64 / SAMPLE_RATE as f64;
        assert!(
            (duration - 10.0).abs() < 0.5,
            "expected ~10s, got {:.1}s",
            duration
        );
    }

    #[test]
    fn decode_tiny_wav() {
        let path = PathBuf::from("test_fixtures/tiny_1s.wav");
        if !path.exists() {
            println!("Skipping: {:?} not found", path);
            return;
        }
        let samples = decode_and_resample(path).expect("should decode tiny wav");
        let duration = samples.len() as f64 / SAMPLE_RATE as f64;
        assert!(
            (duration - 1.0).abs() < 0.2,
            "expected ~1s, got {:.1}s",
            duration
        );
    }

    #[test]
    fn decode_micro_wav() {
        let path = PathBuf::from("test_fixtures/micro_100ms.wav");
        if !path.exists() {
            println!("Skipping: {:?} not found", path);
            return;
        }
        let samples = decode_and_resample(path).expect("should decode micro wav");
        assert!(
            !samples.is_empty(),
            "even 100ms file should produce samples"
        );
    }

    #[test]
    fn decode_stereo_48k_resamples() {
        let path = PathBuf::from("test_fixtures/stereo_48k_5s.wav");
        if !path.exists() {
            println!("Skipping: {:?} not found", path);
            return;
        }
        let samples = decode_and_resample(path).expect("should decode+resample stereo 48kHz");
        let duration = samples.len() as f64 / SAMPLE_RATE as f64;
        // 48kHz stereo → 16kHz mono: duration should be preserved
        assert!(
            (duration - 5.0).abs() < 1.0,
            "expected ~5s, got {:.1}s",
            duration
        );
    }

    #[test]
    fn decode_long_wav() {
        let path = PathBuf::from("test_fixtures/long_90s.wav");
        if !path.exists() {
            println!("Skipping: {:?} not found", path);
            return;
        }
        let samples = decode_and_resample(path).expect("should decode 90s wav");
        let duration = samples.len() as f64 / SAMPLE_RATE as f64;
        assert!(
            (duration - 90.0).abs() < 1.0,
            "expected ~90s, got {:.1}s",
            duration
        );
    }

    #[test]
    fn decode_nonexistent_file_errors() {
        let result = decode_and_resample(PathBuf::from("/tmp/nonexistent_audio.wav"));
        assert!(result.is_err());
    }

    // ── File validation tests ────────────────────────────────────

    #[test]
    fn validate_nonexistent_file() {
        let result = validate_audio_file(std::path::Path::new("/tmp/no_such_file.wav"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("File not found"));
    }

    #[test]
    fn validate_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let result = validate_audio_file(tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));
    }

    #[test]
    fn validate_non_audio_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write;
        write!(tmp, "this is not audio data, just plain text").unwrap();
        let result = validate_audio_file(tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("valid audio"));
    }

    #[test]
    fn validate_real_wav() {
        let path = std::path::Path::new("test_fixtures/short_10s.wav");
        if !path.exists() {
            println!("Skipping: {:?} not found", path);
            return;
        }
        validate_audio_file(path).expect("valid wav should pass validation");
    }

    // ── Save WAV round-trip test ─────────────────────────────────

    #[test]
    fn save_and_reload_wav() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let path = tmp_dir.path().join("test_roundtrip.wav");

        // Generate samples
        let samples: Vec<f32> = (0..16000)
            .map(|i| (i as f32 / 16000.0 * 440.0 * 2.0 * std::f32::consts::PI).sin() * 0.5)
            .collect();

        // Save
        save_wav_file(&path, &samples).expect("save should succeed");
        assert!(path.exists());

        // Reload
        let loaded = decode_and_resample(path.to_path_buf()).expect("should decode saved wav");
        let dur = loaded.len() as f64 / SAMPLE_RATE as f64;
        assert!(
            (dur - 1.0).abs() < 0.2,
            "round-trip duration should be ~1s, got {:.2}s",
            dur
        );
    }

    // ── History schema tests ─────────────────────────────────────

    #[test]
    fn history_entry_has_new_fields() {
        use crate::managers::history::HistoryEntry;

        let entry = HistoryEntry {
            id: 1,
            file_name: "test.wav".to_string(),
            timestamp: 12345,
            saved: false,
            title: "Test".to_string(),
            transcription_text: "hello world".to_string(),
            post_processed_text: None,
            post_process_prompt: None,
            post_process_requested: false,
            duration: Some(42.5),
            source: Some("upload".to_string()),
        };

        assert_eq!(entry.duration, Some(42.5));
        assert_eq!(entry.source.as_deref(), Some("upload"));
    }

    // ── End-to-end pipeline test (no transcription) ──────────────

    #[test]
    fn full_pipeline_decode_chunk_save() {
        let input_path = PathBuf::from("test_fixtures/long_120s_gaps.wav");
        if !input_path.exists() {
            println!("Skipping: {:?} not found", input_path);
            return;
        }

        // 1. Decode
        let samples = decode_and_resample(input_path).expect("decode should succeed");
        let duration = samples.len() as f64 / SAMPLE_RATE as f64;
        assert!(
            (duration - 120.0).abs() < 1.0,
            "expected ~120s, got {:.1}s",
            duration
        );

        // 2. Build chunks (without VAD since we don't have AppHandle in tests)
        //    Test the segment logic directly
        let segments = vec![
            SpeechSegment {
                start: 0,
                end: 320_000,
            }, // 0-20s
            SpeechSegment {
                start: 330_000,
                end: 500_000,
            }, // 20.6-31.25s
            SpeechSegment {
                start: 800_000,
                end: 1_200_000,
            }, // 50-75s (25s)
            SpeechSegment {
                start: 1_500_000,
                end: 1_920_000,
            }, // 93.75-120s (26.25s)
        ];
        let split_points = preferred_split_points(&segments);
        let chunks = chunks_from_split_points(samples.len(), &split_points);

        // Verify all chunks are within bounds
        for (i, (start, end)) in chunks.iter().enumerate() {
            assert!(
                *end <= samples.len(),
                "chunk {} end {} exceeds total {}",
                i,
                end,
                samples.len()
            );
            assert!(
                *start < *end,
                "chunk {} has invalid range {}-{}",
                i,
                start,
                end
            );
            assert!(
                *end - *start <= MAX_CHUNK_SAMPLES,
                "chunk {} too large: {} samples",
                i,
                end - start
            );
        }

        // 3. Save
        let tmp_dir = tempfile::tempdir().unwrap();
        let out_path = tmp_dir.path().join("imported.wav");
        save_wav_file(&out_path, &samples).expect("save should succeed");

        let file_size = fs::metadata(&out_path).unwrap().len();
        assert!(
            file_size > 100_000,
            "saved file should be substantial, got {} bytes",
            file_size
        );

        println!(
            "Pipeline test passed: {:.1}s audio → {} chunks → {} byte WAV",
            duration,
            chunks.len(),
            file_size
        );
    }
}
