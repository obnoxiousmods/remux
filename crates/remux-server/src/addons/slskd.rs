//! Soulseek music acquisition through a local [slskd](https://github.com/slskd/slskd)
//! instance.
//!
//! Uses slskd's batch API with unique destinations, then publishes only complete,
//! decode-verified files. Persistent manifests make repeat playback independent
//! of Soulseek availability. Requires a slskd version with the batch API.

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::time::Instant;
use tracing::{info, warn};
use uuid::Uuid;

use super::{
    AddonCapabilities, AddonKind, AddonMetadata, AddonOption, AddonOptionType,
    AddonPreset, AddonPresetRegistration, MediaKind, ResourceType, StreamAddon,
    opendal::normalize_music_identity,
};
use crate::{
    AppContext, db,
    keyed_lock::KeyedLock,
    stream::{StreamDescriptor, StreamInfo},
};

/// One acquisition per recording at a time. Ten clients starting the same album
/// must not enqueue ten copies of track one.
static ACQUIRE_LOCKS: KeyedLock<Uuid> = KeyedLock::new();

/// Ceiling on concurrent Soulseek acquisitions. slskd has its own download slot
/// limit; this keeps a playlist prefetch from monopolising it.
static ACQUIRE_SLOTS: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(4));

// ---------------------------------------------------------------------------
// Preset
// ---------------------------------------------------------------------------

pub struct SlskdPreset;

inventory::submit! {
    AddonPresetRegistration(|| Box::new(SlskdPreset))
}

impl AddonPreset for SlskdPreset {
    fn id(&self) -> &'static str {
        "slskd"
    }

    fn metadata(&self) -> AddonMetadata {
        AddonMetadata {
            id: "slskd".to_string(),
            display_name: "Soulseek (slskd)".to_string(),
            description: "Acquire music from Soulseek through a local slskd instance. Downloads are verified and saved in the Remux data directory for instant repeat playback.".to_string(),
            icon: None,
            supported_resources: vec![AddonMetadata::simple_resource(ResourceType::Stream)],
            supported_types: vec![MediaKind::Track],
            supported_resources_user: vec![ResourceType::Stream],
            supported_types_user: vec![MediaKind::Track],
            options: vec![
                AddonOption {
                    id: "url".to_string(),
                    name: "slskd URL".to_string(),
                    description: Some("Base URL of the slskd web API.".to_string()),
                    required: true,
                    default: Some(serde_json::Value::String(
                        "http://localhost:5030".to_string(),
                    )),
                    kind: AddonOptionType::Url,
                },
                AddonOption {
                    id: "api_key".to_string(),
                    name: "API key".to_string(),
                    description: Some(
                        "An slskd API key with the `readwrite` role.".to_string(),
                    ),
                    required: true,
                    default: None,
                    kind: AddonOptionType::Password,
                },
                AddonOption {
                    id: "download_dir".to_string(),
                    name: "Completed download directory".to_string(),
                    description: Some("slskd's `directories.downloads` path as this server sees it. Requires the batch API; use a shared writable volume.".to_string()),
                    required: true,
                    default: None,
                    kind: AddonOptionType::String,
                },
                AddonOption {
                    id: "formats".to_string(),
                    name: "Preferred formats".to_string(),
                    description: Some(
                        "Comma-separated extensions, best first.".to_string(),
                    ),
                    required: false,
                    default: Some(serde_json::Value::String(
                        "flac,m4a,mp3,ogg,opus".to_string(),
                    )),
                    kind: AddonOptionType::String,
                },
                AddonOption {
                    id: "min_bitrate".to_string(),
                    name: "Minimum bitrate (kbps)".to_string(),
                    description: Some(
                        "Reject lossy files below this bitrate. 0 disables the check."
                            .to_string(),
                    ),
                    required: false,
                    default: Some(serde_json::Value::from(192)),
                    kind: AddonOptionType::Number {
                        min: Some(0),
                        max: Some(3000),
                    },
                },
                AddonOption {
                    id: "acquire_timeout_secs".to_string(),
                    name: "Acquisition timeout (seconds)".to_string(),
                    description: Some("Total budget for search plus transfer. The caller's own stream timeout still applies.".to_string()),
                    required: false,
                    default: Some(serde_json::Value::from(40)),
                    kind: AddonOptionType::Number {
                        min: Some(5),
                        max: Some(300),
                    },
                },
                AddonOption {
                    id: "hedge_delay_ms".to_string(),
                    name: "Peer hedge delay (ms)".to_string(),
                    description: Some("Delay before starting additional peers in isolated batch directories.".to_string()),
                    required: false,
                    default: Some(serde_json::Value::from(150)),
                    kind: AddonOptionType::Number {
                        min: Some(0),
                        max: Some(30000),
                    },
                },
                AddonOption {
                    id: "max_peers".to_string(),
                    name: "Maximum peers per track".to_string(),
                    description: Some(
                        "Maximum concurrent candidates per track."
                            .to_string(),
                    ),
                    required: false,
                    default: Some(serde_json::Value::from(3)),
                    kind: AddonOptionType::Number {
                        min: Some(1),
                        max: Some(8),
                    },
                },
            ],
        }
    }

    fn from_cfg(
        &self,
        addon_id: Uuid,
        cfg: &serde_json::Value,
        config: &crate::Config,
    ) -> Result<AddonCapabilities> {
        let base_url = cfg["url"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("http://localhost:5030")
            .trim_end_matches('/')
            .to_string();
        let api_key = cfg["api_key"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("slskd: 'api_key' is required"))?
            .to_string();
        let download_dir = cfg["download_dir"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("slskd: 'download_dir' is required"))?;
        let formats = cfg["formats"]
            .as_str()
            .filter(|s| {
                !s.trim()
                    .is_empty()
            })
            .unwrap_or("flac,m4a,mp3,ogg,opus")
            .split(',')
            .map(|s| {
                s.trim()
                    .trim_start_matches('.')
                    .to_lowercase()
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        let addon = Arc::new(SlskdAddon {
            addon_id,
            cache_dir: config
                .data_dir
                .join("music/slskd"),
            max_bytes: config.music_cache_entry_bytes,
            client: super::make_http_client(config),
            base_url,
            api_key,
            download_dir: PathBuf::from(download_dir),
            formats,
            min_bitrate: number(cfg, "min_bitrate", 192),
            acquire_timeout: Duration::from_secs(
                number(cfg, "acquire_timeout_secs", 40).clamp(5, 300) as u64,
            ),
            hedge_delay: Duration::from_millis(
                number(cfg, "hedge_delay_ms", 150).clamp(0, 30_000) as u64,
            ),
            max_peers: number(cfg, "max_peers", 3).clamp(1, 8) as usize,
        });
        Ok(AddonCapabilities {
            kind: Some(addon.clone()),
            stream: Some(addon),
            ..Default::default()
        })
    }
}

fn number(cfg: &serde_json::Value, key: &str, default: i64) -> i64 {
    cfg.get(key)
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| {
                    value
                        .as_str()
                        .and_then(|s| {
                            s.trim()
                                .parse()
                                .ok()
                        })
                })
        })
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// slskd web API
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchState {
    #[serde(default)]
    state: String,
    #[serde(default)]
    ended_at: Option<serde_json::Value>,
    #[serde(default)]
    responses: Vec<SearchResponse>,
}

impl SearchState {
    fn is_complete(&self) -> bool {
        self.state
            .starts_with("Completed")
            && self
                .ended_at
                .is_some()
    }
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SearchResponse {
    username: String,
    #[serde(default)]
    has_free_upload_slot: bool,
    #[serde(default)]
    upload_speed: i64,
    #[serde(default)]
    queue_length: i64,
    #[serde(default)]
    files: Vec<SearchFile>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct SearchFile {
    filename: String,
    #[serde(default)]
    size: i64,
    /// Duration in seconds, when the peer reports it.
    #[serde(default)]
    length: Option<i64>,
    #[serde(default)]
    bit_rate: Option<i64>,
    #[serde(default)]
    bit_depth: Option<i64>,
    #[serde(default)]
    sample_rate: Option<i64>,
    #[serde(default)]
    is_locked: bool,
}

#[derive(Deserialize)]
struct Batch {
    #[serde(default)]
    transfers: Vec<Transfer>,
}

// Registered before enqueue so cancellation during an HTTP request also cleans
// up server-side transfers. Only our unique batch is ever touched.
struct BatchGuard {
    addon: SlskdAddon,
    id: Uuid,
    username: String,
}
impl Drop for BatchGuard {
    fn drop(&mut self) {
        let addon = self
            .addon
            .clone();
        let id = self.id;
        let username = self
            .username
            .clone();
        tokio::spawn(async move {
            // Enqueue may still be completing when its client future is dropped.
            for delay in [0, 500, 2000, 5000, 10000, 20000] {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if let Ok(batch) = addon
                    .get::<Batch>(&format!("/transfers/downloads/batches/{id}"))
                    .await
                {
                    for transfer in batch.transfers {
                        let _ = addon
                            .send(
                                reqwest::Method::DELETE,
                                &format!(
                                    "/transfers/downloads/{}/{}?remove=true",
                                    urlencoding::encode(&username),
                                    transfer.id
                                ),
                                None,
                            )
                            .await;
                    }
                }
            }
            let _ = tokio::fs::remove_dir_all(
                addon
                    .download_dir
                    .join(format!("remux-acquire/{id}")),
            )
            .await;
        });
    }
}

struct SearchGuard {
    addon: SlskdAddon,
    id: Uuid,
}
impl Drop for SearchGuard {
    fn drop(&mut self) {
        let addon = self
            .addon
            .clone();
        let id = self.id;
        tokio::spawn(async move {
            // Deleting a live search races slskd's asynchronous response
            // callback, which can still be persisting its final state. Wait
            // for completion before removing the record.
            let path = format!("/searches/{id}?includeResponses=false");
            let discovery_deadline = Instant::now() + Duration::from_secs(5);
            let mut found = false;
            while Instant::now() < discovery_deadline {
                if let Ok(state) = addon
                    .get::<SearchState>(&path)
                    .await
                {
                    found = true;
                    if state.is_complete() {
                        let _ = addon
                            .send(
                                reqwest::Method::DELETE,
                                &format!("/searches/{id}"),
                                None,
                            )
                            .await;
                        return;
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            if !found {
                return;
            }

            let completion_deadline = Instant::now() + Duration::from_secs(90);
            while Instant::now() < completion_deadline {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Ok(state) = addon
                    .get::<SearchState>(&path)
                    .await
                {
                    if state.is_complete() {
                        let _ = addon
                            .send(
                                reqwest::Method::DELETE,
                                &format!("/searches/{id}"),
                                None,
                            )
                            .await;
                        return;
                    }
                }
            }
        });
    }
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Transfer {
    id: String,
    filename: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    bytes_transferred: i64,
}

impl Transfer {
    fn succeeded(&self) -> bool {
        self.state == "Completed, Succeeded"
    }

    /// A transfer that will never deliver bytes: the peer rejected or cancelled
    /// it, or it errored out. Distinguished from "queued" so a peer that is
    /// merely slow is given its hedge window rather than abandoned instantly.
    fn failed(&self) -> bool {
        self.state
            .starts_with("Completed")
            && !self.succeeded()
    }
}

#[derive(Clone)]
pub struct SlskdAddon {
    addon_id: Uuid,
    cache_dir: PathBuf,
    max_bytes: u64,
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    download_dir: PathBuf,
    formats: Vec<String>,
    min_bitrate: i64,
    acquire_timeout: Duration,
    hedge_delay: Duration,
    max_peers: usize,
}

impl SlskdAddon {
    fn url(&self, path: &str) -> String {
        format!("{}/api/v0{path}", self.base_url)
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .client
            .get(self.url(path))
            .header("X-API-Key", &self.api_key)
            .send()
            .await
            .map_err(|e| e.without_url())?;
        let status = response.status();
        anyhow::ensure!(status.is_success(), "slskd responded {status}");
        response
            .json()
            .await
            .map_err(|e| {
                e.without_url()
                    .into()
            })
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<()> {
        let mut request = self
            .client
            .request(method, self.url(path))
            .header("X-API-Key", &self.api_key);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| e.without_url())?;
        let status = response.status();
        anyhow::ensure!(status.is_success(), "slskd responded {status}");
        Ok(())
    }

    /// Collect a small candidate pool, ending early when preferred free-slot
    /// files are available. Reserve the rest of the budget for acquisition.
    async fn search(
        &self,
        query: &str,
        media: &db::Media,
        artist: Option<&str>,
        deadline: Instant,
    ) -> Result<Vec<Candidate>> {
        let search_id = Uuid::new_v4();
        let _cleanup = SearchGuard {
            addon: self.clone(),
            id: search_id,
        };
        self.send(
            reqwest::Method::POST,
            "/searches",
            Some(serde_json::json!({
                "id": search_id.to_string(),
                "searchText": query,
            })),
        )
        .await
        .context("start slskd search")?;

        let path = format!("/searches/{search_id}?includeResponses=true");
        let mut candidates = Vec::new();
        let search_deadline = deadline.min(Instant::now() + Duration::from_secs(3));
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let state: SearchState = self
                .get(&path)
                .await?;
            candidates = self.candidates(&state.responses, media, artist);
            let good_enough = candidates
                .first()
                .is_some_and(|best| best.free_slot && best.format_rank == 0);
            if (good_enough && candidates.len() >= self.max_peers)
                || state.is_complete()
                || Instant::now() >= search_deadline
            {
                break;
            }
        }
        Ok(candidates)
    }

    /// Rank the offered files. Ordering is by format, then by how quickly the
    /// peer is likely to start sending — a free upload slot outweighs raw speed,
    /// to reduce peer queueing latency.
    fn candidates(
        &self,
        responses: &[SearchResponse],
        media: &db::Media,
        artist: Option<&str>,
    ) -> Vec<Candidate> {
        let title = normalize_music_identity(&media.title);
        if title.is_empty() {
            return Vec::new();
        }
        let artist = artist
            .map(normalize_music_identity)
            .filter(|value| !value.is_empty());
        let mut candidates: Vec<Candidate> = responses
            .iter()
            .flat_map(|response| {
                response
                    .files
                    .iter()
                    .filter_map(|file| {
                        self.candidate(response, file, media, &title, artist.as_deref())
                    })
            })
            .collect();
        candidates.sort_by_key(|candidate| {
            (
                candidate.format_rank,
                !candidate.free_slot,
                candidate.queue_length,
                -candidate.quality_rank,
                -candidate.upload_speed,
            )
        });
        candidates
    }

    fn candidate(
        &self,
        response: &SearchResponse,
        file: &SearchFile,
        media: &db::Media,
        title: &str,
        artist: Option<&str>,
    ) -> Option<Candidate> {
        if file.is_locked || file.size <= 0 || file.size as u64 > self.max_bytes {
            return None;
        }
        let extension = extension_of(&file.filename)?;
        let format_rank = self
            .formats
            .iter()
            .position(|format| *format == extension)?;

        // A lossy file below the floor is worse than the local silence it would
        // replace, and it would also be cached forever.
        if is_lossy(&extension)
            && self.min_bitrate > 0
            && file
                .bit_rate
                .is_some_and(|rate| rate < self.min_bitrate)
        {
            return None;
        }

        if !matches_recording(&file.filename, title, artist) {
            return None;
        }

        // The peer's own duration claim is free to check and rules out a remix,
        // a live take, or a whole-album file pretending to be one track. The
        // decode check after download verifies integrity and duration, not identity.
        if let (Some(expected), Some(actual)) = (
            media
                .runtime
                .filter(|value| *value > 0),
            file.length
                .filter(|value| *value > 0),
        ) && (expected - actual).abs() > 5
        {
            return None;
        }

        Some(Candidate {
            username: response
                .username
                .clone(),
            filename: file
                .filename
                .clone(),
            size: file.size,
            format_rank,
            free_slot: response.has_free_upload_slot,
            queue_length: response.queue_length,
            upload_speed: response.upload_speed,
            quality_rank: file
                .bit_depth
                .unwrap_or(0)
                * 1_000_000
                + file
                    .sample_rate
                    .unwrap_or(0),
            extension,
        })
    }

    async fn acquire_one(
        &self,
        candidate: Candidate,
        media: &db::Media,
    ) -> Result<StreamInfo> {
        let id = Uuid::new_v4();
        let _cleanup = BatchGuard {
            addon: self.clone(),
            id,
            username: candidate
                .username
                .clone(),
        };
        let destination = format!("remux-acquire/{id}");
        self.send(
            reqwest::Method::POST,
            "/transfers/downloads/batches",
            Some(serde_json::json!({
                "id": id, "username": candidate.username,
                "files": [{ "filename": candidate.filename, "size": candidate.size }],
                "options": { "destination": destination }
            })),
        )
        .await
        .context("enqueue slskd batch (batch API required)")?;
        let directory = self
            .download_dir
            .join(destination);
        loop {
            let batch: Batch = self
                .get(&format!("/transfers/downloads/batches/{id}"))
                .await?;
            let transfer = batch
                .transfers
                .iter()
                .find(|t| t.filename == candidate.filename)
                .context("slskd did not accept the requested file")?;
            anyhow::ensure!(
                !transfer.failed(),
                "Soulseek transfer failed: {}",
                transfer.state
            );
            if transfer.succeeded() {
                // Completion precedes the atomic move into downloads. Scan only
                // our private batch directory; do not reproduce slskd's filename sanitizer.
                if let Ok(mut entries) = tokio::fs::read_dir(&directory).await {
                    while let Some(entry) = entries
                        .next_entry()
                        .await?
                    {
                        if !entry
                            .file_type()
                            .await?
                            .is_file()
                        {
                            continue;
                        }
                        let path = entry.path();
                        anyhow::ensure!(
                            entry
                                .metadata()
                                .await?
                                .len()
                                == candidate.size as u64,
                            "Soulseek size mismatch"
                        );
                        let probe =
                            crate::services::music_cache::verify_complete_audio(
                                &path, media,
                            )
                            .await?;
                        let info = StreamInfo {
                            descriptor: StreamDescriptor::Local(path),
                            name: Some(format!(
                                "Soulseek · {}",
                                candidate
                                    .extension
                                    .to_uppercase()
                            )),
                            description: Some(format!("via {}", candidate.username)),
                            filename: Some(
                                candidate
                                    .filename
                                    .rsplit(['\\', '/'])
                                    .next()
                                    .unwrap_or(&candidate.filename)
                                    .to_string(),
                            ),
                            duration: probe
                                .run_time_ticks
                                .map(|ticks| ticks / 10_000_000),
                            size: Some(candidate.size),
                            probe_data: Some(probe),
                            ..Default::default()
                        };
                        return self
                            .publish(self.cache_key(media), info)
                            .await;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn acquire(
        &self,
        candidates: Vec<Candidate>,
        media: &db::Media,
    ) -> Result<StreamInfo> {
        // Distinct peer/file requests, bounded in flight. A bad decode falls
        // through to the next peer, instead of poisoning the recording cache.
        let mut seen = std::collections::HashSet::new();
        let candidates = candidates
            .into_iter()
            .filter(|c| {
                seen.insert((
                    c.username
                        .clone(),
                    c.filename
                        .clone(),
                ))
            })
            .collect::<Vec<_>>();
        let mut jobs = stream::iter(
            candidates
                .into_iter()
                .enumerate()
                .map(|(index, candidate)| async move {
                    if index > 0 {
                        tokio::time::sleep(self.hedge_delay).await;
                    }
                    self.acquire_one(candidate, media)
                        .await
                }),
        )
        .buffer_unordered(self.max_peers);
        while let Some(result) = jobs
            .next()
            .await
        {
            match result {
                Ok(info) => return Ok(info),
                Err(error) => warn!(%error, "Soulseek candidate failed"),
            }
        }
        bail!("all Soulseek candidates failed")
    }

    fn cache_key(&self, media: &db::Media) -> Uuid {
        Uuid::new_v5(
            &self.addon_id,
            format!(
                "{}:{}:{:?}:{:?}",
                media.id,
                media.title,
                media.artist_name(),
                media.runtime
            )
            .as_bytes(),
        )
    }

    async fn cached(&self, key: Uuid) -> Option<StreamInfo> {
        let raw = tokio::fs::read(
            self.cache_dir
                .join(format!("{key}.json")),
        )
        .await
        .ok()?;
        let info: StreamInfo = serde_json::from_slice(&raw).ok()?;
        let StreamDescriptor::Local(ref path) = info.descriptor else {
            return None;
        };
        if *path
            != self
                .cache_dir
                .join(format!("{key}.audio"))
        {
            return None;
        }
        let metadata = tokio::fs::metadata(path)
            .await
            .ok()?;
        (metadata.is_file()
            && Some(metadata.len() as i64) == info.size
            && info
                .probe_data
                .is_some())
        .then_some(info)
    }

    async fn publish(&self, key: Uuid, mut info: StreamInfo) -> Result<StreamInfo> {
        let StreamDescriptor::Local(ref source) = info.descriptor else {
            bail!("expected local audio");
        };
        tokio::fs::create_dir_all(&self.cache_dir).await?;
        let path = self
            .cache_dir
            .join(format!("{key}.audio"));
        let temporary = tempfile::NamedTempFile::new_in(&self.cache_dir)?;
        tokio::fs::copy(source, temporary.path()).await?;
        temporary
            .persist(&path)
            .map_err(|e| e.error)?;
        let _ = tokio::fs::remove_file(source).await;
        if let Some(parent) = source.parent() {
            let _ = tokio::fs::remove_dir(parent).await;
        }
        info.descriptor = StreamDescriptor::Local(path);
        let raw = serde_json::to_vec(&info)?;
        let mut metadata = tempfile::NamedTempFile::new_in(&self.cache_dir)?;
        std::io::Write::write_all(&mut metadata, &raw)?;
        metadata
            .persist(
                self.cache_dir
                    .join(format!("{key}.json")),
            )
            .map_err(|e| e.error)?;
        Ok(info)
    }
}

#[derive(Clone)]
struct Candidate {
    username: String,
    filename: String,
    size: i64,
    format_rank: usize,
    free_slot: bool,
    queue_length: i64,
    upload_speed: i64,
    quality_rank: i64,
    extension: String,
}

fn extension_of(filename: &str) -> Option<String> {
    let basename = filename
        .rsplit(['\\', '/'])
        .next()?;
    let (_, extension) = basename.rsplit_once('.')?;
    (!extension.is_empty() && extension.len() <= 5).then(|| extension.to_lowercase())
}

fn is_lossy(extension: &str) -> bool {
    matches!(extension, "mp3" | "ogg" | "opus" | "m4a" | "aac" | "wma")
}

/// Whether a peer's file is plausibly the requested recording.
///
/// The title must appear in the file's own name and the artist somewhere in its
/// path — Soulseek users file tracks under an artist directory far more often
/// than they put the artist in the filename. This is a pre-filter that keeps us
/// from spending a peer handshake on an unrelated file; decode and duration are also checked before playback. This remains a filename
/// heuristic, not an acoustic fingerprint.
fn matches_recording(filename: &str, title: &str, artist: Option<&str>) -> bool {
    let basename = filename
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(filename);
    let basename = normalize_music_identity(
        basename
            .rsplit_once('.')
            .map_or(basename, |(stem, _)| stem),
    );
    if !basename.contains(title) {
        return false;
    }
    match artist {
        Some(artist) => normalize_music_identity(&filename.replace(['\\', '/'], " "))
            .contains(artist),
        None => true,
    }
}

#[async_trait]
impl AddonKind for SlskdAddon {
    fn id(&self) -> &'static str {
        "slskd"
    }
}

#[async_trait]
impl StreamAddon for SlskdAddon {
    fn supports(&self, media: &db::Media) -> bool {
        media.kind == db::MediaKind::Track
    }

    async fn get_streams(
        &self,
        media: &db::Media,
        ctx: &AppContext,
        _id_prefixes: Option<&[String]>,
    ) -> Result<Vec<StreamInfo>> {
        if media.kind != db::MediaKind::Track {
            return Ok(vec![]);
        }
        let key = self.cache_key(media);
        if let Some(info) = self
            .cached(key)
            .await
        {
            return Ok(vec![info]);
        }
        tokio::time::timeout(self.acquire_timeout, async {
            // Lock before taking a slot so duplicate requests cannot exhaust the pool.
            let _lock = ACQUIRE_LOCKS
                .lock(key)
                .await;
            if let Some(info) = self
                .cached(key)
                .await
            {
                return Ok(vec![info]);
            }
            let _slot = ACQUIRE_SLOTS
                .acquire()
                .await?;
            let grandparent = match media.grandparent_id {
                Some(id) => db::Media::get_by_id(&ctx.db, &id)
                    .await?
                    .map(|row| row.title),
                None => None,
            };
            let artist = media
                .artist_name()
                .map(str::to_string)
                .or_else(|| grandparent.clone());
            let query = media.track_search_query_from(grandparent.as_deref());
            let candidates = self
                .search(
                    &query,
                    media,
                    artist.as_deref(),
                    Instant::now() + self.acquire_timeout,
                )
                .await?;
            if candidates.is_empty() {
                return Ok(vec![]);
            }
            let info = self
                .acquire(candidates, media)
                .await?;
            info!(item_id = %media.id, "Soulseek track verified and cached");
            Ok(vec![info])
        })
        .await
        .context("Soulseek acquisition timed out")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_is_complete_only_after_slskd_persists_ended_at() {
        let mut state = SearchState {
            state: "Completed".into(),
            ended_at: None,
            responses: vec![],
        };
        assert!(!state.is_complete());

        state.ended_at = Some(serde_json::json!("2026-09-29T18:50:39Z"));
        assert!(state.is_complete());
    }

    fn addon() -> SlskdAddon {
        SlskdAddon {
            addon_id: Uuid::nil(),
            cache_dir: PathBuf::from("/cache"),
            max_bytes: 512 * 1024 * 1024,
            client: reqwest::Client::new(),
            base_url: "http://localhost:5030".into(),
            api_key: "key".into(),
            download_dir: PathBuf::from("/downloads"),
            formats: vec!["flac".into(), "mp3".into()],
            min_bitrate: 192,
            acquire_timeout: Duration::from_secs(40),
            hedge_delay: Duration::from_millis(2500),
            max_peers: 3,
        }
    }

    fn track() -> db::Media {
        db::Media {
            title: "Weird Fishes".into(),
            runtime: Some(321),
            ..Default::default()
        }
    }

    fn response(
        username: &str,
        free: bool,
        queue: i64,
        speed: i64,
        files: Vec<SearchFile>,
    ) -> SearchResponse {
        SearchResponse {
            username: username.into(),
            has_free_upload_slot: free,
            upload_speed: speed,
            queue_length: queue,
            files,
        }
    }

    fn file(filename: &str, size: i64, length: Option<i64>) -> SearchFile {
        SearchFile {
            filename: filename.into(),
            size,
            length,
            bit_rate: None,
            bit_depth: Some(16),
            sample_rate: Some(44100),
            is_locked: false,
        }
    }

    #[test]
    fn recording_match_requires_title_in_filename_and_artist_in_path() {
        assert!(matches_recording(
            "Music\\Radiohead\\In Rainbows\\04 - Weird Fishes.flac",
            "weird fishes",
            Some("radiohead"),
        ));
        // Right song, wrong artist: a cover must not satisfy the request.
        assert!(!matches_recording(
            "Music\\Lounge Covers\\04 - Weird Fishes.flac",
            "weird fishes",
            Some("radiohead"),
        ));
        // Artist present, different song.
        assert!(!matches_recording(
            "Music\\Radiohead\\In Rainbows\\05 - All I Need.flac",
            "weird fishes",
            Some("radiohead"),
        ));
        // Artist in the directory, not the filename — the common Soulseek case.
        assert!(matches_recording(
            "Radiohead\\01 Weird Fishes.flac",
            "weird fishes",
            Some("radiohead"),
        ));
    }

    #[test]
    fn typographic_variants_still_match() {
        // Soulseek filenames use whatever apostrophe the ripper emitted.
        assert!(matches_recording(
            "Adele\\21\\03 Don\u{2019}t You Remember.flac",
            &normalize_music_identity("Don't You Remember"),
            Some("adele"),
        ));
    }

    #[test]
    fn ranking_prefers_format_then_a_free_slot_over_raw_speed() {
        let addon = addon();
        let media = track();
        let ranked = addon.candidates(
            &[
                response(
                    "fast-but-queued",
                    false,
                    12,
                    9_000_000,
                    vec![file("Radiohead\\Weird Fishes.flac", 40_000_000, Some(321))],
                ),
                response(
                    "free-slot",
                    true,
                    0,
                    400_000,
                    vec![file("Radiohead\\Weird Fishes.flac", 40_000_000, Some(321))],
                ),
                response(
                    "lossy",
                    true,
                    0,
                    9_000_000,
                    vec![file("Radiohead\\Weird Fishes.mp3", 8_000_000, Some(321))],
                ),
            ],
            &media,
            Some("Radiohead"),
        );
        let order: Vec<&str> = ranked
            .iter()
            .map(|candidate| {
                candidate
                    .username
                    .as_str()
            })
            .collect();
        assert_eq!(order, ["free-slot", "fast-but-queued", "lossy"]);
    }

    #[test]
    fn rejects_locked_unlisted_formats_and_wrong_durations() {
        let addon = addon();
        let media = track();
        let mut locked = file("Radiohead\\Weird Fishes.flac", 40_000_000, Some(321));
        locked.is_locked = true;
        let ranked = addon.candidates(
            &[response(
                "peer",
                true,
                0,
                1,
                vec![
                    locked,
                    // Unlisted format.
                    file("Radiohead\\Weird Fishes.wav", 90_000_000, Some(321)),
                    // A whole-album file wearing the track's name.
                    file("Radiohead\\Weird Fishes.flac", 400_000_000, Some(2702)),
                ],
            )],
            &media,
            Some("Radiohead"),
        );
        assert!(ranked.is_empty());
    }

    #[test]
    fn rejects_lossy_below_the_bitrate_floor() {
        let addon = addon();
        let media = track();
        let mut poor = file("Radiohead\\Weird Fishes.mp3", 3_000_000, Some(321));
        poor.bit_rate = Some(128);
        let mut good = file("Radiohead\\Weird Fishes.mp3", 8_000_000, Some(321));
        good.bit_rate = Some(320);
        let ranked = addon.candidates(
            &[response("peer", true, 0, 1, vec![poor, good])],
            &media,
            Some("Radiohead"),
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].size, 8_000_000);
    }

    #[test]
    fn a_peer_reported_duration_is_optional() {
        let addon = addon();
        let media = track();
        let ranked = addon.candidates(
            &[response(
                "peer",
                true,
                0,
                1,
                vec![file("Radiohead\\Weird Fishes.flac", 40_000_000, None)],
            )],
            &media,
            Some("Radiohead"),
        );
        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn transfer_states_separate_queued_from_dead() {
        let queued = Transfer {
            id: "1".into(),
            filename: "a".into(),
            state: "Queued, Remotely".into(),
            bytes_transferred: 0,
        };
        assert!(!queued.succeeded() && !queued.failed());
        let progressing = Transfer {
            state: "InProgress".into(),
            ..queued.clone()
        };
        assert!(!progressing.succeeded() && !progressing.failed());
        let done = Transfer {
            state: "Completed, Succeeded".into(),
            ..queued.clone()
        };
        assert!(done.succeeded() && !done.failed());
        for state in [
            "Completed, Errored",
            "Completed, Cancelled",
            "Completed, Rejected",
            "Completed, TimedOut",
        ] {
            let dead = Transfer {
                state: state.into(),
                ..queued.clone()
            };
            assert!(dead.failed(), "{state} should be terminal");
        }
    }

    #[test]
    fn extensions_are_parsed_from_windows_style_paths() {
        assert_eq!(extension_of("a\\b\\c.FLAC").as_deref(), Some("flac"));
        assert_eq!(extension_of("a\\b\\noextension"), None);
        assert_eq!(extension_of("a\\b\\c.toolongextension"), None);
    }
    #[derive(Clone)]
    struct FakeSlskd {
        root: PathBuf,
        audio: Arc<Vec<u8>>,
        batches: Arc<
            tokio::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>,
        >,
        enqueues: Arc<std::sync::atomic::AtomicUsize>,
        cancels: Arc<std::sync::atomic::AtomicUsize>,
    }

    async fn fake_slskd(
        root: PathBuf,
    ) -> (SlskdAddon, FakeSlskd, tokio::task::JoinHandle<()>) {
        use axum::{
            Json, Router,
            extract::{Path, State},
            routing::{delete, get, post},
        };
        let audio_path = root.join("fixture.flac");
        let status = tokio::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-y",
            ])
            .arg(&audio_path)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let state = FakeSlskd {
            root: root.clone(),
            audio: Arc::new(
                tokio::fs::read(audio_path)
                    .await
                    .unwrap(),
            ),
            batches: Default::default(),
            enqueues: Default::default(),
            cancels: Default::default(),
        };
        async fn enqueue(
            State(s): State<FakeSlskd>,
            Json(body): Json<serde_json::Value>,
        ) -> Json<serde_json::Value> {
            s.enqueues
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let id = body["id"]
                .as_str()
                .unwrap();
            let directory = s
                .root
                .join(
                    body["options"]["destination"]
                        .as_str()
                        .unwrap(),
                );
            tokio::fs::create_dir_all(&directory)
                .await
                .unwrap();
            let stalled = body["username"] == "stalled";
            let bad = body["username"] == "bad";
            if !stalled {
                let bytes = if bad {
                    vec![
                        0u8;
                        s.audio
                            .len()
                    ]
                } else {
                    s.audio
                        .as_ref()
                        .clone()
                };
                tokio::fs::write(directory.join("sanitized.flac"), bytes)
                    .await
                    .unwrap();
            }
            s.batches.lock().await.insert(id.to_owned(), serde_json::json!({"transfers": [{
                "id": id, "filename": body["files"][0]["filename"],
                "state": if stalled { "Queued, Remotely" } else { "Completed, Succeeded" }
            }]}));
            Json(serde_json::json!({}))
        }
        async fn batch(
            State(s): State<FakeSlskd>,
            Path(id): Path<String>,
        ) -> Json<serde_json::Value> {
            Json(
                s.batches
                    .lock()
                    .await
                    .get(&id)
                    .cloned()
                    .unwrap(),
            )
        }
        async fn cancel(State(s): State<FakeSlskd>) -> axum::http::StatusCode {
            s.cancels
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            axum::http::StatusCode::NO_CONTENT
        }
        let router = Router::new()
            .route("/api/v0/transfers/downloads/batches", post(enqueue))
            .route("/api/v0/transfers/downloads/batches/{id}", get(batch))
            .route(
                "/api/v0/transfers/downloads/{username}/{id}",
                delete(cancel),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let mut addon = addon();
        addon.base_url = format!(
            "http://{}",
            listener
                .local_addr()
                .unwrap()
        );
        addon.download_dir = root.clone();
        addon.cache_dir = root.join("persistent");
        addon.hedge_delay = Duration::from_millis(10);
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .unwrap()
        });
        (addon, state, server)
    }

    fn fixture_candidate(peer: &str, size: usize) -> Candidate {
        Candidate {
            username: peer.into(),
            filename: "artist\\album\\test.flac".into(),
            size: size as i64,
            format_rank: 0,
            free_slot: true,
            queue_length: 0,
            upload_speed: 1000,
            quality_rank: 0,
            extension: "flac".into(),
        }
    }

    #[tokio::test]
    async fn batch_acquisition_decodes_and_replays_offline_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let (addon, state, server) = fake_slskd(
            root.path()
                .to_owned(),
        )
        .await;
        let media = db::Media {
            id: Uuid::new_v4(),
            title: "test".into(),
            runtime: Some(1),
            ..Default::default()
        };
        let info = addon
            .acquire(
                vec![fixture_candidate(
                    "good",
                    state
                        .audio
                        .len(),
                )],
                &media,
            )
            .await
            .unwrap();
        let key = addon.cache_key(&media);
        let StreamDescriptor::Local(ref path) = info.descriptor else {
            panic!()
        };
        assert_eq!(
            tokio::fs::read(path)
                .await
                .unwrap(),
            *state.audio
        );
        server.abort();
        let restarted = addon.clone();
        let start = Instant::now();
        for _ in 0..100 {
            assert!(
                restarted
                    .cached(key)
                    .await
                    .is_some()
            );
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "warm lookup averaged over 20 ms"
        );
        assert_eq!(
            state
                .enqueues
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        tokio::fs::write(path, b"truncated")
            .await
            .unwrap();
        assert!(
            restarted
                .cached(key)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn invalid_audio_falls_through_and_cancelled_race_cleans_up() {
        let root = tempfile::tempdir().unwrap();
        let (addon, state, server) = fake_slskd(
            root.path()
                .to_owned(),
        )
        .await;
        let media = db::Media {
            runtime: Some(1),
            ..Default::default()
        };
        let info = addon
            .acquire(
                vec![
                    fixture_candidate(
                        "bad",
                        state
                            .audio
                            .len(),
                    ),
                    fixture_candidate(
                        "stalled",
                        state
                            .audio
                            .len(),
                    ),
                    fixture_candidate(
                        "good",
                        state
                            .audio
                            .len(),
                    ),
                ],
                &media,
            )
            .await
            .unwrap();
        assert_eq!(
            info.description
                .as_deref(),
            Some("via good")
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            state
                .cancels
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 3
        );
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            addon.acquire(
                vec![fixture_candidate(
                    "stalled",
                    state
                        .audio
                        .len(),
                )],
                &media,
            ),
        )
        .await;
        assert!(result.is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            state
                .cancels
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 4
        );
        server.abort();
    }
}
