use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

use super::{
    AddonCapabilities, AddonKind, AddonMetadata, AddonOption, AddonOptionType,
    AddonPreset, AddonPresetRegistration, MediaKind, ResourceType, StreamAddon,
    stremio::{StremioManifestUrl, parse_manifest_info},
};
use crate::{
    AppContext, db,
    services::stremio as stremio_service,
    stream::{StreamDescriptor, StreamInfo},
};

#[derive(Deserialize)]
struct EclipseSearchResponse {
    tracks: Vec<EclipseTrack>,
}

#[derive(Deserialize)]
struct EclipseTrack {
    id: String,
    title: String,
    artist: String,
    #[serde(default)]
    album: String,
    duration: Option<i64>,
}

#[derive(Deserialize)]
struct EclipseStreamResponse {
    url: String,
    #[serde(default)]
    quality: String,
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<serde_json::Value>,
}

/// Some Eclipse-compatible resolvers occasionally prefix an absolute media URL
/// to the same absolute URL again. Keep the final absolute URL, which is the
/// actual signed CDN resource, instead of persisting an unplayable descriptor.
fn normalize_stream_url(url: &str) -> &str {
    ["/https://", "/http://"]
        .into_iter()
        .filter_map(|marker| url.rfind(marker))
        .max()
        .map(|index| &url[index + 1..])
        .unwrap_or(url)
}

fn eclipse_preset_options(
    default_url: &'static str,
    generate_url: &'static str,
) -> Vec<AddonOption> {
    vec![AddonOption {
        id: "manifest_url".to_string(),
        name: "Manifest URL".to_string(),
        description: Some(format!(
            "Optional. You can generate a new manifest URL at {generate_url}"
        )),
        required: false,
        default: Some(serde_json::Value::String(default_url.to_string())),
        kind: AddonOptionType::Url,
    }]
}

fn eclipse_from_cfg(
    default_url: &'static str,
    cfg: &serde_json::Value,
    config: &crate::Config,
) -> Result<AddonCapabilities> {
    let raw_url = cfg
        .get("manifest_url")
        .and_then(|v| v.as_str())
        .filter(|s| {
            !s.trim()
                .is_empty()
        })
        .unwrap_or(default_url)
        .to_string();
    let manifest_url = StremioManifestUrl::try_new(raw_url)
        .map_err(|e| anyhow!("Invalid manifest_url: {e}"))?;
    let client = super::make_http_client(config);
    let addon = Arc::new(EclipseAddon {
        manifest_url,
        client,
    });
    Ok(AddonCapabilities {
        kind: Some(addon.clone()),
        stream: Some(addon),
        ..Default::default()
    })
}

const MONOCHROME_URL: &str = "https://monochrome1.cyrusna29.workers.dev/u/206f62ce5c9a5c710f2178a16238/manifest.json";
const MONOCHROME_GENERATE_URL: &str = "https://monochrome1.cyrusna29.workers.dev";

pub struct MonochromePreset;

inventory::submit! {
    AddonPresetRegistration(|| Box::new(MonochromePreset))
}

impl AddonPreset for MonochromePreset {
    fn id(&self) -> &'static str {
        "monochrome"
    }

    fn metadata(&self) -> AddonMetadata {
        AddonMetadata {
            id: "monochrome".to_string(),
            display_name: "Monochrome".to_string(),
            description: "Search and stream music".to_string(),
            icon: None,
            supported_resources: vec![
                AddonMetadata::simple_resource(ResourceType::Stream),
                AddonMetadata::simple_resource(ResourceType::Search),
            ],
            supported_types: vec![
                MediaKind::Track,
                MediaKind::Album,
                MediaKind::Artist,
            ],
            supported_resources_user: vec![ResourceType::Stream, ResourceType::Search],
            supported_types_user: vec![
                MediaKind::Track,
                MediaKind::Album,
                MediaKind::Artist,
            ],
            options: eclipse_preset_options(MONOCHROME_URL, MONOCHROME_GENERATE_URL),
        }
    }

    fn from_cfg(
        &self,
        _id: Uuid,
        cfg: &serde_json::Value,
        config: &crate::Config,
    ) -> Result<AddonCapabilities> {
        eclipse_from_cfg(MONOCHROME_URL, cfg, config)
    }
}

const SPOTIFLAC_URL: &str =
    "https://spotiflac.eclipsemusic.app/5baa7290b334d6e2/manifest.json";
const SPOTIFLAC_GENERATE_URL: &str = "https://spotiflac.eclipsemusic.app";

pub struct SpotiFLACPreset;

inventory::submit! {
    AddonPresetRegistration(|| Box::new(SpotiFLACPreset))
}

impl AddonPreset for SpotiFLACPreset {
    fn id(&self) -> &'static str {
        "eclipse_spotiflac"
    }

    fn metadata(&self) -> AddonMetadata {
        AddonMetadata {
            id: "eclipse_spotiflac".to_string(),
            display_name: "SpotiFLAC".to_string(),
            description: "Search and stream music".to_string(),
            icon: None,
            supported_resources: vec![
                AddonMetadata::simple_resource(ResourceType::Stream),
                AddonMetadata::simple_resource(ResourceType::Search),
            ],
            supported_types: vec![
                MediaKind::Track,
                MediaKind::Album,
                MediaKind::Artist,
            ],
            supported_resources_user: vec![ResourceType::Stream, ResourceType::Search],
            supported_types_user: vec![
                MediaKind::Track,
                MediaKind::Album,
                MediaKind::Artist,
            ],
            options: eclipse_preset_options(SPOTIFLAC_URL, SPOTIFLAC_GENERATE_URL),
        }
    }

    fn from_cfg(
        &self,
        _id: Uuid,
        cfg: &serde_json::Value,
        config: &crate::Config,
    ) -> Result<AddonCapabilities> {
        eclipse_from_cfg(SPOTIFLAC_URL, cfg, config)
    }
}

/// Any operator-supplied Eclipse-compatible resolver uses the same verified
/// playback path; no service is implicitly trusted or enabled by this preset.
pub struct EclipsePreset;
inventory::submit! { AddonPresetRegistration(|| Box::new(EclipsePreset)) }
impl AddonPreset for EclipsePreset {
    fn id(&self) -> &'static str {
        "eclipse"
    }
    fn metadata(&self) -> AddonMetadata {
        AddonMetadata {
            id: "eclipse".into(), display_name: "Eclipse-compatible music source".into(),
            description: "Resolve music from a configured Eclipse manifest; complete audio is verified before playback.".into(),
            icon: None,
            supported_resources: vec![AddonMetadata::simple_resource(ResourceType::Stream)],
            supported_types: vec![MediaKind::Track],
            supported_resources_user: vec![ResourceType::Stream],
            supported_types_user: vec![MediaKind::Track],
            options: vec![AddonOption { id: "manifest_url".into(), name: "Manifest URL".into(),
                description: Some("An accessible Eclipse-compatible music manifest.".into()), required: true,
                default: None, kind: AddonOptionType::Url }],
        }
    }
    fn from_cfg(
        &self,
        _id: Uuid,
        cfg: &serde_json::Value,
        config: &crate::Config,
    ) -> Result<AddonCapabilities> {
        eclipse_from_cfg("", cfg, config)
    }
}

pub struct EclipseAddon {
    manifest_url: StremioManifestUrl,
    client: reqwest::Client,
}

impl EclipseAddon {
    fn service(&self) -> Result<stremio_service::StremioService> {
        stremio_service::StremioService::from_url(&self.manifest_url)
    }

    fn base_url(&self) -> &str {
        self.manifest_url
            .as_ref()
    }
}

#[async_trait]
impl AddonKind for EclipseAddon {
    fn id(&self) -> &'static str {
        "eclipse"
    }

    async fn available_info(
        &self,
    ) -> Result<
        Option<(
            Vec<remux_sdks::stremio::ResourceRef>,
            Vec<remux_sdks::stremio::MediaType>,
        )>,
    > {
        let svc = self.service()?;
        let manifest = svc
            .get_manifest()
            .await?;
        Ok(Some(parse_manifest_info(&manifest)))
    }
}

#[async_trait]
impl StreamAddon for EclipseAddon {
    fn supports(&self, media: &db::Media) -> bool {
        matches!(
            media.kind,
            db::MediaKind::Track | db::MediaKind::Album | db::MediaKind::Artist
        )
    }

    async fn get_streams(
        &self,
        media: &db::Media,
        ctx: &AppContext,
        _id_prefixes: Option<&[String]>,
    ) -> Result<Vec<StreamInfo>> {
        eclipse_streams(&self.client, self.base_url(), media, ctx).await
    }
}

/// One gate per resolver origin. Accounts on an origin share its server limit;
/// independent providers do not block each other. Tokens never enter logs.
#[derive(Default)]
struct WorkerGate {
    next: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}
static WORKER_GATES: std::sync::LazyLock<dashmap::DashMap<String, Arc<WorkerGate>>> =
    std::sync::LazyLock::new(Default::default);
static WORKER_CONCURRENCY: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(4));

impl WorkerGate {
    async fn wait(&self) {
        loop {
            let mut next = self
                .next
                .lock()
                .await;
            let now = tokio::time::Instant::now();
            if let Some(when) = *next
                && when > now
            {
                drop(next);
                tokio::time::sleep_until(when).await;
                continue;
            }
            *next = Some(now + std::time::Duration::from_millis(750));
            return;
        }
    }
    async fn defer(&self, delay: std::time::Duration) {
        let until = tokio::time::Instant::now() + delay;
        let mut next = self
            .next
            .lock()
            .await;
        *next = Some(next.map_or(until, |previous| previous.max(until)));
    }
}

fn retry_after(
    value: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<std::time::Duration> {
    if let Ok(seconds) = value
        .trim()
        .parse::<u64>()
    {
        return Some(std::time::Duration::from_secs(seconds.min(86400)));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some(std::time::Duration::from_secs(
        (date.timestamp() - now.timestamp()).clamp(0, 86400) as u64,
    ))
}

/// Whether a failed worker response is worth another attempt.
///
/// Retrying a permanent answer is not merely wasteful, it is the difference
/// between a fast failure and a stalled one: with five attempts and a 1.5 s
/// backoff behind the rate gate, a decommissioned worker consumed the caller's
/// entire resolution budget on every track before reporting the 404 it returned
/// in milliseconds the first time.
fn is_retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

const WORKER_ATTEMPTS: u32 = 2;

async fn worker_get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T> {
    let parsed = reqwest::Url::parse(url)?;
    let origin = parsed
        .origin()
        .ascii_serialization();
    let gate = WORKER_GATES
        .entry(origin.clone())
        .or_default()
        .clone();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    tokio::time::timeout_at(deadline, async {
        for attempt in 0..WORKER_ATTEMPTS {
            let permit = WORKER_CONCURRENCY.acquire().await?;
            gate.wait().await;
            let response = client.get(url).send().await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    drop(permit);
                    if attempt + 1 == WORKER_ATTEMPTS { return Err(error.without_url().into()); }
                    gate.defer(std::time::Duration::from_millis(1500)).await;
                    continue;
                }
            };
            let status = response.status();
            if status.is_success() { return response.json::<T>().await.map_err(|e| e.without_url().into()); }
            let delay = response.headers().get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()).and_then(|v| retry_after(v, chrono::Utc::now()))
                .unwrap_or(std::time::Duration::from_millis(1500));
            drop(response);
            drop(permit);
            if is_retryable(status) {
                gate.defer(delay).await;
                tracing::warn!(%origin, status = status.as_u16(), retry_after_ms = delay.as_millis(), "music resolver cooldown");
            }
            if !is_retryable(status) || attempt + 1 == WORKER_ATTEMPTS {
                return Err(anyhow!("music resolver responded {status}"));
            }
        }
        unreachable!("attempt count is nonzero")
    }).await.map_err(|_| anyhow!("music resolver deadline exceeded"))?
}

fn matches_recording(
    media: &db::Media,
    artist: Option<&str>,
    track: &EclipseTrack,
) -> bool {
    let normalize = super::opendal::normalize_music_identity;
    let title = normalize(&media.title);
    !title.is_empty()
        && normalize(&track.title) == title
        && artist.is_some_and(|artist| {
            !normalize(artist).is_empty()
                && normalize(artist) == normalize(&track.artist)
        })
        && match (media.runtime, track.duration) {
            (Some(expected), Some(actual)) if expected > 0 => {
                (expected - actual).abs() <= 5
            }
            _ => true,
        }
}

async fn eclipse_streams(
    client: &reqwest::Client,
    base_url: &str,
    media: &db::Media,
    ctx: &AppContext,
) -> Result<Vec<StreamInfo>> {
    // Build query: include artist name when available. Prefer the artist row
    // (grandparent for tracks); playlist imports have no artist row, so fall
    // back to the flat artist name stored on the track itself.
    let gp_title = match media.grandparent_id {
        Some(gp_id) => db::Media::get_by_id(&ctx.db, &gp_id)
            .await
            .ok()
            .flatten()
            .map(|m| m.title),
        None => None,
    };
    let query = media.track_search_query_from(gp_title.as_deref());

    let search_url = format!("{}/search?q={}", base_url, urlencoding::encode(&query));
    let resp: EclipseSearchResponse = worker_get_json(client, &search_url).await?;

    if resp
        .tracks
        .is_empty()
    {
        return Ok(vec![]);
    }

    let artist = media
        .artist_name()
        .or(gp_title.as_deref());
    let Some(track) = resp
        .tracks
        .iter()
        .find(|track| matches_recording(media, artist.as_deref(), track))
    else {
        return Ok(vec![]);
    };

    let stream_url = format!("{}/stream/{}", base_url, urlencoding::encode(&track.id));
    let stream_resp: EclipseStreamResponse =
        worker_get_json(client, &stream_url).await?;

    Ok(vec![StreamInfo {
        descriptor: StreamDescriptor::http(normalize_stream_url(&stream_resp.url)),
        name: Some(format!("Eclipse · {}", stream_resp.quality)),
        description: Some(format!("{} · {}", track.artist, track.album)),
        duration: track.duration,
        valid_until: stream_resp
            .expires_at
            .and_then(|value| {
                value
                    .as_i64()
                    .and_then(|n| {
                        chrono::DateTime::from_timestamp(
                            if n > 10_000_000_000 { n / 1000 } else { n },
                            0,
                        )
                    })
                    .or_else(|| {
                        value
                            .as_str()
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .map(|d| d.with_timezone(&chrono::Utc))
                    })
            }),
        ..Default::default()
    }])
}

#[cfg(test)]
mod tests {
    use super::{is_retryable, normalize_stream_url};
    use reqwest::StatusCode;

    #[test]
    fn retry_after_supports_seconds_and_http_dates() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-27T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            super::retry_after("120", now)
                .unwrap()
                .as_secs(),
            120
        );
        assert_eq!(
            super::retry_after("Sun, 27 Sep 2026 00:02:00 GMT", now)
                .unwrap()
                .as_secs(),
            120
        );
        assert_eq!(
            super::retry_after("Sat, 26 Sep 2026 00:00:00 GMT", now)
                .unwrap()
                .as_secs(),
            0
        );
        assert!(super::retry_after("invalid", now).is_none());
    }

    #[tokio::test]
    async fn worker_429_cooldown_is_observed_before_retry() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let url = format!(
            "http://{}/search",
            listener
                .local_addr()
                .unwrap()
        );
        let server = tokio::spawn(async move {
            let mut starts = Vec::new();
            for response in [
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            ] {
                let (mut socket, _) = listener
                    .accept()
                    .await
                    .unwrap();
                let mut buf = [0; 4096];
                socket
                    .read(&mut buf)
                    .await
                    .unwrap();
                starts.push(std::time::Instant::now());
                socket
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
            starts[1].duration_since(starts[0])
        });
        let _: serde_json::Value =
            super::worker_get_json(&reqwest::Client::new(), &url)
                .await
                .unwrap();
        assert!(
            server
                .await
                .unwrap()
                >= std::time::Duration::from_secs(1)
        );
    }

    #[test]
    fn eclipse_optional_metadata_and_exact_recording_identity() {
        let track: super::EclipseTrack =
            serde_json::from_str(r#"{"id":"123","title":"Song","artist":"Artist"}"#)
                .unwrap();
        let mut media = crate::db::Media {
            title: "Song".into(),
            runtime: Some(200),
            ..Default::default()
        };
        assert!(super::matches_recording(&media, Some("Artist"), &track));
        assert!(!super::matches_recording(
            &media,
            Some("Cover Artist"),
            &track
        ));
        media.title = "Other song".into();
        assert!(!super::matches_recording(&media, Some("Artist"), &track));
        let _: super::EclipseStreamResponse =
            serde_json::from_str(r#"{"url":"https://example.org/audio"}"#).unwrap();
    }

    #[test]
    fn permanent_worker_answers_are_not_retried() {
        // A deleted Cloudflare Worker answers 404 in milliseconds. Retrying it
        // five times behind the rate gate is what turned an instant, honest
        // failure into a multi-second stall on every track.
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::GONE,
        ] {
            assert!(!is_retryable(status), "{status} should be permanent");
        }
    }

    #[test]
    fn transient_worker_answers_are_retried() {
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert!(is_retryable(status), "{status} should be retried");
        }
    }

    #[test]
    fn normalizes_duplicated_absolute_media_url() {
        let valid =
            "https://sp-ad-fa.audio.tidal.com/mediatracks/token/0.mp4?token=signed";
        let malformed =
            format!("https://sp-ad-fa.audio.tidal.com/mediatracks/token/{valid}");

        assert_eq!(normalize_stream_url(&malformed), valid);
        assert_eq!(normalize_stream_url(valid), valid);
    }
}
