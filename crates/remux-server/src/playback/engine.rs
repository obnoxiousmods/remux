use anyhow::{Result, anyhow};
#[cfg(unix)]
use libc;
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::{
    common::{TickUnit, ToRunTimeTicks},
    device_profile::{AudioCodec, VideoCodec},
};
use remux_sdks::remux::{EncodingPreset, HardwareAccelerationType, VideoRangeType};

use super::session::{HlsSegmentFile, TranscodeSession, TranscodeState};

pub async fn detect_hardware_acceleration() -> HardwareAccelerationType {
    let detected = probe_hw_accel().await;
    info!(hw_accel = ?detected, "Hardware acceleration detected");
    detected
}

/// Detect the VAAPI driver name for the primary DRM render node.
/// Returns "iHD" for Intel (vendor 0x8086), empty string for others.
/// Probe the VAAPI device by running ffmpeg in verbose mode and inspecting
/// the driver string it reports.  Mirrors Jellyfin's `CheckVaapiDeviceByDriverName`.
///
/// Returns "iHD" for Intel iHD driver, "i965" for Intel legacy, or "" if
/// the device is unavailable or the driver is unknown.
pub async fn detect_vaapi_driver(vaapi_device: &str) -> String {
    let device = if vaapi_device.is_empty() {
        "/dev/dri/renderD128"
    } else {
        vaapi_device
    };

    let result = tokio::process::Command::new(ffmpeg_bin())
        .args([
            "-v",
            "verbose",
            "-hide_banner",
            "-init_hw_device",
            &format!("vaapi=va:{device}"),
        ])
        .output()
        .await;

    let output = match result {
        Ok(o) => String::from_utf8_lossy(&o.stderr).into_owned(),
        Err(e) => {
            warn!("Could not probe VAAPI device for driver detection: {e}");
            return String::new();
        }
    };

    if output.contains("Intel iHD driver") {
        "iHD".to_string()
    } else if output.contains("Intel i965 driver") {
        "i965".to_string()
    } else {
        String::new()
    }
}

/// PCI vendor IDs as reported by `/sys/class/drm/renderD128/device/vendor`.
const PCI_VENDOR_INTEL: &str = "0x8086";
/// NVIDIA's kernel driver creates a DRM render node just like Intel/AMD, but its
/// VA-API shim (nvidia-vaapi-driver) is **decode-only** — `vainfo` reports every
/// profile as `VAEntrypointVLD` and no `VAEntrypointEncSlice`. Selecting VAAPI on
/// NVIDIA therefore always fails at encoder-open time with
/// "No usable encoding entrypoint found", so the vendor must be excluded.
const PCI_VENDOR_NVIDIA: &str = "0x10de";

/// The h264 encoder implementing each acceleration type. Encoder availability is
/// the only trustworthy capability signal — `ffmpeg -hwaccels` lists *decode*
/// methods and says nothing about whether the matching encoder exists.
fn h264_encoder_for(accel: HardwareAccelerationType) -> Option<&'static str> {
    match accel {
        HardwareAccelerationType::Nvenc => Some("h264_nvenc"),
        HardwareAccelerationType::Qsv => Some("h264_qsv"),
        HardwareAccelerationType::Vaapi => Some("h264_vaapi"),
        HardwareAccelerationType::Amf => Some("h264_amf"),
        HardwareAccelerationType::VideoToolbox => Some("h264_videotoolbox"),
        HardwareAccelerationType::V4l2m2m => Some("h264_v4l2m2m"),
        HardwareAccelerationType::Rkmpp => Some("h264_rkmpp"),
        HardwareAccelerationType::None => None,
    }
}

async fn probe_hw_accel() -> HardwareAccelerationType {
    let encoders = match tokio::process::Command::new(ffmpeg_bin())
        .args(["-hide_banner", "-encoders"])
        .output()
        .await
    {
        Ok(out) => String::from_utf8_lossy(&out.stdout).to_string(),
        Err(e) => {
            warn!("Could not run ffmpeg to detect encoders: {e}");
            String::new()
        }
    };

    let candidates = hw_accel_candidates(
        &encoders,
        |p| std::path::Path::new(p).exists(),
        || {
            std::fs::read_to_string("/sys/class/drm/renderD128/device/vendor")
                .ok()
                .map(|s| {
                    s.trim()
                        .to_string()
                })
        },
    );

    // A candidate passing the static checks above can still fail to open at
    // runtime (wrong driver, no free session, kernel/userspace mismatch). Prove
    // each one with a real one-frame encode before committing to it.
    for accel in candidates {
        match encoder_smoke_test(accel).await {
            Ok(()) => return accel,
            Err(e) => warn!(
                ?accel,
                error = %e,
                "Hardware encoder failed its smoke test — trying next candidate"
            ),
        }
    }

    HardwareAccelerationType::None
}

/// Run a one-frame encode to prove the accelerator can actually encode.
async fn encoder_smoke_test(accel: HardwareAccelerationType) -> Result<(), String> {
    let Some(encoder) = h264_encoder_for(accel) else {
        return Ok(());
    };

    let mut args: Vec<String> = vec!["-hide_banner".into(), "-v".into(), "error".into()];
    // VAAPI and QSV encode from GPU surfaces, so the test frame must be uploaded.
    // NVENC/VideoToolbox/V4L2/RKMPP accept system-memory frames directly.
    let needs_upload = matches!(
        accel,
        HardwareAccelerationType::Vaapi | HardwareAccelerationType::Qsv
    );
    if needs_upload {
        args.extend(["-vaapi_device".into(), "/dev/dri/renderD128".into()]);
    }
    args.extend([
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "testsrc=size=320x240:rate=1:duration=1".into(),
    ]);
    if needs_upload {
        args.extend(["-vf".into(), "format=nv12,hwupload".into()]);
    }
    args.extend([
        "-c:v".into(),
        encoder.into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]);

    let out = tokio::process::Command::new(ffmpeg_bin())
        .args(&args)
        .output()
        .await
        .map_err(|e| format!("could not spawn ffmpeg: {e}"))?;

    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr)
            .trim()
            .to_string())
    }
}

/// Acceleration types viable on this host, in preference order.
///
/// `encoders_output` is the stdout of `ffmpeg -encoders`.
pub(crate) fn hw_accel_candidates(
    encoders_output: &str,
    device_exists: impl Fn(&str) -> bool,
    drm_vendor: impl Fn() -> Option<String>,
) -> Vec<HardwareAccelerationType> {
    // `ffmpeg -encoders` rows look like " V....D h264_nvenc  NVIDIA NVENC ...".
    let has_encoder = |name: &str| {
        encoders_output.lines().any(|l| {
            l.split_whitespace()
                .nth(1)
                == Some(name)
        })
    };

    let has_render_node = device_exists("/dev/dri/renderD128");
    let vendor = drm_vendor();
    let is_intel = vendor.as_deref() == Some(PCI_VENDOR_INTEL);
    let is_nvidia = vendor.as_deref() == Some(PCI_VENDOR_NVIDIA);

    let viable = |accel: HardwareAccelerationType| -> bool {
        let encoder_present = h264_encoder_for(accel)
            .is_some_and(has_encoder);
        encoder_present
            && match accel {
                HardwareAccelerationType::Nvenc => device_exists("/dev/nvidia0"),
                HardwareAccelerationType::Qsv => has_render_node && is_intel,
                HardwareAccelerationType::Vaapi => has_render_node && !is_nvidia,
                HardwareAccelerationType::VideoToolbox => cfg!(target_os = "macos"),
                HardwareAccelerationType::V4l2m2m => device_exists("/dev/video0"),
                HardwareAccelerationType::Rkmpp => device_exists("/dev/mpp_service"),
                HardwareAccelerationType::Amf | HardwareAccelerationType::None => false,
            }
    };

    [
        HardwareAccelerationType::Nvenc,
        HardwareAccelerationType::Qsv,
        HardwareAccelerationType::Vaapi,
        HardwareAccelerationType::VideoToolbox,
        HardwareAccelerationType::V4l2m2m,
        HardwareAccelerationType::Rkmpp,
    ]
    .into_iter()
    .filter(|a| viable(*a))
    .collect()
}

/// First viable acceleration type, ignoring runtime validation.
#[cfg(test)]
pub(crate) fn select_hw_accel(
    encoders_output: &str,
    device_exists: impl Fn(&str) -> bool,
    drm_vendor: impl Fn() -> Option<String>,
) -> HardwareAccelerationType {
    hw_accel_candidates(encoders_output, device_exists, drm_vendor)
        .first()
        .copied()
        .unwrap_or(HardwareAccelerationType::None)
}

/// Substrings that identify a failure originating in the hardware encoder or its
/// device stack, as opposed to an I/O, network, or muxer fault that merely
/// occurred while hardware acceleration was enabled.
const HW_FAILURE_SIGNATURES: &[&str] = &[
    "no usable encoding entrypoint",
    "error while opening encoder",
    "could not open encoder",
    "openencodesessionex failed",
    "initializeencoder failed",
    "no capable devices found",
    "cannot load libcuda",
    "device creation failed",
    "failed to create",
    "hwupload",
    "impossible to convert between the formats",
    "function not implemented",
    "the requested vaprofile is not supported",
    "no nvenc capable devices",
];

/// Whether ffmpeg's stderr implicates the hardware encoder itself.
///
/// Returning false for an unrelated fault keeps the session on hardware; the old
/// behaviour blamed HW accel for *any* non-zero exit, so a missing output
/// directory would silently downgrade a session to libx264 for its whole life.
pub(crate) fn is_hw_encoder_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    HW_FAILURE_SIGNATURES
        .iter()
        .any(|sig| lower.contains(sig))
}

/// Stderr signatures implicating the INPUT side of the pipeline: the remote
/// source read failed (flaky debrid/HTTP stream). Only these justify an
/// auto-restart at the last produced segment; encoder and output faults take
/// their own paths.
const INPUT_FAILURE_SIGNATURES: &[&str] = &[
    "error during demuxing",
    "error opening input",
    "connection reset by peer",
    "connection refused",
    "connection timed out",
];

/// Whether ffmpeg's stderr implicates a read failure of the input source.
/// Output-side I/O (e.g. a full disk) must not trigger a restart loop, so a
/// bare "input/output error" only counts alongside ffmpeg's `in#0` marker.
pub(crate) fn is_input_source_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    INPUT_FAILURE_SIGNATURES
        .iter()
        .any(|sig| lower.contains(sig))
        || (lower.contains("in#0") && lower.contains("input/output error"))
}

/// Consecutive hardware-encoder failures before hardware is abandoned
/// process-wide. Without this, every new session re-attempts a doomed encoder,
/// burning one wasted ffmpeg spawn apiece.
const HW_FAILURE_LIMIT: u32 = 3;
static HW_FAILURES: AtomicU32 = AtomicU32::new(0);
static HW_LOCKED_OUT: AtomicBool = AtomicBool::new(false);

fn note_hw_failure(accel: HardwareAccelerationType) {
    let failures = HW_FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
    if failures >= HW_FAILURE_LIMIT && !HW_LOCKED_OUT.swap(true, Ordering::Relaxed) {
        warn!(
            ?accel,
            failures,
            "Hardware encoding failed {HW_FAILURE_LIMIT} times — falling back to software for \
             all new sessions until restart"
        );
    }
}

fn note_hw_success() {
    HW_FAILURES.store(0, Ordering::Relaxed);
}

fn hw_accel_locked_out() -> bool {
    HW_LOCKED_OUT.load(Ordering::Relaxed)
}

/// Map the shared x264-style preset onto NVENC's p1..p7 scale (p1 fastest).
fn nvenc_preset(preset: EncodingPreset) -> &'static str {
    match preset {
        EncodingPreset::Ultrafast => "p1",
        EncodingPreset::Superfast => "p2",
        EncodingPreset::Veryfast => "p3",
        EncodingPreset::Faster | EncodingPreset::Fast => "p4",
        EncodingPreset::Medium => "p5",
        EncodingPreset::Slow => "p6",
        EncodingPreset::Slower | EncodingPreset::Slowest => "p7",
    }
}

/// An active viewer is guaranteed this much encoded media ahead.
const GUARANTEED_BUFFER_SECS: u32 = 180;
/// During otherwise idle capacity one session may race toward completion.
const IDLE_FILL_BUFFER_SECS: u32 = 86_400;
/// A speculative Item Details warmup must never download an entire episode.
const PREWARM_BUFFER_SECS: u32 = 12;
/// Protect remote sources and the encoder from a thundering herd of cold starts.
const STARTUP_PRIORITY_SLOTS: usize = 2;
/// Rotate the idle completion slot so one long episode cannot monopolize it.
const IDLE_FILL_QUANTUM_SECS: u64 = 60;
/// Seconds behind the playback position before a segment is eligible for deletion.
const SEGMENT_KEEP_SECS: u32 = 30;

#[derive(Debug, Clone)]
struct BufferDemand {
    ahead_secs: u32,
    prewarm: bool,
    first_seen: Instant,
    last_seen: Instant,
    last_fill_turn: u64,
}

#[derive(Debug, Default)]
struct AdaptiveBufferState {
    sessions: HashMap<String, BufferDemand>,
    filler: Option<(String, Instant)>,
    fill_turn: u64,
}

impl AdaptiveBufferState {
    fn target_for(
        &mut self,
        session_id: &str,
        ahead_secs: u32,
        prewarm: bool,
        segment_length: u32,
        now: Instant,
    ) -> u32 {
        self.sessions
            .retain(|_, demand| now.saturating_duration_since(demand.last_seen) < Duration::from_secs(5));
        let demand = self
            .sessions
            .entry(session_id.to_string())
            .or_insert(BufferDemand {
                ahead_secs,
                prewarm,
                first_seen: now,
                last_seen: now,
                last_fill_turn: 0,
            });
        demand.ahead_secs = ahead_secs;
        demand.prewarm = prewarm;
        demand.last_seen = now;

        if prewarm {
            return PREWARM_BUFFER_SECS;
        }

        let mut startups = self
            .sessions
            .iter()
            .filter(|(_, candidate)| {
                !candidate.prewarm && candidate.ahead_secs < GUARANTEED_BUFFER_SECS
            })
            .map(|(id, candidate)| (id.clone(), candidate.first_seen))
            .collect::<Vec<_>>();
        startups.sort_by_key(|(_, first_seen)| *first_seen);
        if !startups.is_empty() {
            self.filler = None;
            if startups
                .iter()
                .take(STARTUP_PRIORITY_SLOTS)
                .any(|(id, _)| id == session_id)
            {
                return GUARANTEED_BUFFER_SECS;
            }
            // Non-priority sessions may still produce one complete segment so
            // their manifest becomes playable while they await a startup slot.
            return ahead_secs.max(segment_length);
        }

        let filler_expired = self.filler.as_ref().is_none_or(|(id, started)| {
            !self.sessions.contains_key(id)
                || now.saturating_duration_since(*started)
                    >= Duration::from_secs(IDLE_FILL_QUANTUM_SECS)
        });
        if filler_expired {
            self.fill_turn = self.fill_turn.saturating_add(1);
            let next = self
                .sessions
                .iter()
                .filter(|(_, candidate)| !candidate.prewarm)
                .min_by_key(|(_, candidate)| candidate.last_fill_turn)
                .map(|(id, _)| id.clone());
            self.filler = next.map(|id| {
                if let Some(candidate) = self.sessions.get_mut(&id) {
                    candidate.last_fill_turn = self.fill_turn;
                }
                (id, now)
            });
        }

        if self
            .filler
            .as_ref()
            .is_some_and(|(id, _)| id == session_id)
        {
            IDLE_FILL_BUFFER_SECS
        } else {
            GUARANTEED_BUFFER_SECS
        }
    }

    fn remove(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
        if self
            .filler
            .as_ref()
            .is_some_and(|(id, _)| id == session_id)
        {
            self.filler = None;
        }
    }
}

static ADAPTIVE_BUFFER_STATE: LazyLock<Mutex<AdaptiveBufferState>> =
    LazyLock::new(|| Mutex::new(AdaptiveBufferState::default()));

fn adaptive_buffer_limit_secs(
    session_id: &str,
    ahead_secs: u32,
    prewarm: bool,
    segment_length: u32,
) -> u32 {
    ADAPTIVE_BUFFER_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .target_for(
            session_id,
            ahead_secs,
            prewarm,
            segment_length,
            Instant::now(),
        )
}

fn remove_adaptive_buffer_session(session_id: &str) {
    ADAPTIVE_BUFFER_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(session_id);
}

fn ffmpeg_bin() -> String {
    std::env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into())
}

/// Send SIGSTOP/SIGCONT to a process by PID on Unix.
#[cfg(unix)]
fn send_signal(pid: u32, sig: libc::c_int) {
    unsafe { libc::kill(pid as libc::pid_t, sig) };
}
#[cfg(not(unix))]
fn send_signal(_pid: u32, _sig: i32) {}

/// Spawn the buffer-throttle task. It pauses/resumes ffmpeg so it never
/// encodes more than MAX_BUFFER_SECS ahead of what the client has requested.
fn spawn_buffer_monitor(
    output_dir: PathBuf,
    segment_length: u32,
    playback_offset_secs: Arc<AtomicU32>,
    prewarm: Arc<AtomicBool>,
    ffmpeg_pid: Arc<AtomicU32>,
    mut stop_rx: tokio::sync::oneshot::Receiver<()>,
    play_session_id: String,
) {
    tokio::spawn(async move {
        let mut paused = false;
        let mut ticks: u32 = 0;
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
            }
            ticks += 1;

            let pid = ffmpeg_pid.load(Ordering::Relaxed);
            let produced = count_segments(&output_dir);
            let buffered_secs = produced * segment_length;
            // playback_offset_secs is how far the client has actually played
            // relative to the start of this transcode session (from progress reports).
            let playback_secs = playback_offset_secs.load(Ordering::Relaxed);

            let ahead = buffered_secs.saturating_sub(playback_secs);
            let is_prewarm = prewarm.load(Ordering::Relaxed);
            let max_buffer_secs = adaptive_buffer_limit_secs(
                &play_session_id,
                ahead,
                is_prewarm,
                segment_length,
            );

            if pid != 0 && !paused && ahead >= max_buffer_secs {
                debug!(
                    play_session_id,
                    pid,
                    ahead,
                    prewarm = is_prewarm,
                    "Buffer full — pausing ffmpeg"
                );
                #[cfg(unix)]
                send_signal(pid, libc::SIGSTOP);
                paused = true;
            } else if pid != 0
                && paused
                && ahead < max_buffer_secs.saturating_sub(segment_length * 2)
            {
                debug!(
                    play_session_id,
                    pid, ahead, "Buffer drained — resuming ffmpeg"
                );
                #[cfg(unix)]
                send_signal(pid, libc::SIGCONT);
                paused = false;
            }

            // Every 30 seconds, delete segments that are more than SEGMENT_KEEP_SECS
            // behind the current playback position.
            if ticks % 30 == 0 && playback_secs > SEGMENT_KEEP_SECS {
                let cutoff_idx = (playback_secs - SEGMENT_KEEP_SECS) / segment_length;
                delete_old_segments(&output_dir, cutoff_idx);
            }
        }

        // Ensure ffmpeg isn't left paused when we stop monitoring.
        if paused {
            let pid = ffmpeg_pid.load(Ordering::Relaxed);
            if pid != 0 {
                #[cfg(unix)]
                send_signal(pid, libc::SIGCONT);
            }
        }
        remove_adaptive_buffer_session(&play_session_id);
    });
}

/// Delete segment files whose index is less than `cutoff_idx`.
fn delete_old_segments(dir: &PathBuf, cutoff_idx: u32) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // segment_00042.ts / segment_00042.m4s / etc. — strip everything after the last '_'
        let Some(idx_str) = name
            .rsplit('_')
            .next()
            .and_then(|s| {
                s.split('.')
                    .next()
            })
        else {
            continue;
        };
        let Ok(idx) = idx_str.parse::<u32>() else {
            continue;
        };
        if idx < cutoff_idx {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn count_segments(dir: &PathBuf) -> u32 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| HlsSegmentFile::parse(&e.file_name().to_string_lossy()).is_some())
                .count() as u32
        })
        .unwrap_or(0)
}

fn max_segment_index(dir: &PathBuf) -> Option<u32> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            HlsSegmentFile::parse(&e.file_name().to_string_lossy())
                .map(|segment| segment.index())
        })
        .max()
}

/// Parameters for starting a new HLS transcode job.
#[derive(Debug, Clone)]
pub struct TranscodeParams {
    pub input_url: String,
    pub output_dir: PathBuf,
    pub video_codec: String, // "copy", "libx264", "libx265"
    pub audio_codec: String, // "aac", "copy"
    pub segment_length: u32, // seconds (default 6)
    pub start_time_ticks: Option<i64>,
    /// Sequence number assigned to the first output segment. A new playback
    /// generation always starts at zero even when its source seek is non-zero;
    /// segment-driven recovery may preserve the requested local sequence.
    pub hls_start_number: u32,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub video_bitrate: Option<u32>,
    pub audio_bitrate: Option<u32>,
    pub audio_channels: Option<u32>,
    pub audio_stream_index: Option<i32>,
    pub subtitle_stream_index: Option<i32>,
    pub burn_subtitle: bool,
    /// Native dimensions of the subtitle bitmap (PGS canvas size), used to
    /// scale the subtitle to match the output video resolution.
    pub subtitle_width: Option<u32>,
    pub subtitle_height: Option<u32>,
    pub encoding_preset: Option<EncodingPreset>,
    /// Codec of the source video stream (e.g. "hevc", "h264"), used to apply
    /// codec-specific output flags such as `-tag:v hvc1` for HEVC in HLS.
    pub source_video_codec: Option<String>,
    /// Codec of the source audio stream (e.g. "aac", "ac3"), used to apply
    /// codec-specific bitstream filters such as `aac_adtstoasc` when copying.
    pub source_audio_codec: Option<String>,
    /// The selected VOD source already has persisted ProbeDB/ffprobe stream
    /// metadata. Start with a smaller ffmpeg analysis window; any non-encoder
    /// startup failure retries once with the conservative full budget.
    pub trusted_probe_data: bool,
    /// Effective frame rate of the selected source video stream. NVENC needs an
    /// explicit frame-based GOP in addition to timestamp-forced keyframes so
    /// the HLS muxer can finalize segments while a remote VOD is still open.
    pub source_frame_rate: Option<f32>,
    pub hardware_acceleration_type: HardwareAccelerationType,
    /// VAAPI render device path.
    pub vaapi_device: String,
    /// VAAPI driver name (e.g. "iHD" for Intel). Empty string means auto-detect.
    pub vaapi_driver: String,
    /// HDR type of the source video, used to decide whether tone-mapping or
    /// SDR colour-space override is needed.
    pub source_video_range_type: Option<VideoRangeType>,
    /// Software HDR→SDR tonemapping via tonemapx filter (CPU).
    pub enable_tonemapping: bool,
    /// Hardware HDR→SDR tonemapping via tonemap_vaapi (Intel VAAPI/QSV only).
    pub enable_vpp_tonemapping: bool,
    /// Algorithm for tonemapx: hable, reinhard, mobius, bt2390, bt2446a, none.
    pub tonemapping_algorithm: String,
    /// Desaturation coefficient for tonemapx.
    pub tonemapping_desat: f32,
    /// Peak luminance for tonemapx (nits). 0 = auto.
    pub tonemapping_peak: f32,
    /// When false, HEVC encoding requests fall back to H.264.
    pub allow_hevc_encoding: bool,
    /// When false, AV1 encoding requests fall back to H.264.
    pub allow_av1_encoding: bool,
    /// CRF quality for software H.264 (libx264).
    pub h264_crf: u32,
    /// CRF quality for software H.265 (libx265).
    pub h265_crf: u32,
    /// True for live TV / RTSP streams — disables seeking and enables auto-restart on exit.
    pub is_live: bool,
    /// Apply `loudnorm=I=-14:TP=-1:LRA=11` when transcoding audio. Has no effect
    /// when audio_codec is "copy". See EncodingOptions::normalize_audio_loudness.
    pub normalize_audio_loudness: bool,
}

impl Default for TranscodeParams {
    fn default() -> Self {
        Self {
            input_url: String::new(),
            output_dir: PathBuf::new(),
            video_codec: "copy".to_string(),
            audio_codec: "aac".to_string(),
            segment_length: 6,
            start_time_ticks: None,
            hls_start_number: 0,
            max_width: None,
            max_height: None,
            video_bitrate: None,
            audio_bitrate: None,
            audio_channels: None,
            audio_stream_index: None,
            subtitle_stream_index: None,
            burn_subtitle: false,
            subtitle_width: None,
            subtitle_height: None,
            encoding_preset: None,
            source_video_codec: None,
            source_audio_codec: None,
            trusted_probe_data: false,
            source_frame_rate: None,
            hardware_acceleration_type: HardwareAccelerationType::None,
            vaapi_device: "/dev/dri/renderD128".to_string(),
            vaapi_driver: String::new(),
            source_video_range_type: None,
            enable_tonemapping: false,
            enable_vpp_tonemapping: false,
            tonemapping_algorithm: "hable".to_string(),
            tonemapping_desat: 0.0,
            tonemapping_peak: 0.0,
            allow_hevc_encoding: false,
            allow_av1_encoding: false,
            h264_crf: 23,
            h265_crf: 28,
            is_live: false,
            normalize_audio_loudness: false,
        }
    }
}

/// Return the expected output video dimensions based on transcode params.
fn output_dimensions(params: &TranscodeParams) -> (Option<u32>, Option<u32>) {
    (params.max_width, params.max_height)
}

/// Calculate an NVENC GOP matching the requested HLS segment duration.
///
/// Remote probes do not always provide a usable frame rate. Fall back to 30
/// fps in that case; timestamp-based `-force_key_frames` remains the authority
/// for the exact wall-clock boundary.
fn nvenc_hls_gop_size(frame_rate: Option<f32>, segment_length: u32) -> u32 {
    const FALLBACK_FRAME_RATE: f32 = 30.0;
    const MAX_REASONABLE_FRAME_RATE: f32 = 240.0;

    let frame_rate = frame_rate
        .filter(|rate| {
            rate.is_finite() && *rate >= 1.0 && *rate <= MAX_REASONABLE_FRAME_RATE
        })
        .unwrap_or(FALLBACK_FRAME_RATE);
    let segment_length = segment_length.max(1);

    (frame_rate * segment_length as f32)
        .round()
        .max(1.0) as u32
}

/// Return the ffmpeg input args that enable hardware-accelerated decoding.
/// These are placed **before** the `-i` flag.
fn hw_input_args(
    accel: HardwareAccelerationType,
    vaapi_device: &str,
    vaapi_driver: &str,
) -> Vec<String> {
    // Build the vaapi init_hw_device string, appending ",driver=X" when known.
    // For QSV we always fall back to "iHD" because QSV is Intel-only.
    let effective_driver = |fallback: &'static str| -> String {
        if vaapi_driver.is_empty() {
            fallback.to_string()
        } else {
            vaapi_driver.to_string()
        }
    };
    let vaapi_init = |alias: &str, driver: &str| {
        let driver_opt = if driver.is_empty() {
            String::new()
        } else {
            format!(",driver={driver}")
        };
        format!("vaapi={alias}:{vaapi_device}{driver_opt}")
    };

    match accel {
        // NVENC is an encoder API. Keep decoded frames in system memory until
        // this pipeline uses CUDA-native filters; combining CUDA decode with
        // the software scale filter can corrupt the chroma planes.
        HardwareAccelerationType::Nvenc => vec![],
        HardwareAccelerationType::Vaapi => {
            // Initialise the VAAPI device via init_hw_device so that the
            // driver= option takes effect (fixes iHD resolution on Intel).
            // Frames are decoded in software; hwupload in the filter chain
            // uploads the scaled frame to the VAAPI device for encoding.
            let driver = effective_driver("");
            vec![
                "-init_hw_device".into(),
                vaapi_init("va", &driver),
                "-filter_hw_device".into(),
                "va".into(),
            ]
        }
        HardwareAccelerationType::Qsv => {
            // QSV is Intel-only, so always use iHD as the VAAPI driver.
            // On Linux, QSV is derived from a VAAPI device.  We initialise the
            // VAAPI device first (with an explicit driver so iHD is found on
            // Intel), derive a QSV device from it, then use VAAPI for hardware-
            // assisted decoding.  Frames stay in GPU memory; scale_vaapi +
            // hwmap map them to a QSV surface for the QSV encoder.
            let driver = effective_driver("iHD");
            vec![
                "-init_hw_device".into(),
                vaapi_init("va", &driver),
                "-init_hw_device".into(),
                "qsv=qs@va".into(),
                "-filter_hw_device".into(),
                "qs".into(),
                "-hwaccel".into(),
                "vaapi".into(),
                "-hwaccel_output_format".into(),
                "vaapi".into(),
            ]
        }
        HardwareAccelerationType::VideoToolbox => {
            vec!["-hwaccel".into(), "videotoolbox".into()]
        }
        HardwareAccelerationType::Amf => vec!["-hwaccel".into(), "d3d11va".into()],
        HardwareAccelerationType::Rkmpp => vec!["-hwaccel".into(), "rkmpp".into()],
        // v4l2m2m and None: software decode
        _ => vec![],
    }
}

/// Map a software encoder name to the equivalent hardware encoder name.
/// Returns the original name unchanged if the codec should not be re-mapped
/// (e.g. "copy", "libvpx-vp9") or if `accel` is None.
fn hw_encoder_name(base: &str, accel: HardwareAccelerationType) -> String {
    let suffix = match accel {
        HardwareAccelerationType::Nvenc => "_nvenc",
        HardwareAccelerationType::Vaapi => "_vaapi",
        HardwareAccelerationType::Qsv => "_qsv",
        HardwareAccelerationType::Amf => "_amf",
        HardwareAccelerationType::VideoToolbox => "_videotoolbox",
        HardwareAccelerationType::V4l2m2m => "_v4l2m2m",
        HardwareAccelerationType::Rkmpp => "_rkmpp",
        _ => return base.to_string(),
    };
    match base {
        "libx264" => format!("h264{suffix}"),
        "libx265" => format!("hevc{suffix}"),
        other => other.to_string(),
    }
}

/// Append any filter steps required by the hardware encoder after the
/// scale/video filter chain (e.g. hwupload for VAAPI).
fn hw_filter_suffix(accel: HardwareAccelerationType) -> Option<String> {
    match accel {
        // VAAPI encoder requires frames in VAAPI memory; upload them after scaling.
        HardwareAccelerationType::Vaapi => Some("format=nv12,hwupload".to_string()),
        // QSV: frames are in VAAPI memory after decode; map them to a QSV surface
        // for the QSV encoder.  scale_vaapi (the scale step) already converted the
        // pixel format, so we only need the device remap here.
        HardwareAccelerationType::Qsv => {
            Some("hwmap=derive_device=qsv,format=qsv".to_string())
        }
        _ => None,
    }
}

/// Build a scale filter string for FFmpeg, if needed.
fn build_scale_filter(params: &TranscodeParams) -> Option<String> {
    match (params.max_width, params.max_height) {
        (Some(w), Some(h)) => Some(format!(
            "scale='min({},iw)':'min({},ih)':force_original_aspect_ratio=decrease",
            w, h
        )),
        (Some(w), None) => Some(format!("scale='min({},iw)':-2", w)),
        (None, Some(h)) => Some(format!("scale=-2:'min({},ih)'", h)),
        _ => None,
    }
}

fn build_cuda_video_filter(params: &TranscodeParams, hdr: bool) -> String {
    let scale = match (params.max_width, params.max_height) {
        (Some(w), Some(h)) => format!(
            "scale_cuda=w='min({w},iw)':h='min({h},ih)':format=yuv420p:force_original_aspect_ratio=decrease"
        ),
        (Some(w), None) => format!("scale_cuda=w='min({w},iw)':h=-2:format=yuv420p"),
        (None, Some(h)) => format!("scale_cuda=w=-2:h='min({h},ih)':format=yuv420p"),
        _ => "scale_cuda=format=yuv420p".to_string(),
    };
    if hdr {
        format!("tonemap_cuda=format=yuv420p,{scale}")
    } else {
        scale
    }
}

/// Build a VAAPI hardware scale filter for QSV transcoding.
///
/// When using QSV (VAAPI-decode → QSV-encode pipeline) frames live in VAAPI
/// GPU memory, so we must use `scale_vaapi` instead of the CPU `scale` filter.
/// Always returns `Some` — at minimum a format-conversion pass is needed so the
/// `hwmap=derive_device=qsv` suffix can map frames into QSV memory.
/// `extra_hw_frames=24` follows Jellyfin's recommendation for VAAPI VPP pools.
fn build_qsv_scale_filter(max_width: Option<u32>, max_height: Option<u32>) -> String {
    match (max_width, max_height) {
        (Some(w), Some(h)) => format!(
            "scale_vaapi=w='min({w},iw)':h='min({h},ih)':format=nv12:extra_hw_frames=24:force_original_aspect_ratio=decrease"
        ),
        (Some(w), None) => {
            format!("scale_vaapi=w='min({w},iw)':h=-2:format=nv12:extra_hw_frames=24")
        }
        (None, Some(h)) => {
            format!("scale_vaapi=w=-2:h='min({h},ih)':format=nv12:extra_hw_frames=24")
        }
        _ => "scale_vaapi=format=nv12:extra_hw_frames=24".to_string(),
    }
}

fn is_hdr(range_type: Option<&VideoRangeType>) -> bool {
    matches!(
        range_type,
        Some(VideoRangeType::Hdr10)
            | Some(VideoRangeType::Hdr10Plus)
            | Some(VideoRangeType::Hlg)
            | Some(VideoRangeType::Dovi)
            | Some(VideoRangeType::DoviWithHdr10)
            | Some(VideoRangeType::DoviWithHlg)
    )
}

/// QSV device-init args without hardware-decode flags.
/// Used when the source is HDR: we keep the VAAPI+QSV device setup so the
/// QSV encoder is available, but decode in software so CPU filters like
/// `setparams` can run before the encoder.
fn qsv_init_only_args(vaapi_device: &str, vaapi_driver: &str) -> Vec<String> {
    let driver = if vaapi_driver.is_empty() {
        "iHD".to_string()
    } else {
        vaapi_driver.to_string()
    };
    let driver_opt = if driver.is_empty() {
        String::new()
    } else {
        format!(",driver={driver}")
    };
    vec![
        "-init_hw_device".into(),
        format!("vaapi=va:{vaapi_device}{driver_opt}"),
        "-init_hw_device".into(),
        "qsv=qs@va".into(),
        "-filter_hw_device".into(),
        "qs".into(),
    ]
}

/// Build the ffmpeg CLI args for an HLS transcode.
pub(crate) fn build_hls_args(params: &TranscodeParams) -> Vec<String> {
    let accel = params.hardware_acceleration_type;
    let is_hw = !matches!(accel, HardwareAccelerationType::None);
    let hdr = is_hdr(
        params
            .source_video_range_type
            .as_ref(),
    );

    // Tone-map decisions (only apply to HDR + transcode, never to copy).
    let do_vpp_tonemap = hdr
        && params.enable_vpp_tonemapping
        && matches!(
            accel,
            HardwareAccelerationType::Vaapi | HardwareAccelerationType::Qsv
        );
    let do_sw_tonemap = hdr && params.enable_tonemapping && !do_vpp_tonemap;

    let ffmpeg_video_codec = {
        let base = match params
            .video_codec
            .as_str()
        {
            "copy" => "copy",
            _ => "libx264",
        };
        // Subtitle burn-in requires re-encoding; can't copy video.
        let base = if params.burn_subtitle
            && params
                .subtitle_stream_index
                .is_some()
            && base == "copy"
        {
            "libx264"
        } else {
            base
        };
        if base != "copy" && is_hw {
            hw_encoder_name(base, accel)
        } else {
            base.to_string()
        }
    };
    let ffmpeg_audio_codec = match params
        .audio_codec
        .as_str()
    {
        "copy" => "copy",
        _ => "aac",
    };

    // fMP4 (fragmented MP4) is required for HEVC on iOS Safari per Apple's HLS
    // authoring specification.  MPEG-TS cannot carry HEVC correctly in HLS.
    let is_hevc_copy = ffmpeg_video_codec == "copy"
        && params
            .source_video_codec
            .as_deref()
            .and_then(|s| {
                s.parse::<VideoCodec>()
                    .ok()
            })
            .as_ref()
            .map(VideoCodec::is_hevc)
            .unwrap_or(false);

    let (analyze_duration, probe_size) = if params.trusted_probe_data {
        ("250000", "262144")
    } else {
        ("1000000", "1000000")
    };
    let mut args: Vec<String> = vec![
        "-v".into(),
        "error".into(),
        "-analyzeduration".into(),
        analyze_duration.into(),
        "-probesize".into(),
        probe_size.into(),
        "-reconnect".into(),
        "1".into(),
        "-reconnect_at_eof".into(),
        "1".into(),
        "-reconnect_streamed".into(),
        "1".into(),
        "-reconnect_delay_max".into(),
        "5".into(),
        "-timeout".into(),
        "30000000".into(),
        "-rw_timeout".into(),
        "30000000".into(),
    ];

    // Hardware acceleration input flags (before -ss and -i).
    // For QSV+HDR without VPP tonemapping: SW-decode so CPU filters can run.
    // For QSV+HDR with VPP tonemapping: keep VAAPI hw-decode (tonemap_vaapi needs GPU frames).
    if ffmpeg_video_codec != "copy"
        && !params.burn_subtitle
        && matches!(accel, HardwareAccelerationType::Nvenc)
    {
        args.extend([
            "-hwaccel".into(),
            "cuda".into(),
            "-hwaccel_output_format".into(),
            "cuda".into(),
        ]);
    } else if hdr && matches!(accel, HardwareAccelerationType::Qsv) && !do_vpp_tonemap {
        args.extend(qsv_init_only_args(
            &params.vaapi_device,
            &params.vaapi_driver,
        ));
    } else {
        args.extend(hw_input_args(
            accel,
            &params.vaapi_device,
            &params.vaapi_driver,
        ));
    }

    // Input seek (fast, before -i) — not applicable to live streams.
    // Without -noaccurate_seek, ffmpeg decodes from the nearest keyframe
    // up to the exact target time (only a few seconds), giving correct
    // subtitle sync at the cost of ~1-3s additional startup latency.
    if !params.is_live {
        if let Some(ticks) = params.start_time_ticks {
            let secs = ticks as f64 / 10_000_000.0;
            args.extend(["-ss".into(), format!("{:.6}", secs)]);
        }
    }

    // RTSP reliability: force TCP transport and set a 5 s connection timeout
    if params.is_live
        && params
            .input_url
            .starts_with("rtsp://")
    {
        args.extend([
            "-rtsp_transport".into(),
            "tcp".into(),
            "-timeout".into(),
            "5000000".into(),
        ]);
    }

    // A resumed session must begin a fresh output timeline. Preserving source
    // timestamps after an input-side seek would make an acknowledged
    // StartTimeTicks session expose an absolute media timeline instead of the
    // local-zero timeline used by transcoded sessions.
    if ffmpeg_video_codec == "copy"
        && params
            .start_time_ticks
            .is_none()
    {
        args.push("-copyts".into());
    }
    args.extend([
        "-i".into(),
        params
            .input_url
            .clone(),
        "-avoid_negative_ts".into(),
        "disabled".into(),
        "-max_muxing_queue_size".into(),
        "2048".into(),
    ]);

    // HW filter suffix appended after scale.
    // QSV+VPP: tonemap_vaapi in VAAPI memory then hwmap to QSV surface.
    // QSV+HDR (SW decode, no VPP): just format=nv12 — QSV encoder accepts system-memory NV12.
    // Otherwise: standard hw_filter_suffix (e.g. format=nv12,hwupload for VAAPI).
    let cuda_native = ffmpeg_video_codec != "copy"
        && !params.burn_subtitle
        && matches!(accel, HardwareAccelerationType::Nvenc);
    let hw_suffix = if cuda_native {
        Some(build_cuda_video_filter(params, hdr))
    } else if do_vpp_tonemap && matches!(accel, HardwareAccelerationType::Qsv)
    {
        let vpp =
            "tonemap_vaapi=format=nv12:p=bt709:t=bt709:m=bt709:extra_hw_frames=32";
        Some(format!("{vpp},hwmap=derive_device=qsv,format=qsv"))
    } else if hdr && matches!(accel, HardwareAccelerationType::Qsv) && !do_vpp_tonemap {
        Some("format=nv12".to_string())
    } else {
        hw_filter_suffix(accel)
    };

    // Stream mapping
    if params.burn_subtitle {
        if let Some(sub_idx) = params.subtitle_stream_index {
            // Image subtitle (PGS/DVD): bitmap overlay via filter_complex.
            // Scale subtitle bitmap to output dimensions (matching Jellyfin's approach).
            // When output size is known, scale to that; otherwise pass through as-is.
            let (out_w, out_h) = output_dimensions(params);
            let sub_scale = match (out_w, out_h) {
                (Some(w), Some(h)) => format!("scale={w}:{h}:fast_bilinear"),
                (Some(w), None) => format!("scale={w}:-1:fast_bilinear"),
                (None, Some(h)) => format!("scale=-1:{h}:fast_bilinear"),
                _ => String::new(),
            };

            // PGS bitmaps are BGRA; bare `scale` converts to a pixel format
            // compatible with overlay before any resize step (mirrors Jellyfin).
            let sub_preproc = if sub_scale.is_empty() {
                "scale".to_string()
            } else {
                format!("scale,{sub_scale}")
            };
            let main_scale_part = build_scale_filter(params)
                .map(|s| format!("{s}"))
                .unwrap_or_default();
            let main_preproc = main_scale_part;
            let overlay = "overlay=eof_action=pass:repeatlast=0";
            let filter = if main_preproc.is_empty() {
                match &hw_suffix {
                    Some(suf) => format!(
                        "[0:{sub_idx}]{sub_preproc}[sub];[0:v:0][sub]{overlay}[vraw];[vraw]{suf}[v]"
                    ),
                    None => format!(
                        "[0:{sub_idx}]{sub_preproc}[sub];[0:v:0][sub]{overlay}[v]"
                    ),
                }
            } else {
                match &hw_suffix {
                    Some(suf) => format!(
                        "[0:{sub_idx}]{sub_preproc}[sub];[0:v:0]{main_preproc}[main];[main][sub]{overlay}[vraw];[vraw]{suf}[v]"
                    ),
                    None => format!(
                        "[0:{sub_idx}]{sub_preproc}[sub];[0:v:0]{main_preproc}[main];[main][sub]{overlay}[v]"
                    ),
                }
            };
            args.extend(["-filter_complex".into(), filter]);
            args.extend(["-map".into(), "[v]".into()]);
            if let Some(audio_idx) = params.audio_stream_index {
                args.extend(["-map".into(), format!("0:{}", audio_idx)]);
            } else {
                args.extend(["-map".into(), "0:a?".into()]);
            }
        }
    } else {
        // QSV uses VAAPI hw scale when: normal (non-HDR) path, or VPP tonemap path
        // (frames already in VAAPI memory from hw decode). SW tonemap + HDR always
        // uses CPU scale regardless of hw type.
        let scale_filter = if ffmpeg_video_codec != "copy" {
            if cuda_native {
                None
            } else if matches!(accel, HardwareAccelerationType::Qsv)
                && (!hdr || do_vpp_tonemap)
            {
                Some(build_qsv_scale_filter(params.max_width, params.max_height))
            } else {
                build_scale_filter(params)
            }
        } else {
            None
        };
        let vf = match (&scale_filter, &hw_suffix) {
            (Some(s), Some(suf)) => Some(format!("{s},{suf}")),
            (Some(s), None) => Some(s.clone()),
            (None, Some(suf)) if ffmpeg_video_codec != "copy" => Some(suf.clone()),
            _ => None,
        };
        let vf = if hdr && ffmpeg_video_codec != "copy" {
            if cuda_native {
                vf
            } else if do_vpp_tonemap && matches!(accel, HardwareAccelerationType::Vaapi) {
                // VAAPI VPP: frames are in VAAPI memory after hwupload; append tonemap_vaapi.
                let vpp = "tonemap_vaapi=format=nv12:p=bt709:t=bt709:m=bt709:extra_hw_frames=32";
                let base = vf.unwrap_or_default();
                Some(if base.is_empty() {
                    format!("format=nv12,hwupload,{vpp}")
                } else {
                    format!("{base},{vpp}")
                })
            } else if do_vpp_tonemap {
                // QSV VPP: tonemap_vaapi already embedded in hw_suffix above.
                vf
            } else if do_sw_tonemap {
                // Software tonemapx: CPU filter, output SDR. Rebuild filter chain
                // from scale + tonemapx (bypassing hw_suffix which carries hwupload
                // or format conversions that conflict with tonemapx).
                let algo = &params.tonemapping_algorithm;
                let desat = params.tonemapping_desat;
                let peak = params.tonemapping_peak;
                let out_fmt = if matches!(
                    accel,
                    HardwareAccelerationType::None | HardwareAccelerationType::V4l2m2m
                ) {
                    "yuv420p"
                } else {
                    "nv12"
                };
                let tonemapx = format!(
                    "tonemapx=tonemap={algo}:desat={desat:.1}:peak={peak:.1}:t=bt709:m=bt709:p=bt709:format={out_fmt}"
                );
                let upload = if matches!(accel, HardwareAccelerationType::Vaapi) {
                    ",hwupload"
                } else {
                    ""
                };
                Some(match scale_filter.as_deref() {
                    Some(s) => format!("{s},{tonemapx}{upload}"),
                    None => format!("{tonemapx}{upload}"),
                })
            } else {
                // No tone mapping: rewrite colour metadata so clients treat output as SDR.
                let setparams =
                    "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709";
                Some(match vf {
                    Some(f) => format!("{setparams},{f}"),
                    None => setparams.to_string(),
                })
            }
        } else {
            vf
        };
        if let Some(ref filter) = vf {
            args.extend(["-vf".into(), filter.clone()]);
        }
        if let Some(audio_idx) = params.audio_stream_index {
            args.extend([
                "-map".into(),
                "0:v:0".into(),
                "-map".into(),
                format!("0:{}", audio_idx),
            ]);
        }
    }

    // Video codec
    args.extend(["-c:v".into(), ffmpeg_video_codec.clone()]);

    if ffmpeg_video_codec == "copy" {
        if is_hevc_copy {
            args.extend(["-tag:v".into(), "hvc1".into()]);
            // Strip embedded Dolby Vision RPU NALs only when the source is actually DoVi;
            // dovi_rpu only supports hevc/av1 and will crash ffmpeg on any other codec.
            let is_dovi = matches!(
                params.source_video_range_type,
                Some(VideoRangeType::Dovi)
                    | Some(VideoRangeType::DoviWithHdr10)
                    | Some(VideoRangeType::DoviWithHlg)
                    | Some(VideoRangeType::DoviWithSdr)
            );
            if is_dovi {
                args.extend(["-bsf:v".into(), "dovi_rpu=strip=1".into()]);
            }
        }
    } else if is_hw {
        if ffmpeg_video_codec == "h264_nvenc" {
            // h264_nvenc is 8-bit only. The CUDA-native filter chain above owns
            // conversion to an 8-bit surface. Do not also pass `-pix_fmt` here:
            // for CUDA hardware frames FFmpeg inserts a software `auto_scale`
            // between scale_cuda/tonemap_cuda and NVENC, and that filter cannot
            // cross the hardware-frame boundary. Non-CUDA hardware paths already
            // select their accepted surface format in their filter suffix.
            let gop_size =
                nvenc_hls_gop_size(params.source_frame_rate, params.segment_length);
            args.extend([
                "-profile:v".into(),
                "high".into(),
                "-preset".into(),
                nvenc_preset(
                    params
                        .encoding_preset
                        .unwrap_or_default(),
                )
                .into(),
                // Low-latency tuning matches libx264's `-tune zerolatency`, which
                // matters for HLS segments served while still transcoding.
                "-tune".into(),
                "ll".into(),
                // Quality-targeted VBR, mirroring the CRF used for software.
                "-rc".into(),
                "vbr".into(),
                "-cq".into(),
                params
                    .h264_crf
                    .to_string(),
                // FFmpeg 8's h264_nvenc path does not reliably honour
                // expression-only forced keyframes. Without an explicit GOP,
                // a remote VOD can become one file-sized HLS segment that is
                // not published until EOF.
                "-g".into(),
                gop_size.to_string(),
                "-keyint_min".into(),
                gop_size.to_string(),
                "-forced-idr".into(),
                "1".into(),
            ]);
            // Treat the client's bitrate as a ceiling, not a CBR target.
            if let Some(bitrate) = params.video_bitrate {
                args.extend([
                    "-maxrate".into(),
                    bitrate.to_string(),
                    "-bufsize".into(),
                    (bitrate * 2).to_string(),
                ]);
            }
        } else if let Some(bitrate) = params.video_bitrate {
            // Other HW encoders use plain bitrate control.
            args.extend(["-b:v".into(), bitrate.to_string()]);
        }
    } else if ffmpeg_video_codec == "libx264" {
        let preset = params
            .encoding_preset
            .unwrap_or_default()
            .to_string();
        args.extend([
            "-profile:v".into(),
            "high".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-crf".into(),
            params
                .h264_crf
                .to_string(),
            "-preset".into(),
            preset,
            "-tune".into(),
            "zerolatency".into(),
        ]);
        // Use client's max bitrate as a ceiling, not a CBR target.
        if let Some(bitrate) = params.video_bitrate {
            args.extend([
                "-maxrate".into(),
                bitrate.to_string(),
                "-bufsize".into(),
                (bitrate * 2).to_string(),
            ]);
        }
    } else if ffmpeg_video_codec == "libx265" {
        let preset = params
            .encoding_preset
            .unwrap_or_default()
            .to_string();
        args.extend([
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-crf".into(),
            params
                .h265_crf
                .to_string(),
            "-preset".into(),
            preset,
        ]);
        if let Some(bitrate) = params.video_bitrate {
            args.extend([
                "-maxrate".into(),
                bitrate.to_string(),
                "-bufsize".into(),
                (bitrate * 2).to_string(),
            ]);
        }
    } else if let Some(bitrate) = params.video_bitrate {
        args.extend(["-b:v".into(), bitrate.to_string()]);
    }

    // HLS can only close a segment on a video keyframe. Hardware encoders may
    // otherwise choose a very long GOP, leaving one ever-growing .ts file and
    // no playlist until the muxer overflows. Force a boundary at the requested
    // segment cadence for every encoded-video path; copied video keeps the
    // source GOP unchanged.
    if ffmpeg_video_codec != "copy" {
        args.extend([
            "-force_key_frames".into(),
            format!("expr:gte(t,n_forced*{})", params.segment_length),
        ]);
    }

    // Audio codec
    args.extend(["-c:a".into(), ffmpeg_audio_codec.into()]);
    if ffmpeg_audio_codec == "copy" {
        // AAC streams from IPTV sources often use ADTS framing, which is not
        // valid inside MP4/fMP4 containers. Apply the reframing filter when copying.
        if params
            .source_audio_codec
            .as_deref()
            .and_then(|s| {
                s.parse::<AudioCodec>()
                    .ok()
            })
            .as_ref()
            .map(AudioCodec::needs_adts_reframe)
            .unwrap_or(false)
        {
            args.extend(["-bsf:a".into(), "aac_adtstoasc".into()]);
        }
    } else {
        let audio_bitrate = params
            .audio_bitrate
            .unwrap_or(128_000);
        args.extend(["-b:a".into(), audio_bitrate.to_string()]);
        if let Some(ch) = params.audio_channels {
            args.extend(["-ac".into(), ch.to_string()]);
        }
        if params.normalize_audio_loudness {
            args.extend(["-af".into(), "loudnorm=I=-14:TP=-1:LRA=11".into()]);
        }
    }

    // HLS output
    let playlist = params
        .output_dir
        .join("main.m3u8");
    let seg_ext = if is_hevc_copy { "m4s" } else { "ts" };
    let segment = params
        .output_dir
        .join(format!("segment_%05d.{}", seg_ext));

    args.extend([
        "-f".into(),
        "hls".into(),
        "-hls_time".into(),
        params
            .segment_length
            .to_string(),
        "-start_number".into(),
        params
            .hls_start_number
            .to_string(),
        "-hls_segment_filename".into(),
        segment
            .to_string_lossy()
            .into_owned(),
        "-hls_playlist_type".into(),
        "event".into(),
        "-hls_list_size".into(),
        "0".into(),
    ]);

    if is_hevc_copy {
        // Apple HLS spec: HEVC must be delivered in fMP4/CMAF segments, not MPEG-TS.
        // Use just the filename for init; ffmpeg places it alongside the segments.
        args.extend([
            "-hls_segment_type".into(),
            "fmp4".into(),
            "-hls_fmp4_init_filename".into(),
            "init.mp4".into(),
        ]);
    }

    args.push(
        playlist
            .to_string_lossy()
            .into_owned(),
    );

    args
}

/// Build environment variable overrides needed for the ffmpeg process.
///
/// Mirrors Jellyfin's rule: only set LIBVA_DRIVER_NAME for "i965" because
/// i965 has *lower* priority than iHD in libva's lookup order — without the
/// env var, libva would pick iHD first and ignore i965.  For iHD the
/// `driver=iHD` option inside `-init_hw_device` is sufficient and no env
/// override is needed.
fn ffmpeg_env_overrides(
    accel: HardwareAccelerationType,
    vaapi_driver: &str,
) -> Vec<(String, String)> {
    match accel {
        HardwareAccelerationType::Vaapi | HardwareAccelerationType::Qsv => {
            if vaapi_driver == "i965" {
                // Force i965 via env so libva doesn't prefer iHD over it.
                vec![("LIBVA_DRIVER_NAME".to_string(), "i965".to_string())]
            } else {
                // iHD (and unknown): driver= in init_hw_device is enough.
                vec![]
            }
        }
        _ => vec![],
    }
}

/// Spawn an ffmpeg process, drain its stderr to DEBUG logs, and wait for it
/// to finish (or be killed via `kill_rx`).  Returns the exit result and the
/// accumulated stderr text for error reporting.
async fn run_ffmpeg(
    args: Vec<String>,
    env_overrides: Vec<(String, String)>,
    kill_rx: tokio::sync::oneshot::Receiver<()>,
    monitor_stop_tx: tokio::sync::oneshot::Sender<()>,
    ffmpeg_pid_out: Arc<AtomicU32>,
    output_dir: std::path::PathBuf,
    output_tx: Arc<tokio::sync::watch::Sender<u64>>,
) -> (
    Option<std::result::Result<std::process::ExitStatus, std::io::Error>>,
    String,
) {
    let mut cmd = tokio::process::Command::new(ffmpeg_bin());
    cmd.args(&args)
        .stderr(Stdio::piped());
    for (k, v) in env_overrides {
        cmd.env(k, v);
    }
    let child = cmd.spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            let _ = monitor_stop_tx.send(());
            return (Some(Err(e)), String::new());
        }
    };

    let pid = child
        .id()
        .unwrap_or(0);
    ffmpeg_pid_out.store(pid, Ordering::Relaxed);
    if pid > 0 {
        let _ = std::fs::write(output_dir.join(".pid"), pid.to_string());
    }
    info!(
        ffmpeg_pid = pid,
        output_dir = %output_dir.display(),
        "HLS ffmpeg process started"
    );
    let notifier_dir = output_dir.clone();
    let output_notifier = tokio::spawn(async move {
        let mut version = *output_tx.borrow();
        let mut previous_signature = hls_output_signature(&notifier_dir);
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let signature = hls_output_signature(&notifier_dir);
            if signature == previous_signature {
                continue;
            }
            previous_signature = signature;
            version = version.wrapping_add(1);
            let _ = output_tx.send(version);
        }
    });
    let stderr = child
        .stderr
        .take();

    let (stderr_tx, stderr_rx) = tokio::sync::oneshot::channel::<String>();
    if let Some(stderr) = stderr {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            let mut buf = String::new();
            while let Ok(Some(line)) = lines
                .next_line()
                .await
            {
                if !line.is_empty() {
                    debug!("ffmpeg: {}", line);
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
            let _ = stderr_tx.send(buf);
        });
    } else {
        let _ = stderr_tx.send(String::new());
    }

    let pid = ffmpeg_pid_out.load(Ordering::Relaxed);
    let result = tokio::select! {
        r = child.wait() => Some(r),
        _ = kill_rx => {
            // Resume if the buffer monitor paused ffmpeg so kill() can land.
            #[cfg(unix)]
            send_signal(pid, libc::SIGCONT);
            let _ = child.kill().await;
            let _ = child.wait().await;
            None
        }
    };

    let _ = monitor_stop_tx.send(());
    output_notifier.abort();
    let stderr_out = stderr_rx
        .await
        .unwrap_or_default();
    (result, stderr_out)
}

fn hls_output_signature(output_dir: &std::path::Path) -> (u64, u64) {
    std::fs::read_dir(output_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.metadata().ok())
                .fold((0_u64, 0_u64), |(count, bytes), metadata| {
                    (count + 1, bytes.saturating_add(metadata.len()))
                })
        })
        .unwrap_or_default()
}

/// Start an HLS transcode job by spawning ffmpeg.
/// If hardware-accelerated encoding fails, automatically retries once with
/// software encoding so a broken HW driver doesn't permanently block playback.
pub async fn start_transcode(
    session: Arc<RwLock<TranscodeSession>>,
    params: TranscodeParams,
) -> Result<()> {
    {
        let mut s = session
            .write()
            .await;
        s.state = TranscodeState::Running;
        let _ = s
            .state_tx
            .send(TranscodeState::Running);
    }

    std::fs::create_dir_all(&params.output_dir)
        .map_err(|e| anyhow!("Failed to create output dir: {}", e))?;

    let session_clone = session.clone();
    tokio::spawn(async move {
        let mut params = params;
        let mut sw_fallback = false;
        let mut hw_intact_retry = false;
        let mut full_probe_retry = false;
        let mut live_restarts = 0u32;
        const MAX_LIVE_RESTARTS: u32 = 10;
        let mut input_restarts = 0u32;
        const MAX_INPUT_RESTARTS: u32 = 3;
        // The session's clock origin stays the ticks of the initial request.
        // Input-failure restarts advance params.start_time_ticks for ffmpeg
        // but must never move the acknowledged origin or the claim check.
        let original_start_ticks = params.start_time_ticks;

        // Respect a process-wide hardware lockout from earlier failures so this
        // session doesn't pay for a spawn we already know is doomed.
        if hw_accel_locked_out()
            && !matches!(
                params.hardware_acceleration_type,
                HardwareAccelerationType::None
            )
        {
            debug!(
                accel = ?params.hardware_acceleration_type,
                "Hardware acceleration locked out after repeated failures — starting in software"
            );
            params.hardware_acceleration_type = HardwareAccelerationType::None;
            sw_fallback = true;
        }

        loop {
            let args = build_hls_args(&params);
            debug!("ffmpeg args: {}", args.join(" "));

            let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
            let (monitor_stop_tx, monitor_stop_rx) =
                tokio::sync::oneshot::channel::<()>();

            let ffmpeg_pid = Arc::new(AtomicU32::new(0));
            let (output_dir, output_tx) = {
                let mut s = session_clone
                    .write()
                    .await;
                s.start_time_secs = original_start_ticks
                    .map(|t| (t / 10_000_000) as u32)
                    .unwrap_or(0);
                // The clock origin is the initial request's ticks, even when
                // an input-failure restart advances params.start_time_ticks:
                // claim checks and start acknowledgements must keep naming the
                // original request. Any start measured for a previous
                // generation is invalidated.
                s.requested_start_ticks = original_start_ticks
                    .unwrap_or(0)
                    .max(0);
                s.actual_start_ticks = None;
                s.kill_tx = Some(kill_tx);
                spawn_buffer_monitor(
                    s.output_dir
                        .clone(),
                    s.segment_length,
                    s.playback_offset_secs
                        .clone(),
                    s.prewarm
                        .clone(),
                    ffmpeg_pid.clone(),
                    monitor_stop_rx,
                    s.id.clone(),
                );
                (
                    s.output_dir
                        .clone(),
                    s.output_tx
                        .clone(),
                )
            };

            let env_overrides = ffmpeg_env_overrides(
                params.hardware_acceleration_type,
                &params.vaapi_driver,
            );
            let (result, stderr_out) = run_ffmpeg(
                args,
                env_overrides,
                kill_rx,
                monitor_stop_tx,
                ffmpeg_pid,
                output_dir,
                output_tx,
            )
            .await;

            let using_hw = !matches!(
                params.hardware_acceleration_type,
                HardwareAccelerationType::None
            );

            let ffmpeg_failed = matches!(&result, Some(Ok(s)) if !s.success());

            // Only blame the encoder when ffmpeg's stderr actually implicates it.
            // A missing output directory or unreadable input can fail a session
            // while HW accel happens to be on; downgrading for those costs ~3x the
            // CPU for the rest of the session and hides the real fault.
            if ffmpeg_failed && using_hw && !sw_fallback && is_hw_encoder_failure(&stderr_out) {
                warn!(
                    accel = ?params.hardware_acceleration_type,
                    stderr = stderr_out.trim(),
                    "HW-accelerated transcode failed — retrying with software encoding"
                );
                note_hw_failure(params.hardware_acceleration_type);
                // Clean partial output so the retry starts fresh.
                let _ = std::fs::remove_dir_all(&params.output_dir);
                let _ = std::fs::create_dir_all(&params.output_dir);
                params.hardware_acceleration_type = HardwareAccelerationType::None;
                sw_fallback = true;
                continue;
            }

            if ffmpeg_failed
                && params.trusted_probe_data
                && !full_probe_retry
                && count_segments(&params.output_dir) == 0
            {
                warn!(
                    stderr = stderr_out.trim(),
                    "Fast probe-budget startup failed — retrying with full analysis"
                );
                let _ = std::fs::remove_dir_all(&params.output_dir);
                let _ = std::fs::create_dir_all(&params.output_dir);
                params.trusted_probe_data = false;
                full_probe_retry = true;
                continue;
            }

            // Upstream input failure (flaky remote source): restart ffmpeg at
            // the last produced segment with the session, output dir, and
            // segment numbering intact. The client rides out a short stall
            // with its buffer preserved instead of cold-starting a new
            // session. Bounded per session; deliberately stopped sessions and
            // encoder-side faults take their own paths.
            let session_stopped = session_clone
                .read()
                .await
                .stopped;
            if ffmpeg_failed
                && !params.is_live
                && input_restarts < MAX_INPUT_RESTARTS
                && is_input_source_failure(&stderr_out)
                && !session_stopped
            {
                input_restarts += 1;
                let resume_idx = max_segment_index(&params.output_dir).unwrap_or(0);
                let resume_ticks = original_start_ticks
                    .unwrap_or(0)
                    .max(0)
                    + resume_idx as i64
                        * params.segment_length as i64
                        * 10_000_000;
                let backoff_secs = 1u64 << (input_restarts - 1);
                let session_id = session_clone
                    .read()
                    .await
                    .id
                    .clone();
                warn!(
                    session_id = %session_id,
                    attempt = input_restarts,
                    resume_idx,
                    stderr = stderr_out.trim(),
                    "Input source read failed — restarting transcode at the last segment"
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                if session_clone
                    .read()
                    .await
                    .stopped
                {
                    debug!(
                        session_id = %session_id,
                        "Session stopped during input-failure backoff — not restarting"
                    );
                    break;
                }
                params.start_time_ticks = Some(resume_ticks);
                params.hls_start_number = resume_idx;
                continue;
            }

            // Failure unrelated to the encoder: retry once keeping HW accel,
            // recreating the output dir in case it was the cause.
            if ffmpeg_failed && using_hw && !hw_intact_retry {
                warn!(
                    accel = ?params.hardware_acceleration_type,
                    stderr = stderr_out.trim(),
                    "Transcode failed for a non-encoder reason — retrying with hardware encoding intact"
                );
                let _ = std::fs::remove_dir_all(&params.output_dir);
                let _ = std::fs::create_dir_all(&params.output_dir);
                hw_intact_retry = true;
                continue;
            }

            if !ffmpeg_failed && using_hw {
                note_hw_success();
            }

            let mut s = session_clone
                .write()
                .await;
            s.kill_tx = None;

            // Live streams: auto-restart on unexpected exit; only stop when killed.
            if params.is_live {
                match result {
                    None => {
                        debug!(session_id = %s.id, "live stream killed by session stop");
                        s.wait_done
                            .notify_one();
                        break;
                    }
                    Some(r) => {
                        let (status_str, stderr_str) = match &r {
                            Ok(st) => (
                                format!("{st}"),
                                stderr_out
                                    .trim()
                                    .to_string(),
                            ),
                            Err(e) => ("error".to_string(), e.to_string()),
                        };
                        if live_restarts < MAX_LIVE_RESTARTS {
                            live_restarts += 1;
                            warn!(
                                session_id = %s.id,
                                status = %status_str,
                                restart = live_restarts,
                                stderr = %stderr_str,
                                "live stream ffmpeg exited unexpectedly, restarting"
                            );
                            drop(s);
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                            continue;
                        } else {
                            let err_msg = format!(
                                "live stream ffmpeg exited after {MAX_LIVE_RESTARTS} restarts \
                                 (status {status_str}): {stderr_str}"
                            );
                            error!(session_id = %s.id, error = %err_msg, "Live stream failed");
                            s.state = TranscodeState::Error(err_msg.clone());
                            let _ = s
                                .state_tx
                                .send(TranscodeState::Error(err_msg));
                            s.wait_done
                                .notify_one();
                            break;
                        }
                    }
                }
            }

            match result {
                Some(Ok(status)) if status.success() => {
                    s.state = TranscodeState::Complete;
                    let _ = s
                        .state_tx
                        .send(TranscodeState::Complete);
                    info!(session_id = %s.id, sw_fallback, "Transcode completed successfully");
                }
                Some(Ok(status)) => {
                    let err_msg = format!(
                        "ffmpeg exited with status {}: {}",
                        status,
                        stderr_out.trim()
                    );
                    error!(session_id = %s.id, error = %err_msg, "Transcode failed");
                    s.state = TranscodeState::Error(err_msg.clone());
                    let _ = s
                        .state_tx
                        .send(TranscodeState::Error(err_msg));
                }
                Some(Err(e)) => {
                    let err_msg = format!("Failed to wait for ffmpeg: {}", e);
                    error!(session_id = %s.id, error = %err_msg, "Transcode error");
                    s.state = TranscodeState::Error(err_msg.clone());
                    let _ = s
                        .state_tx
                        .send(TranscodeState::Error(err_msg));
                }
                None => {
                    debug!(
                        session_id = %s.id,
                        session_age_ms = s.created_at.elapsed().as_millis(),
                        playlist_exists = s.variant_playlist_path().exists(),
                        stderr = %stderr_out.trim(),
                        "ffmpeg killed by session stop"
                    );
                }
            }

            s.wait_done
                .notify_one();
            break;
        }
    });

    Ok(())
}

/// Parameters for a progressive (non-HLS) transcode that streams to stdout.
#[derive(Debug, Clone)]
pub struct ProgressiveTranscodeParams {
    pub input_url: String,
    pub container: String,   // "mp4", "ts", "mkv", "webm"
    pub video_codec: String, // "copy", "libx264", "libx265", "libvpx-vp9"
    pub audio_codec: String, // "copy", "aac", "libopus"
    pub start_time_ticks: Option<i64>,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub video_bitrate: Option<u32>,
    pub audio_bitrate: Option<u32>,
    pub audio_channels: Option<u32>,
    pub audio_stream_index: Option<i32>,
    pub subtitle_stream_index: Option<i32>,
    pub burn_subtitle: bool,
    pub subtitle_width: Option<u32>,
    pub subtitle_height: Option<u32>,
    pub encoding_preset: Option<EncodingPreset>,
    pub source_video_codec: Option<String>,
    pub source_audio_codec: Option<String>,
    pub hardware_acceleration_type: HardwareAccelerationType,
    pub vaapi_device: String,
    /// VAAPI driver name (e.g. "iHD" for Intel). Empty string means auto-detect.
    pub vaapi_driver: String,
    pub source_video_range_type: Option<VideoRangeType>,
    pub enable_tonemapping: bool,
    pub enable_vpp_tonemapping: bool,
    pub tonemapping_algorithm: String,
    pub tonemapping_desat: f32,
    pub tonemapping_peak: f32,
    pub allow_hevc_encoding: bool,
    pub allow_av1_encoding: bool,
    pub h264_crf: u32,
    pub h265_crf: u32,
    /// Apply `loudnorm=I=-14:TP=-1:LRA=11` when transcoding audio. Has no effect
    /// when audio_codec is "copy". See EncodingOptions::normalize_audio_loudness.
    pub normalize_audio_loudness: bool,
}

/// Build the ffmpeg CLI args for a progressive transcode piped to stdout.
pub(crate) fn build_progressive_args(
    params: &ProgressiveTranscodeParams,
) -> Vec<String> {
    let accel = params.hardware_acceleration_type;
    let is_hw = !matches!(accel, HardwareAccelerationType::None);
    let hdr = is_hdr(
        params
            .source_video_range_type
            .as_ref(),
    );

    let do_vpp_tonemap = hdr
        && params.enable_vpp_tonemapping
        && matches!(
            accel,
            HardwareAccelerationType::Vaapi | HardwareAccelerationType::Qsv
        );
    let do_sw_tonemap = hdr && params.enable_tonemapping && !do_vpp_tonemap;

    let ffmpeg_video_codec = {
        let base = match params
            .video_codec
            .as_str()
        {
            "copy" => "copy",
            _ => "libx264",
        };
        let base = if params.burn_subtitle
            && params
                .subtitle_stream_index
                .is_some()
            && base == "copy"
        {
            "libx264"
        } else {
            base
        };
        if base != "copy" && is_hw {
            hw_encoder_name(base, accel)
        } else {
            base.to_string()
        }
    };
    let ffmpeg_audio_codec = match params
        .audio_codec
        .as_str()
    {
        "copy" => "copy",
        "aac" => "aac",
        "libopus" | "opus" => "libopus",
        "mp3" => "libmp3lame",
        other => other,
    };

    // When stream-copying into MP4 we need bitstream filters; promote to MKV instead.
    let format = {
        let requested = match params
            .container
            .as_str()
        {
            "ts" | "mpegts" => "mpegts",
            "webm" => "webm",
            "mkv" | "matroska" => "matroska",
            _ => "mp4",
        };
        if ffmpeg_video_codec == "copy" && requested == "mp4" {
            "matroska"
        } else {
            requested
        }
    };

    let mut args: Vec<String> = vec![
        "-v".into(),
        "error".into(),
        "-analyzeduration".into(),
        "5000000".into(),
        "-probesize".into(),
        "5000000".into(),
        "-reconnect".into(),
        "1".into(),
        "-reconnect_at_eof".into(),
        "1".into(),
        "-reconnect_streamed".into(),
        "1".into(),
        "-reconnect_delay_max".into(),
        "5".into(),
        "-timeout".into(),
        "30000000".into(),
        "-rw_timeout".into(),
        "30000000".into(),
    ];

    // Hardware acceleration input flags (before -ss and -i).
    if hdr && matches!(accel, HardwareAccelerationType::Qsv) && !do_vpp_tonemap {
        args.extend(qsv_init_only_args(
            &params.vaapi_device,
            &params.vaapi_driver,
        ));
    } else {
        args.extend(hw_input_args(
            accel,
            &params.vaapi_device,
            &params.vaapi_driver,
        ));
    }

    // Input seek (fast, before -i)
    if let Some(ticks) = params.start_time_ticks {
        let secs = ticks as f64 / 10_000_000.0;
        args.extend(["-ss".into(), format!("{:.6}", secs)]);
    }

    args.extend([
        "-i".into(),
        params
            .input_url
            .clone(),
    ]);

    let hw_suffix = if do_vpp_tonemap && matches!(accel, HardwareAccelerationType::Qsv)
    {
        let vpp =
            "tonemap_vaapi=format=nv12:p=bt709:t=bt709:m=bt709:extra_hw_frames=32";
        Some(format!("{vpp},hwmap=derive_device=qsv,format=qsv"))
    } else if hdr && matches!(accel, HardwareAccelerationType::Qsv) && !do_vpp_tonemap {
        Some("format=nv12".to_string())
    } else {
        hw_filter_suffix(accel)
    };

    // Stream mapping
    let scale_filter = if ffmpeg_video_codec != "copy" {
        if matches!(accel, HardwareAccelerationType::Qsv) && (!hdr || do_vpp_tonemap) {
            Some(build_qsv_scale_filter(params.max_width, params.max_height))
        } else {
            let tp = TranscodeParams {
                max_width: params.max_width,
                max_height: params.max_height,
                ..Default::default()
            };
            build_scale_filter(&tp)
        }
    } else {
        None
    };

    if params.burn_subtitle {
        if let Some(sub_idx) = params.subtitle_stream_index {
            // Image subtitle (PGS/DVD): bitmap overlay via filter_complex.
            let (out_w, out_h) = (params.max_width, params.max_height);
            let sub_scale = match (out_w, out_h) {
                (Some(w), Some(h)) => format!("scale={w}:{h}:fast_bilinear"),
                (Some(w), None) => format!("scale={w}:-1:fast_bilinear"),
                (None, Some(h)) => format!("scale=-1:{h}:fast_bilinear"),
                _ => String::new(),
            };
            let sub_preproc = if sub_scale.is_empty() {
                "scale".to_string()
            } else {
                format!("scale,{sub_scale}")
            };
            let overlay = "overlay=eof_action=pass:repeatlast=0";
            let filter = match (&scale_filter, &hw_suffix) {
                (Some(main_scale), Some(suf)) => format!(
                    "[0:{sub_idx}]{sub_preproc}[sub];[0:v:0]{main_scale}[main];[main][sub]{overlay}[vraw];[vraw]{suf}[v]"
                ),
                (Some(main_scale), None) => format!(
                    "[0:{sub_idx}]{sub_preproc}[sub];[0:v:0]{main_scale}[main];[main][sub]{overlay}[v]"
                ),
                (None, Some(suf)) => format!(
                    "[0:{sub_idx}]{sub_preproc}[sub];[0:v:0][sub]{overlay}[vraw];[vraw]{suf}[v]"
                ),
                (None, None) => {
                    format!("[0:{sub_idx}]{sub_preproc}[sub];[0:v:0][sub]{overlay}[v]")
                }
            };
            args.extend(["-filter_complex".into(), filter]);
            args.extend(["-map".into(), "[v]".into()]);
            if let Some(audio_idx) = params.audio_stream_index {
                args.extend(["-map".into(), format!("0:{}", audio_idx)]);
            } else {
                args.extend(["-map".into(), "0:a?".into()]);
            }
        }
    } else {
        let vf = match (&scale_filter, &hw_suffix) {
            (Some(s), Some(suf)) => Some(format!("{s},{suf}")),
            (Some(s), None) => Some(s.clone()),
            (None, Some(suf)) if ffmpeg_video_codec != "copy" => Some(suf.clone()),
            _ => None,
        };
        let vf = if hdr && ffmpeg_video_codec != "copy" {
            if do_vpp_tonemap && matches!(accel, HardwareAccelerationType::Vaapi) {
                let vpp = "tonemap_vaapi=format=nv12:p=bt709:t=bt709:m=bt709:extra_hw_frames=32";
                let base = vf.unwrap_or_default();
                Some(if base.is_empty() {
                    format!("format=nv12,hwupload,{vpp}")
                } else {
                    format!("{base},{vpp}")
                })
            } else if do_vpp_tonemap {
                vf
            } else if do_sw_tonemap {
                let algo = &params.tonemapping_algorithm;
                let desat = params.tonemapping_desat;
                let peak = params.tonemapping_peak;
                let out_fmt = if matches!(
                    accel,
                    HardwareAccelerationType::None | HardwareAccelerationType::V4l2m2m
                ) {
                    "yuv420p"
                } else {
                    "nv12"
                };
                let tonemapx = format!(
                    "tonemapx=tonemap={algo}:desat={desat:.1}:peak={peak:.1}:t=bt709:m=bt709:p=bt709:format={out_fmt}"
                );
                let upload = if matches!(accel, HardwareAccelerationType::Vaapi) {
                    ",hwupload"
                } else {
                    ""
                };
                Some(match scale_filter.as_deref() {
                    Some(s) => format!("{s},{tonemapx}{upload}"),
                    None => format!("{tonemapx}{upload}"),
                })
            } else {
                let setparams =
                    "setparams=color_primaries=bt709:color_trc=bt709:colorspace=bt709";
                Some(match vf {
                    Some(f) => format!("{setparams},{f}"),
                    None => setparams.to_string(),
                })
            }
        } else {
            vf
        };
        if let Some(ref filter) = vf {
            args.extend(["-vf".into(), filter.clone()]);
        }
        if params
            .audio_stream_index
            .is_some()
            || params
                .subtitle_stream_index
                .is_some()
        {
            args.extend(["-map".into(), "0:v:0".into()]);
            if let Some(audio_idx) = params.audio_stream_index {
                args.extend(["-map".into(), format!("0:{}", audio_idx)]);
            } else {
                args.extend(["-map".into(), "0:a?".into()]);
            }
            if let Some(sub_idx) = params.subtitle_stream_index {
                if sub_idx >= 0 {
                    args.extend(["-map".into(), format!("0:{}?", sub_idx)]);
                }
            }
        }
    }

    // Video
    args.extend(["-c:v".into(), ffmpeg_video_codec.clone()]);
    if ffmpeg_video_codec == "copy" {
        // Apply hvc1 codec tag for HEVC Apple compatibility
        if params
            .source_video_codec
            .as_deref()
            .and_then(|s| {
                s.parse::<VideoCodec>()
                    .ok()
            })
            .as_ref()
            .map(VideoCodec::is_hevc)
            .unwrap_or(false)
        {
            args.extend(["-tag:v".into(), "hvc1".into()]);
        }
    } else if is_hw {
        if let Some(bitrate) = params.video_bitrate {
            args.extend(["-b:v".into(), bitrate.to_string()]);
        }
    } else if ffmpeg_video_codec == "libx264" {
        let preset = params
            .encoding_preset
            .unwrap_or_default()
            .to_string();
        args.extend([
            "-profile:v".into(),
            "high".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-crf".into(),
            params
                .h264_crf
                .to_string(),
            "-preset".into(),
            preset,
        ]);
        if let Some(bitrate) = params.video_bitrate {
            args.extend([
                "-maxrate".into(),
                bitrate.to_string(),
                "-bufsize".into(),
                (bitrate * 2).to_string(),
            ]);
        }
    } else if ffmpeg_video_codec == "libx265" {
        let preset = params
            .encoding_preset
            .unwrap_or_default()
            .to_string();
        args.extend([
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-crf".into(),
            params
                .h265_crf
                .to_string(),
            "-preset".into(),
            preset,
        ]);
        if let Some(bitrate) = params.video_bitrate {
            args.extend([
                "-maxrate".into(),
                bitrate.to_string(),
                "-bufsize".into(),
                (bitrate * 2).to_string(),
            ]);
        }
    } else if let Some(bitrate) = params.video_bitrate {
        args.extend(["-b:v".into(), bitrate.to_string()]);
    }

    // Audio
    args.extend(["-c:a".into(), ffmpeg_audio_codec.into()]);
    if ffmpeg_audio_codec == "copy" {
        if params
            .source_audio_codec
            .as_deref()
            .and_then(|s| {
                s.parse::<AudioCodec>()
                    .ok()
            })
            .as_ref()
            .map(AudioCodec::needs_adts_reframe)
            .unwrap_or(false)
        {
            args.extend(["-bsf:a".into(), "aac_adtstoasc".into()]);
        }
    } else {
        if let Some(bitrate) = params.audio_bitrate {
            args.extend(["-b:a".into(), bitrate.to_string()]);
        }
        if let Some(ch) = params.audio_channels {
            args.extend(["-ac".into(), ch.to_string()]);
        }
        if params.normalize_audio_loudness {
            args.extend(["-af".into(), "loudnorm=I=-14:TP=-1:LRA=11".into()]);
        }
    }

    // Format-specific flags
    args.extend(["-strict".into(), "unofficial".into()]);
    if format == "mp4" {
        args.extend([
            "-movflags".into(),
            "frag_keyframe+empty_moov+default_base_moof".into(),
        ]);
    }

    args.extend(["-f".into(), format.into(), "pipe:1".into()]);

    args
}

/// Start a progressive transcode that returns a readable byte stream.
pub fn start_progressive_transcode(
    params: ProgressiveTranscodeParams,
) -> Result<
    impl futures::Stream<Item = std::result::Result<bytes::Bytes, std::io::Error>>,
> {
    let args = build_progressive_args(&params);
    debug!("ffmpeg progressive args: {:?}", args);

    let env_overrides =
        ffmpeg_env_overrides(params.hardware_acceleration_type, &params.vaapi_driver);
    let mut cmd = tokio::process::Command::new(ffmpeg_bin());
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env_overrides {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn ffmpeg: {}", e))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture ffmpeg stdout"))?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture ffmpeg stderr"))?;

    // Log stderr line-by-line at DEBUG and reap child when done.
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines
            .next_line()
            .await
        {
            if !line.is_empty() {
                debug!("ffmpeg: {}", line);
            }
        }
        match child
            .wait()
            .await
        {
            Ok(status) if !status.success() => {
                if status.code() == Some(224) {
                    debug!(
                        "progressive ffmpeg exited after client disconnect: {}",
                        status
                    )
                } else {
                    error!("progressive ffmpeg exited: {}", status)
                }
            }
            Ok(status) => debug!("progressive ffmpeg exited: {}", status),
            Err(e) => error!("progressive ffmpeg wait error: {}", e),
        }
    });

    info!(
        "Starting progressive transcode (container={}, vcodec={}, acodec={})",
        params.container, params.video_codec, params.audio_codec
    );

    Ok(tokio_util::io::ReaderStream::new(stdout))
}

/// Generate the variant (child) HLS playlist server-side as a VOD playlist.
///
/// Lists ALL segments from time 0 to the end of the media so HLS.js can seek
/// to any position immediately. Each segment URL includes `runtimeTicks` and
/// `actualSegmentLengthTicks` query params so the segment handler knows the
/// cumulative position when it needs to restart FFmpeg.
pub fn generate_variant_playlist(
    session: &TranscodeSession,
    query_string: &str,
) -> String {
    let runtime_ticks = session.runtime_ticks;
    let segment_length = session.segment_length;
    let play_session_id = &session.id;
    let start_time_secs = session.start_time_secs;
    let use_fmp4 = session.use_fmp4();
    // fMP4 segments require HLS version 7; standard TS segments need version 6.
    let version = if use_fmp4 { 7u32 } else { 6u32 };
    let seg_ext = if use_fmp4 { "m4s" } else { "ts" };

    let extra_qs = if query_string.is_empty() {
        String::new()
    } else {
        format!("&{}", query_string)
    };

    if runtime_ticks <= 0 || segment_length == 0 {
        // Fallback: single long segment (shouldn't happen in practice)
        let mut buf = format!(
            "#EXTM3U\n\
             #EXT-X-PLAYLIST-TYPE:VOD\n\
             #EXT-X-VERSION:{version}\n\
             #EXT-X-TARGETDURATION:{seg}\n\
             #EXT-X-MEDIA-SEQUENCE:0\n",
            version = version,
            seg = segment_length,
        );
        if use_fmp4 {
            buf.push_str(&format!(
                "#EXT-X-MAP:URI=\"init.mp4?PlaySessionId={}\"\n",
                play_session_id
            ));
        }
        buf.push_str(&format!(
            "#EXTINF:{seg}.000000, nodesc\n\
             segment_00000.{ext}?PlaySessionId={psid}{qs}\n\
             #EXT-X-ENDLIST\n",
            seg = segment_length,
            ext = seg_ext,
            psid = play_session_id,
            qs = extra_qs,
        ));
        return buf;
    }

    let seg_length_ticks = (segment_length as i64)
        .to_ticks(TickUnit::Seconds)
        .unwrap_or(0);
    let whole_segments = runtime_ticks / seg_length_ticks;
    let remaining_ticks = runtime_ticks % seg_length_ticks;
    let total_segments = whole_segments + if remaining_ticks > 0 { 1 } else { 0 };

    let target_duration = segment_length; // always an integer ceiling

    let mut buf = String::with_capacity(total_segments as usize * 120);
    buf.push_str("#EXTM3U\n");
    buf.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    buf.push_str(&format!("#EXT-X-VERSION:{}\n", version));
    buf.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target_duration));
    buf.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    if start_time_secs > 0 {
        buf.push_str(&format!(
            "#EXT-X-START:TIME-OFFSET={:.6},PRECISE=YES\n",
            start_time_secs as f64
        ));
    }
    if use_fmp4 {
        buf.push_str(&format!(
            "#EXT-X-MAP:URI=\"init.mp4?PlaySessionId={}\"\n",
            play_session_id
        ));
    }

    let mut cumulative_ticks: i64 = 0;
    for i in 0..total_segments {
        let length_ticks = if i < whole_segments {
            seg_length_ticks
        } else {
            remaining_ticks
        };
        let length_secs = length_ticks as f64 / 10_000_000.0;

        buf.push_str(&format!("#EXTINF:{:.6}, nodesc\n", length_secs));
        buf.push_str(&format!(
            "segment_{:05}.{}?PlaySessionId={}&runtimeTicks={}&actualSegmentLengthTicks={}{}\n",
            i,
            seg_ext,
            play_session_id,
            cumulative_ticks,
            length_ticks,
            extra_qs,
        ));

        cumulative_ticks += length_ticks;
    }

    buf.push_str("#EXT-X-ENDLIST\n");
    buf
}

/// Generate the HEVC codec string for HLS CODECS attribute.
/// Matches Jellyfin's `HlsCodecStringHelpers.GetH265String()`.
fn hevc_hls_codec_string(profile: Option<&str>, level: Option<f64>) -> String {
    let profile_part = match profile {
        Some(p)
            if p.eq_ignore_ascii_case("main 10")
                || p.eq_ignore_ascii_case("main10") =>
        {
            "2.4"
        }
        _ => "1.4",
    };
    let level_val = level.unwrap_or(150.0) as i32;
    format!("hvc1.{}.L{}.B0", profile_part, level_val)
}

/// Generate a master HLS playlist that references the variant playlist.
pub fn generate_master_playlist(session: &TranscodeSession) -> String {
    use remux_sdks::remux::VideoRangeType;

    let play_session_id = &session.id;

    let video_codec_str: String = match session
        .video_codec
        .as_str()
    {
        "copy" => {
            if session
                .source_video_codec
                .as_deref()
                .and_then(|s| {
                    s.parse::<VideoCodec>()
                        .ok()
                })
                .as_ref()
                .map(VideoCodec::is_hevc)
                .unwrap_or(false)
            {
                hevc_hls_codec_string(
                    session
                        .source_video_profile
                        .as_deref(),
                    session.source_video_level,
                )
            } else {
                "avc1.640028".to_string()
            }
        }
        "h264" | "libx264" => "avc1.640028".to_string(),
        "hevc" | "libx265" => hevc_hls_codec_string(
            session
                .source_video_profile
                .as_deref(),
            session.source_video_level,
        ),
        _ => "avc1.640028".to_string(),
    };
    let audio_codec_str = if session.audio_codec == "copy" {
        // Use the actual source codec when copying so the CODECS attribute
        // matches the bitstream. Browsers that see "mp4a.40.2" but receive
        // eac3 will fail to initialize the audio decoder.
        match session
            .source_audio_codec
            .as_deref()
            .and_then(|s| {
                s.parse::<AudioCodec>()
                    .ok()
            }) {
            Some(AudioCodec::Eac3) => "ec-3",
            Some(AudioCodec::Ac3) => "ac-3",
            _ => "mp4a.40.2",
        }
    } else {
        "mp4a.40.2"
    };
    let codecs = format!("{},{}", video_codec_str, audio_codec_str);

    // VIDEO-RANGE: only meaningful for copy-mode (passthrough) video. Transcoded output is SDR.
    let video_range_attr = if session.video_codec == "copy" {
        match session.source_video_range_type {
            Some(VideoRangeType::Hdr10)
            | Some(VideoRangeType::Hdr10Plus)
            | Some(VideoRangeType::DoviWithHdr10)
            | Some(VideoRangeType::Dovi) => ",VIDEO-RANGE=PQ",
            Some(VideoRangeType::Hlg) | Some(VideoRangeType::DoviWithHlg) => {
                ",VIDEO-RANGE=HLG"
            }
            _ => ",VIDEO-RANGE=SDR",
        }
    } else {
        ",VIDEO-RANGE=SDR"
    };

    let resolution_attr =
        match (session.source_video_width, session.source_video_height) {
            (Some(w), Some(h)) => format!(",RESOLUTION={}x{}", w, h),
            _ => String::new(),
        };

    let frame_rate_attr = match session.source_frame_rate {
        Some(fps) if fps > 0.0 => format!(",FRAME-RATE={:.3}", fps),
        _ => String::new(),
    };

    debug!(
        source_video_codec = ?session.source_video_codec,
        source_video_profile = ?session.source_video_profile,
        source_video_level = ?session.source_video_level,
        source_video_range_type = ?session.source_video_range_type,
        source_video_width = session.source_video_width,
        source_video_height = session.source_video_height,
        source_frame_rate = session.source_frame_rate,
        video_codec = session.video_codec.as_str(),
        codecs = codecs.as_str(),
        "generating master HLS playlist"
    );

    format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:6\n\
         #EXT-X-INDEPENDENT-SEGMENTS\n\
         #EXT-X-STREAM-INF:BANDWIDTH=2000000,AVERAGE-BANDWIDTH=2000000,CODECS=\"{codecs}\"{video_range}{resolution}{frame_rate}\n\
         main.m3u8?PlaySessionId={play_session_id}\n",
        codecs = codecs,
        video_range = video_range_attr,
        resolution = resolution_attr,
        frame_rate = frame_rate_attr,
        play_session_id = play_session_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use remux_sdks::remux::TranscodeReasons;
    use std::path::PathBuf;
    use uuid::Uuid;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn args_contains(args: &[String], flag: &str) -> bool {
        args.iter()
            .any(|a| a == flag)
    }

    fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.windows(2)
            .find(|w| w[0] == flag)
            .map(|w| w[1].as_str())
    }

    #[test]
    fn speculative_prewarm_has_a_bounded_buffer() {
        let now = Instant::now();
        let mut state = AdaptiveBufferState::default();
        assert_eq!(state.target_for("warm", 0, true, 6, now), 12);
    }

    #[test]
    fn adaptive_buffer_prioritizes_two_cold_starts() {
        let now = Instant::now();
        let mut state = AdaptiveBufferState::default();
        assert_eq!(state.target_for("first", 0, false, 6, now), 180);
        assert_eq!(
            state.target_for(
                "second",
                0,
                false,
                6,
                now + Duration::from_millis(1)
            ),
            180
        );
        assert_eq!(
            state.target_for(
                "third",
                0,
                false,
                6,
                now + Duration::from_millis(2)
            ),
            6
        );
    }

    #[test]
    fn new_startup_preempts_idle_completion_fill() {
        let now = Instant::now();
        let mut state = AdaptiveBufferState::default();
        assert_eq!(
            state.target_for("ready", GUARANTEED_BUFFER_SECS, false, 6, now),
            IDLE_FILL_BUFFER_SECS
        );
        assert_eq!(
            state.target_for(
                "cold",
                0,
                false,
                6,
                now + Duration::from_millis(1)
            ),
            GUARANTEED_BUFFER_SECS
        );
        assert_eq!(
            state.target_for(
                "ready",
                GUARANTEED_BUFFER_SECS,
                false,
                6,
                now + Duration::from_millis(2)
            ),
            GUARANTEED_BUFFER_SECS
        );
    }

    // ── select_hw_accel tests ─────────────────────────────────────────────────

    fn no_devices(_: &str) -> bool {
        false
    }
    fn all_devices(_: &str) -> bool {
        true
    }

    /// Render an `ffmpeg -encoders` listing containing exactly `encoders`.
    fn encoders_listing(encoders: &[&str]) -> String {
        std::iter::once("Encoders:".to_string())
            .chain(
                encoders
                    .iter()
                    .map(|e| format!(" V....D {e} test encoder")),
            )
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The tests below predate encode-capability gating and are written against
    /// `ffmpeg -hwaccels` output. Translate to an encoder listing under the old
    /// assumption that every decode hwaccel implies its matching encoder, so they
    /// keep asserting the device/vendor precedence rules they were written for.
    fn select_hw_accel(
        hwaccels_output: &str,
        device_exists: impl Fn(&str) -> bool,
        drm_vendor: impl Fn() -> Option<String>,
    ) -> HardwareAccelerationType {
        let encoders: Vec<&str> = hwaccels_output
            .lines()
            .filter_map(|l| match l.trim() {
                "cuda" => Some("h264_nvenc"),
                "qsv" => Some("h264_qsv"),
                "vaapi" => Some("h264_vaapi"),
                "videotoolbox" => Some("h264_videotoolbox"),
                "v4l2m2m" => Some("h264_v4l2m2m"),
                "rkmpp" => Some("h264_rkmpp"),
                _ => None,
            })
            .collect();
        super::select_hw_accel(&encoders_listing(&encoders), device_exists, drm_vendor)
    }

    #[test]
    fn hw_none_when_no_hwaccels_listed() {
        assert_eq!(
            select_hw_accel("", all_devices, || None),
            HardwareAccelerationType::None
        );
    }

    #[test]
    fn hw_none_when_devices_absent() {
        let hwaccels =
            "Hardware acceleration methods:\ncuda\nvaapi\nqsv\nv4l2m2m\nrkmpp\n";
        assert_eq!(
            select_hw_accel(hwaccels, no_devices, || None),
            HardwareAccelerationType::None
        );
    }

    #[test]
    fn hw_nvenc_requires_cuda_and_device() {
        let hwaccels = "Hardware acceleration methods:\ncuda\n";
        // Device present
        assert_eq!(
            select_hw_accel(hwaccels, |p| p == "/dev/nvidia0", || None),
            HardwareAccelerationType::Nvenc
        );
        // Device absent
        assert_eq!(
            select_hw_accel(hwaccels, no_devices, || None),
            HardwareAccelerationType::None
        );
    }

    #[test]
    fn hw_vaapi_requires_vaapi_and_device() {
        let hwaccels = "Hardware acceleration methods:\nvaapi\n";
        assert_eq!(
            select_hw_accel(hwaccels, |p| p == "/dev/dri/renderD128", || None),
            HardwareAccelerationType::Vaapi
        );
        assert_eq!(
            select_hw_accel(hwaccels, no_devices, || None),
            HardwareAccelerationType::None
        );
    }

    #[test]
    fn hw_qsv_beats_vaapi_when_both_available() {
        let hwaccels = "Hardware acceleration methods:\nvaapi\nqsv\n";
        assert_eq!(
            select_hw_accel(
                hwaccels,
                |p| p == "/dev/dri/renderD128",
                || { Some("0x8086".to_string()) }
            ),
            HardwareAccelerationType::Qsv
        );
    }

    #[test]
    fn hw_qsv_requires_dri_device() {
        let hwaccels = "Hardware acceleration methods:\nqsv\n";
        assert_eq!(
            select_hw_accel(
                hwaccels,
                |p| p == "/dev/dri/renderD128",
                || { Some("0x8086".to_string()) }
            ),
            HardwareAccelerationType::Qsv
        );
        assert_eq!(
            select_hw_accel(hwaccels, no_devices, || Some("0x8086".to_string())),
            HardwareAccelerationType::None
        );
    }

    #[test]
    fn hw_rkmpp_requires_mpp_service_device() {
        let hwaccels = "Hardware acceleration methods:\nrkmpp\n";
        assert_eq!(
            select_hw_accel(hwaccels, |p| p == "/dev/mpp_service", || None),
            HardwareAccelerationType::Rkmpp
        );
        // This was the Asahi Linux bug: rkmpp listed but device absent → None
        assert_eq!(
            select_hw_accel(hwaccels, no_devices, || None),
            HardwareAccelerationType::None
        );
    }

    #[test]
    fn hw_v4l2m2m_requires_video0_device() {
        let hwaccels = "Hardware acceleration methods:\nv4l2m2m\n";
        assert_eq!(
            select_hw_accel(hwaccels, |p| p == "/dev/video0", || None),
            HardwareAccelerationType::V4l2m2m
        );
        assert_eq!(
            select_hw_accel(hwaccels, no_devices, || None),
            HardwareAccelerationType::None
        );
    }

    #[test]
    fn hw_videotoolbox_only_on_macos() {
        let hwaccels = "Hardware acceleration methods:\nvideotoolbox\n";
        let expected = if cfg!(target_os = "macos") {
            HardwareAccelerationType::VideoToolbox
        } else {
            HardwareAccelerationType::None
        };
        assert_eq!(select_hw_accel(hwaccels, all_devices, || None), expected);
    }

    #[test]
    fn hw_priority_nvenc_over_vaapi() {
        // When both are available nvenc wins
        let hwaccels = "Hardware acceleration methods:\ncuda\nvaapi\n";
        assert_eq!(
            select_hw_accel(hwaccels, all_devices, || None),
            HardwareAccelerationType::Nvenc
        );
    }

    #[test]
    fn hw_falls_through_to_rkmpp_when_others_absent() {
        let hwaccels =
            "Hardware acceleration methods:\ncuda\nvaapi\nqsv\nv4l2m2m\nrkmpp\n";
        // Only /dev/mpp_service present
        assert_eq!(
            select_hw_accel(hwaccels, |p| p == "/dev/mpp_service", || None),
            HardwareAccelerationType::Rkmpp
        );
    }

    // ── HW-failure attribution (verbatim stderr captured in production) ───────

    /// Real VAAPI-on-NVIDIA failure: genuinely the encoder, so fall back.
    #[test]
    fn hw_failure_detected_for_missing_vaapi_entrypoint() {
        let stderr = "[h264_vaapi @ 0x55baa0438440] No usable encoding entrypoint found for \
             profile VAProfileH264High (7).\n[vost#0:0/h264_vaapi @ 0x55baa04313c0] Error while \
             opening encoder - maybe incorrect parameters such as bit_rate, rate, width or \
             height.\n[out#0/hls @ 0x55ba9fcccd80] Nothing was written into output file";
        assert!(is_hw_encoder_failure(stderr));
    }

    /// Real VAAPI tonemap failure.
    #[test]
    fn hw_failure_detected_for_unsupported_vaapi_profile() {
        let stderr = "[Parsed_tonemap_vaapi_2 @ 0x7f414c004a00] Failed to create processing \
             pipeline config: 12 (the requested VAProfile is not supported).";
        assert!(is_hw_encoder_failure(stderr));
    }

    /// Real NVENC session that failed because its output directory vanished. The
    /// encoder opened fine — this must NOT be attributed to hardware, which is
    /// what previously downgraded live sessions to libx264.
    #[test]
    fn io_failure_not_attributed_to_hw_encoder() {
        let stderr = "[hls @ 0x562e2c6285c0] Failed to open file \
             'transcode_sessions/106d41f9211549a78cc66230de74726e/segment_00000.ts'\n\
             [vost#0:0/h264_nvenc @ 0x562e2c647040] Error submitting a packet to the muxer: No \
             such file or directory\n[out#0/hls @ 0x562e2c61b000] Error muxing a packet\n\
             [out#0/hls @ 0x562e2c61b000] Task finished with error code: -2 (No such file or \
             directory)\n[hls @ 0x562e2c6285c0] failed to rename file \
             transcode_sessions/106d41f9211549a78cc66230de74726e/main.m3u8.tmp to \
             transcode_sessions/106d41f9211549a78cc66230de74726e/main.m3u8: No such file or \
             directory";
        assert!(
            !is_hw_encoder_failure(stderr),
            "a vanished output dir is not an encoder fault"
        );
    }

    #[test]
    fn network_failure_not_attributed_to_hw_encoder() {
        let stderr = "[in#0 @ 0x55b0] Error opening input: Server returned 404 Not Found\n\
             Error opening input file https://example.invalid/movie.mkv.";
        assert!(!is_hw_encoder_failure(stderr));
    }

    #[test]
    fn hw_failure_detected_for_exhausted_nvenc_sessions() {
        let stderr = "[h264_nvenc @ 0x5566] OpenEncodeSessionEx failed: out of memory (10): \
             (no details)";
        assert!(is_hw_encoder_failure(stderr));
    }

    // ── encode-capability / vendor gating (regression: NVIDIA box chose VAAPI) ──

    /// The production failure: an NVIDIA host whose ffmpeg lists `h264_vaapi` and
    /// whose NVIDIA driver supplies /dev/dri/renderD128. nvidia-vaapi-driver is
    /// decode-only, so VAAPI would fail at encoder-open. Must not be selected.
    #[test]
    fn hw_never_selects_vaapi_on_nvidia_hardware() {
        let encoders = encoders_listing(&["h264_vaapi"]);
        assert_eq!(
            super::select_hw_accel(
                &encoders,
                |p| p == "/dev/dri/renderD128",
                || Some("0x10de".to_string()),
            ),
            HardwareAccelerationType::None,
            "VAAPI is decode-only on NVIDIA and must never be selected"
        );
    }

    /// VAAPI stays selectable on non-NVIDIA hardware with a render node.
    #[test]
    fn hw_selects_vaapi_on_amd() {
        let encoders = encoders_listing(&["h264_vaapi"]);
        assert_eq!(
            super::select_hw_accel(
                &encoders,
                |p| p == "/dev/dri/renderD128",
                || Some("0x1002".to_string()),
            ),
            HardwareAccelerationType::Vaapi
        );
    }

    /// Selection keys off `ffmpeg -encoders`, never the decode-side `-hwaccels`
    /// list: a build advertising `cuda` decode but lacking h264_nvenc is not NVENC.
    #[test]
    fn hw_nvenc_requires_encoder_not_decode_support() {
        let encoders = encoders_listing(&["h264_vaapi"]);
        assert_eq!(
            super::select_hw_accel(&encoders, all_devices, || None),
            HardwareAccelerationType::Vaapi,
            "cuda decode without h264_nvenc must not yield Nvenc"
        );
    }

    #[test]
    fn hw_selects_nvenc_when_encoder_and_device_present() {
        let encoders = encoders_listing(&["h264_nvenc", "h264_vaapi"]);
        assert_eq!(
            super::select_hw_accel(&encoders, all_devices, || Some("0x10de".to_string())),
            HardwareAccelerationType::Nvenc
        );
    }

    /// On NVIDIA, NVENC is the only candidate — VAAPI must be filtered out, so a
    /// failed NVENC smoke test falls through to software rather than to VAAPI.
    #[test]
    fn hw_candidates_on_nvidia_exclude_vaapi() {
        let encoders = encoders_listing(&["h264_nvenc", "h264_vaapi"]);
        assert_eq!(
            hw_accel_candidates(&encoders, all_devices, || Some("0x10de".to_string())),
            vec![HardwareAccelerationType::Nvenc]
        );
    }

    /// An encoder listed by ffmpeg but with no matching device is not viable.
    #[test]
    fn hw_nvenc_requires_device_node() {
        let encoders = encoders_listing(&["h264_nvenc"]);
        assert_eq!(
            super::select_hw_accel(&encoders, no_devices, || None),
            HardwareAccelerationType::None
        );
    }

    fn default_hls(output_dir: PathBuf) -> TranscodeParams {
        TranscodeParams {
            input_url: "http://localhost/test.mkv".into(),
            output_dir,
            ..Default::default()
        }
    }

    fn default_progressive() -> ProgressiveTranscodeParams {
        ProgressiveTranscodeParams {
            input_url: "http://localhost/test.mkv".into(),
            container: "mp4".into(),
            video_codec: "copy".into(),
            audio_codec: "aac".into(),
            start_time_ticks: None,
            max_width: None,
            max_height: None,
            video_bitrate: None,
            audio_bitrate: None,
            audio_channels: None,
            audio_stream_index: None,
            subtitle_stream_index: None,
            burn_subtitle: false,
            subtitle_width: None,
            subtitle_height: None,
            encoding_preset: None,
            source_video_codec: None,
            source_audio_codec: None,
            hardware_acceleration_type: HardwareAccelerationType::None,
            vaapi_device: "/dev/dri/renderD128".into(),
            vaapi_driver: String::new(),
            source_video_range_type: None,
            enable_tonemapping: false,
            enable_vpp_tonemapping: false,
            tonemapping_algorithm: "hable".into(),
            tonemapping_desat: 0.0,
            tonemapping_peak: 0.0,
            allow_hevc_encoding: false,
            allow_av1_encoding: false,
            h264_crf: 23,
            h265_crf: 28,
            normalize_audio_loudness: false,
        }
    }

    // ── HLS tests ────────────────────────────────────────────────────────────

    #[test]
    fn hls_basic_copy() {
        let dir = PathBuf::from("/tmp/test_session");
        let args = build_hls_args(&default_hls(dir.clone()));

        assert_eq!(arg_after(&args, "-c:v"), Some("copy"));
        assert_eq!(arg_after(&args, "-c:a"), Some("aac"));
        assert_eq!(arg_after(&args, "-f"), Some("hls"));
        // Default TS segments — no fmp4 flag
        assert!(!args_contains(&args, "-hls_segment_type"));
        // Playlist and segment paths
        assert!(
            args.iter()
                .any(|a| a.ends_with("main.m3u8"))
        );
        assert!(
            args.iter()
                .any(|a| a.contains("segment_") && a.ends_with(".ts"))
        );
    }

    #[test]
    fn hls_hevc_copy_uses_fmp4() {
        let dir = PathBuf::from("/tmp/test_hevc");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "copy".into(),
            source_video_codec: Some("hevc".into()),
            ..default_hls(dir)
        });

        assert_eq!(arg_after(&args, "-hls_segment_type"), Some("fmp4"));
        assert_eq!(
            arg_after(&args, "-hls_fmp4_init_filename"),
            Some("init.mp4")
        );
        assert_eq!(arg_after(&args, "-tag:v"), Some("hvc1"));
        // No DoVi range type → dovi_rpu must NOT be injected (would crash on non-HEVC/AV1)
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-bsf:v" && w[1].contains("dovi_rpu"))
        );
        // Segments use .m4s extension
        assert!(
            args.iter()
                .any(|a| a.contains("segment_") && a.ends_with(".m4s"))
        );
    }

    #[test]
    fn hls_hevc_dovi_copy_strips_rpu() {
        let dir = PathBuf::from("/tmp/test_hevc_dovi");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "copy".into(),
            source_video_codec: Some("hevc".into()),
            source_video_range_type: Some(VideoRangeType::DoviWithHdr10),
            ..default_hls(dir)
        });
        assert_eq!(arg_after(&args, "-tag:v"), Some("hvc1"));
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-bsf:v" && w[1].contains("dovi_rpu")),
            "dovi_rpu bsf must be present for DoVi HEVC copy"
        );
    }

    #[test]
    fn hls_aac_copy_adds_adtstoasc() {
        let dir = PathBuf::from("/tmp/test_aac_copy");
        let args = build_hls_args(&TranscodeParams {
            audio_codec: "copy".into(),
            source_audio_codec: Some("aac".into()),
            ..default_hls(dir)
        });
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-bsf:a" && w[1] == "aac_adtstoasc"),
            "aac_adtstoasc bsf must be present when copying AAC audio"
        );
    }

    #[test]
    fn hls_aac_transcode_no_adtstoasc() {
        let dir = PathBuf::from("/tmp/test_aac_transcode");
        let args = build_hls_args(&TranscodeParams {
            audio_codec: "aac".into(),
            source_audio_codec: Some("aac".into()),
            ..default_hls(dir)
        });
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-bsf:a"),
            "aac_adtstoasc must NOT be added when re-encoding audio"
        );
    }

    #[test]
    fn hls_non_aac_copy_no_adtstoasc() {
        let dir = PathBuf::from("/tmp/test_ac3_copy");
        let args = build_hls_args(&TranscodeParams {
            audio_codec: "copy".into(),
            source_audio_codec: Some("ac3".into()),
            ..default_hls(dir)
        });
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-bsf:a"),
            "aac_adtstoasc must NOT be added for non-AAC audio"
        );
    }

    #[test]
    fn hls_hevc_copy_hvc1_tag_alias() {
        // "hvc1" codec string should also trigger fMP4 path
        let dir = PathBuf::from("/tmp/test_hvc1");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "copy".into(),
            source_video_codec: Some("hvc1".into()),
            ..default_hls(dir)
        });
        assert_eq!(arg_after(&args, "-hls_segment_type"), Some("fmp4"));
        assert_eq!(arg_after(&args, "-tag:v"), Some("hvc1"));
    }

    #[test]
    fn hls_libx264_transcode_flags() {
        let dir = PathBuf::from("/tmp/test_x264");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "libx264".into(),
            ..default_hls(dir)
        });

        assert_eq!(arg_after(&args, "-c:v"), Some("libx264"));
        assert_eq!(arg_after(&args, "-crf"), Some("23"));
        assert_eq!(arg_after(&args, "-preset"), Some("ultrafast"));
        assert_eq!(arg_after(&args, "-profile:v"), Some("high"));
        assert_eq!(arg_after(&args, "-tune"), Some("zerolatency"));
        assert_eq!(arg_after(&args, "-pix_fmt"), Some("yuv420p"));
    }

    #[test]
    fn hls_libx264_custom_preset_and_bitrate() {
        let dir = PathBuf::from("/tmp/test_x264_br");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "libx264".into(),
            encoding_preset: Some(EncodingPreset::Veryfast),
            video_bitrate: Some(4_000_000),
            ..default_hls(dir)
        });

        assert_eq!(arg_after(&args, "-preset"), Some("veryfast"));
        assert_eq!(arg_after(&args, "-maxrate"), Some("4000000"));
        assert_eq!(arg_after(&args, "-bufsize"), Some("8000000"));
    }

    #[test]
    fn hls_seek_offset_placed_before_input() {
        let dir = PathBuf::from("/tmp/test_seek");
        let ticks: i64 = 30i64
            .to_ticks(TickUnit::Seconds)
            .unwrap();
        let args = build_hls_args(&TranscodeParams {
            start_time_ticks: Some(ticks),
            ..default_hls(dir)
        });

        let ss_pos = args
            .iter()
            .position(|a| a == "-ss")
            .expect("-ss missing");
        let i_pos = args
            .iter()
            .position(|a| a == "-i")
            .expect("-i missing");
        assert!(ss_pos < i_pos, "-ss must come before -i");
        assert_eq!(args[ss_pos + 1], "30.000000");

        // Source time and HLS sequence time are intentionally independent.
        // The player maps this fresh local-zero stream back onto item time.
        assert_eq!(arg_after(&args, "-start_number"), Some("0"));
    }

    #[test]
    fn hls_no_seek_start_number_zero() {
        let dir = PathBuf::from("/tmp/test_noseek");
        let args = build_hls_args(&default_hls(dir));
        assert_eq!(arg_after(&args, "-start_number"), Some("0"));
        assert!(!args_contains(&args, "-ss"));
    }

    #[test]
    fn input_failure_signatures_match_source_reads_only() {
        // The observed flaky-source failure.
        assert!(super::is_input_source_failure(
            "[in#0/matroska,webm @ 0x55de1330dbc0] Error during demuxing: Input/output error"
        ));
        assert!(super::is_input_source_failure(
            "Error opening input file https://example.invalid/x.mkv: Server returned 5xx"
        ));
        assert!(super::is_input_source_failure(
            "[tls @ 0x1] Connection reset by peer"
        ));
        // Output-side I/O (full disk) must not restart into a loop.
        assert!(!super::is_input_source_failure(
            "[out#0/mpegts @ 0x1] Error writing frame: Input/output error"
        ));
        // Encoder faults take the HW fallback path instead.
        assert!(!super::is_input_source_failure(
            "[h264_nvenc @ 0x1] OpenEncodeSessionEx failed: out of memory"
        ));
        // Client kills carry no input signature.
        assert!(!super::is_input_source_failure(""));
    }

    #[test]
    fn trusted_probe_uses_reduced_ffmpeg_analysis_budget() {
        let dir = PathBuf::from("/tmp/test_trusted_probe");
        let trusted = build_hls_args(&TranscodeParams {
            trusted_probe_data: true,
            ..default_hls(dir.clone())
        });
        let conservative = build_hls_args(&default_hls(dir));

        assert_eq!(arg_after(&trusted, "-analyzeduration"), Some("250000"));
        assert_eq!(arg_after(&trusted, "-probesize"), Some("262144"));
        assert_eq!(
            arg_after(&conservative, "-analyzeduration"),
            Some("1000000")
        );
        assert_eq!(arg_after(&conservative, "-probesize"), Some("1000000"));
    }

    #[test]
    fn hls_recovery_can_preserve_requested_local_sequence() {
        let dir = PathBuf::from("/tmp/test_recovery_sequence");
        let ticks: i64 = 132i64
            .to_ticks(TickUnit::Seconds)
            .unwrap();
        let args = build_hls_args(&TranscodeParams {
            start_time_ticks: Some(ticks),
            hls_start_number: 22,
            ..default_hls(dir)
        });

        assert_eq!(arg_after(&args, "-ss"), Some("132.000000"));
        assert_eq!(arg_after(&args, "-start_number"), Some("22"));
    }

    #[test]
    fn resumed_vod_playlist_advertises_start_offset_and_full_seek_map() {
        let session = TranscodeSession {
            id: "play-session".into(),
            item_id: Uuid::nil(),
            media_source_id: Uuid::nil(),
            output_dir: PathBuf::from("/tmp/test_playlist"),
            input_url: "http://example.invalid/video".into(),
            state: TranscodeState::Running,
            state_tx: Arc::new(tokio::sync::watch::channel(TranscodeState::Running).0),
            output_tx: Arc::new(tokio::sync::watch::channel(0).0),
            created_at: std::time::Instant::now(),
            video_codec: "copy".into(),
            audio_codec: "aac".into(),
            audio_stream_index: None,
            subtitle_stream_index: None,
            burn_subtitle: false,
            segment_length: 6,
            transcode_reasons: TranscodeReasons::default(),
            kill_tx: None,
            wait_done: Arc::new(tokio::sync::Notify::new()),
            last_segment_index: Arc::new(AtomicU32::new(0)),
            start_time_secs: 30,
            requested_start_ticks: 300_000_000,
            actual_start_ticks: None,
            stopped: false,
            playback_offset_secs: Arc::new(AtomicU32::new(0)),
            prewarm: Arc::new(AtomicBool::new(false)),
            runtime_ticks: 120i64
                .to_ticks(TickUnit::Seconds)
                .unwrap(),
            runtime_is_probed: true,
            is_live: false,
            source_video_codec: Some("h264".into()),
            source_audio_codec: Some("aac".into()),
            source_video_profile: None,
            source_video_level: None,
            source_video_range_type: None,
            source_video_width: None,
            source_video_height: None,
            source_frame_rate: None,
        };

        let playlist = generate_variant_playlist(&session, "");

        assert!(playlist.contains("#EXT-X-START:TIME-OFFSET=30.000000,PRECISE=YES"));
        assert!(playlist.contains("segment_00000.ts?PlaySessionId=play-session&runtimeTicks=0&actualSegmentLengthTicks=60000000"));
        assert!(playlist.contains("segment_00005.ts?PlaySessionId=play-session&runtimeTicks=300000000&actualSegmentLengthTicks=60000000"));
    }

    #[test]
    fn hls_scale_filter_both_dimensions() {
        let dir = PathBuf::from("/tmp/test_scale");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "libx264".into(),
            max_width: Some(1920),
            max_height: Some(1080),
            ..default_hls(dir)
        });

        let vf = arg_after(&args, "-vf").expect("-vf missing");
        assert!(vf.contains("min(1920,iw)"), "vf: {vf}");
        assert!(vf.contains("min(1080,ih)"), "vf: {vf}");
        assert!(
            vf.contains("force_original_aspect_ratio=decrease"),
            "vf: {vf}"
        );
    }

    #[test]
    fn hls_scale_filter_width_only() {
        let dir = PathBuf::from("/tmp/test_scale_w");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "libx264".into(),
            max_width: Some(1280),
            ..default_hls(dir)
        });
        let vf = arg_after(&args, "-vf").expect("-vf missing");
        assert!(vf.contains("min(1280,iw)"), "vf: {vf}");
        assert!(vf.contains(":-2"), "vf: {vf}");
    }

    #[test]
    fn hls_audio_bitrate_and_channels() {
        let dir = PathBuf::from("/tmp/test_audio");
        let args = build_hls_args(&TranscodeParams {
            audio_bitrate: Some(192_000),
            audio_channels: Some(2),
            ..default_hls(dir)
        });

        assert_eq!(arg_after(&args, "-b:a"), Some("192000"));
        assert_eq!(arg_after(&args, "-ac"), Some("2"));
    }

    #[test]
    fn hls_audio_copy_no_bitrate_flags() {
        let dir = PathBuf::from("/tmp/test_acopy");
        let args = build_hls_args(&TranscodeParams {
            audio_codec: "copy".into(),
            ..default_hls(dir)
        });
        assert_eq!(arg_after(&args, "-c:a"), Some("copy"));
        assert!(
            !args_contains(&args, "-b:a"),
            "must not set -b:a when copying audio"
        );
        assert!(
            !args_contains(&args, "-ac"),
            "must not set -ac when copying audio"
        );
    }

    #[test]
    fn hls_subtitle_burn_forces_reencode_and_filter_complex() {
        let dir = PathBuf::from("/tmp/test_sub");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "copy".into(),
            burn_subtitle: true,
            subtitle_stream_index: Some(2),
            ..default_hls(dir)
        });

        // copy → libx264 forced by subtitle burn
        assert_eq!(arg_after(&args, "-c:v"), Some("libx264"));
        // filter_complex with overlay
        let fc = arg_after(&args, "-filter_complex").expect("-filter_complex missing");
        assert!(fc.contains("overlay"), "filter_complex: {fc}");
        assert!(
            fc.contains("[0:2]"),
            "filter_complex should reference sub stream: {fc}"
        );
        // map [v] output label
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-map" && w[1] == "[v]")
        );
    }

    #[test]
    fn hls_subtitle_burn_with_scale_in_filter_complex() {
        let dir = PathBuf::from("/tmp/test_sub_scale");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "libx264".into(),
            burn_subtitle: true,
            subtitle_stream_index: Some(3),
            max_width: Some(1280),
            max_height: Some(720),
            ..default_hls(dir)
        });

        let fc = arg_after(&args, "-filter_complex").expect("-filter_complex missing");
        assert!(
            fc.contains("scale=1280:720:fast_bilinear"),
            "sub scale: {fc}"
        );
        assert!(fc.contains("min(1280,iw)"), "video scale: {fc}");
        assert!(fc.contains("overlay"), "overlay: {fc}");
    }

    #[test]
    fn hls_nvenc_hardware_accel() {
        let dir = PathBuf::from("/tmp/test_nvenc");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "libx264".into(),
            hardware_acceleration_type: HardwareAccelerationType::Nvenc,
            source_frame_rate: Some(23.976_025),
            ..default_hls(dir)
        });

        assert_eq!(arg_after(&args, "-hwaccel"), Some("cuda"));
        assert_eq!(arg_after(&args, "-hwaccel_output_format"), Some("cuda"));
        assert!(
            arg_after(&args, "-vf")
                .is_some_and(|filter| filter == "scale_cuda=format=yuv420p"),
            "NVDEC frames must remain on the GPU through format conversion"
        );
        assert_eq!(arg_after(&args, "-c:v"), Some("h264_nvenc"));
        // CUDA filters own the 8-bit surface conversion. A separate output
        // pixel-format request inserts an incompatible software auto_scale.
        assert_eq!(
            arg_after(&args, "-pix_fmt"),
            None,
            "NVENC must consume the CUDA filter's hardware surface directly"
        );
        assert_eq!(arg_after(&args, "-profile:v"), Some("high"));
        assert_eq!(arg_after(&args, "-preset"), Some("p1"));
        assert_eq!(arg_after(&args, "-g"), Some("144"));
        assert_eq!(arg_after(&args, "-keyint_min"), Some("144"));
        assert_eq!(arg_after(&args, "-forced-idr"), Some("1"));
        assert_eq!(
            arg_after(&args, "-force_key_frames"),
            Some("expr:gte(t,n_forced*6)")
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == "-copyts"),
            "transcoded HLS must start on a fresh output timeline"
        );
    }

    #[test]
    fn hls_nvenc_uses_safe_gop_when_remote_frame_rate_is_missing_or_invalid() {
        for frame_rate in [None, Some(0.0), Some(f32::NAN), Some(241.0)] {
            let args = build_hls_args(&TranscodeParams {
                video_codec: "libx264".into(),
                hardware_acceleration_type: HardwareAccelerationType::Nvenc,
                source_frame_rate: frame_rate,
                ..default_hls(PathBuf::from("/tmp/test_nvenc_fallback_gop"))
            });

            assert_eq!(arg_after(&args, "-g"), Some("180"));
            assert_eq!(arg_after(&args, "-keyint_min"), Some("180"));
            assert_eq!(arg_after(&args, "-forced-idr"), Some("1"));
        }
    }

    #[test]
    fn hls_non_nvenc_paths_do_not_receive_nvenc_only_gop_flags() {
        for video_codec in ["copy", "libx264"] {
            let args = build_hls_args(&TranscodeParams {
                video_codec: video_codec.into(),
                source_frame_rate: Some(60.0),
                ..default_hls(PathBuf::from("/tmp/test_non_nvenc_gop"))
            });

            assert_eq!(arg_after(&args, "-g"), None);
            assert_eq!(arg_after(&args, "-keyint_min"), None);
            assert_eq!(arg_after(&args, "-forced-idr"), None);
        }
    }

    #[test]
    fn hls_nvenc_hdr_uses_gpu_native_tonemap() {
        let args = build_hls_args(&TranscodeParams {
            video_codec: "libx264".into(),
            hardware_acceleration_type: HardwareAccelerationType::Nvenc,
            source_video_range_type: Some(VideoRangeType::Hdr10),
            ..default_hls(PathBuf::from("/tmp/test_nvenc_hdr"))
        });

        assert_eq!(
            arg_after(&args, "-vf"),
            Some("tonemap_cuda=format=yuv420p,scale_cuda=format=yuv420p")
        );
    }

    #[test]
    fn hls_resumed_transcode_uses_accurate_seek() {
        let dir = PathBuf::from("/tmp/test_resumed_transcode_seek");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "libx264".into(),
            start_time_ticks: Some(10_340_000_000),
            ..default_hls(dir)
        });

        assert!(!args.iter().any(|arg| arg == "-noaccurate_seek"));
        assert!(!args.iter().any(|arg| arg == "-copyts"));
        assert_eq!(
            arg_after(&args, "-force_key_frames"),
            Some("expr:gte(t,n_forced*6)")
        );
    }

    #[test]
    fn hls_transcode_forces_segment_keyframes_but_stream_copy_does_not() {
        let encoded = build_hls_args(&TranscodeParams {
            video_codec: "libx264".into(),
            segment_length: 4,
            ..default_hls(PathBuf::from("/tmp/test_encoded_keyframes"))
        });
        let copied = build_hls_args(&TranscodeParams {
            video_codec: "copy".into(),
            segment_length: 4,
            ..default_hls(PathBuf::from("/tmp/test_copied_keyframes"))
        });

        assert_eq!(
            arg_after(&encoded, "-force_key_frames"),
            Some("expr:gte(t,n_forced*4)")
        );
        assert_eq!(arg_after(&copied, "-force_key_frames"), None);
    }

    #[test]
    fn hls_stream_copy_retains_source_timestamps() {
        let dir = PathBuf::from("/tmp/test_stream_copy_timestamps");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "copy".into(),
            ..default_hls(dir)
        });

        assert!(
            args.iter().any(|arg| arg == "-copyts"),
            "stream-copy HLS must retain source timestamps"
        );
    }

    #[test]
    fn hls_resumed_stream_copy_starts_a_fresh_output_timeline() {
        let args = build_hls_args(&TranscodeParams {
            video_codec: "copy".into(),
            start_time_ticks: Some(10_340_000_000),
            ..default_hls(PathBuf::from("/tmp/test_resumed_copy_timestamps"))
        });

        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "-ss" && pair[1] == "1034.000000")
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == "-copyts"),
            "resumed stream-copy HLS must reset its output timeline"
        );
    }

    #[test]
    fn hls_vaapi_hardware_accel() {
        let dir = PathBuf::from("/tmp/test_vaapi");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "libx264".into(),
            hardware_acceleration_type: HardwareAccelerationType::Vaapi,
            vaapi_device: "/dev/dri/renderD128".into(),
            vaapi_driver: "iHD".into(),
            ..default_hls(dir)
        });

        // init_hw_device with driver= instead of legacy -vaapi_device
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-init_hw_device"
                    && w[1].contains("vaapi=va:")
                    && w[1].contains("/dev/dri/renderD128")
                    && w[1].contains("driver=iHD")),
            "expected init_hw_device vaapi with iHD driver, got: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-filter_hw_device" && w[1] == "va"),
            "expected -filter_hw_device va"
        );
        // legacy -vaapi_device must NOT appear
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-vaapi_device")
        );
        // Encoder remapped
        assert_eq!(arg_after(&args, "-c:v"), Some("h264_vaapi"));
    }

    #[test]
    fn hls_codec_list_defaults_to_h264() {
        let dir = PathBuf::from("/tmp/test_codec_list");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "av1,hevc,vp9,h264".into(),
            ..default_hls(dir)
        });
        assert_eq!(arg_after(&args, "-c:v"), Some("libx264"));
    }

    #[test]
    fn hls_vaapi_no_driver_omits_driver_option() {
        let dir = PathBuf::from("/tmp/test_vaapi_amd");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "libx264".into(),
            hardware_acceleration_type: HardwareAccelerationType::Vaapi,
            vaapi_device: "/dev/dri/renderD128".into(),
            vaapi_driver: String::new(), // AMD / unknown: no driver=
            ..default_hls(dir)
        });

        let init_dev = args
            .windows(2)
            .find(|w| w[0] == "-init_hw_device")
            .expect("init_hw_device missing");
        assert!(
            !init_dev[1].contains("driver="),
            "unexpected driver= in {}",
            init_dev[1]
        );
        assert!(init_dev[1].contains("vaapi=va:"));
    }

    #[test]
    fn hls_qsv_hardware_accel() {
        let dir = PathBuf::from("/tmp/test_qsv");
        let args = build_hls_args(&TranscodeParams {
            video_codec: "libx264".into(),
            hardware_acceleration_type: HardwareAccelerationType::Qsv,
            vaapi_device: "/dev/dri/renderD128".into(),
            vaapi_driver: "iHD".into(),
            ..default_hls(dir)
        });

        // VAAPI device init with iHD driver
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-init_hw_device"
                    && w[1].starts_with("vaapi=va:")
                    && w[1].contains("driver=iHD")),
            "expected VAAPI init with iHD: {args:?}"
        );
        // QSV device derived from VAAPI
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-init_hw_device" && w[1] == "qsv=qs@va"),
            "expected qsv=qs@va init"
        );
        // filter_hw_device points at QSV
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-filter_hw_device" && w[1] == "qs"),
            "expected -filter_hw_device qs"
        );
        // VAAPI hwaccel (not qsv) for decode
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-hwaccel" && w[1] == "vaapi"),
            "expected -hwaccel vaapi"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-hwaccel_output_format" && w[1] == "vaapi"),
            "expected -hwaccel_output_format vaapi"
        );
        // Encoder remapped to QSV
        assert_eq!(arg_after(&args, "-c:v"), Some("h264_qsv"));
        // scale_vaapi in -vf
        let vf = arg_after(&args, "-vf").expect("-vf missing for QSV");
        assert!(
            vf.contains("scale_vaapi"),
            "expected scale_vaapi in vf: {vf}"
        );
        assert!(
            vf.contains("hwmap=derive_device=qsv"),
            "expected hwmap in vf: {vf}"
        );
        assert!(vf.contains("format=qsv"), "expected format=qsv in vf: {vf}");
    }

    // ── progressive tests ────────────────────────────────────────────────────

    #[test]
    fn progressive_basic_copy() {
        let args = build_progressive_args(&default_progressive());

        assert_eq!(arg_after(&args, "-c:v"), Some("copy"));
        assert_eq!(arg_after(&args, "-c:a"), Some("aac"));
        // Copy into mp4 is promoted to matroska to avoid bsf issues
        assert_eq!(arg_after(&args, "-f"), Some("matroska"));
        assert!(args.last() == Some(&"pipe:1".to_string()));
    }

    #[test]
    fn progressive_mp4_transcode_uses_mp4_format() {
        let args = build_progressive_args(&ProgressiveTranscodeParams {
            video_codec: "libx264".into(),
            container: "mp4".into(),
            ..default_progressive()
        });
        assert_eq!(arg_after(&args, "-f"), Some("mp4"));
        assert!(args_contains(&args, "-movflags"));
    }

    #[test]
    fn progressive_ts_container() {
        let args = build_progressive_args(&ProgressiveTranscodeParams {
            container: "ts".into(),
            ..default_progressive()
        });
        assert_eq!(arg_after(&args, "-f"), Some("mpegts"));
    }

    #[test]
    fn progressive_seek_before_input() {
        let ticks: i64 = 60i64
            .to_ticks(TickUnit::Seconds)
            .unwrap();
        let args = build_progressive_args(&ProgressiveTranscodeParams {
            start_time_ticks: Some(ticks),
            ..default_progressive()
        });
        let ss_pos = args
            .iter()
            .position(|a| a == "-ss")
            .expect("-ss missing");
        let i_pos = args
            .iter()
            .position(|a| a == "-i")
            .expect("-i missing");
        assert!(ss_pos < i_pos);
        assert_eq!(args[ss_pos + 1], "60.000000");
    }

    #[test]
    fn progressive_hevc_copy_adds_hvc1_tag() {
        let args = build_progressive_args(&ProgressiveTranscodeParams {
            source_video_codec: Some("hevc".into()),
            ..default_progressive()
        });
        assert_eq!(arg_after(&args, "-tag:v"), Some("hvc1"));
    }

    #[test]
    fn progressive_nvenc_remaps_encoder() {
        let args = build_progressive_args(&ProgressiveTranscodeParams {
            video_codec: "libx264".into(),
            container: "mp4".into(),
            hardware_acceleration_type: HardwareAccelerationType::Nvenc,
            ..default_progressive()
        });
        assert_eq!(arg_after(&args, "-c:v"), Some("h264_nvenc"));
    }

    #[test]
    fn progressive_subtitle_burn_filter_complex() {
        let args = build_progressive_args(&ProgressiveTranscodeParams {
            video_codec: "copy".into(),
            burn_subtitle: true,
            subtitle_stream_index: Some(1),
            ..default_progressive()
        });
        // copy → libx264 forced
        assert_eq!(arg_after(&args, "-c:v"), Some("libx264"));
        let fc = arg_after(&args, "-filter_complex").expect("-filter_complex missing");
        assert!(fc.contains("overlay"), "fc: {fc}");
    }

    #[test]
    fn progressive_audio_channels_and_bitrate() {
        let args = build_progressive_args(&ProgressiveTranscodeParams {
            audio_codec: "aac".into(),
            audio_bitrate: Some(256_000),
            audio_channels: Some(6),
            ..default_progressive()
        });
        assert_eq!(arg_after(&args, "-b:a"), Some("256000"));
        assert_eq!(arg_after(&args, "-ac"), Some("6"));
    }

    #[test]
    fn progressive_reconnect_flags_present() {
        let args = build_progressive_args(&default_progressive());
        assert!(args_contains(&args, "-reconnect"));
        assert!(args_contains(&args, "-reconnect_at_eof"));
        assert!(args_contains(&args, "-reconnect_streamed"));
    }

    #[test]
    fn hls_reconnect_flags_present() {
        let args = build_hls_args(&default_hls(PathBuf::from("/tmp/hls-test-out")));
        assert!(args_contains(&args, "-reconnect"));
        assert!(args_contains(&args, "-reconnect_at_eof"));
        assert!(args_contains(&args, "-reconnect_streamed"));
        assert!(args_contains(&args, "-rw_timeout"));
    }

    // ── Loudness normalisation tests ─────────────────────────────────────────

    #[test]
    fn hls_loudnorm_added_when_transcoding_audio() {
        let dir = PathBuf::from("/tmp/test_session");
        let args = build_hls_args(&TranscodeParams {
            audio_codec: "aac".into(),
            normalize_audio_loudness: true,
            ..default_hls(dir)
        });
        assert!(args_contains(&args, "loudnorm=I=-14:TP=-1:LRA=11"));
    }

    #[test]
    fn hls_loudnorm_absent_when_disabled() {
        let dir = PathBuf::from("/tmp/test_session");
        let args = build_hls_args(&TranscodeParams {
            audio_codec: "aac".into(),
            normalize_audio_loudness: false,
            ..default_hls(dir)
        });
        assert!(!args_contains(&args, "loudnorm=I=-14:TP=-1:LRA=11"));
    }

    #[test]
    fn hls_loudnorm_absent_when_audio_copy() {
        let dir = PathBuf::from("/tmp/test_session");
        let args = build_hls_args(&TranscodeParams {
            audio_codec: "copy".into(),
            normalize_audio_loudness: true,
            ..default_hls(dir)
        });
        assert!(!args_contains(&args, "loudnorm=I=-14:TP=-1:LRA=11"));
    }

    #[test]
    fn progressive_loudnorm_added_when_transcoding_audio() {
        let args = build_progressive_args(&ProgressiveTranscodeParams {
            audio_codec: "aac".into(),
            normalize_audio_loudness: true,
            ..default_progressive()
        });
        assert!(args_contains(&args, "loudnorm=I=-14:TP=-1:LRA=11"));
    }

    #[test]
    fn progressive_loudnorm_absent_when_disabled() {
        let args = build_progressive_args(&ProgressiveTranscodeParams {
            audio_codec: "aac".into(),
            normalize_audio_loudness: false,
            ..default_progressive()
        });
        assert!(!args_contains(&args, "loudnorm=I=-14:TP=-1:LRA=11"));
    }

    #[test]
    fn progressive_loudnorm_absent_when_audio_copy() {
        let args = build_progressive_args(&ProgressiveTranscodeParams {
            audio_codec: "copy".into(),
            normalize_audio_loudness: true,
            ..default_progressive()
        });
        assert!(!args_contains(&args, "loudnorm=I=-14:TP=-1:LRA=11"));
    }
}
