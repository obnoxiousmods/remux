use crate::ResultExt;
use async_trait::async_trait;
use axum::{body::Body, http::HeaderMap, response::Response};
use axum_anyhow::ApiResult as Result;
use futures_util::{StreamExt, TryStreamExt};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::AppState;

/// Typed representation of how a stream is accessed (transport mechanism).
///
/// Each variant maps to a [`StreamSource`] implementation via [`into_source`],
/// or for addon-owned streams, to the addon's [`AddonKind::serve_stream`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum StreamDescriptor {
    Http {
        url: String,
        /// HTTP request headers to send when fetching this stream.
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        request_headers: std::collections::HashMap<String, String>,
        /// HTTP response headers to forward to the client.
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        response_headers: std::collections::HashMap<String, String>,
    },
    Local(PathBuf),
    Rtsp {
        url: String,
    },
    Torrent {
        info_hash: String,
        /// Filename hint for multi-file torrents (matched by name).
        file_hint: Option<String>,
        /// Direct file index within the torrent (takes precedence over file_hint).
        file_idx: Option<usize>,
        /// Tracker announce URLs (populated from the stream's `sources`).
        #[serde(default)]
        trackers: Vec<String>,
    },
    Opendal {
        addon_id: Uuid,
        path: String,
    },
}

impl Default for StreamDescriptor {
    fn default() -> Self {
        Self::Http {
            url: String::new(),
            request_headers: Default::default(),
            response_headers: Default::default(),
        }
    }
}

impl StreamDescriptor {
    pub fn http(url: impl Into<String>) -> Self {
        Self::Http {
            url: url.into(),
            request_headers: Default::default(),
            response_headers: Default::default(),
        }
    }

    pub fn rtsp(url: impl Into<String>) -> Self {
        Self::Rtsp { url: url.into() }
    }

    /// Input URL/path for ffprobe and ffmpeg (server-side tools).
    /// `Local` → raw filesystem path. `Http` → URL as-is.
    /// `Torrent`/`Opendal` → our stream proxy, which resolves them on demand.
    pub fn server_input(&self, media_id: Uuid, port: u16) -> String {
        match self {
            Self::Http { url, .. } | Self::Rtsp { url } => url.clone(),
            Self::Local(path) => path
                .to_string_lossy()
                .into_owned(),
            Self::Torrent { .. } | Self::Opendal { .. } => {
                format!("http://127.0.0.1:{}/stream/{}", port, media_id)
            }
        }
    }

    /// URL to hand to the Jellyfin client for direct play.
    /// `Http` streams play directly. Everything else routes through our stream proxy
    /// (client can't access local FS; Torrent/Opendal need server-side resolution).
    pub fn client_url(&self, media_id: Uuid, server_base: &str) -> String {
        match self {
            Self::Http { url, .. } => url.clone(),
            _ => format!("{}/stream/{}", server_base.trim_end_matches('/'), media_id),
        }
    }

    /// The raw HTTP URL for `Http` variants, or `None` for everything else.
    pub fn as_http_url(&self) -> Option<&str> {
        match self {
            Self::Http { url, .. } => Some(url),
            _ => None,
        }
    }

    /// If this descriptor is owned by an addon (needs its credentials/config to
    /// serve), return the addon's ID so the endpoint can dispatch to
    /// `AddonKind::serve_stream` instead of `into_source`.
    pub fn addon_id(&self) -> Option<Uuid> {
        match self {
            Self::Opendal { addon_id, .. } => Some(*addon_id),
            _ => None,
        }
    }

    /// Instantiate the runtime service for self-contained variants.
    /// Do **not** call this for `Opendal` — those must go through the addon.
    pub fn into_source(self) -> Box<dyn StreamSource> {
        match self {
            Self::Http {
                url,
                request_headers,
                response_headers,
            } => Box::new(HttpSource {
                url,
                request_headers,
                response_headers,
            }),
            Self::Local(path) => Box::new(LocalSource { path }),
            Self::Torrent {
                info_hash,
                file_hint,
                file_idx,
                trackers,
            } => Box::new(TorrentSource {
                info_hash,
                file_hint,
                file_idx,
                trackers,
            }),
            Self::Rtsp { .. } => {
                panic!("Rtsp descriptors must be served through the transcode path")
            }
            Self::Opendal { .. } => {
                panic!("Opendal descriptors must be served through their addon")
            }
        }
    }
}

/// Combined stream descriptor and provider metadata stored in `db::Media.stream_info`.
///
/// Replaces the old split between `db::Media.url` (transport) and
/// `db::Media.provider_info` (Stremio metadata). All addons populate whichever
/// fields they have; the rest are `None` / empty.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StreamInfo {
    pub descriptor: StreamDescriptor,
    /// Filename from the provider (e.g. "Movie.2021.1080p.BluRay.mkv").
    /// Used for resolution matching during probe fallback.
    pub filename: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    /// Addon that produced this stream (stamped by the service layer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub seeders: Option<i64>,
    pub size: Option<i64>,
    pub duration: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtitles: Vec<crate::sdks::stremio::Subtitle>,
    /// Catchup URL template from M3U `catchup-source` attribute.
    /// `{utc}` / `{utcend}` placeholders are substituted at playback time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catchup_source: Option<String>,
    /// Number of days of catchup available (`catchup-days` attribute).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catchup_days: Option<i64>,
    /// Pre-probed codec/bitrate metadata from the addon.
    /// Extracted into `db::Media.probe_data` on conversion; not persisted here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_data: Option<crate::api::MediaSourceInfo>,
}

impl StreamInfo {
    pub fn is_p2p(&self) -> bool {
        matches!(self.descriptor, StreamDescriptor::Torrent { .. })
    }

    pub fn resolution_tag(&self) -> Option<String> {
        let src = self
            .filename
            .as_deref()
            .or(self
                .name
                .as_deref())?;
        crate::db::min_screen_size(&hunch::hunch(src)).map(|s| s.to_owned())
    }
}

/// A runtime service that can serve stream bytes as an HTTP response.
///
/// Implemented by self-contained variants (`Http`, `Local`, `Torrent`).
/// Addon-owned variants (`Opendal`) are served through `AddonKind::serve_stream`.
#[async_trait]
pub trait StreamSource: Send + Sync {
    async fn serve(&self, state: &AppState, headers: &HeaderMap) -> Result<Response>;
}

pub struct HttpSource {
    pub url: String,
    pub request_headers: std::collections::HashMap<String, String>,
    pub response_headers: std::collections::HashMap<String, String>,
}

#[derive(Clone)]
struct SegmentedMp4Layout {
    discovered_at: Instant,
    lengths: Arc<[u64]>,
}

/// Tidal's lossless delivery URLs expose one fragmented-MP4 resource per
/// segment (`0.mp4` is only the initialization fragment). Jellyfin clients,
/// however, request `/Items/{id}/File` as one seekable byte resource. Cache the
/// small header-derived virtual layout so AVFoundation's initial 0-1 probe and
/// subsequent range requests do not rediscover every segment.
static SEGMENTED_MP4_LAYOUTS: LazyLock<
    tokio::sync::RwLock<HashMap<String, SegmentedMp4Layout>>,
> = LazyLock::new(|| tokio::sync::RwLock::new(HashMap::new()));

const SEGMENTED_LAYOUT_TTL: Duration = Duration::from_secs(5 * 60);
const SEGMENTED_LAYOUT_LIMIT: usize = 256;
const MAX_SEGMENT_COUNT: usize = 2048;
const MAX_INIT_SEGMENT_BYTES: u64 = 128 * 1024;
const SEGMENT_DISCOVERY_BATCH_SIZE: usize = 16;

fn tidal_segment_zero_url(raw: &str) -> Option<url::Url> {
    let parsed = url::Url::parse(raw).ok()?;
    let host = parsed.host_str()?;
    if host != "audio.tidal.com" && !host.ends_with(".audio.tidal.com") {
        return None;
    }
    parsed
        .path()
        .ends_with("/0.mp4")
        .then_some(parsed)
}

fn numbered_segment_url(base: &url::Url, index: usize) -> url::Url {
    let mut url = base.clone();
    let prefix = base
        .path()
        .strip_suffix("0.mp4")
        .expect("numbered_segment_url requires a segment-zero URL");
    url.set_path(&format!("{prefix}{index}.mp4"));
    url
}

fn upstream_resource_length(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(http::header::CONTENT_RANGE)
        .and_then(|value| {
            value
                .to_str()
                .ok()
        })
        .and_then(|value| value.rsplit_once('/'))
        .and_then(|(_, total)| {
            total
                .parse::<u64>()
                .ok()
        })
        .or_else(|| response.content_length())
        .filter(|length| *length > 0)
}

/// Convert one virtual byte range into concrete per-segment ranges.
fn segmented_ranges(lengths: &[u64], start: u64, end: u64) -> Vec<(usize, u64, u64)> {
    let mut ranges = Vec::new();
    let mut offset = 0_u64;
    for (index, length) in lengths
        .iter()
        .copied()
        .enumerate()
    {
        let segment_start = offset;
        let segment_end = offset + length.saturating_sub(1);
        offset = offset.saturating_add(length);
        if length == 0 || end < segment_start {
            break;
        }
        if start > segment_end {
            continue;
        }
        ranges.push((
            index,
            start.saturating_sub(segment_start),
            end.min(segment_end) - segment_start,
        ));
    }
    ranges
}

pub struct LocalSource {
    pub path: PathBuf,
}

/// Public trackers used as fallback when a torrent stream provides none.
/// Sourced from https://github.com/ngosang/trackerslist (trackers_best).
const DEFAULT_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.qu.ax:6969/announce",
    "udp://wepzone.net:6969/announce",
    "udp://tracker.srv00.com:6969/announce",
];

pub struct TorrentSource {
    pub info_hash: String,
    pub file_hint: Option<String>,
    pub file_idx: Option<usize>,
    pub trackers: Vec<String>,
}

impl TorrentSource {
    fn to_magnet(&self) -> String {
        let mut m = format!("magnet:?xt=urn:btih:{}", self.info_hash);
        let trackers: &[String] = &self.trackers;
        if trackers.is_empty() {
            for t in DEFAULT_TRACKERS {
                m.push_str(&format!("&tr={}", urlencoding::encode(t)));
            }
        } else {
            for t in trackers {
                m.push_str(&format!("&tr={}", urlencoding::encode(t)));
            }
        }
        if let Some(idx) = self.file_idx {
            m.push_str(&format!("&file_idx={}", idx));
        }
        if let Some(hint) = &self.file_hint {
            m.push_str(&format!("&file={}", urlencoding::encode(hint)));
        }
        m
    }
}

impl HttpSource {
    fn apply_request_headers(
        &self,
        mut request: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        for (name, value) in &self.request_headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request
    }

    async fn segmented_mp4_layout(
        &self,
        client: &reqwest::Client,
        base: &url::Url,
    ) -> Result<Arc<[u64]>> {
        if let Some(cached) = SEGMENTED_MP4_LAYOUTS
            .read()
            .await
            .get(&self.url)
            .filter(|layout| {
                layout
                    .discovered_at
                    .elapsed()
                    < SEGMENTED_LAYOUT_TTL
            })
            .cloned()
        {
            return Ok(cached.lengths);
        }

        let mut lengths = Vec::new();
        'discovery: for batch_start in
            (0..MAX_SEGMENT_COUNT).step_by(SEGMENT_DISCOVERY_BATCH_SIZE)
        {
            let batch_end =
                (batch_start + SEGMENT_DISCOVERY_BATCH_SIZE).min(MAX_SEGMENT_COUNT);
            let mut inspected = futures_util::stream::iter(batch_start..batch_end)
                .map(|index| async move {
                    // A 0-0 GET works with CDNs that reject or omit size
                    // metadata from HEAD. If honored, Content-Range supplies
                    // the full segment size; if ignored, Content-Length does.
                    let response = self
                        .apply_request_headers(
                            client
                                .get(numbered_segment_url(base, index))
                                .header(http::header::RANGE, "bytes=0-0"),
                        )
                        .send()
                        .await;
                    (index, response)
                })
                .buffer_unordered(SEGMENT_DISCOVERY_BATCH_SIZE)
                .collect::<Vec<_>>()
                .await;
            inspected.sort_by_key(|(index, _)| *index);

            for (index, response) in inspected {
                let response = response.context_bad_request(
                    "failed to inspect fragmented audio segment",
                )?;
                if response
                    .status()
                    .is_success()
                {
                    let length =
                        upstream_resource_length(&response).ok_or_else(|| {
                            anyhow::anyhow!(
                                "fragmented audio segment {index} has no length"
                            )
                        })?;
                    lengths.push(length);
                    continue;
                }
                if index == 0 {
                    return Err(anyhow::anyhow!(
                        "fragmented audio initialization request returned {}",
                        response.status()
                    )
                    .into());
                }
                if matches!(
                    response.status(),
                    reqwest::StatusCode::BAD_REQUEST
                        | reqwest::StatusCode::NOT_FOUND
                        | reqwest::StatusCode::RANGE_NOT_SATISFIABLE
                ) {
                    break 'discovery;
                }
                return Err(anyhow::anyhow!(
                    "fragmented audio segment {index} request returned {}",
                    response.status()
                )
                .into());
            }
        }

        if lengths.len() < 2 {
            return Err(
                anyhow::anyhow!("fragmented audio has no media segments").into()
            );
        }
        if lengths.len() == MAX_SEGMENT_COUNT {
            return Err(anyhow::anyhow!(
                "fragmented audio exceeded {MAX_SEGMENT_COUNT} segments"
            )
            .into());
        }
        if lengths[0] > MAX_INIT_SEGMENT_BYTES {
            return Err(anyhow::anyhow!(
                "fragmented audio initialization segment is unexpectedly large"
            )
            .into());
        }

        let lengths: Arc<[u64]> = lengths.into();
        let mut cache = SEGMENTED_MP4_LAYOUTS
            .write()
            .await;
        cache.retain(|_, layout| {
            layout
                .discovered_at
                .elapsed()
                < SEGMENTED_LAYOUT_TTL
        });
        if cache.len() >= SEGMENTED_LAYOUT_LIMIT {
            cache.clear();
        }
        cache.insert(
            self.url
                .clone(),
            SegmentedMp4Layout {
                discovered_at: Instant::now(),
                lengths: lengths.clone(),
            },
        );
        Ok(lengths)
    }

    async fn serve_segmented_mp4(
        &self,
        client: &reqwest::Client,
        headers: &HeaderMap,
        base: url::Url,
    ) -> Result<Response> {
        let lengths = self
            .segmented_mp4_layout(client, &base)
            .await?;
        let total_size = lengths
            .iter()
            .copied()
            .sum::<u64>();
        let requested_range = headers
            .get(http::header::RANGE)
            .and_then(|value| {
                value
                    .to_str()
                    .ok()
            });
        let (start, end, status) = if let Some(range) = requested_range {
            let (start, end) = parse_range(range, total_size)
                .context_bad_request("invalid Range header")?;
            (start, end, http::StatusCode::PARTIAL_CONTENT)
        } else {
            (0, total_size - 1, http::StatusCode::OK)
        };
        let parts = segmented_ranges(&lengths, start, end);
        let body_length = end - start + 1;
        let request_headers = self
            .request_headers
            .clone();
        let stream_client = client.clone();
        let body_stream = async_stream::stream! {
            'segments: for (index, local_start, local_end) in parts {
                let mut request = stream_client
                    .get(numbered_segment_url(&base, index))
                    .header(http::header::RANGE, format!("bytes={local_start}-{local_end}"));
                for (name, value) in &request_headers {
                    request = request.header(name.as_str(), value.as_str());
                }
                let response = match request.send().await {
                    Ok(response) => response,
                    Err(error) => {
                        yield Err::<bytes::Bytes, io::Error>(io::Error::other(error));
                        break;
                    }
                };
                if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
                    && response.status() != reqwest::StatusCode::OK
                {
                    yield Err::<bytes::Bytes, io::Error>(io::Error::other(format!(
                        "fragmented audio segment {index} returned {}",
                        response.status()
                    )));
                    break;
                }
                // Some CDNs ignore Range and answer 200 with the full segment.
                // Preserve the virtual resource's exact byte contract by
                // trimming that response and by never yielding past the
                // requested local end.
                let mut skip = if response.status() == reqwest::StatusCode::OK {
                    local_start
                } else {
                    0
                };
                let mut remaining = local_end - local_start + 1;
                let mut chunks = response.bytes_stream();
                while let Some(chunk) = chunks.next().await {
                    match chunk {
                        Ok(mut chunk) => {
                            if skip >= chunk.len() as u64 {
                                skip -= chunk.len() as u64;
                                continue;
                            }
                            if skip > 0 {
                                chunk = chunk.slice(skip as usize..);
                                skip = 0;
                            }
                            if chunk.len() as u64 > remaining {
                                chunk = chunk.slice(..remaining as usize);
                            }
                            remaining -= chunk.len() as u64;
                            yield Ok::<bytes::Bytes, io::Error>(chunk);
                            if remaining == 0 {
                                break;
                            }
                        }
                        Err(error) => {
                            yield Err::<bytes::Bytes, io::Error>(io::Error::other(error));
                            break 'segments;
                        }
                    }
                }
                if remaining != 0 {
                    yield Err::<bytes::Bytes, io::Error>(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("fragmented audio segment {index} ended {remaining} bytes early"),
                    ));
                    break;
                }
            }
        };

        let mut response = Response::builder()
            .status(status)
            .header(http::header::CONTENT_TYPE, "audio/mp4")
            .header(http::header::CONTENT_LENGTH, body_length)
            .header(http::header::ACCEPT_RANGES, "bytes");
        if status == http::StatusCode::PARTIAL_CONTENT {
            response = response.header(
                http::header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{total_size}"),
            );
        }
        let mut response = response
            .body(Body::from_stream(body_stream))
            .unwrap();
        for (name, value) in &self.response_headers {
            if let (Ok(name), Ok(value)) = (
                http::header::HeaderName::try_from(name.as_str()),
                http::HeaderValue::from_str(value),
            ) {
                response
                    .headers_mut()
                    .insert(name, value);
            }
        }
        Ok(response)
    }
}

#[async_trait]
impl StreamSource for HttpSource {
    async fn serve(&self, _state: &AppState, headers: &HeaderMap) -> Result<Response> {
        let client = reqwest::Client::new();
        if let Some(base) = tidal_segment_zero_url(&self.url) {
            return self
                .serve_segmented_mp4(&client, headers, base)
                .await;
        }

        let mut req = client.get(&self.url);
        if let Some(v) = headers.get(http::header::RANGE) {
            req = req.header(http::header::RANGE, v.clone());
        }
        req = self.apply_request_headers(req);

        let upstream = req
            .send()
            .await
            .context_bad_request("upstream request failed")?;

        let status = upstream.status();
        let upstream_headers = upstream
            .headers()
            .clone();
        let body = Body::from_stream(
            upstream
                .bytes_stream()
                .map_err(io::Error::other),
        );

        let mut resp = Response::builder()
            .status(status)
            .body(body)
            .unwrap();
        let out = resp.headers_mut();
        for (k, v) in &upstream_headers {
            match k.as_str() {
                "content-length" | "content-type" | "accept-ranges"
                | "content-range" | "last-modified" => {
                    out.insert(k, v.clone());
                }
                _ => {}
            }
        }
        if !out.contains_key(http::header::CONTENT_TYPE) {
            out.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/octet-stream"),
            );
        }
        for (name, value) in &self.response_headers {
            if let (Ok(name), Ok(value)) = (
                http::header::HeaderName::try_from(name.as_str()),
                http::HeaderValue::from_str(value),
            ) {
                out.insert(name, value);
            }
        }

        Ok(resp)
    }
}

#[async_trait]
impl StreamSource for LocalSource {
    async fn serve(&self, _state: &AppState, headers: &HeaderMap) -> Result<Response> {
        let file = tokio::fs::File::open(&self.path)
            .await
            .context_not_found("file not found")?;
        let metadata = file
            .metadata()
            .await
            .context_bad_request("failed to read file metadata")?;
        let file_size = metadata.len();
        let content_type = mime_from_path(&self.path);

        let range_str = headers
            .get(http::header::RANGE)
            .and_then(|v| {
                v.to_str()
                    .ok()
            })
            .map(str::to_owned);

        if let Some(range) = range_str {
            let (start, end) = parse_range(&range, file_size)
                .context_bad_request("invalid Range header")?;
            let length = end - start + 1;

            let mut file = file;
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .context_bad_request("seek failed")?;

            let body = Body::from_stream(ReaderStream::new(file.take(length)));

            Ok(Response::builder()
                .status(http::StatusCode::PARTIAL_CONTENT)
                .header(http::header::CONTENT_TYPE, content_type)
                .header(http::header::CONTENT_LENGTH, length)
                .header(http::header::ACCEPT_RANGES, "bytes")
                .header(
                    http::header::CONTENT_RANGE,
                    format!("bytes {}-{}/{}", start, end, file_size),
                )
                .body(body)
                .unwrap())
        } else {
            let body = Body::from_stream(ReaderStream::new(file));

            Ok(Response::builder()
                .status(http::StatusCode::OK)
                .header(http::header::CONTENT_TYPE, content_type)
                .header(http::header::CONTENT_LENGTH, file_size)
                .header(http::header::ACCEPT_RANGES, "bytes")
                .body(body)
                .unwrap())
        }
    }
}

#[async_trait]
impl StreamSource for TorrentSource {
    async fn serve(&self, state: &AppState, headers: &HeaderMap) -> Result<Response> {
        let resolved = state
            .ctx
            .torrent
            .resolve_url(&self.to_magnet())
            .await
            .context_bad_request("failed to resolve torrent")?;

        HttpSource {
            url: resolved,
            request_headers: Default::default(),
            response_headers: Default::default(),
        }
        .serve(state, headers)
        .await
    }
}

pub fn parse_range(range: &str, file_size: u64) -> anyhow::Result<(u64, u64)> {
    if file_size == 0 {
        anyhow::bail!("cannot range an empty resource");
    }
    let bytes = range
        .strip_prefix("bytes=")
        .ok_or_else(|| anyhow::anyhow!("expected bytes= prefix"))?;
    if bytes.contains(',') {
        anyhow::bail!("multiple byte ranges are not supported");
    }
    let (start_str, end_str) = bytes
        .split_once('-')
        .ok_or_else(|| anyhow::anyhow!("malformed range"))?;

    if start_str.is_empty() {
        let suffix: u64 = end_str.parse()?;
        if suffix == 0 {
            anyhow::bail!("suffix length must be greater than zero");
        }
        return Ok((file_size.saturating_sub(suffix), file_size - 1));
    }

    let start: u64 = start_str.parse()?;
    if start >= file_size {
        anyhow::bail!("range starts beyond the resource");
    }
    let end: u64 = if end_str.is_empty() {
        file_size - 1
    } else {
        end_str
            .parse::<u64>()?
            .min(file_size - 1)
    };
    if end < start {
        anyhow::bail!("range end precedes its start");
    }

    Ok((start, end))
}

pub fn mime_from_path(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
    {
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("avi") => "video/x-msvideo",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("ts") => "video/mp2t",
        Some("mp3") => "audio/mpeg",
        Some("flac") => "audio/flac",
        Some("aac") => "audio/aac",
        Some("ogg") => "audio/ogg",
        Some("opus") => "audio/opus",
        Some("m4a") => "audio/mp4",
        Some("wav") => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// Extract the `urn:btih:` info-hash from a magnet URI.
fn extract_btih(magnet: &str) -> Option<String> {
    url::Url::parse(magnet)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == "xt")
        .and_then(|(_, v)| {
            v.strip_prefix("urn:btih:")
                .map(|h| h.to_ascii_lowercase())
        })
}

fn extract_query_param(url: &str, param: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .query_pairs()
        .find(|(k, _)| k == param)
        .map(|(_, v)| v.into_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        numbered_segment_url, parse_range, segmented_ranges, tidal_segment_zero_url,
    };
    use axum::{
        Router,
        body::Body,
        extract::Path,
        http::{HeaderMap, Method, StatusCode, header},
        response::Response,
        routing::any,
    };

    #[test]
    fn recognizes_only_tidal_segment_zero_urls() {
        assert!(
            tidal_segment_zero_url(
                "https://listen.audio.tidal.com/path/0.mp4?token=secret"
            )
            .is_some()
        );
        assert!(
            tidal_segment_zero_url("https://audio.tidal.com/path/0.mp4?token=secret")
                .is_some()
        );
        assert!(tidal_segment_zero_url("https://example.com/path/0.mp4").is_none());
        assert!(
            tidal_segment_zero_url("https://listen.audio.tidal.com/path/10.mp4")
                .is_none()
        );
    }

    #[test]
    fn numbered_segment_preserves_signed_query() {
        let base = tidal_segment_zero_url(
            "https://listen.audio.tidal.com/path/0.mp4?token=a%2Bb&expires=123",
        )
        .unwrap();
        let segment = numbered_segment_url(&base, 42);
        assert_eq!(segment.path(), "/path/42.mp4");
        assert_eq!(segment.query(), base.query());
    }

    #[test]
    fn splits_virtual_ranges_exactly_across_segments() {
        let lengths = [3, 5, 2];
        assert_eq!(segmented_ranges(&lengths, 0, 1), vec![(0, 0, 1)]);
        assert_eq!(segmented_ranges(&lengths, 2, 5), vec![(0, 2, 2), (1, 0, 2)]);
        assert_eq!(segmented_ranges(&lengths, 8, 9), vec![(2, 0, 1)]);
    }

    #[test]
    fn parses_open_ended_and_suffix_ranges() {
        assert_eq!(parse_range("bytes=2-5", 10).unwrap(), (2, 5));
        assert_eq!(parse_range("bytes=7-", 10).unwrap(), (7, 9));
        assert_eq!(parse_range("bytes=-3", 10).unwrap(), (7, 9));
        assert_eq!(parse_range("bytes=-30", 10).unwrap(), (0, 9));
    }

    #[test]
    fn rejects_unsatisfiable_or_ambiguous_ranges() {
        assert!(parse_range("bytes=10-", 10).is_err());
        assert!(parse_range("bytes=7-6", 10).is_err());
        assert!(parse_range("bytes=-0", 10).is_err());
        assert!(parse_range("bytes=0-1,4-5", 10).is_err());
        assert!(parse_range("bytes=0-", 0).is_err());
    }

    async fn mock_segment(method: Method, Path(name): Path<String>) -> Response {
        let index = name
            .strip_suffix(".mp4")
            .and_then(|value| {
                value
                    .parse::<usize>()
                    .ok()
            });
        let bytes: &'static [u8] = match index {
            Some(0) => b"abc",
            Some(1) => b"DEFGH",
            Some(2) => b"ij",
            _ => {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .unwrap();
            }
        };
        let body = if method == Method::HEAD {
            Body::empty()
        } else {
            // Deliberately ignore Range. The virtual stream must still trim
            // each full segment to the exact requested byte interval.
            Body::from(bytes)
        };
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_LENGTH, bytes.len())
            .body(body)
            .unwrap()
    }

    #[tokio::test]
    async fn serves_exact_virtual_bytes_when_upstream_ignores_range() {
        let app = Router::new().route("/{segment}", any(mock_segment));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let address = listener
            .local_addr()
            .unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .unwrap()
        });

        let base =
            url::Url::parse(&format!("http://{address}/0.mp4?signature=kept")).unwrap();
        let source = super::HttpSource {
            url: base.to_string(),
            request_headers: Default::default(),
            response_headers: Default::default(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            header::RANGE,
            "bytes=2-5"
                .parse()
                .unwrap(),
        );
        let response = source
            .serve_segmented_mp4(&reqwest::Client::new(), &headers, base)
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        let body = axum::body::to_bytes(response.into_body(), 16)
            .await
            .unwrap();
        assert_eq!(&body[..], b"cDEF");

        server.abort();
    }
}
