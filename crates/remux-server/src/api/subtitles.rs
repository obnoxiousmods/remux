use anyhow::anyhow;
use axum::{
    body::Body,
    extract::{Path, State},
    response::IntoResponse,
};
use axum_anyhow::ApiResult as Result;
use http::{Response, StatusCode};
use remux_macros::get;
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex, OnceLock},
};
use tokio::sync::{Semaphore, watch};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::{
    AppState, IntoApiError, OptionExt, ResultExt, api, db, db::auth,
    keyed_lock::KeyedLock,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum SubtitleArtifactKind {
    NormalizedText,
    RawAss,
    RawBinary,
}

type SubtitleArtifactKey = (Uuid, Uuid, i64, SubtitleArtifactKind);

static SUBTITLE_ARTIFACT_LOCKS: KeyedLock<SubtitleArtifactKey> = KeyedLock::new();
static SUBTITLE_EXTRACTION_CAPACITY: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(2));
static SUBTITLE_RECENT_FAILURES: LazyLock<
    Mutex<HashMap<SubtitleArtifactKey, std::time::Instant>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));
const SUBTITLE_FAILURE_COOLDOWN: std::time::Duration =
    std::time::Duration::from_secs(5);
const SUBTITLE_EXTRACTION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(900);

fn subtitle_failure_is_cooling_down(key: &SubtitleArtifactKey) -> bool {
    let mut failures = SUBTITLE_RECENT_FAILURES
        .lock()
        .unwrap();
    failures.retain(|_, failed_at| failed_at.elapsed() < SUBTITLE_FAILURE_COOLDOWN);
    failures.contains_key(key)
}

fn subtitle_error_is_transient(error: &anyhow::Error) -> bool {
    let detail = error.to_string();
    detail.contains("429 Too Many Requests")
        || detail.contains("Server returned 429")
        || detail.contains("timed out")
        || detail.contains("Stream ends prematurely")
        || detail.contains("Input/output error")
        || detail.contains("Read error")
}

fn record_transient_subtitle_failure(key: SubtitleArtifactKey, error: &anyhow::Error) {
    if subtitle_error_is_transient(error) {
        SUBTITLE_RECENT_FAILURES
            .lock()
            .unwrap()
            .insert(key, std::time::Instant::now());
    }
}

fn clear_subtitle_failure(key: &SubtitleArtifactKey) {
    SUBTITLE_RECENT_FAILURES
        .lock()
        .unwrap()
        .remove(key);
}

fn ffmpeg_bin() -> String {
    std::env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into())
}

const SUBTITLE_HTTP_INPUT_OPTIONS: &[&str] = &[
    "-reconnect",
    "1",
    "-reconnect_streamed",
    "1",
    "-reconnect_delay_max",
    "5",
    "-reconnect_on_http_error",
    "429,500,502,503,504",
    "-timeout",
    "30000000",
    "-rw_timeout",
    "30000000",
];

fn subtitle_http_input_options(input: &str) -> &'static [&'static str] {
    let is_http = url::Url::parse(input)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https"));
    if is_http {
        SUBTITLE_HTTP_INPUT_OPTIONS
    } else {
        &[]
    }
}

/// Tracks in-progress batch subtitle extractions. Subtitle endpoint waits on these
/// instead of launching a competing on-demand FFmpeg process.
type SubtitleExtractionKey = (Uuid, Uuid);

static BATCH_EXTRACTING: OnceLock<
    Mutex<HashMap<SubtitleExtractionKey, watch::Receiver<bool>>>,
> = OnceLock::new();
static BATCH_CANCELLATIONS: OnceLock<
    Mutex<HashMap<SubtitleExtractionKey, watch::Sender<bool>>>,
> = OnceLock::new();
const SUBTITLE_PREFETCH_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

fn batch_extraction_map()
-> &'static Mutex<HashMap<SubtitleExtractionKey, watch::Receiver<bool>>> {
    BATCH_EXTRACTING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn batch_cancellation_map()
-> &'static Mutex<HashMap<SubtitleExtractionKey, watch::Sender<bool>>> {
    BATCH_CANCELLATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Schedule speculative subtitle cache warming behind a short playback-start
/// grace period. HLS startup cancels this work for the item before opening its
/// upstream source, so background FFmpeg never competes with first frame.
pub(crate) fn schedule_subtitle_pre_extraction(
    data_dir: std::path::PathBuf,
    input_url: String,
    item_id: Uuid,
    cache_source_id: Uuid,
    subtitle_streams: Vec<SubtitleExtractionPlan>,
) {
    let extraction_key = (item_id, cache_source_id);
    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    {
        let mut cancellations = batch_cancellation_map()
            .lock()
            .unwrap();
        if cancellations.contains_key(&extraction_key) {
            debug!(%item_id, %cache_source_id, "subtitle pre-extraction already scheduled");
            return;
        }
        cancellations.insert(extraction_key, cancel_tx);
    }

    tokio::spawn(async move {
        let cancelled = tokio::select! {
            () = tokio::time::sleep(SUBTITLE_PREFETCH_GRACE) => false,
            changed = cancel_rx.changed() => changed.is_ok() && *cancel_rx.borrow(),
        };
        if !cancelled {
            pre_extract_all_subtitles_to_cache(
                data_dir,
                input_url,
                item_id,
                cache_source_id,
                subtitle_streams,
                cancel_rx,
            )
            .await;
        } else {
            debug!(%item_id, %cache_source_id, "subtitle pre-extraction cancelled during playback-start grace");
        }
        batch_cancellation_map()
            .lock()
            .unwrap()
            .remove(&extraction_key);
    });
}

/// Cancel every speculative subtitle extraction for an item. The selected
/// subtitle endpoint remains on-demand and is unaffected.
pub(crate) fn cancel_subtitle_pre_extraction(item_id: Uuid) {
    let cancellations = batch_cancellation_map()
        .lock()
        .unwrap()
        .iter()
        .filter(|((candidate_item_id, _), _)| *candidate_item_id == item_id)
        .map(|(_, tx)| tx.clone())
        .collect::<Vec<_>>();
    for cancellation in cancellations {
        let _ = cancellation.send(true);
    }
}

pub(crate) fn subtitle_cache_source_id(
    media: &db::Media,
    probed_size: Option<i64>,
) -> Uuid {
    let Some(stream_info) = media
        .stream_info
        .as_ref()
    else {
        return media.id;
    };
    let normalized_filename = stream_info
        .filename
        .as_deref()
        .map(|filename| {
            filename
                .replace('\\', "/")
                .to_ascii_lowercase()
        });
    let size = probed_size.or(stream_info.size);
    let identity = match &stream_info.descriptor {
        crate::stream::StreamDescriptor::Torrent {
            info_hash,
            file_hint,
            file_idx,
            ..
        } => Some(format!(
            "torrent:{}:{}:{}",
            info_hash.to_ascii_lowercase(),
            file_idx.map_or_else(String::new, |index| index.to_string()),
            normalized_filename
                .as_deref()
                .or(file_hint.as_deref())
                .unwrap_or_default()
        )),
        crate::stream::StreamDescriptor::Http { .. } => normalized_filename
            .zip(size)
            .map(|(filename, size)| format!("http:{filename}:{size}")),
        crate::stream::StreamDescriptor::Local(path) => {
            Some(format!("local:{}", path.to_string_lossy()))
        }
        crate::stream::StreamDescriptor::Opendal { addon_id, path } => {
            Some(format!("opendal:{addon_id}:{path}"))
        }
        crate::stream::StreamDescriptor::Rtsp { .. } => None,
    };

    identity
        .map(|identity| Uuid::new_v5(&Uuid::NAMESPACE_URL, identity.as_bytes()))
        .unwrap_or(media.id)
}

fn is_valid_ass_document(bytes: &[u8]) -> bool {
    let content = String::from_utf8_lossy(bytes);
    content.contains("[Script Info]")
        && content.contains("[Events]")
        && content
            .lines()
            .any(|line| {
                line.trim_start()
                    .starts_with("Dialogue:")
            })
}

fn is_usable_srt_document(bytes: &[u8]) -> bool {
    let content = String::from_utf8_lossy(bytes);
    content
        .lines()
        .any(|line| line.contains(" --> "))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SubtitleExtractionPlan {
    pub stream_index: i64,
    pub preserve_raw_ass: bool,
    pub binary_format: Option<&'static str>,
}

pub(crate) fn subtitle_extraction_plan(
    stream_index: i64,
    codec: Option<&str>,
) -> SubtitleExtractionPlan {
    let normalized_codec = codec
        .map(str::trim)
        .map(str::to_ascii_lowercase);
    let preserve_raw_ass = normalized_codec
        .as_deref()
        .is_some_and(|codec| matches!(codec, "ass" | "ssa"));
    let binary_format = normalized_codec
        .as_deref()
        .filter(|codec| matches!(*codec, "pgssub" | "sup" | "hdmv_pgs_subtitle"))
        .map(|_| "sup");
    SubtitleExtractionPlan {
        stream_index,
        preserve_raw_ass,
        binary_format,
    }
}

#[cfg(test)]
mod local_tests {
    use super::{
        is_usable_srt_document, is_valid_ass_document, subtitle_cache_source_id,
        subtitle_extraction_error_response, subtitle_extraction_plan,
        subtitle_http_input_options,
    };
    use crate::{db, stream};
    use uuid::Uuid;

    fn http_stream_media(id: Uuid, url: &str) -> db::Media {
        db::Media {
            id,
            stream_info: Some(stream::StreamInfo {
                descriptor: stream::StreamDescriptor::http(url),
                filename: Some("S01E03-Kingdom of Lies.mkv".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn ass_validation_requires_dialogue_events() {
        assert!(!is_valid_ass_document(
            b"[Script Info]\n[Events]\nFormat: Layer, Start, End, Text\n"
        ));
        assert!(is_valid_ass_document(
            b"[Script Info]\n[Events]\nDialogue: 0,0:00:00.00,0:00:01.00,Hello\n"
        ));
    }

    #[test]
    fn srt_validation_accepts_completed_cues_and_rejects_empty_output() {
        assert!(!is_usable_srt_document(b""));
        assert!(!is_usable_srt_document(b"subtitle extraction failed"));
        assert!(is_usable_srt_document(
            b"1\n00:00:01,000 --> 00:00:02,000\nHello\n\n"
        ));
    }

    #[test]
    fn stable_cache_identity_ignores_refreshed_http_url() {
        let first = http_stream_media(Uuid::new_v4(), "https://debrid/first-token");
        let refreshed =
            http_stream_media(Uuid::new_v4(), "https://debrid/second-token");

        assert_eq!(
            subtitle_cache_source_id(&first, Some(6_302_221_349)),
            subtitle_cache_source_id(&refreshed, Some(6_302_221_349))
        );
        assert_ne!(
            subtitle_cache_source_id(&first, Some(6_302_221_349)),
            subtitle_cache_source_id(&refreshed, Some(1_450_000_000))
        );
    }

    #[test]
    fn extraction_plan_preserves_only_native_ass_formats() {
        for codec in [Some("ass"), Some("SSA"), Some(" ass ")] {
            assert!(subtitle_extraction_plan(3, codec).preserve_raw_ass);
            assert_eq!(subtitle_extraction_plan(3, codec).binary_format, None);
        }
        for codec in [
            Some("srt"),
            Some("subrip"),
            Some("vtt"),
            Some("webvtt"),
            Some("pgssub"),
            Some("dvdsub"),
            Some("dvbsub"),
            Some("hdmv_pgs_subtitle"),
            None,
        ] {
            assert!(!subtitle_extraction_plan(3, codec).preserve_raw_ass);
        }
        for codec in [Some("pgssub"), Some("sup"), Some("hdmv_pgs_subtitle")] {
            assert_eq!(
                subtitle_extraction_plan(3, codec).binary_format,
                Some("sup")
            );
        }
        for codec in [Some("srt"), Some("ass"), Some("dvdsub"), None] {
            assert_eq!(subtitle_extraction_plan(3, codec).binary_format, None);
        }
    }

    #[test]
    fn transient_extraction_failures_are_retryable() {
        let response = subtitle_extraction_error_response(&anyhow::anyhow!(
            "Error opening input files: Server returned 429 Too Many Requests"
        ));
        assert_eq!(response.status(), http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get("Retry-After")
                .unwrap(),
            "5"
        );

        let response = subtitle_extraction_error_response(&anyhow::anyhow!(
            "unsupported subtitle codec"
        ));
        assert_eq!(response.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            response
                .headers()
                .get("Retry-After")
                .is_none()
        );
    }

    #[test]
    fn subtitle_http_inputs_resume_interrupted_reads() {
        let options =
            subtitle_http_input_options("http://127.0.0.1:3008/stream/source");
        assert!(!options.contains(&"-reconnect_at_eof"));
        assert!(options.contains(&"-reconnect_streamed"));
        assert!(options.contains(&"-reconnect_on_http_error"));
        assert!(subtitle_http_input_options("/media/source.mkv").is_empty());

        let response = subtitle_extraction_error_response(&anyhow::anyhow!(
            "Stream ends prematurely: Input/output error"
        ));
        assert_eq!(response.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    }
}

/// Extract an embedded subtitle stream to the SRT cache and return the cache path.
/// The cache key is
/// `{data_dir}/subtitle-cache/{item_id}_{cache_source_id}_{stream_index}.srt`.
/// Returns immediately if the cache already exists and is non-empty.
pub(crate) async fn extract_subtitle_to_cache(
    data_dir: &std::path::Path,
    input_url: &str,
    map_spec: &str,
    item_id: uuid::Uuid,
    cache_source_id: uuid::Uuid,
    stream_index: i64,
) -> anyhow::Result<std::path::PathBuf> {
    let cache_dir = data_dir.join("subtitle-cache");
    tokio::fs::create_dir_all(&cache_dir)
        .await
        .map_err(|e| anyhow!("failed to create subtitle cache dir: {e}"))?;
    let cache_path =
        cache_dir.join(format!("{item_id}_{cache_source_id}_{stream_index}.srt"));
    let temp_path = cache_dir.join(format!(
        "{item_id}_{cache_source_id}_{stream_index}_{}.srt.tmp",
        Uuid::new_v4()
    ));

    // Return cached copy if it exists and is non-empty.
    if cache_path.exists() {
        let bytes = tokio::fs::read(&cache_path)
            .await
            .unwrap_or_default();
        let content = String::from_utf8_lossy(&bytes);
        if !content
            .trim()
            .is_empty()
        {
            return Ok(cache_path);
        }
    }

    let _capacity = SUBTITLE_EXTRACTION_CAPACITY
        .acquire()
        .await
        .map_err(|_| anyhow!("subtitle extraction capacity closed"))?;
    let mut cmd = tokio::process::Command::new(ffmpeg_bin());
    cmd.kill_on_drop(true);
    // No -copyts (here or in the ASS/PGS extractors): cues must be 0-based
    // relative to the container start, matching the batch pre-extraction path
    // and the video timeline. With -copyts, containers with a non-zero start
    // time (e.g. ~1.4s for MPEG-TS) bake that offset into every cue.
    cmd.args(["-y", "-nostdin"]);
    cmd.args(subtitle_http_input_options(input_url));
    cmd.args([
        "-i",
        input_url,
        "-map",
        map_spec,
        "-an",
        "-vn",
        "-c:s",
        "srt",
        "-flush_packets",
        "1",
        "-f",
        "srt",
        temp_path
            .to_str()
            .ok_or_else(|| anyhow!("invalid cache path"))?,
    ]);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());

    let output = tokio::time::timeout(SUBTITLE_EXTRACTION_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            let p = temp_path.clone();
            tokio::spawn(async move {
                let _ = tokio::fs::remove_file(p).await;
            });
            anyhow!("subtitle extraction timed out")
        })?
        .map_err(|e| anyhow!("failed to run ffmpeg: {e}"))?;

    if !output
        .status
        .success()
    {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let partial = tokio::fs::read(&temp_path)
            .await
            .unwrap_or_default();
        if is_usable_srt_document(&partial) {
            warn!(
                %item_id,
                %cache_source_id,
                stream_index,
                bytes = partial.len(),
                "subtitle input ended with an error; publishing recovered SRT cues"
            );
            if let Err(error) = tokio::fs::rename(&temp_path, &cache_path).await {
                let _ = tokio::fs::remove_file(&temp_path).await;
                anyhow::bail!(
                    "failed to publish recovered subtitle cache entry: {error}"
                );
            }
            return Ok(cache_path);
        }
        let _ = tokio::fs::remove_file(&temp_path).await;
        anyhow::bail!("ffmpeg subtitle extraction failed: {stderr}");
    }

    let bytes = tokio::fs::read(&temp_path)
        .await
        .map_err(|e| anyhow!("failed to read cached subtitle: {e}"))?;
    if !is_usable_srt_document(&bytes) {
        let _ = tokio::fs::remove_file(&temp_path).await;
        anyhow::bail!("subtitle extraction produced empty output");
    }

    if let Err(error) = tokio::fs::rename(&temp_path, &cache_path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        anyhow::bail!("failed to publish subtitle cache entry: {error}");
    }

    Ok(cache_path)
}

async fn extract_text_subtitle_detached(
    data_dir: std::path::PathBuf,
    input_url: String,
    map_spec: String,
    item_id: Uuid,
    cache_source_id: Uuid,
    stream_index: i64,
) -> anyhow::Result<(Vec<u8>, bool)> {
    let cache_dir = data_dir.join("subtitle-cache");
    let cache_name = format!("{item_id}_{cache_source_id}_{stream_index}.srt");
    let partial_prefix = format!("{item_id}_{cache_source_id}_{stream_index}_");
    let mut extraction = tokio::spawn(async move {
        let artifact_key = (
            item_id,
            cache_source_id,
            stream_index,
            SubtitleArtifactKind::NormalizedText,
        );
        let _artifact_guard = SUBTITLE_ARTIFACT_LOCKS
            .lock(artifact_key)
            .await;
        let cache_path = data_dir
            .join("subtitle-cache")
            .join(format!("{item_id}_{cache_source_id}_{stream_index}.srt"));
        if tokio::fs::read(&cache_path)
            .await
            .ok()
            .is_some_and(|bytes| is_usable_srt_document(&bytes))
        {
            return Ok(cache_path);
        }
        extract_subtitle_to_cache(
            &data_dir,
            &input_url,
            &map_spec,
            item_id,
            cache_source_id,
            stream_index,
        )
        .await
    });

    loop {
        tokio::select! {
            result = &mut extraction => {
                let path = result
                    .map_err(|error| anyhow!("text subtitle extraction task failed: {error}"))??;
                let bytes = tokio::fs::read(path)
                    .await
                    .map_err(|error| anyhow!("failed to read extracted subtitle: {error}"))?;
                return Ok((bytes, false));
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                // Text cues are useful long before FFmpeg has traversed a large
                // remote container. Return a stable in-memory snapshot while the
                // detached extraction keeps filling and eventually publishes the
                // complete cache. The client refreshes snapshots marked partial.
                let mut entries = match tokio::fs::read_dir(&cache_dir).await {
                    Ok(entries) => entries,
                    Err(_) => continue,
                };
                let mut best = Vec::new();
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name == cache_name
                        || !name.starts_with(&partial_prefix)
                        || !name.ends_with(".srt.tmp")
                    {
                        continue;
                    }
                    let bytes = tokio::fs::read(entry.path()).await.unwrap_or_default();
                    let cue_count = bytes
                        .windows(5)
                        .filter(|window| *window == b" --> ")
                        .count();
                    if cue_count >= 3 && bytes.len() > best.len() {
                        best = bytes;
                    }
                }
                if !best.is_empty() {
                    return Ok((best, true));
                }
            }
        }
    }
}

async fn extract_raw_ass_to_cache(
    cache_dir: &std::path::Path,
    input_url: &str,
    map_spec: &str,
    item_id: Uuid,
    cache_source_id: Uuid,
    stream_index: i64,
) -> anyhow::Result<std::path::PathBuf> {
    let cache_path =
        cache_dir.join(format!("{item_id}_{cache_source_id}_{stream_index}.ass"));
    if tokio::fs::read(&cache_path)
        .await
        .ok()
        .is_some_and(|bytes| is_valid_ass_document(&bytes))
    {
        return Ok(cache_path);
    }

    let temp_path = cache_dir.join(format!(
        "{item_id}_{cache_source_id}_{stream_index}_{}.tmp.ass",
        Uuid::new_v4()
    ));
    let _capacity = SUBTITLE_EXTRACTION_CAPACITY
        .acquire()
        .await
        .map_err(|_| anyhow!("subtitle extraction capacity closed"))?;
    let mut cmd = tokio::process::Command::new(ffmpeg_bin());
    cmd.kill_on_drop(true);
    cmd.args(["-y", "-nostdin"]);
    cmd.args(subtitle_http_input_options(input_url));
    cmd.args([
        "-i",
        input_url,
        "-map",
        map_spec,
        "-an",
        "-vn",
        "-c:s",
        "copy",
        "-f",
        "ass",
        temp_path
            .to_str()
            .ok_or_else(|| anyhow!("invalid ASS cache path"))?,
    ]);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());

    let output =
        match tokio::time::timeout(SUBTITLE_EXTRACTION_TIMEOUT, cmd.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => {
                let _ = tokio::fs::remove_file(&temp_path).await;
                return Err(anyhow!("failed to run ffmpeg: {error}"));
            }
            Err(_) => {
                let _ = tokio::fs::remove_file(&temp_path).await;
                anyhow::bail!("ASS subtitle extraction timed out");
            }
        };
    if !output
        .status
        .success()
    {
        let _ = tokio::fs::remove_file(&temp_path).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ASS subtitle extraction failed: {stderr}");
    }
    let bytes = tokio::fs::read(&temp_path)
        .await
        .map_err(|e| anyhow!("failed to read ASS subtitle: {e}"))?;
    if !is_valid_ass_document(&bytes) {
        let _ = tokio::fs::remove_file(&temp_path).await;
        anyhow::bail!("ASS subtitle extraction produced no dialogue events");
    }
    if let Err(error) = tokio::fs::rename(&temp_path, &cache_path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        anyhow::bail!("failed to publish ASS subtitle cache entry: {error}");
    }
    Ok(cache_path)
}

async fn extract_binary_subtitle_to_cache(
    cache_dir: &std::path::Path,
    input_url: &str,
    map_spec: &str,
    item_id: Uuid,
    cache_source_id: Uuid,
    stream_index: i64,
    output_format: &str,
) -> anyhow::Result<std::path::PathBuf> {
    tokio::fs::create_dir_all(cache_dir)
        .await
        .map_err(|e| anyhow!("failed to create subtitle cache dir: {e}"))?;
    let extension = match output_format {
        "sup" | "pgssub" => "sup",
        other => anyhow::bail!("unsupported binary subtitle format: {other}"),
    };
    let cache_path = cache_dir.join(format!(
        "{item_id}_{cache_source_id}_{stream_index}.{extension}"
    ));
    if tokio::fs::metadata(&cache_path)
        .await
        .ok()
        .is_some_and(|m| m.len() > 0)
    {
        return Ok(cache_path);
    }
    let temp_path = cache_dir.join(format!(
        "{item_id}_{cache_source_id}_{stream_index}_{}.tmp.{extension}",
        Uuid::new_v4()
    ));
    let _capacity = SUBTITLE_EXTRACTION_CAPACITY
        .acquire()
        .await
        .map_err(|_| anyhow!("subtitle extraction capacity closed"))?;
    let mut cmd = tokio::process::Command::new(ffmpeg_bin());
    cmd.kill_on_drop(true);
    cmd.args(["-y", "-nostdin"]);
    cmd.args(subtitle_http_input_options(input_url));
    cmd.args([
        "-i",
        input_url,
        "-map",
        map_spec,
        "-an",
        "-vn",
        "-c:s",
        "copy",
        "-f",
        "sup",
        temp_path
            .to_str()
            .ok_or_else(|| anyhow!("invalid binary subtitle cache path"))?,
    ]);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::piped());
    let output = tokio::time::timeout(SUBTITLE_EXTRACTION_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow!("binary subtitle extraction timed out"))?
        .map_err(|e| anyhow!("failed to run ffmpeg: {e}"))?;
    if !output
        .status
        .success()
    {
        let _ = tokio::fs::remove_file(&temp_path).await;
        anyhow::bail!(
            "binary subtitle extraction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if !tokio::fs::metadata(&temp_path)
        .await
        .ok()
        .is_some_and(|m| m.len() > 0)
    {
        let _ = tokio::fs::remove_file(&temp_path).await;
        anyhow::bail!(
            "binary subtitle extraction produced empty output: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if let Err(error) = tokio::fs::rename(&temp_path, &cache_path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        anyhow::bail!("failed to publish binary subtitle cache entry: {error}");
    }
    Ok(cache_path)
}

async fn extract_raw_ass_detached(
    cache_dir: std::path::PathBuf,
    input_url: String,
    map_spec: String,
    item_id: Uuid,
    cache_source_id: Uuid,
    stream_index: i64,
) -> anyhow::Result<std::path::PathBuf> {
    tokio::spawn(async move {
        let artifact_key = (
            item_id,
            cache_source_id,
            stream_index,
            SubtitleArtifactKind::RawAss,
        );
        let _artifact_guard = SUBTITLE_ARTIFACT_LOCKS
            .lock(artifact_key)
            .await;
        let cache_path =
            cache_dir.join(format!("{item_id}_{cache_source_id}_{stream_index}.ass"));
        if tokio::fs::read(&cache_path)
            .await
            .ok()
            .is_some_and(|bytes| is_valid_ass_document(&bytes))
        {
            return Ok(cache_path);
        }
        extract_raw_ass_to_cache(
            &cache_dir,
            &input_url,
            &map_spec,
            item_id,
            cache_source_id,
            stream_index,
        )
        .await
    })
    .await
    .map_err(|error| anyhow!("ASS subtitle extraction task failed: {error}"))?
}

async fn extract_binary_subtitle_detached(
    cache_dir: std::path::PathBuf,
    input_url: String,
    map_spec: String,
    item_id: Uuid,
    cache_source_id: Uuid,
    stream_index: i64,
    output_format: String,
) -> anyhow::Result<std::path::PathBuf> {
    tokio::spawn(async move {
        let artifact_key = (
            item_id,
            cache_source_id,
            stream_index,
            SubtitleArtifactKind::RawBinary,
        );
        let _artifact_guard = SUBTITLE_ARTIFACT_LOCKS
            .lock(artifact_key)
            .await;
        let cache_path =
            cache_dir.join(format!("{item_id}_{cache_source_id}_{stream_index}.sup"));
        if tokio::fs::metadata(&cache_path)
            .await
            .ok()
            .is_some_and(|metadata| metadata.len() > 0)
        {
            return Ok(cache_path);
        }
        extract_binary_subtitle_to_cache(
            &cache_dir,
            &input_url,
            &map_spec,
            item_id,
            cache_source_id,
            stream_index,
            &output_format,
        )
        .await
    })
    .await
    .map_err(|error| anyhow!("binary subtitle extraction task failed: {error}"))?
}

fn subtitle_extraction_error_response(error: &anyhow::Error) -> Response<Body> {
    let transient = subtitle_error_is_transient(error);
    let mut response = Response::builder().status(if transient {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    });
    if transient {
        response = response.header("Retry-After", "5");
    }
    response
        .header("Cache-Control", "no-store")
        .body(Body::from(if transient {
            "subtitle source temporarily unavailable"
        } else {
            "subtitle extraction failed"
        }))
        .unwrap()
}

/// Pre-extract subtitle streams for a media source in one FFmpeg pass.
/// Text streams get normalized SRT, ASS/SSA also retain their styled source,
/// and PGS streams retain raw SUP data for bitmap-capable clients.
/// Mirrors Jellyfin's one-command/multiple-output cache warming, but remains
/// cancellable so playback startup always owns the upstream connection.
/// The `subtitles_stream` endpoint falls back to on-demand extraction for any cache misses.
async fn pre_extract_all_subtitles_to_cache(
    data_dir: std::path::PathBuf,
    input_url: String,
    item_id: uuid::Uuid,
    cache_source_id: uuid::Uuid,
    subtitle_streams: Vec<SubtitleExtractionPlan>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    if *cancel_rx.borrow() {
        return;
    }
    let cache_dir = data_dir.join("subtitle-cache");
    let _ = tokio::fs::create_dir_all(&cache_dir).await;

    let cache_is_populated = |path: &std::path::Path| {
        std::fs::read(path)
            .ok()
            .map(|bytes| {
                if path
                    .to_string_lossy()
                    .contains(".ass")
                {
                    is_valid_ass_document(&bytes)
                } else {
                    !bytes.is_empty()
                        && !String::from_utf8_lossy(&bytes)
                            .trim()
                            .is_empty()
                }
            })
            .unwrap_or(false)
    };
    let mut to_extract = Vec::new();
    for plan in &subtitle_streams {
        let idx = plan.stream_index;
        let srt_path = cache_dir.join(format!("{item_id}_{cache_source_id}_{idx}.srt"));
        let raw_ass_path =
            cache_dir.join(format!("{item_id}_{cache_source_id}_{idx}.ass"));
        let raw_binary_path =
            cache_dir.join(format!("{item_id}_{cache_source_id}_{idx}.sup"));
        // Bitmap subtitles cannot be converted to text. Preserve them as SUP
        // in the same FFmpeg pass instead of poisoning every SRT output.
        let needs_srt = plan
            .binary_format
            .is_none()
            && !cache_is_populated(&srt_path);
        let needs_raw_ass = plan.preserve_raw_ass && !cache_is_populated(&raw_ass_path);
        let needs_raw_binary = plan
            .binary_format
            .is_some()
            && !cache_is_populated(&raw_binary_path);
        if needs_srt || needs_raw_ass || needs_raw_binary {
            let srt_output = needs_srt.then(|| {
                (
                    cache_dir.join(format!(
                        "{item_id}_{cache_source_id}_{idx}_{}.batch.tmp.srt",
                        Uuid::new_v4()
                    )),
                    srt_path,
                )
            });
            let ass_output = needs_raw_ass.then(|| {
                (
                    cache_dir.join(format!(
                        "{item_id}_{cache_source_id}_{idx}_{}.batch.tmp.ass",
                        Uuid::new_v4()
                    )),
                    raw_ass_path,
                )
            });
            let binary_output = needs_raw_binary.then(|| {
                (
                    cache_dir.join(format!(
                        "{item_id}_{cache_source_id}_{idx}_{}.batch.tmp.sup",
                        Uuid::new_v4()
                    )),
                    raw_binary_path,
                )
            });
            to_extract.push((idx, srt_output, ass_output, binary_output));
        } else {
            debug!(%item_id, stream_index = idx, "subtitle caches hit, skipping");
        }
    }

    if to_extract.is_empty() {
        debug!(%item_id, "all {} subtitle track(s) already cached", subtitle_streams.len());
        return;
    }

    let indices: Vec<i64> = to_extract
        .iter()
        .map(|(i, _, _, _)| *i)
        .collect();
    info!(
        %item_id,
        %cache_source_id,
        ?indices,
        "pre-extracting {} subtitle track(s) in background",
        to_extract.len()
    );

    // Register in-progress signal so the subtitle endpoint can wait on us
    // instead of launching a competing FFmpeg process.
    let extraction_key = (item_id, cache_source_id);
    let (done_tx, done_rx) = watch::channel(false);
    {
        let mut extracting = batch_extraction_map()
            .lock()
            .unwrap();
        if extracting.contains_key(&extraction_key) {
            debug!(%item_id, %cache_source_id, "subtitle extraction already in progress, reusing existing work");
            return;
        }
        extracting.insert(extraction_key, done_rx);
    }

    {
        let capacity = SUBTITLE_EXTRACTION_CAPACITY.acquire();
        let _capacity = match tokio::select! {
            result = capacity => Some(result),
            changed = cancel_rx.changed() => {
                let _ = changed;
                None
            }
        } {
            None => {
                debug!(%item_id, %cache_source_id, "subtitle pre-extraction cancelled while queued");
                let _ = done_tx.send(true);
                batch_extraction_map()
                    .lock()
                    .unwrap()
                    .remove(&extraction_key);
                return;
            }
            Some(Ok(permit)) => permit,
            Some(Err(_)) => {
                warn!(%item_id, %cache_source_id, "subtitle extraction capacity closed");
                let _ = done_tx.send(true);
                batch_extraction_map()
                    .lock()
                    .unwrap()
                    .remove(&extraction_key);
                return;
            }
        };
        let mut cmd = tokio::process::Command::new(ffmpeg_bin());
        cmd.kill_on_drop(true);
        // -y: overwrite without prompting (hangs forever waiting for stdin otherwise)
        // -nostdin: don't read from stdin at all
        // -c:s srt: convert to SRT so the cache is always valid SRT (not raw ASS/VTT bytes)
        cmd.args(["-y", "-nostdin"]);
        cmd.args(subtitle_http_input_options(&input_url));
        cmd.args(["-i", &input_url]);
        for (idx, srt_output, ass_output, binary_output) in &to_extract {
            if let Some(p) = srt_output
                .as_ref()
                .and_then(|(temp_path, _)| temp_path.to_str())
            {
                cmd.args([
                    "-map",
                    &format!("0:{idx}"),
                    "-an",
                    "-vn",
                    "-c:s",
                    "srt",
                    "-flush_packets",
                    "1",
                    "-f",
                    "srt",
                    p,
                ]);
            }
            if let Some(p) = ass_output
                .as_ref()
                .and_then(|(temp_path, _)| temp_path.to_str())
            {
                cmd.args([
                    "-map",
                    &format!("0:{idx}"),
                    "-an",
                    "-vn",
                    "-c:s",
                    "copy",
                    "-f",
                    "ass",
                    p,
                ]);
            }
            if let Some(p) = binary_output
                .as_ref()
                .and_then(|(temp_path, _)| temp_path.to_str())
            {
                cmd.args([
                    "-map",
                    &format!("0:{idx}"),
                    "-an",
                    "-vn",
                    "-c:s",
                    "copy",
                    "-f",
                    "sup",
                    p,
                ]);
            }
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::piped());

        let start = std::time::Instant::now();
        let extraction =
            tokio::time::timeout(SUBTITLE_EXTRACTION_TIMEOUT, cmd.output());
        let outcome = tokio::select! {
            result = extraction => Some(result),
            changed = cancel_rx.changed() => {
                let _ = changed;
                None
            }
        };
        if outcome.is_none() {
            for (_, srt_output, ass_output, binary_output) in &to_extract {
                for output in [srt_output, ass_output, binary_output] {
                    if let Some((temp_path, _)) = output {
                        let _ = tokio::fs::remove_file(temp_path).await;
                    }
                }
            }
            debug!(%item_id, %cache_source_id, ?indices, "cancelled subtitle pre-extraction for active playback");
            let _ = done_tx.send(true);
            batch_extraction_map()
                .lock()
                .unwrap()
                .remove(&extraction_key);
            return;
        }
        match outcome.expect("checked above") {
            Ok(Ok(output)) => {
                let elapsed = start
                    .elapsed()
                    .as_secs_f32();
                if output
                    .status
                    .success()
                {
                    for (_, srt_output, ass_output, binary_output) in &to_extract {
                        for output in [srt_output, ass_output, binary_output] {
                            if let Some((temp_path, cache_path)) = output {
                                if cache_is_populated(temp_path) {
                                    if tokio::fs::rename(temp_path, cache_path)
                                        .await
                                        .is_err()
                                    {
                                        let _ = tokio::fs::remove_file(temp_path).await;
                                    }
                                } else {
                                    let _ = tokio::fs::remove_file(temp_path).await;
                                }
                            }
                        }
                    }
                    info!(%item_id, %cache_source_id, ?indices, elapsed_secs = elapsed, "batch subtitle extraction completed");
                } else {
                    for (_, srt_output, ass_output, binary_output) in &to_extract {
                        for output in [srt_output, ass_output, binary_output] {
                            if let Some((temp_path, _)) = output {
                                let _ = tokio::fs::remove_file(temp_path).await;
                            }
                        }
                    }
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    warn!(%item_id, %cache_source_id, ?indices, elapsed_secs = elapsed, %stderr, "batch subtitle extraction non-zero exit");
                }
            }
            Ok(Err(e)) => {
                for (_, srt_output, ass_output, binary_output) in &to_extract {
                    for output in [srt_output, ass_output, binary_output] {
                        if let Some((temp_path, _)) = output {
                            let _ = tokio::fs::remove_file(temp_path).await;
                        }
                    }
                }
                warn!(%item_id, %cache_source_id, ?indices, "failed to spawn ffmpeg for batch subtitle extraction: {e}");
            }
            Err(_) => {
                for (_, srt_output, ass_output, binary_output) in &to_extract {
                    for output in [srt_output, ass_output, binary_output] {
                        if let Some((temp_path, _)) = output {
                            let _ = tokio::fs::remove_file(temp_path).await;
                        }
                    }
                }
                warn!(%item_id, %cache_source_id, ?indices, "batch subtitle extraction timed out after 120s");
            }
        }
    }

    // Signal done and clean up (drop tx signals all receivers).
    let _ = done_tx.send(true);
    batch_extraction_map()
        .lock()
        .unwrap()
        .remove(&extraction_key);
}

/// Subtitle extraction endpoint - extracts a subtitle stream from a media source
/// and optionally converts it to the requested format (vtt, srt, ass).
// Jellyfin clients include a start-position-ticks segment in the path.
#[get(
    "/videos/{item_id}/{media_source_id}/subtitles/{stream_index}/{start_ticks}/stream.{format}"
)]
pub async fn subtitles_stream(
    State(state): State<AppState>,
    session: auth::AuthSession,
    Path((item_id, media_source_id, stream_index, _start_ticks, format)): Path<(
        Uuid,
        Uuid,
        i64,
        String,
        String,
    )>,
) -> Result<impl IntoResponse> {
    subtitles_stream_inner(
        state,
        session,
        item_id,
        media_source_id,
        stream_index,
        format,
    )
    .await
}

/// Jellyfin also accepts the tickless subtitle route (defaults the start-position
/// ticks segment to 0) — Moonfin for webOS uses it.
/// https://github.com/jellyfin/jellyfin/blob/master/Jellyfin.Api/Controllers/SubtitleController.cs
#[get("/videos/{item_id}/{media_source_id}/subtitles/{stream_index}/stream.{format}")]
pub async fn subtitles_stream_tickless(
    State(state): State<AppState>,
    session: auth::AuthSession,
    Path((item_id, media_source_id, stream_index, format)): Path<(
        Uuid,
        Uuid,
        i64,
        String,
    )>,
) -> Result<impl IntoResponse> {
    subtitles_stream_inner(
        state,
        session,
        item_id,
        media_source_id,
        stream_index,
        format,
    )
    .await
}

async fn subtitles_stream_inner(
    state: AppState,
    session: auth::AuthSession,
    item_id: Uuid,
    media_source_id: Uuid,
    stream_index: i64,
    format: String,
) -> Result<impl IntoResponse> {
    // Try to resolve as an external subtitle injected during PlaybackInfo.
    // fetch_subtitles is cached (24h Stremio / SQLite Opendal) so this is cheap.
    if let Some(item_media) = db::Media::get_by_id(
        &state
            .ctx
            .db,
        &item_id,
    )
    .await
    .ok()
    .flatten()
    {
        let source_media = crate::services::StreamService::lookup(
            &state.ctx,
            item_id,
            Some(media_source_id),
            None,
            Some(
                session
                    .user
                    .id,
            ),
        )
        .await
        .ok();
        if let Some(ref source) = source_media {
            let embedded_indices: std::collections::HashSet<i64> = source
                .probe_data
                .as_ref()
                .map(|p| {
                    p.media_streams
                        .iter()
                        .map(|s| s.index)
                        .collect()
                })
                .unwrap_or_default();
            let next_idx = embedded_indices
                .iter()
                .max()
                .map_or(0, |m| m + 1);
            let i = stream_index - next_idx;
            // Only attempt external resolution if the index is not an embedded stream.
            if i >= 0 && !embedded_indices.contains(&stream_index) {
                let sub_langs = db::Settings::get_config_or_default(
                    &state
                        .ctx
                        .db,
                )
                .await
                .subtitle_languages
                .unwrap_or_default();
                let subs = state
                    .ctx
                    .addons
                    .fetch_subtitles(
                        &item_media,
                        &state
                            .ctx
                            .db,
                        false,
                        Some(
                            session
                                .user
                                .id,
                        ),
                    )
                    .await;
                let source_info = api::MediaSourceInfo::from(source.clone());
                let scored = scored_external_subtitles(
                    &subs,
                    &sub_langs,
                    &source_info.name,
                    &source_info.path,
                );
                if let Some(sub) = scored.get(i as usize) {
                    if let Some(ref descriptor) = sub.url {
                        let output_format = format.to_ascii_lowercase();
                        let resp = match descriptor {
                            crate::stream::StreamDescriptor::Opendal {
                                addon_id,
                                ..
                            } => {
                                let addon = state
                                    .ctx
                                    .addons
                                    .get(*addon_id)
                                    .ok_or_else(|| {
                                        anyhow!("addon not found for subtitle")
                                    })?;
                                let stream_cap = addon
                                    .stream
                                    .as_ref()
                                    .ok_or_else(|| {
                                        anyhow!("addon has no stream capability")
                                    })?;
                                stream_cap
                                    .serve_stream(
                                        descriptor,
                                        &axum::http::HeaderMap::new(),
                                    )
                                    .await
                                    .map_err(|e| anyhow!("{e:?}"))?
                            }
                            _ => descriptor
                                .clone()
                                .into_source()
                                .serve(&state, &axum::http::HeaderMap::new())
                                .await
                                .map_err(|e| anyhow!("{e:?}"))?,
                        };
                        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                            .await
                            .map_err(|e| anyhow!("read subtitle bytes: {e}"))?;
                        let body = String::from_utf8_lossy(&bytes).into_owned();
                        let (converted, content_type) = match output_format.as_str() {
                            "vtt" | "webvtt" => (
                                crate::conversions::srt_to_vtt(&body),
                                "text/vtt; charset=utf-8",
                            ),
                            "js" => (
                                crate::conversions::srt_to_jellyfin_json(&body),
                                "application/json",
                            ),
                            _ => (body, "text/plain; charset=utf-8"),
                        };
                        return Ok(Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", content_type)
                            .header("Cache-Control", "public, max-age=3600")
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Body::from(converted))
                            .unwrap());
                    }
                }
            }
        }
    }

    let media = crate::services::StreamService::lookup(
        &state.ctx,
        item_id,
        Some(media_source_id),
        None,
        Some(
            session
                .user
                .id,
        ),
    )
    .await?;
    let cache_source_id = subtitle_cache_source_id(
        &media,
        media
            .probe_data
            .as_ref()
            .and_then(|probe| probe.size),
    );

    let url = media
        .stream_info
        .as_ref()
        .map(|si| {
            si.descriptor
                .server_input(
                    media.id,
                    state
                        .ctx
                        .config
                        .port,
                )
        })
        .context_not_found("media source has no URL")?;

    let output_format = format.to_ascii_lowercase();
    let is_json = matches!(output_format.as_str(), "js" | "json");
    let (ffmpeg_format, content_type) = match output_format.as_str() {
        "vtt" | "webvtt" => ("webvtt", "text/vtt; charset=utf-8"),
        "srt" | "subrip" => ("srt", "text/plain; charset=utf-8"),
        "ass" | "ssa" => ("ass", "text/plain; charset=utf-8"),
        "pgssub" | "sup" => ("sup", "application/octet-stream"),
        "js" | "json" => ("srt", "application/json; charset=utf-8"),
        _ => ("srt", "text/plain; charset=utf-8"),
    };

    let subtitle_ordinal = media
        .probe_data
        .as_ref()
        .and_then(|probe| {
            let mut sub_indexes: Vec<i64> = probe
                .media_streams
                .iter()
                .filter(|s| matches!(s.type_, Some(api::MediaStreamType::Subtitle)))
                .map(|s| s.index)
                .collect();
            sub_indexes.sort_unstable();
            sub_indexes
                .iter()
                .position(|idx| *idx == stream_index)
        })
        .context_not_found("subtitle stream not found")?;
    let map_spec = format!("0:s:{subtitle_ordinal}");

    let is_passthrough =
        matches!(output_format.as_str(), "ass" | "ssa" | "sup" | "pgssub");
    let is_binary = matches!(output_format.as_str(), "sup" | "pgssub");

    // ASS/SSA must bypass the SRT cache. Converting to SRT destroys styles,
    // drawings, transforms, and karaoke effects that client renderers need.
    if matches!(output_format.as_str(), "ass" | "ssa") {
        let artifact_key = (
            item_id,
            cache_source_id,
            stream_index,
            SubtitleArtifactKind::RawAss,
        );
        let cache_dir = state
            .ctx
            .config
            .data_dir
            .join("subtitle-cache");
        tokio::fs::create_dir_all(&cache_dir)
            .await
            .map_err(|e| anyhow!("failed to create subtitle cache dir: {e}"))?;
        let cache_path =
            cache_dir.join(format!("{item_id}_{cache_source_id}_{stream_index}.ass"));

        let mut cached = tokio::fs::read(&cache_path)
            .await
            .ok()
            .filter(|bytes| is_valid_ass_document(bytes));
        if cached.is_none() {
            let in_progress_rx = batch_extraction_map()
                .lock()
                .unwrap()
                .get(&(item_id, cache_source_id))
                .cloned();
            if let Some(mut rx) = in_progress_rx {
                if !*rx.borrow() {
                    info!(%item_id, %media_source_id, stream_index, "batch ASS extraction in progress - waiting for it to finish");
                    let _ =
                        tokio::time::timeout(SUBTITLE_EXTRACTION_TIMEOUT, rx.changed())
                            .await;
                    cached = tokio::fs::read(&cache_path)
                        .await
                        .ok()
                        .filter(|bytes| is_valid_ass_document(bytes));
                }
            }
        }
        let bytes = if let Some(bytes) = cached {
            debug!(%item_id, %media_source_id, stream_index, "raw ASS subtitle cache hit");
            bytes
        } else {
            if subtitle_failure_is_cooling_down(&artifact_key) {
                return Ok(subtitle_extraction_error_response(&anyhow!(
                    "subtitle extraction timed out during retry cooldown"
                )));
            }
            let published_path = match extract_raw_ass_detached(
                cache_dir.clone(),
                url.clone(),
                map_spec.clone(),
                item_id,
                cache_source_id,
                stream_index,
            )
            .await
            {
                Ok(path) => path,
                Err(error) => {
                    record_transient_subtitle_failure(artifact_key, &error);
                    error!(%item_id, stream_index, %map_spec, "ASS subtitle extraction failed: {error}");
                    return Ok(subtitle_extraction_error_response(&error));
                }
            };
            clear_subtitle_failure(&artifact_key);
            tokio::fs::read(&published_path)
                .await
                .map_err(|e| anyhow!("failed to read ASS subtitle: {e}"))?
        };

        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", content_type)
            .header("Cache-Control", "public, max-age=3600")
            .header("Access-Control-Allow-Origin", "*")
            .body(Body::from(bytes))
            .unwrap());
    }

    // Binary formats (PGS/SUP): extract on-the-fly as raw bytes.
    if is_binary {
        let artifact_key = (
            item_id,
            cache_source_id,
            stream_index,
            SubtitleArtifactKind::RawBinary,
        );
        let cache_dir = state
            .ctx
            .config
            .data_dir
            .join("subtitle-cache");
        let expected_cache_path =
            cache_dir.join(format!("{item_id}_{cache_source_id}_{stream_index}.sup"));
        let mut cached = tokio::fs::metadata(&expected_cache_path)
            .await
            .ok()
            .is_some_and(|metadata| metadata.len() > 0);
        if !cached {
            let in_progress_rx = batch_extraction_map()
                .lock()
                .unwrap()
                .get(&(item_id, cache_source_id))
                .cloned();
            if let Some(mut rx) = in_progress_rx {
                if !*rx.borrow() {
                    info!(%item_id, %media_source_id, stream_index, "batch PGS extraction in progress - waiting for it to finish");
                    let _ =
                        tokio::time::timeout(SUBTITLE_EXTRACTION_TIMEOUT, rx.changed())
                            .await;
                    cached = tokio::fs::metadata(&expected_cache_path)
                        .await
                        .ok()
                        .is_some_and(|metadata| metadata.len() > 0);
                }
            }
        }
        if subtitle_failure_is_cooling_down(&artifact_key) {
            return Ok(subtitle_extraction_error_response(&anyhow!(
                "subtitle extraction timed out during retry cooldown"
            )));
        }
        let cache_path = if cached {
            debug!(%item_id, %media_source_id, stream_index, "raw PGS subtitle cache hit");
            expected_cache_path
        } else {
            match extract_binary_subtitle_detached(
                cache_dir.clone(),
                url.clone(),
                map_spec.clone(),
                item_id,
                cache_source_id,
                stream_index,
                output_format.clone(),
            )
            .await
            {
                Ok(path) => path,
                Err(error) => {
                    record_transient_subtitle_failure(artifact_key, &error);
                    error!(%item_id, stream_index, %map_spec, "binary subtitle extraction failed: {error}");
                    return Ok(subtitle_extraction_error_response(&error));
                }
            }
        };
        clear_subtitle_failure(&artifact_key);
        let bytes = tokio::fs::read(cache_path)
            .await
            .map_err(|e| anyhow!("failed to read binary subtitle cache: {e}"))?;
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", content_type)
            .header("Cache-Control", "public, max-age=3600")
            .header("Access-Control-Allow-Origin", "*")
            .body(Body::from(bytes))
            .unwrap());
    }

    // Text formats: serve from SRT cache (populated by pre_extract_all_subtitles_to_cache
    // at PlaybackInfo time). Falls back to on-demand extraction on cache miss.
    let artifact_key = (
        item_id,
        cache_source_id,
        stream_index,
        SubtitleArtifactKind::NormalizedText,
    );
    let cache_file = state
        .ctx
        .config
        .data_dir
        .join("subtitle-cache")
        .join(format!("{item_id}_{cache_source_id}_{stream_index}.srt"));
    let is_cached = |path: &std::path::Path| -> bool {
        path.exists()
            && std::fs::read(path)
                .ok()
                .map(|b| {
                    !String::from_utf8_lossy(&b)
                        .trim()
                        .is_empty()
                })
                .unwrap_or(false)
    };

    if is_cached(&cache_file) {
        debug!(%item_id, stream_index, "subtitle cache hit");
    } else {
        // Check if a batch extraction is in progress for this item.
        // If so, wait for it to finish rather than launching a competing FFmpeg process.
        let in_progress_rx = batch_extraction_map()
            .lock()
            .unwrap()
            .get(&(item_id, cache_source_id))
            .cloned();
        if let Some(mut rx) = in_progress_rx {
            if !*rx.borrow() {
                info!(%item_id, stream_index, "batch extraction in progress — waiting for it to finish");
                let _ = tokio::time::timeout(SUBTITLE_EXTRACTION_TIMEOUT, rx.changed())
                    .await;
            }
        }

        if is_cached(&cache_file) {
            info!(%item_id, stream_index, "subtitle ready after waiting for batch extraction");
        } else {
            info!(%item_id, stream_index, %map_spec, "subtitle cache miss — extracting on-demand");
        }
    }
    if !is_cached(&cache_file) && subtitle_failure_is_cooling_down(&artifact_key) {
        return Ok(subtitle_extraction_error_response(&anyhow!(
            "subtitle extraction timed out during retry cooldown"
        )));
    }
    let (cached_bytes, is_partial) = match extract_text_subtitle_detached(
        state
            .ctx
            .config
            .data_dir
            .clone(),
        url.clone(),
        map_spec.clone(),
        item_id,
        cache_source_id,
        stream_index,
    )
    .await
    {
        Ok(payload) => payload,
        Err(e) => {
            record_transient_subtitle_failure(artifact_key, &e);
            error!(%item_id, stream_index, %map_spec, "subtitle extraction failed: {e}");
            return Ok(subtitle_extraction_error_response(&e));
        }
    };
    if !is_partial {
        clear_subtitle_failure(&artifact_key);
    }

    let cached = String::from_utf8_lossy(&cached_bytes).into_owned();

    let body = if is_passthrough {
        cached
    } else if is_json {
        crate::conversions::srt_to_jellyfin_json(&cached)
    } else if ffmpeg_format == "webvtt" {
        crate::conversions::srt_to_vtt(&cached)
    } else {
        cached
    };

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", content_type)
        .header(
            "Cache-Control",
            if is_partial {
                "no-store"
            } else {
                "public, max-age=3600"
            },
        )
        .header("Access-Control-Allow-Origin", "*")
        .header("Access-Control-Expose-Headers", "X-Remux-Subtitle-Partial")
        .body(Body::from(body))
        .unwrap();
    if is_partial {
        response
            .headers_mut()
            .insert(
                "X-Remux-Subtitle-Partial",
                http::HeaderValue::from_static("true"),
            );
    }
    Ok(response)
}

pub(crate) use remux_sdks::remux::lang_to_two_letter;

pub(crate) fn subtitle_path_hint(sub: &crate::addons::SubtitleInfo) -> &str {
    match &sub.url {
        Some(crate::stream::StreamDescriptor::Http { url, .. }) => url.as_str(),
        Some(crate::stream::StreamDescriptor::Local(p)) => p
            .to_str()
            .unwrap_or(""),
        Some(crate::stream::StreamDescriptor::Opendal { path, .. }) => path.as_str(),
        _ => "",
    }
}

pub(crate) fn descriptor_to_subtitle_url(sub: &crate::addons::SubtitleInfo) -> String {
    match &sub.url {
        Some(d) => serde_json::to_string(d).unwrap_or_default(),
        None => String::new(),
    }
}

fn score_sub_url(
    sub: &crate::addons::SubtitleInfo,
    source_name: &Option<String>,
    source_path: &Option<String>,
) -> i32 {
    fn tokens(s: &str) -> std::collections::HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 2)
            .map(|t| t.to_lowercase())
            .collect()
    }
    let hint = subtitle_path_hint(sub);
    let sub_file = hint
        .rsplit('/')
        .next()
        .unwrap_or(hint);
    let sub_tok = tokens(sub_file);
    let mut src_tok = tokens(
        source_name
            .as_deref()
            .unwrap_or(""),
    );
    src_tok.extend(tokens(
        source_path
            .as_deref()
            .unwrap_or(""),
    ));
    sub_tok
        .intersection(&src_tok)
        .count() as i32
}

/// Filter, score, sort, and deduplicate external subtitles for a single source.
/// Returns the ordered list of subtitles that will be assigned stream indices.
pub(crate) fn scored_external_subtitles<'a>(
    subs: &'a [crate::addons::SubtitleInfo],
    sub_langs: &[String],
    source_name: &Option<String>,
    source_path: &Option<String>,
) -> Vec<&'a crate::addons::SubtitleInfo> {
    let filtered: Vec<&crate::addons::SubtitleInfo> = if sub_langs.is_empty() {
        subs.iter()
            .collect()
    } else {
        subs.iter()
            .filter(|s| {
                let two = s
                    .lang
                    .as_deref()
                    .and_then(lang_to_two_letter);
                two.map_or(false, |two| {
                    sub_langs
                        .iter()
                        .any(|p| two.eq_ignore_ascii_case(p.trim()))
                })
            })
            .collect()
    };

    let mut scored: Vec<_> = filtered
        .into_iter()
        .map(|s| (score_sub_url(s, source_name, source_path), s))
        .collect();
    scored.sort_by(|(sa, a), (sb, b)| {
        let rank = |s: &&crate::addons::SubtitleInfo| {
            let two = s
                .lang
                .as_deref()
                .and_then(lang_to_two_letter);
            sub_langs
                .iter()
                .position(|p| {
                    two.as_deref()
                        .map_or(false, |t| t.eq_ignore_ascii_case(p.trim()))
                })
                .unwrap_or(usize::MAX)
        };
        rank(a)
            .cmp(&rank(b))
            .then(sb.cmp(sa))
    });

    let mut lang_counts: std::collections::HashMap<String, usize> = Default::default();
    scored
        .into_iter()
        .filter_map(|(_, s)| {
            let key = s
                .lang
                .clone()
                .unwrap_or_else(|| "und".to_string());
            let count = lang_counts
                .entry(key)
                .or_insert(0);
            if *count < 2 {
                *count += 1;
                Some(s)
            } else {
                None
            }
        })
        .collect()
}

/// Inject external subtitles into a list of `MediaSourceInfo` entries.
pub(crate) async fn inject_external_subtitles(
    ctx: &crate::AppContext,
    subtitle_media: &crate::db::Media,
    media_sources: &mut Vec<api::MediaSourceInfo>,
    item_id: Uuid,
    api_key: &str,
    sub_langs: Vec<String>,
    user_id: Option<uuid::Uuid>,
) {
    let subs = ctx
        .addons
        .fetch_subtitles(subtitle_media, &ctx.db, false, user_id)
        .await;
    if subs.is_empty() {
        return;
    }

    for source in media_sources.iter_mut() {
        let next_idx = source
            .media_streams
            .iter()
            .map(|s| s.index)
            .max()
            .map_or(0, |m| m + 1);

        let scored =
            scored_external_subtitles(&subs, &sub_langs, &source.name, &source.path);

        let wants_default = !sub_langs.is_empty()
            && source
                .default_subtitle_stream_index
                .is_none();
        for (i, sub) in scored
            .into_iter()
            .enumerate()
        {
            let mut stream = crate::conversions::subtitle_to_media_stream(sub);
            let idx = next_idx + i as i64;
            stream.index = idx;
            stream.delivery_url = Some(format!(
                "/Videos/{item_id}/{source_id}/Subtitles/{idx}/0/Stream.vtt?ApiKey={api_key}",
                source_id = source.id,
            ));
            if wants_default && i == 0 {
                stream.is_default = Some(true);
                source.default_subtitle_stream_index = Some(next_idx);
            }
            source
                .media_streams
                .push(stream);
        }
    }
}

#[cfg(test)]
mod language_code_tests {
    use super::*;

    #[test]
    fn lang_to_two_letter_normalizes_codes() {
        // Already two letters: kept as-is, just trimmed and lowercased.
        assert_eq!(lang_to_two_letter("en"), Some("en".to_string()));
        assert_eq!(lang_to_two_letter("  EN "), Some("en".to_string()));
        // Three-letter ISO 639-3 codes are mapped down to two letters.
        assert_eq!(lang_to_two_letter("eng"), Some("en".to_string()));
        assert_eq!(lang_to_two_letter("spa"), Some("es".to_string()));
        // Empty or unrecognizable input gives nothing back.
        assert_eq!(lang_to_two_letter(""), None);
        assert_eq!(lang_to_two_letter("   "), None);
        assert_eq!(lang_to_two_letter("xyz"), None);
    }

    #[tokio::test]
    async fn playback_cancels_subtitle_prefetch_during_grace_period() {
        let item_id = Uuid::new_v4();
        let cache_source_id = Uuid::new_v4();
        schedule_subtitle_pre_extraction(
            std::env::temp_dir(),
            "https://example.invalid/video.mkv".to_string(),
            item_id,
            cache_source_id,
            Vec::new(),
        );
        assert!(
            batch_cancellation_map()
                .lock()
                .unwrap()
                .contains_key(&(item_id, cache_source_id))
        );

        cancel_subtitle_pre_extraction(item_id);
        for _ in 0..20 {
            if !batch_cancellation_map()
                .lock()
                .unwrap()
                .contains_key(&(item_id, cache_source_id))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("cancelled subtitle prefetch remained registered");
    }
}

#[cfg(test)]
mod tests {
    use http::header::HeaderValue;

    use crate::integration_test::{auth_header_with_token, authenticated_server};

    /// Jellyfin's tickless subtitle route (`.../Subtitles/{index}/Stream.{format}`,
    /// no start-position-ticks segment) must dispatch to the same handler as the
    /// canonical route. With a non-existent item both produce the identical
    /// handler response — an unregistered route would yield axum's bare 404.
    #[tokio::test]
    async fn tickless_subtitle_route_dispatches_to_handler() {
        let (server, guard, token) = authenticated_server().await;
        let auth = auth_header_with_token(&token);
        let bogus = "00000000-0000-0000-0000-000000000000";
        let _ = &guard;

        let canonical = server
            .get(&format!("/videos/{bogus}/{bogus}/subtitles/2/0/stream.ass"))
            .add_header(
                http::header::AUTHORIZATION,
                HeaderValue::from_str(&auth).unwrap(),
            )
            .expect_failure()
            .await;
        let tickless = server
            .get(&format!("/videos/{bogus}/{bogus}/subtitles/2/stream.ass"))
            .add_header(
                http::header::AUTHORIZATION,
                HeaderValue::from_str(&auth).unwrap(),
            )
            .expect_failure()
            .await;

        assert_eq!(
            canonical.status_code(),
            tickless.status_code(),
            "both subtitle route forms must reach the same handler"
        );
        assert!(
            !tickless
                .text()
                .is_empty(),
            "tickless route must dispatch to the subtitle handler, not a bare route-miss 404"
        );
    }
}
