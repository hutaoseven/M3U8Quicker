use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::error::AppError;
use crate::state::AppState;

const SUBTITLE_TRACK_HEIGHT_RATIO_NUMERATOR: u32 = 22;
const SUBTITLE_TRACK_HEIGHT_RATIO_DENOMINATOR: u32 = 100;
const MIN_SUBTITLE_TRACK_HEIGHT: u32 = 128;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SubtitleCue {
    start_ms: u64,
    end_ms: u64,
    text: String,
}

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

pub async fn convert_multi_track_hls_to_mp4(
    ffmpeg_path: &Path,
    video_playlist: &Path,
    audio_playlist: Option<&Path>,
    subtitle_playlist: Option<&Path>,
    mp4_path: &Path,
) -> Result<(), AppError> {
    let temp_dir = subtitle_playlist
        .map(|_| std::env::temp_dir().join(format!("m3u8quicker_subtitles_{}", uuid::Uuid::new_v4())));
    let subtitle_input_path = if let (Some(subtitle_playlist), Some(temp_dir)) =
        (subtitle_playlist, temp_dir.as_ref())
    {
        tokio::fs::create_dir_all(temp_dir).await?;
        let subtitle_srt_path = temp_dir.join("subtitle.srt");
        export_hls_subtitle_playlist_to_srt(subtitle_playlist, &subtitle_srt_path).await?;
        Some(subtitle_srt_path)
    } else {
        None
    };

    let subtitle_dimensions = if subtitle_input_path.is_some() {
        probe_video_dimensions(ffmpeg_path, video_playlist)
            .await
            .map(calculate_subtitle_track_size)
    } else {
        None
    };
    let args = build_multi_track_hls_to_mp4_args(
        video_playlist,
        audio_playlist,
        subtitle_input_path.as_deref(),
        subtitle_dimensions,
        mp4_path,
    );
    let result = run_ffmpeg_command(ffmpeg_path, &args).await;

    if let Some(temp_dir) = temp_dir {
        let _ = tokio::fs::remove_dir_all(temp_dir).await;
    }

    result
}

fn build_multi_track_hls_to_mp4_args(
    video_playlist: &Path,
    audio_playlist: Option<&Path>,
    subtitle_playlist: Option<&Path>,
    subtitle_dimensions: Option<(u32, u32)>,
    mp4_path: &Path,
) -> Vec<String> {
    let mut args = vec!["-y".to_string(), "-hide_banner".to_string()];
    for input in [Some(video_playlist), audio_playlist].into_iter().flatten() {
        args.push("-allowed_extensions".to_string());
        args.push("ALL".to_string());
        args.push("-i".to_string());
        args.push(input.to_string_lossy().into_owned());
    }
    if let Some(subtitle_playlist) = subtitle_playlist {
        args.push("-i".to_string());
        args.push(subtitle_playlist.to_string_lossy().into_owned());
    }

    args.push("-map".to_string());
    args.push("0:v:0".to_string());

    let mut next_input_index = 1usize;
    if audio_playlist.is_some() {
        args.push("-map".to_string());
        args.push(format!("{}:a:0", next_input_index));
        next_input_index += 1;
    }
    if subtitle_playlist.is_some() {
        args.push("-map".to_string());
        args.push(format!("{}:s:0", next_input_index));
    }

    args.extend([
        "-c:v".to_string(),
        "copy".to_string(),
        "-c:a".to_string(),
        "copy".to_string(),
        "-c:s".to_string(),
        "mov_text".to_string(),
    ]);
    if let Some((subtitle_width, subtitle_height)) = subtitle_dimensions {
        args.push("-s:s:0".to_string());
        args.push(format!("{}x{}", subtitle_width, subtitle_height));
        args.push("-height:s:0".to_string());
        args.push(subtitle_height.to_string());
    }
    args.extend([
        "-movflags".to_string(),
        "+faststart".to_string(),
        mp4_path.to_string_lossy().into_owned(),
    ]);

    args
}

async fn run_ffmpeg_command(ffmpeg_path: &Path, args: &[String]) -> Result<(), AppError> {
    run_ffmpeg_command_in_dir(ffmpeg_path, args, None).await
}

async fn run_ffmpeg_command_in_dir(
    ffmpeg_path: &Path,
    args: &[String],
    current_dir: Option<&Path>,
) -> Result<(), AppError> {
    let mut command = tokio::process::Command::new(ffmpeg_path);
    command
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null());
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }

    let output = command
        .output()
        .await
        .map_err(|e| AppError::Conversion(format!("启动 FFmpeg 失败: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr
            .lines()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" | ");
        let detail = if tail.trim().is_empty() {
            format!("FFmpeg 退出码 {}", output.status)
        } else {
            tail
        };
        return Err(AppError::Conversion(format!("FFmpeg 处理失败: {}", detail)));
    }

    Ok(())
}

fn ffprobe_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "ffprobe.exe"
    } else {
        "ffprobe"
    }
}

async fn probe_video_dimensions(ffmpeg_path: &Path, video_playlist: &Path) -> Option<(u32, u32)> {
    let sibling_ffprobe = ffmpeg_path.with_file_name(ffprobe_binary_name());
    if let Some(dimensions) = run_ffprobe_dimensions(sibling_ffprobe.as_os_str(), video_playlist).await
    {
        return Some(dimensions);
    }

    run_ffprobe_dimensions(OsStr::new("ffprobe"), video_playlist).await
}

async fn run_ffprobe_dimensions(
    ffprobe_command: &OsStr,
    video_playlist: &Path,
) -> Option<(u32, u32)> {
    let output = tokio::process::Command::new(ffprobe_command)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0:s=x",
        ])
        .arg(video_playlist)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    parse_video_dimensions(String::from_utf8_lossy(&output.stdout).trim())
}

async fn export_hls_subtitle_playlist_to_srt(
    subtitle_playlist: &Path,
    subtitle_srt_path: &Path,
) -> Result<(), AppError> {
    let cues = collect_hls_subtitle_cues(subtitle_playlist).await?;
    if cues.is_empty() {
        return Err(AppError::Conversion(
            "字幕内容为空，无法生成修正后的字幕文件".to_string(),
        ));
    }

    tokio::fs::write(subtitle_srt_path, render_srt_content(&cues)).await?;
    Ok(())
}

async fn collect_hls_subtitle_cues(subtitle_playlist: &Path) -> Result<Vec<SubtitleCue>, AppError> {
    let playlist_content = tokio::fs::read(subtitle_playlist).await?;
    let playlist = m3u8_rs::parse_playlist_res(&playlist_content).map_err(|_| {
        AppError::InvalidInput("字幕播放列表格式无效，无法重建字幕时间轴".to_string())
    })?;
    let m3u8_rs::Playlist::MediaPlaylist(media_playlist) = playlist else {
        return Err(AppError::InvalidInput(
            "字幕播放列表不是有效的媒体列表，无法重建字幕时间轴".to_string(),
        ));
    };
    let parent_dir = subtitle_playlist.parent().ok_or_else(|| {
        AppError::InvalidInput("字幕播放列表路径无效，无法重建字幕时间轴".to_string())
    })?;

    let mut cues = Vec::new();
    let mut leading_empty_duration_ms = 0u64;
    let mut encountered_non_empty_segment = false;
    for segment in media_playlist.segments {
        let segment_path = parent_dir.join(segment.uri);
        let segment_content = tokio::fs::read_to_string(&segment_path).await?;
        let segment_cues = parse_webvtt_cues(&segment_content);
        if !encountered_non_empty_segment && segment_cues.is_empty() {
            leading_empty_duration_ms += (segment.duration * 1000.0).round() as u64;
        } else {
            encountered_non_empty_segment = true;
        }
        cues.extend(segment_cues);
    }

    Ok(normalize_subtitle_cues(apply_leading_empty_offset(
        cues,
        leading_empty_duration_ms,
    )))
}

fn parse_webvtt_cues(content: &str) -> Vec<SubtitleCue> {
    let normalized = content.replace("\r\n", "\n");
    let mut cues = Vec::new();

    for raw_block in normalized.split("\n\n") {
        let block = raw_block.trim();
        if block.is_empty() || block.eq_ignore_ascii_case("WEBVTT") {
            continue;
        }

        let lines = block.lines().collect::<Vec<_>>();
        let timestamp_index = lines.iter().position(|line| line.contains("-->"));
        let Some(timestamp_index) = timestamp_index else {
            continue;
        };

        let Some((start_ms, end_ms)) = parse_webvtt_time_range(lines[timestamp_index]) else {
            continue;
        };
        let text = lines
            .iter()
            .skip(timestamp_index + 1)
            .copied()
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        let normalized_text = normalize_subtitle_text(&text);
        if normalized_text.is_empty() || normalized_text == "WEBVTT" {
            continue;
        }

        cues.push(SubtitleCue {
            start_ms,
            end_ms,
            text,
        });
    }

    cues
}

fn parse_webvtt_time_range(line: &str) -> Option<(u64, u64)> {
    let (start, end) = line.split_once("-->")?;
    let start_ms = parse_webvtt_timestamp(start.trim())?;
    let end_ms = parse_webvtt_timestamp(end.split_whitespace().next()?.trim())?;
    Some((start_ms, end_ms))
}

fn parse_webvtt_timestamp(raw: &str) -> Option<u64> {
    let parts = raw.trim().split(':').collect::<Vec<_>>();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] => (0u64, minutes.parse::<u64>().ok()?, *seconds),
        [hours, minutes, seconds] => (
            hours.parse::<u64>().ok()?,
            minutes.parse::<u64>().ok()?,
            *seconds,
        ),
        _ => return None,
    };
    let (seconds, millis) = seconds.split_once('.')?;
    let seconds = seconds.parse::<u64>().ok()?;
    let millis = millis.parse::<u64>().ok()?;

    Some((((hours * 60 + minutes) * 60) + seconds) * 1000 + millis)
}

fn normalize_subtitle_cues(mut cues: Vec<SubtitleCue>) -> Vec<SubtitleCue> {
    cues.sort_by(|left, right| {
        left.start_ms
            .cmp(&right.start_ms)
            .then(left.end_ms.cmp(&right.end_ms))
            .then(left.text.cmp(&right.text))
    });

    let mut seen = HashSet::new();
    cues.retain(|cue| {
        seen.insert((
            cue.start_ms,
            cue.end_ms,
            normalize_subtitle_text(&cue.text),
        ))
    });
    cues
}

fn apply_leading_empty_offset(mut cues: Vec<SubtitleCue>, leading_empty_duration_ms: u64) -> Vec<SubtitleCue> {
    let Some(first_start_ms) = cues.first().map(|cue| cue.start_ms) else {
        return cues;
    };

    if leading_empty_duration_ms == 0 {
        return cues;
    }

    // Some segmented WebVTT sources keep a blank intro segment outside the cue timeline.
    // When the first real cue starts well before that blank duration, shift the whole
    // subtitle timeline forward by the blank intro duration.
    if first_start_ms + 250 >= leading_empty_duration_ms {
        return cues;
    }

    for cue in &mut cues {
        cue.start_ms = cue.start_ms.saturating_add(leading_empty_duration_ms);
        cue.end_ms = cue.end_ms.saturating_add(leading_empty_duration_ms);
    }

    cues
}

fn normalize_subtitle_text(text: &str) -> String {
    text.replace(['\u{feff}', '\u{a0}'], "").trim().to_string()
}

fn render_srt_content(cues: &[SubtitleCue]) -> String {
    let mut output = String::new();

    for (index, cue) in cues.iter().enumerate() {
        output.push_str(&(index + 1).to_string());
        output.push('\n');
        output.push_str(&format!(
            "{} --> {}\n",
            format_srt_timestamp(cue.start_ms),
            format_srt_timestamp(cue.end_ms)
        ));
        output.push_str(cue.text.trim_end());
        output.push_str("\n\n");
    }

    output
}

fn format_srt_timestamp(total_ms: u64) -> String {
    let hours = total_ms / 3_600_000;
    let minutes = (total_ms % 3_600_000) / 60_000;
    let seconds = (total_ms % 60_000) / 1_000;
    let millis = total_ms % 1_000;

    format!("{:02}:{:02}:{:02},{:03}", hours, minutes, seconds, millis)
}

fn parse_video_dimensions(raw: &str) -> Option<(u32, u32)> {
    let (width, height) = raw.trim().split_once('x')?;
    let width = width.trim().parse::<u32>().ok()?;
    let height = height.trim().parse::<u32>().ok()?;
    if width == 0 || height == 0 {
        return None;
    }

    Some((width, height))
}

fn calculate_subtitle_track_size((video_width, video_height): (u32, u32)) -> (u32, u32) {
    let scaled_height =
        video_height.saturating_mul(SUBTITLE_TRACK_HEIGHT_RATIO_NUMERATOR)
            / SUBTITLE_TRACK_HEIGHT_RATIO_DENOMINATOR;
    let subtitle_height = scaled_height.max(MIN_SUBTITLE_TRACK_HEIGHT).min(video_height);
    (video_width, subtitle_height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn build_multi_track_hls_to_mp4_args_maps_audio_and_subtitle_inputs() {
        let args = build_multi_track_hls_to_mp4_args(
            &PathBuf::from("video/index.m3u8"),
            Some(Path::new("audio/index.m3u8")),
            Some(Path::new("subtitle/subtitle.srt")),
            Some((1920, 238)),
            &PathBuf::from("output.mp4"),
        );

        assert!(args.windows(2).any(|window| window == ["-map", "0:v:0"]));
        assert!(args.windows(2).any(|window| window == ["-map", "1:a:0"]));
        assert!(args.windows(2).any(|window| window == ["-map", "2:s:0"]));
        assert!(args.windows(2).any(|window| window == ["-c:s", "mov_text"]));
        assert!(args.windows(2).any(|window| window == ["-i", "subtitle/subtitle.srt"]));
        assert!(args.windows(2).any(|window| window == ["-s:s:0", "1920x238"]));
        assert!(args.windows(2).any(|window| window == ["-height:s:0", "238"]));
    }

    #[test]
    fn build_multi_track_hls_to_mp4_args_skips_missing_audio_input() {
        let args = build_multi_track_hls_to_mp4_args(
            &PathBuf::from("video/index.m3u8"),
            None,
            Some(Path::new("subtitle/subtitle.srt")),
            Some((1280, 128)),
            &PathBuf::from("output.mp4"),
        );

        assert!(args.windows(2).any(|window| window == ["-map", "0:v:0"]));
        assert!(args.windows(2).any(|window| window == ["-map", "1:s:0"]));
        assert!(!args.windows(2).any(|window| window == ["-map", "1:a:0"]));
        assert!(args.windows(2).any(|window| window == ["-s:s:0", "1280x128"]));
    }

    #[test]
    fn parse_webvtt_cues_reads_timestamped_text_blocks() {
        let cues = parse_webvtt_cues(
            "WEBVTT\n\n00:00:03.700 --> 00:00:05.068\nBut guess what?\n",
        );

        assert_eq!(
            cues,
            vec![SubtitleCue {
                start_ms: 3_700,
                end_ms: 5_068,
                text: "But guess what?".to_string(),
            }]
        );
    }

    #[test]
    fn normalize_subtitle_cues_deduplicates_overlap_segments() {
        let cues = normalize_subtitle_cues(vec![
            SubtitleCue {
                start_ms: 28_992,
                end_ms: 32_829,
                text: "but I've applied\nfor about roughly three decades".to_string(),
            },
            SubtitleCue {
                start_ms: 697,
                end_ms: 3_667,
                text: "I am billed as the world's\ngreatest mind reader.".to_string(),
            },
            SubtitleCue {
                start_ms: 28_992,
                end_ms: 32_829,
                text: "but I've applied\nfor about roughly three decades".to_string(),
            },
        ]);

        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ms, 697);
        assert_eq!(cues[1].start_ms, 28_992);
    }

    #[test]
    fn render_srt_content_formats_cues_in_order() {
        let srt = render_srt_content(&[
            SubtitleCue {
                start_ms: 697,
                end_ms: 3_667,
                text: "I am billed as the world's\ngreatest mind reader.".to_string(),
            },
            SubtitleCue {
                start_ms: 3_700,
                end_ms: 5_068,
                text: "But guess what?".to_string(),
            },
        ]);

        assert_eq!(
            srt,
            "1\n00:00:00,697 --> 00:00:03,667\nI am billed as the world's\ngreatest mind reader.\n\n2\n00:00:03,700 --> 00:00:05,068\nBut guess what?\n\n"
        );
    }

    #[test]
    fn apply_leading_empty_offset_shifts_timeline_forward() {
        let cues = apply_leading_empty_offset(
            vec![
                SubtitleCue {
                    start_ms: 697,
                    end_ms: 3_667,
                    text: "I am billed as the world's\ngreatest mind reader.".to_string(),
                },
                SubtitleCue {
                    start_ms: 28_992,
                    end_ms: 32_829,
                    text: "but I've applied\nfor about roughly three decades".to_string(),
                },
            ],
            3_504,
        );

        assert_eq!(cues[0].start_ms, 4_201);
        assert_eq!(cues[1].start_ms, 32_496);
    }

    #[test]
    fn apply_leading_empty_offset_skips_when_shift_would_be_redundant() {
        let cues = apply_leading_empty_offset(
            vec![SubtitleCue {
                start_ms: 4_000,
                end_ms: 5_000,
                text: "Hello".to_string(),
            }],
            3_504,
        );

        assert_eq!(cues[0].start_ms, 4_000);
    }

    #[test]
    fn parse_video_dimensions_supports_ffprobe_csv_output() {
        assert_eq!(parse_video_dimensions("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_video_dimensions(" 1280x720 "), Some((1280, 720)));
        assert_eq!(parse_video_dimensions(""), None);
    }

    #[test]
    fn calculate_subtitle_track_size_uses_larger_box_height() {
        assert_eq!(calculate_subtitle_track_size((1920, 1080)), (1920, 237));
        assert_eq!(calculate_subtitle_track_size((1280, 360)), (1280, 128));
    }

}
