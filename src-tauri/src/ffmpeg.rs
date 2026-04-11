use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::error::AppError;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FfmpegStatus {
    NotInstalled,
    Installed { path: String, version: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct FfmpegDownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub stage: FfmpegDownloadStage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FfmpegDownloadStage {
    Downloading,
    Unpacking,
    Done,
}

fn ffmpeg_dir(app_handle: &AppHandle) -> PathBuf {
    app_handle
        .path()
        .app_data_dir()
        .expect("Failed to get app data dir")
        .join("ffmpeg")
}

fn ffmpeg_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    }
}

fn ffmpeg_binary_path(app_handle: &AppHandle) -> PathBuf {
    ffmpeg_dir(app_handle).join(ffmpeg_binary_name())
}

async fn probe_ffmpeg_version(path: &Path) -> Option<String> {
    let output = tokio::process::Command::new(path)
        .arg("-version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse "ffmpeg version N.N.N ..." from first line
    stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("ffmpeg version "))
        .map(|rest| {
            rest.split_whitespace()
                .next()
                .unwrap_or("unknown")
                .to_string()
        })
}

async fn detect_at_path(path: &Path) -> Option<FfmpegStatus> {
    if !path.exists() {
        return None;
    }
    let version = probe_ffmpeg_version(path).await?;
    Some(FfmpegStatus::Installed {
        path: path.to_string_lossy().into_owned(),
        version,
    })
}

async fn detect_system_ffmpeg() -> Option<FfmpegStatus> {
    let output = tokio::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = stdout
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("ffmpeg version "))
        .map(|rest| {
            rest.split_whitespace()
                .next()
                .unwrap_or("unknown")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".to_string());

    // Resolve actual path via `where` (Windows) or `which` (Unix)
    let which_cmd = if cfg!(target_os = "windows") {
        "where"
    } else {
        "which"
    };
    let resolved_path = tokio::process::Command::new(which_cmd)
        .arg("ffmpeg")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "ffmpeg".to_string());

    Some(FfmpegStatus::Installed {
        path: resolved_path,
        version,
    })
}

/// Detect ffmpeg: 1) user custom path → 2) app data dir → 3) system PATH
pub async fn detect_ffmpeg(app_handle: &AppHandle) -> FfmpegStatus {
    // 1. User-specified custom path
    let custom_path = app_handle
        .state::<AppState>()
        .ffmpeg_path
        .lock()
        .await
        .clone();
    if let Some(ref custom) = custom_path {
        let path = Path::new(custom);
        if let Some(status) = detect_at_path(path).await {
            return status;
        }
    }

    // 2. App data dir managed copy
    let managed_path = ffmpeg_binary_path(app_handle);
    if let Some(status) = detect_at_path(&managed_path).await {
        return status;
    }

    // 3. System PATH
    if let Some(status) = detect_system_ffmpeg().await {
        return status;
    }

    FfmpegStatus::NotInstalled
}

/// Resolve the ffmpeg binary path if available (for use by conversion fallback).
pub async fn resolve_ffmpeg_path(app_handle: &AppHandle) -> Option<PathBuf> {
    match detect_ffmpeg(app_handle).await {
        FfmpegStatus::Installed { path, .. } => Some(PathBuf::from(path)),
        FfmpegStatus::NotInstalled => None,
    }
}

/// Download ffmpeg to app data dir using ffmpeg-sidecar, emitting progress events.
pub async fn download_ffmpeg(app_handle: AppHandle) -> Result<PathBuf, AppError> {
    let dest_dir = ffmpeg_dir(&app_handle);

    let download_url = ffmpeg_sidecar::download::ffmpeg_download_url()
        .map_err(|e| AppError::Internal(format!("Failed to get ffmpeg download URL: {}", e)))?;

    let app_handle_progress = app_handle.clone();

    // Download and unpack in a blocking task since ffmpeg-sidecar's API is synchronous
    let final_path = tokio::task::spawn_blocking(move || -> Result<PathBuf, AppError> {
        std::fs::create_dir_all(&dest_dir)?;

        let archive_path = ffmpeg_sidecar::download::download_ffmpeg_package_with_progress(
            &download_url,
            &dest_dir,
            |event| {
                use ffmpeg_sidecar::download::FfmpegDownloadProgressEvent as P;
                let progress = match event {
                    P::Starting => FfmpegDownloadProgress {
                        downloaded_bytes: 0,
                        total_bytes: 0,
                        stage: FfmpegDownloadStage::Downloading,
                    },
                    P::Downloading {
                        total_bytes,
                        downloaded_bytes,
                    } => FfmpegDownloadProgress {
                        downloaded_bytes,
                        total_bytes,
                        stage: FfmpegDownloadStage::Downloading,
                    },
                    P::UnpackingArchive => FfmpegDownloadProgress {
                        downloaded_bytes: 0,
                        total_bytes: 0,
                        stage: FfmpegDownloadStage::Unpacking,
                    },
                    P::Done => FfmpegDownloadProgress {
                        downloaded_bytes: 0,
                        total_bytes: 0,
                        stage: FfmpegDownloadStage::Done,
                    },
                };
                let _ = app_handle_progress.emit("ffmpeg-download-progress", &progress);
            },
        )
        .map_err(|e| AppError::Internal(format!("Failed to download ffmpeg: {}", e)))?;

        ffmpeg_sidecar::download::unpack_ffmpeg(&archive_path, &dest_dir)
            .map_err(|e| AppError::Internal(format!("Failed to unpack ffmpeg: {}", e)))?;

        // Clean up the archive
        let _ = std::fs::remove_file(&archive_path);

        let binary_path = dest_dir.join(ffmpeg_binary_name());
        if !binary_path.exists() {
            return Err(AppError::Internal(
                "ffmpeg binary not found after unpacking".to_string(),
            ));
        }

        Ok(binary_path)
    })
    .await
    .map_err(|e| AppError::Internal(format!("Download task join error: {}", e)))??;

    let _ = app_handle.emit(
        "ffmpeg-download-progress",
        &FfmpegDownloadProgress {
            downloaded_bytes: 0,
            total_bytes: 0,
            stage: FfmpegDownloadStage::Done,
        },
    );

    Ok(final_path)
}

/// Convert TS to MP4 using ffmpeg: stream-copy with faststart.
pub async fn convert_ts_to_mp4(
    ffmpeg_path: &Path,
    ts_path: &Path,
    mp4_path: &Path,
) -> Result<(), AppError> {
    let output = tokio::process::Command::new(ffmpeg_path)
        .args(["-y", "-i"])
        .arg(ts_path)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(mp4_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| AppError::Conversion(format!("Failed to run ffmpeg: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr.lines().rev().take(5).collect::<Vec<_>>().join("\n");
        return Err(AppError::Conversion(format!("ffmpeg exited with {}: {}", output.status, tail)));
    }

    Ok(())
}
