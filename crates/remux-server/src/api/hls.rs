use std::sync::{Arc, atomic::Ordering};

use axum::{
    body::Body,
    extract::{Path, State},
    response::IntoResponse,
};
use axum_anyhow::ApiResult as Result;
use axum_extra::extract::Query;
use http::{Response, StatusCode};
use remux_macros::get;
use tokio_util::io::ReaderStream;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

use crate::{
    AppState, IntoApiError, OptionExt, ResultExt, api, common,
    common::{TickUnit, ToRunTimeTicks},
    db,
    db::auth,
    playback::session::{HlsSegmentFile, HlsSegmentFormat, TranscodeSession, TranscodeState},
};

/// Serializes the lookup-or-create-transcode sequence per play_session_id so
/// two racing requests for the same session can't each spawn their own
/// ffmpeg process.
static TRANSCODE_CREATE_LOCKS: crate::keyed_lock::KeyedLock<String> =
    crate::keyed_lock::KeyedLock::new();

const PLAYBACK_START_TICKS_DATA_ID: &str = "com.remux.playback-start-ticks";
const PREWARM_LEASE_SECS: u64 = 90;

async fn wait_for_transcode_path(
    session: &Arc<tokio::sync::RwLock<TranscodeSession>>,
    path: &std::path::Path,
    timeout: std::time::Duration,
) -> bool {
    let output_tx = session
        .read()
        .await
        .output_tx
        .clone();
    let mut output_rx = output_tx.subscribe();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero()
            || tokio::time::timeout(remaining, output_rx.changed())
                .await
                .is_err()
        {
            return path.exists();
        }
    }
}

async fn wait_for_variant_playlist(
    session: &Arc<tokio::sync::RwLock<TranscodeSession>>,
    path: &std::path::Path,
    timeout: std::time::Duration,
) -> String {
    let output_tx = session
        .read()
        .await
        .output_tx
        .clone();
    let mut output_rx = output_tx.subscribe();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(text) = tokio::fs::read_to_string(path).await {
            if text.contains("#EXT-X-TARGETDURATION") {
                return text;
            }
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero()
            || tokio::time::timeout(remaining, output_rx.changed())
                .await
                .is_err()
        {
            return String::new();
        }
    }
}

fn add_playback_start_acknowledgement(
    playlist: String,
    start_time_ticks: Option<i64>,
) -> String {
    let Some(ticks) = start_time_ticks.filter(|ticks| *ticks > 0) else {
        return playlist;
    };
    let acknowledgement = format!(
        "#EXT-X-SESSION-DATA:DATA-ID=\"{}\",VALUE=\"{}\"\n",
        PLAYBACK_START_TICKS_DATA_ID, ticks,
    );
    playlist.replacen("#EXTM3U\n", &format!("#EXTM3U\n{acknowledgement}"), 1)
}

fn add_playback_generation_to_master(
    playlist: String,
    start_time_ticks: Option<i64>,
) -> String {
    let Some(ticks) = start_time_ticks.filter(|ticks| *ticks > 0) else {
        return playlist;
    };
    playlist
        .lines()
        .map(|line| {
            if line.starts_with('#') || !line.contains(".m3u8") {
                return line.to_string();
            }
            let separator = if line.contains('?') { '&' } else { '?' };
            format!("{line}{separator}StartTimeTicks={ticks}")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// A session may be claimed — left running rather than restarted — only when
/// the request matches the transcode it is already running. The seek must
/// match to the exact tick: truncating to whole seconds could bind the
/// acknowledgement to ticks ffmpeg never used. An explicitly different audio
/// stream index must also never claim: the running ffmpeg has the previous
/// track mapped, while the client believes its own selection is playing
/// (a prewarm started with the server's default index and claimed by a player
/// with a preference index played the default track behind an English UI).
/// None means "no opinion" and never blocks a claim. This must hold for every
/// repeated master request, prewarm or not: playlist retries during the
/// readiness window re-send the same parameters and have to be idempotent,
/// while a different value is a genuine change and forces a restart.
fn can_claim_hls_session(
    session_requested_ticks: i64,
    session_audio_index: Option<i32>,
    request_ticks: Option<i64>,
    request_audio_index: Option<i32>,
) -> bool {
    let ticks_match = session_requested_ticks == request_ticks
        .unwrap_or(0)
        .max(0);
    let audio_matches = match (session_audio_index, request_audio_index) {
        (Some(running), Some(requested)) => running == requested,
        _ => true,
    };
    ticks_match && audio_matches
}

/// Ticks to acknowledge as the playback start: the measured keyframe-aligned
/// start once known, otherwise the exact ticks ffmpeg was started with.
fn acknowledged_start_ticks(
    requested_start_ticks: i64,
    actual_start_ticks: Option<i64>,
) -> Option<i64> {
    actual_start_ticks.or((requested_start_ticks > 0).then_some(requested_start_ticks))
}

fn ffprobe_bin() -> String {
    std::env::var("FFPROBE_PATH").unwrap_or_else(|_| "ffprobe".into())
}

/// MPEG-TS timestamps wrap modulo 2^33 ticks of a 90 kHz clock. A copy+seek
/// session's pre-target keyframe packets carry negative PTS (we pass
/// `-avoid_negative_ts disabled`), which mpegts stores wrapped near the top of
/// the range, so ffprobe reports them as a huge positive time.
const MPEGTS_WRAP_SECS: f64 = 8_589_934_592.0 / 90_000.0; // 2^33 / 90000

/// Convert the first video packet's pts_time of a copy+seek session's first
/// segment into a signed offset from the requested start. The input seek may
/// land on a keyframe on either side of the request (container- and
/// demuxer-dependent), so the offset is genuinely signed: a live Matroska
/// session measured its first video packet at +1.483s. Magnitudes beyond two
/// minutes are demuxer noise and clamp to zero (actual == requested).
fn pts_offset_secs(pts_time: f64) -> f64 {
    let unwrapped = if pts_time > MPEGTS_WRAP_SECS / 2.0 {
        pts_time - MPEGTS_WRAP_SECS
    } else {
        pts_time
    };
    if unwrapped.abs() > 120.0 { 0.0 } else { unwrapped }
}

/// Lowest-index segment file currently on disk — the first file ffmpeg
/// produced for this generation (segment-driven restarts bump -start_number,
/// so it is not necessarily segment_00000).
fn first_segment_path(dir: &std::path::Path, use_fmp4: bool) -> Option<std::path::PathBuf> {
    let ext = if use_fmp4 { "m4s" } else { "ts" };
    let mut best: Option<(u32, std::path::PathBuf)> = None;
    for entry in std::fs::read_dir(dir)
        .ok()?
        .flatten()
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(idx) = name
            .strip_prefix("segment_")
            .and_then(|n| n.strip_suffix(&format!(".{ext}")))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if best
            .as_ref()
            .map_or(true, |(best_idx, _)| idx < *best_idx)
        {
            best = Some((idx, entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

/// Parse ffprobe's csv packet output for the first pts_time value. The
/// `csv=p=0` writer terminates the row with a comma (`1.483000,`), which a
/// bare f64 parse rejects.
fn parse_ffprobe_pts_time(stdout: &str) -> Option<f64> {
    stdout
        .lines()
        .find_map(|line| {
            line
                .trim()
                .trim_end_matches(',')
                .parse::<f64>()
                .ok()
        })
}

/// Read the first video packet's pts_time from a segment file. Bounded and
/// local-only: `-read_intervals %+#1` stops after the first packet.
async fn probe_first_video_pts_secs(path: &std::path::Path) -> Option<f64> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(ffprobe_bin())
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "packet=pts_time",
                "-read_intervals",
                "%+#1",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output
        .status
        .success()
    {
        return None;
    }
    parse_ffprobe_pts_time(&String::from_utf8_lossy(&output.stdout))
}

/// Shared session setup: look up or create the transcode session for an HLS
/// request. Returns the session handle and the resolved play_session_id.
async fn create_hls_session(
    state: &AppState,
    auth: &auth::AuthSession,
    id: Uuid,
    q: &api::HlsVideoQuery,
) -> Result<(Arc<tokio::sync::RwLock<TranscodeSession>>, String)> {
    let play_session_id = q
        .play_session_id
        .clone()
        .unwrap_or_else(|| {
            common::get_uuid()
                .as_simple()
                .to_string()
        });

    debug!("Using play session ID: {}", play_session_id);

    let encoding_opts_hls = crate::db::Settings::get_encoding_config(
        &state
            .ctx
            .db,
    )
    .await
    .unwrap_or_default();
    let video_transcode_enabled_hls = encoding_opts_hls
        .enable_video_transcoding
        .unwrap_or(true);
    let video_codec_raw = q
        .video_codec
        .as_deref()
        .unwrap_or("copy");
    let video_codec = if video_codec_raw == "copy" || !video_transcode_enabled_hls {
        "copy".to_string()
    } else {
        "h264".to_string()
    };
    let audio_codec = q
        .audio_codec
        .clone()
        .unwrap_or_else(|| "aac".to_string());
    let segment_length = q
        .segment_length
        .unwrap_or(6) as u32;

    // Look up existing session or create a new one.
    // When the client seeks it sends the same PlaySessionId but with a new
    // StartTimeTicks.  In that case we must stop the old transcode job and
    // restart from the requested position — otherwise the player waits for
    // segments that the old job will never produce at the new offset.
    //
    // Serialize the whole stop/lookup/create/attach sequence per
    // play_session_id: the lookup-or-create path below awaits DB queries and
    // filesystem ops with no lock held, so two requests racing for the same
    // session would otherwise both see no existing transcode and each spawn
    // their own ffmpeg process, with the loser's session silently overwritten
    // (and its ffmpeg process orphaned) by attach_transcode.
    let _create_guard = TRANSCODE_CREATE_LOCKS
        .lock(play_session_id.clone())
        .await;
    let is_seeking = q
        .start_time_ticks
        .is_some_and(|t| t > 0);
    if is_seeking {
        if let Some(existing) = state
            .ctx
            .sessions
            .get_transcode(&play_session_id)
        {
            // A repeated master request carrying the same StartTimeTicks and
            // audio track is a playlist retry, not a seek: claiming must be
            // idempotent, or every retry during the readiness window kills
            // and restarts the transcode and the first segment never
            // materialises. A different tick value is a genuine seek, and a
            // different explicit audio index is a genuine track change —
            // both stop the session and restart from the request. Truncated
            // seconds could acknowledge ticks ffmpeg never used (baked-in
            // subtitle desync).
            let can_claim = {
                let current = existing.read().await;
                can_claim_hls_session(
                    current.requested_start_ticks,
                    current.audio_stream_index,
                    q.start_time_ticks,
                    q.audio_stream_index
                        .filter(|&v| v >= 0),
                )
            };
            if !can_claim {
                debug!(
                    play_session_id = %play_session_id,
                    start_time_ticks = ?q.start_time_ticks,
                    "seek detected — stopping old transcode session and restarting"
                );
                state
                    .ctx
                    .sessions
                    .stop_transcode(&play_session_id)
                    .await;
            }
        }
    }
    let session = if let Some(existing) = state
        .ctx
        .sessions
        .get_transcode(&play_session_id)
    {
        existing
            .read()
            .await
            .prewarm
            .store(q.prewarm.unwrap_or(false), Ordering::Relaxed);
        existing
    } else {
        // Fetch media info to get the stream URL
        let media_source_id = q
            .media_source_id
            .unwrap_or(id);
        let media = db::Media::get_by_id(
            &state
                .ctx
                .db,
            &media_source_id,
        )
        .await?
        .context_not_found("media not found")?;

        let mut resolved_media = media.clone();
        if resolved_media.kind == db::MediaKind::StreamGroup {
            let gid = resolved_media.id;
            let candidates = db::StreamGroup::streams_for(
                &state
                    .ctx
                    .db,
                &gid,
                &id,
            )
            .await?;
            resolved_media = candidates
                .into_iter()
                .next()
                .context_not_found("no streams available for this group")?;
        }
        if matches!(
            resolved_media.kind,
            db::MediaKind::Movie | db::MediaKind::Episode
        ) {
            let sources = resolved_media
                .streams(
                    &state
                        .ctx
                        .db,
                )
                .await?;
            resolved_media = if let Some(wanted) = q.media_source_id {
                sources
                    .iter()
                    .find(|s| s.id == wanted)
                    .cloned()
            } else {
                None
            }
            .or_else(|| {
                sources
                    .into_iter()
                    .next()
            })
            .context_not_found("no playable source found")?;
        } else if resolved_media.kind == db::MediaKind::Track {
            let sources = resolved_media
                .streams(
                    &state
                        .ctx
                        .db,
                )
                .await?;
            resolved_media = sources
                .into_iter()
                .next()
                .context_not_found("no stream found for track")?;
        }

        let input_url = resolved_media
            .stream_info
            .as_ref()
            .map(|si| {
                si.descriptor
                    .server_input(
                        resolved_media.id,
                        state
                            .ctx
                            .config
                            .port,
                    )
            })
            .context_not_found("media source has no URL")?;

        // Kill any earlier startup transcode this device left running for the
        // same channel before we spawn a new ffmpeg. IPTV proxies allow one
        // reader per channel; a stale ffmpeg holding that connection would 503
        // this one and neither could produce segments (the per-PlaySessionId
        // create-lock doesn't cover a client that rotates PlaySessionIds).
        state
            .ctx
            .sessions
            .reap_competing_startups(&auth.device.id, &input_url, &play_session_id)
            .await;

        let output_dir =
            std::path::PathBuf::from("transcode_sessions").join(&play_session_id);
        // Keep the API stable (no RunId in URLs) by reusing one on-disk path per
        // PlaySessionId and clearing stale segments when a transcode restarts.
        let _ = std::fs::remove_dir_all(&output_dir);
        // Use the URL-path item (`id`) to determine liveness so that a specific
        // stream source (MediaSourceId = stream child, kind = Stream) doesn't
        // incorrectly produce is_live = false for live-TV sessions.
        let parent_is_tv_channel = if id != media_source_id {
            let db = &state
                .ctx
                .db;
            match db::Media::get_by_id(db, &id).await {
                Ok(opt) => opt.map_or(false, |m| m.kind == db::MediaKind::TvChannel),
                Err(e) => {
                    warn!(err = %e, item_id = %id, "failed to look up parent media for is_live; treating as not-live");
                    false
                }
            }
        } else {
            false
        };
        let is_live =
            resolved_media.kind == db::MediaKind::TvChannel || parent_is_tv_channel;

        // --- Why we force audio transcoding for live channels ---
        //
        // IPTV/broadcast streams frequently carry AAC encoded in LATM format
        // (Low-overhead MPEG-4 Audio Transport Multiplex). ffprobe identifies
        // LATM streams as codec "aac" but reports sample_rate=0 because the
        // sample rate is stored implicitly inside the AudioSpecificConfig
        // bitstream rather than in an ADTS header. When ffmpeg copies these
        // bits into an HLS/TS segment without re-encoding, the resulting
        // segment still contains LATM-framed audio.
        //
        // Native clients (Swiftfin, Streamyfin, VLC) decode via OS-level
        // hardware decoders that handle LATM transparently. Safari running
        // hls.js goes through the browser's Media Source Extensions (MSE)
        // API, which only accepts standard ADTS-framed AAC. Feeding it LATM
        // produces a silent or immediately-failed decode — the segment HTTP
        // response is 200 and the bytes arrive, but the player can't present
        // any video.
        //
        // The Jellyfin Web device profile declares MaxAudioChannels=6 and
        // lists "aac" as a supported codec without any channel-count or
        // sample-rate codec-profile conditions, so our PlaybackInfo decision
        // layer has no basis on which to reject copy — it looks like a
        // perfectly legal copy of a supported codec. Jellyfin server has a
        // SampleRate<=0 guard in CanStreamCopyAudio, but that guard is gated
        // on the client explicitly requesting an AudioSampleRate, which
        // Jellyfin Web does not do; so Jellyfin server has the same gap.
        //
        // Workaround: always re-encode audio to standard ADTS AAC stereo for
        // live channels, regardless of what the client negotiated. The
        // existing audio_channels logic (None for copy, Some(2) for transcode)
        // then kicks in automatically and produces the correct stereo downmix.
        let audio_codec = resolve_live_audio_codec(is_live, &audio_codec);

        // Live streams have no fixed duration — skip all runtime lookups.
        let (runtime_ticks, runtime_is_probed) = if is_live {
            (0, false)
        } else {
            // A successful probe describes the selected file and is authoritative.
            // Stored runtime is item metadata and may describe another cut.
            let stored_ticks = resolved_media
                .runtime
                .or(media.runtime)
                .filter(|&r| r > 0)
                .and_then(|r| r.to_ticks(TickUnit::Seconds));
            let probe_ticks = resolved_media
                .probe_data
                .as_ref()
                .and_then(|p| p.run_time_ticks)
                .filter(|&t| t > 0);
            let rt = probe_ticks.or(stored_ticks);
            let runtime_ticks = match rt {
                Some(t) if t > 0 => t,
                _ => db::Media::get_by_id(
                    &state
                        .ctx
                        .db,
                    &id,
                )
                .await
                .ok()
                .flatten()
                .and_then(|m| m.runtime)
                .filter(|&r| r > 0)
                .and_then(|r| r.to_ticks(TickUnit::Seconds))
                .unwrap_or(0),
            };
            (runtime_ticks, probe_ticks.is_some())
        };
        debug!(runtime_ticks, is_live, segment_length, "transcode session");
        let source_video_stream = resolved_media
            .probe_data
            .as_ref()
            .and_then(|p| p.video_stream());
        let source_video_codec = source_video_stream
            .as_ref()
            .and_then(|s| {
                s.codec
                    .clone()
            });
        let source_video_profile = source_video_stream
            .as_ref()
            .and_then(|s| {
                s.profile
                    .clone()
            });
        let source_video_level = source_video_stream
            .as_ref()
            .and_then(|s| s.level);
        let source_video_range_type = source_video_stream
            .as_ref()
            .and_then(|s| s.video_range_type);
        let source_video_width = source_video_stream
            .as_ref()
            .and_then(|s| s.width);
        let source_video_height = source_video_stream
            .as_ref()
            .and_then(|s| s.height);
        let source_frame_rate = source_video_stream
            .as_ref()
            .and_then(|s| s.real_frame_rate);
        debug!(
            ?source_video_codec,
            ?source_video_profile,
            ?source_video_level,
            ?source_video_range_type,
            source_video_width,
            source_video_height,
            source_frame_rate,
            "source video codec for HLS session"
        );
        let source_audio_stream = resolved_media
            .probe_data
            .as_ref()
            .and_then(|p| p.audio_stream());
        let source_audio_codec = source_audio_stream.and_then(|s| {
            s.codec
                .clone()
        });
        let trusted_probe_data = !is_live
            && resolved_media
                .probe_data
                .as_ref()
                .is_some_and(|probe| probe.video_stream().is_some());
        let burn_subtitle =
            q.subtitle_method == Some(api::SubtitleDeliveryMethod::Encode);
        let session = TranscodeSession::new(
            play_session_id.clone(),
            id,
            media_source_id,
            input_url.clone(),
            output_dir,
            video_codec.clone(),
            audio_codec.clone(),
            q.audio_stream_index
                .map(|v| v as i32)
                .filter(|&v| v >= 0),
            q.subtitle_stream_index
                .map(|v| v as i32),
            burn_subtitle,
            segment_length,
            // Parse reasons from query param (set by playbackinfo on the transcoding URL)
            q.transcode_reasons
                .as_deref()
                .map(api::TranscodeReasons::from_query_value)
                .unwrap_or_default(),
            runtime_ticks,
            runtime_is_probed,
            q.prewarm.unwrap_or(false),
            is_live,
            source_video_codec,
            source_audio_codec,
            source_video_profile,
            source_video_level,
            source_video_range_type,
            source_video_width,
            source_video_height,
            source_frame_rate,
        );

        // Record the exact seek this ffmpeg process will perform before the
        // session becomes visible to other requests — the prewarm claim check
        // and the start acknowledgement read this value.
        session
            .write()
            .await
            .requested_start_ticks = q
            .start_time_ticks
            .unwrap_or(0)
            .max(0);

        state
            .ctx
            .sessions
            .attach_transcode(&play_session_id, session.clone());

        // Start transcoding in background
        let session_clone = session.clone();
        let encoding_opts = encoding_opts_hls.clone();
        let params = crate::playback::engine::TranscodeParams {
            input_url,
            output_dir: session
                .read()
                .await
                .output_dir
                .clone(),
            video_codec: video_codec.clone(),
            audio_codec: audio_codec.clone(),
            segment_length,
            start_time_ticks: q.start_time_ticks,
            hls_start_number: 0,
            max_width: q
                .max_width
                .map(|v| v as u32),
            max_height: q
                .max_height
                .map(|v| v as u32),
            video_bitrate: source_video_stream
                .and_then(|s| s.bit_rate)
                .map(|b| {
                    let source = b as u32;
                    let target = q
                        .video_bit_rate
                        .map_or(source, |v| source.min(v as u32));
                    q.max_streaming_bitrate
                        .map_or(target, |c| target.min(c as u32))
                }),
            audio_bitrate: q
                .audio_bit_rate
                .map(|v| v as u32),
            // Force stereo downmix when transcoding audio — multi-channel AAC
            // (e.g. 6.1 from DTS-HD) causes MEDIA_ERR_SRC_NOT_SUPPORTED on most
            // browsers and iOS Safari.
            audio_channels: if audio_codec == "copy" { None } else { Some(2) },
            audio_stream_index: q
                .audio_stream_index
                .map(|v| v as i32)
                .filter(|&v| v >= 0),
            subtitle_stream_index: q
                .subtitle_stream_index
                .map(|v| v as i32),
            burn_subtitle,
            subtitle_width: None,
            subtitle_height: None,
            encoding_preset: encoding_opts.encoding_preset,
            source_video_codec: session
                .read()
                .await
                .source_video_codec
                .clone(),
            source_audio_codec: session
                .read()
                .await
                .source_audio_codec
                .clone(),
            trusted_probe_data,
            source_frame_rate,
            hardware_acceleration_type: encoding_opts
                .hardware_acceleration_type
                .unwrap_or_default(),
            vaapi_device: encoding_opts
                .vaapi_device
                .unwrap_or_else(|| "/dev/dri/renderD128".to_string()),
            vaapi_driver: encoding_opts
                .vaapi_driver
                .unwrap_or_default(),
            source_video_range_type,
            enable_tonemapping: encoding_opts
                .enable_tonemapping
                .unwrap_or(false),
            enable_vpp_tonemapping: encoding_opts
                .enable_vpp_tonemapping
                .unwrap_or(false),
            tonemapping_algorithm: encoding_opts
                .tonemapping_algorithm
                .unwrap_or_else(|| "hable".to_string()),
            tonemapping_desat: encoding_opts
                .tonemapping_desat
                .unwrap_or(0.0),
            tonemapping_peak: encoding_opts
                .tonemapping_peak
                .unwrap_or(0.0),
            allow_hevc_encoding: encoding_opts
                .allow_hevc_encoding
                .unwrap_or(false),
            allow_av1_encoding: encoding_opts
                .allow_av1_encoding
                .unwrap_or(false),
            h264_crf: encoding_opts
                .h264_crf
                .unwrap_or(23),
            h265_crf: encoding_opts
                .h265_crf
                .unwrap_or(28),
            is_live,
            normalize_audio_loudness: encoding_opts
                .normalize_audio_loudness
                .unwrap_or(false),
        };

        // Spawn the transcode task with proper error handling
        let media_title_for_log = resolved_media
            .title
            .clone();
        let transcode_reasons_for_log = q
            .transcode_reasons
            .clone();
        let log_user = auth
            .user
            .username
            .clone();
        let log_client = auth
            .device
            .app_name
            .clone();
        let play_session_id_for_log = play_session_id.clone();
        let session_clone = session.clone();
        tokio::spawn(async move {
            let play_session_id = play_session_id_for_log;
            let start_secs = params
                .start_time_ticks
                .unwrap_or(0)
                / 10_000_000;
            let resolution = match (params.max_width, params.max_height) {
                (Some(w), Some(h)) => format!("{}x{}", w, h),
                (Some(w), None) => format!("{}w", w),
                (None, Some(h)) => format!("{}h", h),
                _ => "native".to_string(),
            };
            info!(
                play_session_id = %play_session_id,
                title = %media_title_for_log,
                user = %log_user,
                client = %log_client,
                video_codec = %params.video_codec,
                audio_codec = %params.audio_codec,
                resolution,
                video_bitrate = ?params.video_bitrate,
                hw_accel = ?params.hardware_acceleration_type,
                transcode_reasons = ?transcode_reasons_for_log,
                start_secs,
                "▶ Playback started (transcode)"
            );
            if let Err(e) =
                crate::playback::engine::start_transcode(session_clone, params).await
            {
                error!("Transcode failed: {:#}", e);
            }
        });

        session
    };

    Ok((session, play_session_id))
}

#[get("/videos/{id}/master.m3u8")]
pub async fn master_hls_video(
    State(state): State<AppState>,
    auth: auth::AuthSession,
    Path(id): Path<Uuid>,
    Query(q): Query<api::HlsVideoQuery>,
) -> Result<impl IntoResponse> {
    debug!("master_hls_video: item_id={}, q={:?}", id, q);
    let (session, _) = create_hls_session(&state, &auth, id, &q).await?;

    // The master is served immediately: holding it until ffmpeg produced a
    // measurable first segment made every start and recovery wait on the
    // slowest part of the pipeline. The measured actual start reaches clients
    // asynchronously through the hls-playback-start endpoint (and through
    // this acknowledgement on later master loads).
    // Acknowledge the session's stored start, not this request's ticks: for a
    // claimed prewarm these are the exact ticks ffmpeg was started with, and
    // once measured they are the keyframe-aligned actual start.
    let acknowledged_ticks = {
        let s = session
            .read()
            .await;
        acknowledged_start_ticks(s.requested_start_ticks, s.actual_start_ticks)
    };

    let session_read = session
        .read()
        .await;
    let master_playlist = add_playback_start_acknowledgement(
        add_playback_generation_to_master(
            crate::playback::engine::generate_master_playlist(&session_read),
            q.start_time_ticks,
        ),
        acknowledged_ticks,
    );
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/vnd.apple.mpegurl")
        .header("Cache-Control", "no-cache, no-store")
        .header(
            "X-Remux-Playback-Start-Ticks",
            acknowledged_ticks
                .unwrap_or(0)
                .to_string(),
        )
        .header(
            "X-Remux-Prewarm-Lease-Seconds",
            if q.prewarm.unwrap_or(false) {
                PREWARM_LEASE_SECS.to_string()
            } else {
                "0".to_string()
            },
        )
        .body(Body::from(master_playlist))
        .unwrap())
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HlsPlaybackStartQuery {
    #[serde(alias = "playSessionId")]
    pub play_session_id: Option<String>,
}

/// Playback-start acknowledgement for a running HLS session. The master
/// playlist is served without waiting for ffmpeg output, so clients poll this
/// after the manifest loads: for copy+seek sessions it measures the
/// keyframe-aligned actual start from the first produced segment once one
/// exists and reports it; everything else echoes the requested ticks. A null
/// `actualStartTicks` means "not measured yet", never an error.
#[get("/videos/{id}/hls-playback-start")]
pub async fn hls_playback_start(
    State(state): State<AppState>,
    _session: auth::AuthSession,
    Path(_id): Path<Uuid>,
    Query(q): Query<HlsPlaybackStartQuery>,
) -> Result<impl IntoResponse> {
    let Some(play_session_id) = q
        .play_session_id
        .filter(|id| !id.is_empty())
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let Some(session) = state
        .ctx
        .sessions
        .get_transcode(&play_session_id)
    else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let measurement = {
        let s = session
            .read()
            .await;
        if s.requested_start_ticks > 0
            && s.video_codec == "copy"
            && !s.is_live
            && s.actual_start_ticks.is_none()
        {
            Some((s.requested_start_ticks, s.output_dir.clone(), s.use_fmp4()))
        } else {
            None
        }
    };
    if let Some((requested_ticks, dir, use_fmp4)) = measurement {
        // Cheap spot measurement only: the master is no longer gated on this,
        // so a session with no segment yet answers null rather than waiting.
        if let Some(path) = first_segment_path(&dir, use_fmp4) {
            for attempt in 0..3 {
                if let Some(pts_time) = probe_first_video_pts_secs(&path).await {
                    let ticks = requested_ticks
                        + (pts_offset_secs(pts_time) * 10_000_000.0).round() as i64;
                    session
                        .write()
                        .await
                        .actual_start_ticks = Some(ticks);
                    info!(
                        play_session_id = %play_session_id,
                        actual_start_ticks = ticks,
                        "measured keyframe-aligned stream start"
                    );
                    break;
                }
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
        }
    }
    let (requested_start_ticks, actual_start_ticks) = {
        let s = session
            .read()
            .await;
        (s.requested_start_ticks, s.actual_start_ticks)
    };
    Ok(axum::Json(serde_json::json!({
        "remux": {
            "requestedStartTicks": requested_start_ticks,
            "actualStartTicks": actual_start_ticks,
        }
    })).into_response())
}

/// Safari/iOS live TV endpoint: creates the transcode session and returns the
/// variant playlist directly so the player gets segment URLs without a
/// master→variant redirect.
#[get("/videos/{id}/live.m3u8")]
pub async fn live_hls_video(
    State(state): State<AppState>,
    auth: auth::AuthSession,
    Path(id): Path<Uuid>,
    Query(mut q): Query<api::HlsVideoQuery>,
) -> Result<impl IntoResponse> {
    debug!("live_hls_video: item_id={}, q={:?}", id, q);
    let (_, play_session_id) = create_hls_session(&state, &auth, id, &q).await?;
    q.play_session_id = Some(play_session_id);
    variant_hls_video_inner(state, q).await
}

/// Variant HLS playlist - alternate URL used by some clients.
#[get("/videos/{id}/main.m3u8")]
pub async fn variant_hls_video_alt(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<api::HlsVideoQuery>,
) -> Result<impl IntoResponse> {
    variant_hls_video_inner(state, q).await
}

/// Serves the variant (child) HLS playlist generated by the transcoding engine.
#[get("/videos/{id}/main/stream.m3u8")]
pub async fn variant_hls_video(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<api::HlsVideoQuery>,
) -> Result<impl IntoResponse> {
    variant_hls_video_inner(state, q).await
}

/// Returns the audio codec to use for HLS transcoding.
///
/// Live IPTV sources often carry LATM-encoded AAC (see the large comment block
/// inside `create_hls_session`). When the client has negotiated `copy` for a
/// live channel we override it to `aac` so ffmpeg re-encodes to standard ADTS
/// stereo, which MSE-based players (Safari + hls.js) can actually decode.
fn resolve_live_audio_codec(is_live: bool, requested: &str) -> String {
    if is_live && requested == "copy" {
        "aac".to_string()
    } else {
        requested.to_string()
    }
}

async fn variant_hls_video_inner(
    state: AppState,
    q: api::HlsVideoQuery,
) -> Result<impl IntoResponse> {
    let play_session_id = q
        .play_session_id
        .context_not_found("PlaySessionId is required")?;

    let Some(session) = state
        .ctx
        .sessions
        .get_transcode(&play_session_id)
    else {
        return Ok(hls_state_response(
            StatusCode::GONE,
            "session-gone",
            None,
        ));
    };

    // Keep the session alive.
    state
        .ctx
        .sessions
        .ping(&play_session_id);

    let session_read = session
        .read()
        .await;
    let is_live = session_read.is_live;
    let playlist_path = session_read.variant_playlist_path();
    let session_created_at = session_read.created_at;
    let psid = session_read
        .id
        .clone();

    // FFmpeg is authoritative for every HLS segment boundary. Source keyframes
    // routinely make real segment durations differ from the requested target,
    // so a synthetic playlist can reference segments FFmpeg will never create.
    {
        drop(session_read);
        // For live streams, serve the ffmpeg-written EVENT playlist directly.
        // For fMP4 VOD, also use ffmpeg's playlist because fMP4 segments snap to
        // keyframe boundaries so actual durations differ from our target.
        // For resumed TS-HLS sessions, ffmpeg's playlist carries the fresh
        // local-zero timeline expected by clients that acknowledged
        // StartTimeTicks. Segment-driven recovery can still preserve a
        // requested local MEDIA-SEQUENCE.
        // Poll until ffmpeg has opened the playlist and written its header:
        // the muxer emits #EXT-X-TARGETDURATION at open, while the first
        // #EXTINF only appears when the first full segment closes. Clients
        // poll EVENT playlists, so serving the header lets the decoder mount
        // one segment earlier without inventing boundaries ffmpeg will never
        // create.
        let playlist_wait_started_at = std::time::Instant::now();
        let content = wait_for_variant_playlist(
            &session,
            &playlist_path,
            std::time::Duration::from_secs(15),
        )
        .await;

        // FFmpeg has not opened a playlist within the bounded wait.
        // A 200 with an empty body is a terminal parser failure for Media3 and
        // other strict HLS clients; a 503 lets the client's playlist loader
        // back off and retry until the transcode writes its header.
        if content.is_empty() {
            let session_state = session
                .read()
                .await
                .state
                .clone();
            warn!(
                play_session_id = %play_session_id,
                session_age_ms = session_created_at.elapsed().as_millis(),
                playlist_wait_ms = playlist_wait_started_at.elapsed().as_millis(),
                playlist_path = %playlist_path.display(),
                ?session_state,
                "HLS child playlist not ready before startup deadline"
            );
            return Ok(if matches!(
                session_state,
                TranscodeState::Complete | TranscodeState::Error(_)
            ) {
                hls_state_response(StatusCode::GONE, "transcode-ended", None)
            } else {
                hls_state_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "playlist-not-ready",
                    Some("1"),
                )
            });
        }

        info!(
            play_session_id = %play_session_id,
            session_age_ms = session_created_at.elapsed().as_millis(),
            playlist_wait_ms = playlist_wait_started_at.elapsed().as_millis(),
            playlist_bytes = content.len(),
            has_first_segment = content.contains("#EXTINF:"),
            "HLS child playlist ready"
        );

        // For non-live VOD sessions: once ffmpeg finishes it appends
        // #EXT-X-ENDLIST and the playlist type stays as EVENT. Upgrade
        // EVENT→VOD so hls.js treats the stream as a completed VOD rather than
        // a live feed; leave live streams untouched.
        let is_complete = !is_live && content.contains("#EXT-X-ENDLIST");

        // Include the acknowledged source offset in every media URL. A managed
        // seek can intentionally reuse its PlaySessionId, but the bytes behind
        // segment_00000 are then a different generation. Segment responses are
        // cacheable for completed playback, so reusing the old URL would let
        // hls.js or the browser replay the previous generation after a seek.
        let generation_suffix = q
            .start_time_ticks
            .filter(|ticks| *ticks > 0)
            .map(|ticks| format!("&StartTimeTicks={ticks}"))
            .unwrap_or_default();

        // Inject ?PlaySessionId=... into segment/map lines so hls_segment_inner can find the session.
        let content = content
            .lines()
            .map(|line| {
                if !line.starts_with('#')
                    && (line.ends_with(".ts") || line.ends_with(".m4s"))
                {
                    format!(
                        "{}?PlaySessionId={}{}",
                        line, psid, generation_suffix
                    )
                } else if line.starts_with("#EXT-X-MAP:")
                    && !line.contains("PlaySessionId")
                {
                    // Inject PlaySessionId into the fMP4 init segment URI.
                    // e.g. #EXT-X-MAP:URI="init.mp4" → #EXT-X-MAP:URI="init.mp4?PlaySessionId=…"
                    line.replace(
                        "\"init.mp4\"",
                        &format!(
                            "\"init.mp4?PlaySessionId={}{}\"",
                            psid, generation_suffix
                        ),
                    )
                } else if is_complete && line == "#EXT-X-PLAYLIST-TYPE:EVENT" {
                    "#EXT-X-PLAYLIST-TYPE:VOD".to_string()
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/vnd.apple.mpegurl")
            .header("Cache-Control", "no-cache, no-store")
            .body(Body::from(content))
            .unwrap());
    }
}

#[cfg(test)]
mod tests {
    use http::StatusCode;

    use super::{
        add_playback_generation_to_master, add_playback_start_acknowledgement,
        hls_state_response,
    };

    #[test]
    fn master_playlist_acknowledges_applied_start_ticks() {
        let playlist = add_playback_start_acknowledgement(
            "#EXTM3U\n#EXT-X-VERSION:3\n".to_string(),
            Some(1_543_905_219),
        );
        assert!(playlist.contains(
            "#EXT-X-SESSION-DATA:DATA-ID=\"com.remux.playback-start-ticks\",VALUE=\"1543905219\""
        ));
    }

    #[test]
    fn master_playlist_omits_zero_start_acknowledgement() {
        let playlist = "#EXTM3U\n#EXT-X-VERSION:3\n".to_string();
        assert_eq!(
            add_playback_start_acknowledgement(playlist.clone(), Some(0)),
            playlist
        );
    }

    #[test]
    fn master_playlist_versions_child_uri_for_offset_generation() {
        let playlist = add_playback_generation_to_master(
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nmain.m3u8?PlaySessionId=session\n"
                .to_string(),
            Some(3_200_000_000),
        );
        assert!(playlist.contains(
            "main.m3u8?PlaySessionId=session&StartTimeTicks=3200000000"
        ));
    }

    #[test]
    fn master_playlist_keeps_initial_child_uri_stable() {
        let playlist =
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nmain.m3u8?PlaySessionId=session\n"
                .to_string();
        assert_eq!(
            add_playback_generation_to_master(playlist.clone(), None),
            playlist
        );
    }

    #[test]
    fn live_channel_forces_aac_over_copy() {
        assert_eq!(super::resolve_live_audio_codec(true, "copy"), "aac");
        assert_eq!(super::resolve_live_audio_codec(false, "copy"), "copy");
        assert_eq!(super::resolve_live_audio_codec(true, "aac"), "aac");
        assert_eq!(super::resolve_live_audio_codec(true, "ac3"), "ac3");
    }

    #[test]
    fn hls_state_response_exposes_retry_and_terminal_semantics() {
        let retry = hls_state_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "segment-not-ready",
            Some("1"),
        );
        assert_eq!(retry.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(retry.headers()["Retry-After"], "1");
        assert_eq!(retry.headers()["X-Remux-Hls-State"], "segment-not-ready");

        let gone = hls_state_response(StatusCode::GONE, "session-gone", None);
        assert_eq!(gone.status(), StatusCode::GONE);
        assert_eq!(gone.headers()["X-Remux-Hls-State"], "session-gone");
        assert!(!gone.headers().contains_key("Retry-After"));
    }

    #[test]
    fn session_claim_requires_exact_tick_match() {
        // Exact match: claim allowed, prewarm or not — playlist retries are
        // idempotent and must never restart the transcode.
        assert!(super::can_claim_hls_session(1_000_000_000, Some(2), Some(1_000_000_000), Some(2)));
        // A sub-second difference must NOT claim — ffmpeg was started with
        // different ticks than the acknowledgement would report; this is a
        // genuine seek and restarts the session.
        assert!(!super::can_claim_hls_session(1_000_000_000, Some(2), Some(1_000_000_001), Some(2)));
        assert!(!super::can_claim_hls_session(1_000_400_000, None, Some(1_000_900_000), None));
        // No seek in the request: claim only matches a from-start session.
        assert!(super::can_claim_hls_session(0, None, None, None));
        assert!(!super::can_claim_hls_session(1_000_000_000, None, None, None));
        // A different explicit audio index is a genuine track change, never a
        // claim: the running ffmpeg keeps the old track mapped otherwise.
        assert!(!super::can_claim_hls_session(1_000_000_000, Some(1), Some(1_000_000_000), Some(2)));
        assert!(super::can_claim_hls_session(1_000_000_000, Some(2), Some(1_000_000_000), Some(2)));
        // No opinion on either side never blocks a claim.
        assert!(super::can_claim_hls_session(1_000_000_000, None, Some(1_000_000_000), Some(2)));
        assert!(super::can_claim_hls_session(1_000_000_000, Some(1), Some(1_000_000_000), None));
    }

    #[test]
    fn acknowledged_start_prefers_measured_actual() {
        // From-start session: nothing to acknowledge.
        assert_eq!(super::acknowledged_start_ticks(0, None), None);
        // Unmeasured seek: fall back to the exact requested ticks.
        assert_eq!(
            super::acknowledged_start_ticks(1_000_000_000, None),
            Some(1_000_000_000)
        );
        // Measured keyframe-aligned start wins over the requested ticks.
        assert_eq!(
            super::acknowledged_start_ticks(1_000_000_000, Some(985_000_000)),
            Some(985_000_000)
        );
    }

    #[test]
    fn pts_offset_unwraps_negative_mpegts_timestamps() {
        // Plain negative PTS (keyframe before the request): used as-is.
        assert_eq!(super::pts_offset_secs(-1.5), -1.5);
        // A keyframe exactly at the requested time measures zero.
        assert_eq!(super::pts_offset_secs(0.0), 0.0);
        // Positive PTS (keyframe after the request, seen on live Matroska):
        // signed and used as-is.
        assert_eq!(super::pts_offset_secs(1.483), 1.483);
        assert_eq!(super::pts_offset_secs(0.04), 0.04);
        // TS-wrapped negative PTS: -1.5s stored modulo 2^33/90000.
        let wrapped = super::MPEGTS_WRAP_SECS - 1.5;
        let offset = super::pts_offset_secs(wrapped);
        assert!((offset - -1.5).abs() < 1e-6, "offset: {offset}");
        // Implausible magnitudes are demuxer noise and clamp to zero.
        assert_eq!(super::pts_offset_secs(121.0), 0.0);
        assert_eq!(super::pts_offset_secs(-121.0), 0.0);
    }

    #[test]
    fn ffprobe_pts_time_parses_csv_trailing_comma() {
        assert_eq!(super::parse_ffprobe_pts_time("1.483000,\n"), Some(1.483));
        assert_eq!(super::parse_ffprobe_pts_time("95443.7,\n"), Some(95443.7));
        assert_eq!(super::parse_ffprobe_pts_time(""), None);
        assert_eq!(super::parse_ffprobe_pts_time("N/A\n"), None);
    }

    #[test]
    fn first_segment_picks_lowest_index_of_expected_format() {
        let dir = std::env::temp_dir().join(format!(
            "remux-hls-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["segment_00007.ts", "segment_00003.ts", "segment_00012.m4s"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        // TS session: lowest .ts wins; .m4s is a foreign format and ignored.
        let found = super::first_segment_path(&dir, false).unwrap();
        assert!(found.ends_with("segment_00003.ts"));
        // fMP4 session sees only the .m4s file.
        let found = super::first_segment_path(&dir, true).unwrap();
        assert!(found.ends_with("segment_00012.m4s"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Serves individual HLS segment files.
/// The full filename is retained so fMP4 remains fMP4 during disk recovery.
#[get("/videos/{id}/main/{segment_file}")]
pub async fn hls_segment(
    State(state): State<AppState>,
    Path((id, segment_file)): Path<(Uuid, String)>,
    Query(q): Query<api::HlsVideoQuery>,
) -> Result<impl IntoResponse> {
    hls_segment_inner(state, segment_file, q).await
}

/// Segment route at the same level as main.m3u8 — browsers resolve bare
/// segment filenames relative to the variant playlist URL.
#[get("/videos/{id}/{segment_file}")]
pub async fn hls_segment_flat(
    State(state): State<AppState>,
    Path((id, segment_file)): Path<(Uuid, String)>,
    Query(q): Query<api::HlsVideoQuery>,
) -> Result<impl IntoResponse> {
    hls_segment_inner(state, segment_file, q).await
}

/// Jellyfin-compatible HLS segment route: /Videos/{id}/hls1/{playlistId}/{segmentFile}
#[get("/videos/{id}/hls1/{playlist_id}/{segment_file}")]
pub async fn hls1_segment(
    State(state): State<AppState>,
    Path((id, _playlist_id, segment_file)): Path<(Uuid, String, String)>,
    Query(q): Query<api::HlsVideoQuery>,
) -> Result<impl IntoResponse> {
    hls_segment_inner(state, segment_file, q).await
}

fn hls_state_response(
    status: StatusCode,
    state: &'static str,
    retry_after: Option<&'static str>,
) -> Response<Body> {
    let mut response = Response::builder()
        .status(status)
        .header("Cache-Control", "no-store")
        .header("X-Remux-Hls-State", state);
    if let Some(retry_after) = retry_after {
        response = response.header("Retry-After", retry_after);
    }
    response
        .body(Body::empty())
        .unwrap()
}

/// Find the highest segment index currently on disk in `dir`.
fn get_current_transcoding_index(dir: &std::path::Path) -> Option<u32> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut max_idx: Option<u32> = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Accept both MPEG-TS (.ts) and fMP4 (.m4s) segment files.
        if let Some(idx_str) = name
            .strip_suffix(".ts")
            .or_else(|| name.strip_suffix(".m4s"))
            .and_then(|s| {
                s.rsplit('_')
                    .next()
            })
        {
            if let Ok(idx) = idx_str.parse::<u32>() {
                max_idx = Some(max_idx.map_or(idx, |m: u32| m.max(idx)));
            }
        }
    }
    max_idx
}

async fn hls_segment_inner(
    state: AppState,
    segment_file: String,
    q: api::HlsVideoQuery,
) -> Result<impl IntoResponse> {
    let play_session_id = q
        .play_session_id
        .context_not_found("PlaySessionId is required")?;

    trace!(
        segment_file = %segment_file,
        play_session_id = %play_session_id,
        runtime_ticks = ?q.runtime_ticks,
        "HLS segment request"
    );

    let session = state
        .ctx
        .sessions
        .get_transcode(&play_session_id);

    if segment_file == "init.mp4" {
        let init_path = match &session {
            Some(s) => s
                .read()
                .await
                .init_segment_path(),
            None => state
                .ctx
                .sessions
                .hls_file_path(&play_session_id, "init.mp4"),
        };
        // Wait briefly for ffmpeg to write the init segment.
        if let Some(active) = session.as_ref() {
            wait_for_transcode_path(
                active,
                &init_path,
                std::time::Duration::from_secs(10),
            )
            .await;
        }
        if !init_path.exists() {
            return Ok(match &session {
                Some(session)
                    if matches!(
                        session
                            .read()
                            .await
                            .state,
                        TranscodeState::Complete | TranscodeState::Error(_)
                    ) =>
                {
                    hls_state_response(StatusCode::GONE, "transcode-ended", None)
                }
                Some(_) => hls_state_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "init-segment-not-ready",
                    Some("1"),
                ),
                None => hls_state_response(StatusCode::GONE, "session-gone", None),
            });
        }
        state
            .ctx
            .sessions
            .ping(&play_session_id);
        let file = tokio::fs::File::open(&init_path).await?;
        let stream = ReaderStream::new(file);
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "video/mp4")
            .header("Cache-Control", "public, max-age=86400")
            .body(Body::from_stream(stream))
            .unwrap());
    }

    let Some(segment) = HlsSegmentFile::parse(&segment_file) else {
        return Ok(hls_state_response(
            StatusCode::NOT_FOUND,
            "unknown-segment",
            None,
        ));
    };

    // Derive the segment path — either from the live session or from the base
    // dir directly (handles server restart where session is gone but files remain).
    let segment_path = match &session {
        Some(s) => {
            let s = s
                .read()
                .await;
            let expected_format = if s.use_fmp4() {
                HlsSegmentFormat::FragmentedMp4
            } else {
                HlsSegmentFormat::MpegTs
            };
            if segment.format() != expected_format {
                return Ok(hls_state_response(
                    StatusCode::NOT_FOUND,
                    "segment-format-mismatch",
                    None,
                ));
            }
            segment.path_in(&s.output_dir)
        }
        None => state
            .ctx
            .sessions
            .hls_file_path(&play_session_id, segment.filename()),
    };

    let requested_idx = segment.index();

    if let Some(ref session) = session {
        // Update playback position for the buffer monitor.
        use std::sync::atomic::Ordering;
        let s = session
            .read()
            .await;
        let prev = s
            .last_segment_index
            .load(Ordering::Relaxed);
        if requested_idx > prev {
            s.last_segment_index
                .store(requested_idx, Ordering::Relaxed);
        }
    }

    // If the segment doesn't exist and we have a live session, check whether
    // FFmpeg needs to be restarted at a different position (like Jellyfin does).
    if !segment_path.exists() {
        if let Some(session) = &session {
            let s = session
                .read()
                .await;
            let output_dir = s
                .output_dir
                .clone();
            let segment_length = s.segment_length;
            let current_idx = get_current_transcoding_index(&output_dir);
            let segment_gap_threshold = 24 / segment_length;

            let needs_restart = match current_idx {
                None => {
                    // No segments on disk yet. If FFmpeg is still running
                    // (Starting/Running), just fall through to the wait loop —
                    // killing it here causes an infinite restart cycle.
                    matches!(
                        s.state,
                        TranscodeState::Error(_) | TranscodeState::Complete
                    )
                }
                Some(cur) if requested_idx < cur => true, // seeking backward
                Some(cur)
                    if requested_idx.saturating_sub(cur) > segment_gap_threshold =>
                {
                    true
                } // too far ahead
                _ => false, // within range — just wait for FFmpeg
            };

            if needs_restart {
                // Guard against concurrent restart: only proceed if FFmpeg
                // is actually running (kill_tx is Some). If another request
                // already killed it and started a new one, just wait.
                let has_running_ffmpeg = s
                    .kill_tx
                    .is_some();
                if !has_running_ffmpeg {
                    drop(s);
                    // Another request already restarted — fall through to wait loop.
                } else {
                    debug!(
                        requested_idx,
                        ?current_idx,
                        segment_gap_threshold,
                        "Segment-driven transcode restart"
                    );

                    // Gather params we need before dropping the read lock.
                    let input_url = s
                        .input_url
                        .clone();
                    let video_codec = s
                        .video_codec
                        .clone();
                    let audio_codec = s
                        .audio_codec
                        .clone();
                    let audio_stream_index = s.audio_stream_index;
                    let subtitle_stream_index = s.subtitle_stream_index;
                    let burn_subtitle = s.burn_subtitle;
                    let session_start_secs = s.start_time_secs;
                    drop(s);

                    // Kill running FFmpeg and clean up stale segments (params
                    // like bitrate/codec may change, so old segments are invalid).
                    {
                        let (kill_tx, wait_done) = {
                            let mut s = session
                                .write()
                                .await;
                            (
                                s.kill_tx
                                    .take(),
                                s.wait_done
                                    .clone(),
                            )
                        };
                        if let Some(kill_tx) = kill_tx {
                            let notification = wait_done.notified();
                            let _ = kill_tx.send(());
                            notification.await;
                        }
                    }
                    let _ = std::fs::remove_dir_all(&output_dir);
                    let _ = std::fs::create_dir_all(&output_dir);

                    // Calculate the seek position from the runtimeTicks query param
                    // (cumulative ticks to start of this segment) provided by our
                    // server-generated VOD playlist. Fall back to segment_index * segment_length.
                    let start_time_ticks = q
                        .runtime_ticks
                        .unwrap_or_else(|| {
                            (session_start_secs as i64
                                + requested_idx as i64 * segment_length as i64)
                                .to_ticks(TickUnit::Seconds)
                                .unwrap_or(0)
                        });

                    let encoding_opts = crate::db::Settings::get_encoding_config(
                        &state
                            .ctx
                            .db,
                    )
                    .await
                    .unwrap_or_default();
                    let params = crate::playback::engine::TranscodeParams {
                        input_url,
                        output_dir: output_dir.clone(),
                        video_codec,
                        audio_codec: audio_codec.clone(),
                        segment_length,
                        start_time_ticks: Some(start_time_ticks),
                        hls_start_number: requested_idx,
                        max_width: q
                            .max_width
                            .map(|v| v as u32),
                        max_height: q
                            .max_height
                            .map(|v| v as u32),
                        video_bitrate: q
                            .video_bit_rate
                            .map(|v| v as u32),
                        audio_bitrate: q
                            .audio_bit_rate
                            .map(|v| v as u32),
                        audio_channels: if audio_codec == "copy" {
                            None
                        } else {
                            Some(2)
                        },
                        audio_stream_index,
                        subtitle_stream_index,
                        burn_subtitle,
                        subtitle_width: None,
                        subtitle_height: None,
                        encoding_preset: encoding_opts.encoding_preset,
                        source_video_codec: session
                            .read()
                            .await
                            .source_video_codec
                            .clone(),
                        source_audio_codec: session
                            .read()
                            .await
                            .source_audio_codec
                            .clone(),
                        trusted_probe_data: session
                            .read()
                            .await
                            .source_video_codec
                            .is_some(),
                        source_frame_rate: session
                            .read()
                            .await
                            .source_frame_rate,
                        hardware_acceleration_type: encoding_opts
                            .hardware_acceleration_type
                            .unwrap_or_default(),
                        vaapi_device: encoding_opts
                            .vaapi_device
                            .unwrap_or_else(|| "/dev/dri/renderD128".to_string()),
                        vaapi_driver: encoding_opts
                            .vaapi_driver
                            .unwrap_or_default(),
                        source_video_range_type: session
                            .read()
                            .await
                            .source_video_range_type,
                        enable_tonemapping: encoding_opts
                            .enable_tonemapping
                            .unwrap_or(false),
                        enable_vpp_tonemapping: encoding_opts
                            .enable_vpp_tonemapping
                            .unwrap_or(false),
                        tonemapping_algorithm: encoding_opts
                            .tonemapping_algorithm
                            .unwrap_or_else(|| "hable".to_string()),
                        tonemapping_desat: encoding_opts
                            .tonemapping_desat
                            .unwrap_or(0.0),
                        tonemapping_peak: encoding_opts
                            .tonemapping_peak
                            .unwrap_or(0.0),
                        allow_hevc_encoding: encoding_opts
                            .allow_hevc_encoding
                            .unwrap_or(false),
                        allow_av1_encoding: encoding_opts
                            .allow_av1_encoding
                            .unwrap_or(false),
                        h264_crf: encoding_opts
                            .h264_crf
                            .unwrap_or(23),
                        h265_crf: encoding_opts
                            .h265_crf
                            .unwrap_or(28),
                        is_live: false,
                        normalize_audio_loudness: encoding_opts
                            .normalize_audio_loudness
                            .unwrap_or(false),
                    };

                    // Reinitialise the session's state for the new transcode run.
                    {
                        let mut s = session
                            .write()
                            .await;
                        s.state = TranscodeState::Starting;
                        let _ = s
                            .state_tx
                            .send(TranscodeState::Starting);
                        s.start_time_secs = (start_time_ticks / 10_000_000) as u32;
                        s.requested_start_ticks = start_time_ticks;
                        // The new generation performs a fresh input seek; any
                        // previously measured keyframe-aligned start is stale.
                        s.actual_start_ticks = None;
                        s.playback_offset_secs
                            .store(0, std::sync::atomic::Ordering::Relaxed);
                    }

                    let session_clone = session.clone();
                    tokio::spawn(async move {
                        if let Err(e) = crate::playback::engine::start_transcode(
                            session_clone,
                            params,
                        )
                        .await
                        {
                            error!("Transcode restart failed: {:#}", e);
                        }
                    });
                } // else: has_running_ffmpeg
            } // needs_restart
        }
    }

    // Stay below the client's fragment timeout. If the transcode is alive but
    // late, return a retryable 503 instead of a terminal-looking 404.
    // If there's no live session (e.g. after server restart), only serve from disk.
    if let Some(active) = session.as_ref() {
        wait_for_transcode_path(
            active,
            &segment_path,
            std::time::Duration::from_secs(12),
        )
        .await;
    }

    if !segment_path.exists() {
        if session.is_none() {
            return Ok(hls_state_response(StatusCode::GONE, "session-gone", None));
        }
        if let Some(session) = &session {
            if matches!(
                session
                    .read()
                    .await
                    .state,
                TranscodeState::Complete | TranscodeState::Error(_)
            ) {
                return Ok(hls_state_response(
                    StatusCode::GONE,
                    "transcode-ended",
                    None,
                ));
            }
        }
        return Ok(hls_state_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "segment-not-ready",
            Some("1"),
        ));
    }

    // Keep the session alive — the segment request counts as activity.
    state
        .ctx
        .sessions
        .ping(&play_session_id);

    let file = tokio::fs::File::open(&segment_path).await?;
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    // fMP4 segments (.m4s) use video/mp4; MPEG-TS segments use video/mp2t.
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", segment.format().content_type())
        .header("Cache-Control", "public, max-age=86400")
        .body(body)
        .unwrap())
}
