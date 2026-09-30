//! Complete, decode-verified audio objects. Partial objects are never playable.
use crate::{
    AppContext, db,
    stream::{StreamDescriptor, StreamInfo},
};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
    time::SystemTime,
};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

static FILLS: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(2));
static LOCKS: crate::keyed_lock::KeyedLock<Uuid> = crate::keyed_lock::KeyedLock::new();
static RESERVED: LazyLock<Mutex<HashMap<PathBuf, u64>>> =
    LazyLock::new(Default::default);
struct Reservation(PathBuf);
impl Drop for Reservation {
    fn drop(&mut self) {
        RESERVED
            .lock()
            .unwrap()
            .remove(&self.0);
    }
}
fn reserve(dir: &Path, temp: &Path, bytes: u64, capacity: u64) -> Result<Reservation> {
    let mut reserved = RESERVED
        .lock()
        .unwrap();
    let mut entries = Vec::new();
    let mut used = reserved
        .iter()
        .filter(|(path, _)| path.parent() == Some(dir))
        .map(|(_, bytes)| *bytes)
        .sum::<u64>();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path == temp || reserved.contains_key(&path) {
            continue;
        }
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        used = used.saturating_add(meta.len());
        entries.push((
            meta.modified()
                .unwrap_or(SystemTime::UNIX_EPOCH),
            path,
            meta.len(),
        ));
    }
    entries.sort_by_key(|e| e.0);
    for (_, path, length) in entries {
        if used.saturating_add(bytes) <= capacity {
            break;
        }
        // Open readers retain their inode; an evicted object is never truncated.
        std::fs::remove_file(path)?;
        used = used.saturating_sub(length);
    }
    anyhow::ensure!(
        used.saturating_add(bytes) <= capacity,
        "music cache capacity exhausted"
    );
    reserved.insert(temp.to_owned(), bytes);
    Ok(Reservation(temp.to_owned()))
}

pub(crate) fn touch(ctx: &AppContext, path: &Path) {
    if path.starts_with(
        ctx.config
            .data_dir
            .join("cache/music"),
    ) {
        if let Ok(file) = std::fs::File::open(path) {
            let _ = file
                .set_times(std::fs::FileTimes::new().set_modified(SystemTime::now()));
        }
    }
}

pub(crate) async fn qualify(
    ctx: &AppContext,
    media: &db::Media,
    info: StreamInfo,
    user_id: Option<Uuid>,
) -> Result<StreamInfo> {
    qualify_inner(&ctx.config, media, info, user_id, Some(ctx)).await
}

#[cfg(test)]
async fn qualify_config(
    config: &crate::Config,
    media: &db::Media,
    info: StreamInfo,
    user_id: Option<Uuid>,
) -> Result<StreamInfo> {
    qualify_inner(config, media, info, user_id, None).await
}

async fn qualify_inner(
    config: &crate::Config,
    media: &db::Media,
    mut info: StreamInfo,
    user_id: Option<Uuid>,
    ctx: Option<&AppContext>,
) -> Result<StreamInfo> {
    info.infer_valid_until();
    let (url, request_headers) = match &info.descriptor {
        StreamDescriptor::Http {
            url,
            request_headers,
            ..
        } => (url.clone(), request_headers.clone()),
        StreamDescriptor::Opendal { addon_id, path } => {
            (format!("opendal://{addon_id}/{path}"), Default::default())
        }
        StreamDescriptor::Local(path) => {
            let file = tokio::fs::File::open(path).await?;
            anyhow::ensure!(
                file.metadata()
                    .await?
                    .is_file(),
                "not a regular music file"
            );
            return Ok(info);
        }
        _ => bail!("unsupported unverified music transport"),
    };
    let root = config
        .data_dir
        .join("cache/music");
    let headers: std::collections::BTreeMap<_, _> = request_headers
        .iter()
        .collect();
    let key = Uuid::new_v5(
        &media.id,
        format!("{user_id:?}:{url}:{headers:?}").as_bytes(),
    );
    let _guard = LOCKS
        .lock(key)
        .await;
    tokio::fs::create_dir_all(&root).await?;
    let path = root.join(format!("{key}.audio"));
    let metadata_path = root.join(format!("{key}.json"));
    if path.is_file() {
        if let Ok(raw) = tokio::fs::read(&metadata_path).await {
            if let Ok(probe) = serde_json::from_slice(&raw) {
                info.probe_data = Some(probe);
                info.valid_until = None;
                info.size = Some(
                    tokio::fs::metadata(&path)
                        .await?
                        .len() as i64,
                );
                info.descriptor = StreamDescriptor::Local(path.clone());
                info.access_user_id = user_id;
                if let Ok(file) = std::fs::File::open(&path) {
                    let _ = file.set_times(
                        std::fs::FileTimes::new().set_modified(SystemTime::now()),
                    );
                }
                return Ok(info);
            }
        }
    }
    let _permit = FILLS
        .acquire()
        .await?;
    anyhow::ensure!(
        info.valid_until
            .is_none_or(|expires| expires > chrono::Utc::now()),
        "music locator already expired"
    );
    let response = match &info.descriptor {
        StreamDescriptor::Opendal { addon_id, .. } => {
            let ctx = ctx.context("addon context required")?;
            let addon = ctx
                .addons
                .get(*addon_id)
                .context("music storage addon unavailable")?;
            addon
                .stream
                .as_ref()
                .context("storage addon has no stream capability")?
                .serve_stream(&info.descriptor, &http::HeaderMap::new())
                .await
        }
        _ => {
            crate::stream::HttpSource {
                url,
                request_headers,
                response_headers: Default::default(),
            }
            .serve_object(&http::HeaderMap::new())
            .await
        }
    }
    .map_err(|_| anyhow::anyhow!("music source request failed"))?;
    anyhow::ensure!(
        response.status() == http::StatusCode::OK,
        "music source returned {}",
        response.status()
    );
    let declared = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| {
            v.to_str()
                .ok()
        })
        .and_then(|v| {
            v.parse::<u64>()
                .ok()
        });
    let limit = config
        .music_cache_entry_bytes
        .min(config.music_cache_bytes);
    anyhow::ensure!(
        limit > 0 && declared.is_none_or(|n| n > 0 && n <= limit),
        "music object exceeds cache limit"
    );
    let temporary = tempfile::Builder::new()
        .prefix("fill-")
        .suffix(".part")
        .tempfile_in(&root)?;
    let _reservation = reserve(
        &root,
        temporary.path(),
        declared
            .unwrap_or(limit)
            .saturating_add(16384),
        config.music_cache_bytes,
    )?;
    let mut file = tokio::fs::File::from_std(temporary.reopen()?);
    let mut body = response
        .into_body()
        .into_data_stream();
    let mut written = 0u64;
    while let Some(chunk) = body
        .next()
        .await
    {
        let chunk = chunk?;
        written += chunk.len() as u64;
        anyhow::ensure!(
            written <= declared.unwrap_or(limit),
            "music body exceeds declared size"
        );
        file.write_all(&chunk)
            .await?;
    }
    file.flush()
        .await?;
    anyhow::ensure!(
        written > 0 && declared.is_none_or(|n| n == written),
        "truncated music body"
    );
    drop(file);
    let probe = verify_complete_audio(temporary.path(), media).await?;
    let raw = serde_json::to_vec(&probe)?;
    anyhow::ensure!(raw.len() <= 16384, "music metadata exceeds reservation");
    let mut metadata = tempfile::NamedTempFile::new_in(&root)?;
    std::io::Write::write_all(&mut metadata, &raw)?;
    metadata
        .persist(&metadata_path)
        .map_err(|e| e.error)?;
    temporary
        .persist(&path)
        .map_err(|e| e.error)?;
    info.size = Some(written as i64);
    info.probe_data = Some(probe);
    info.valid_until = None;
    info.descriptor = StreamDescriptor::Local(path);
    info.access_user_id = user_id;
    tracing::info!(item_id = %media.id, source = ?info.source, bytes = written, "music object complete and decode verified");
    Ok(info)
}

/// Decode the entire object before publishing it for playback.
pub(crate) async fn verify_complete_audio(
    path: &Path,
    media: &db::Media,
) -> Result<crate::api::MediaSourceInfo> {
    let decode = tokio::process::Command::new(
        std::env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".into()),
    )
    .kill_on_drop(true)
    .args(["-nostdin", "-v", "error", "-xerror", "-i"])
    .arg(path)
    .args(["-map", "0:a:0", "-f", "null", "-"])
    .output()
    .await
    .context("decode music object")?;
    anyhow::ensure!(
        decode
            .status
            .success(),
        "music object failed complete audio decode"
    );
    let probe_path = path
        .to_string_lossy()
        .to_string();
    let (probe, _) = tokio::task::spawn_blocking(move || {
        crate::playback::probe::probe_media(&probe_path)
    })
    .await??;
    anyhow::ensure!(
        probe
            .audio_stream()
            .is_some(),
        "music object has no audio"
    );
    if let Some(expected) = media
        .runtime
        .filter(|value| *value > 0)
    {
        let actual = probe
            .run_time_ticks
            .context("music duration could not be verified")?;
        anyhow::ensure!(
            (actual / 10_000_000 - expected).abs() <= 5,
            "music duration does not match recording"
        );
    }
    Ok(probe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::AsyncReadExt;

    // A real HTTP origin, including deliberately truncated response bodies.
    async fn origin(
        bytes: Vec<u8>,
        extra_length: usize,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let url = format!(
            "http://{}/recording",
            listener
                .local_addr()
                .unwrap()
        );
        let count = Arc::new(AtomicUsize::new(0));
        let requests = count.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener
                .accept()
                .await
            {
                let mut request = [0; 4096];
                let _ = socket
                    .read(&mut request)
                    .await;
                requests.fetch_add(1, Ordering::SeqCst);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n",
                    bytes.len() + extra_length
                );
                let _ = socket
                    .write_all(header.as_bytes())
                    .await;
                let _ = socket
                    .write_all(&bytes)
                    .await;
                let _ = socket
                    .shutdown()
                    .await;
            }
        });
        (url, count, task)
    }
    fn wav() -> Vec<u8> {
        let file = tempfile::NamedTempFile::new().unwrap();
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-f",
                "wav",
            ])
            .arg(file.path())
            .output()
            .unwrap();
        assert!(
            output
                .status
                .success()
        );
        std::fs::read(file.path()).unwrap()
    }
    fn config(root: &Path) -> crate::Config {
        crate::Config {
            data_dir: root.to_owned(),
            music_cache_bytes: 4 * 1024 * 1024,
            music_cache_entry_bytes: 1024 * 1024,
            ..Default::default()
        }
    }
    fn track() -> db::Media {
        db::Media {
            id: Uuid::new_v4(),
            kind: db::MediaKind::Track,
            runtime: Some(1),
            ..Default::default()
        }
    }
    fn remote(url: &str) -> StreamInfo {
        StreamInfo {
            descriptor: StreamDescriptor::http(url),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn ten_complete_tracks_decode_and_replay_without_upstream_requests() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let (url, count, server) = origin(wav(), 0).await;
        let owner = Some(Uuid::new_v4());
        let tracks: Vec<_> = (0..10)
            .map(|_| track())
            .collect();
        for media in &tracks {
            let result = qualify_config(&cfg, media, remote(&url), owner)
                .await
                .unwrap();
            assert!(
                result
                    .probe_data
                    .as_ref()
                    .unwrap()
                    .audio_stream()
                    .is_some()
            );
            assert_eq!(result.access_user_id, owner);
            assert!(
                matches!(result.descriptor, StreamDescriptor::Local(ref path) if path.is_file())
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 10);
        for media in &tracks {
            let mut expired_locator = remote(&url);
            expired_locator.valid_until =
                Some(chrono::Utc::now() - chrono::Duration::seconds(1));
            let cached = qualify_config(&cfg, media, expired_locator, owner)
                .await
                .unwrap();
            assert!(
                cached
                    .valid_until
                    .is_none()
            );
            assert!(
                cached
                    .size
                    .is_some_and(|n| n > 0)
            );
        }
        assert_eq!(count.load(Ordering::SeqCst), 10);
        server.abort();
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_fill_but_users_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let media = track();
        let owner = Some(Uuid::new_v4());
        let (url, count, server) = origin(wav(), 0).await;
        let (a, b) = tokio::join!(
            qualify_config(&cfg, &media, remote(&url), owner),
            qualify_config(&cfg, &media, remote(&url), owner)
        );
        assert_eq!(
            serde_json::to_value(
                a.unwrap()
                    .descriptor
            )
            .unwrap(),
            serde_json::to_value(
                b.unwrap()
                    .descriptor
            )
            .unwrap()
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        qualify_config(&cfg, &media, remote(&url), Some(Uuid::new_v4()))
            .await
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn incomplete_invalid_and_wrong_recording_bodies_never_become_playable() {
        for (bytes, extra, duration) in [
            (wav(), 100, 1),
            (b"not audio".to_vec(), 0, 1),
            (wav(), 0, 300),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let cfg = config(dir.path());
            let mut media = track();
            media.runtime = Some(duration);
            let (url, _, server) = origin(bytes, extra).await;
            assert!(
                qualify_config(&cfg, &media, remote(&url), None)
                    .await
                    .is_err()
            );
            assert_eq!(
                std::fs::read_dir(
                    dir.path()
                        .join("cache/music")
                )
                .unwrap()
                .count(),
                0
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn cancelled_fill_removes_partial_object() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let url = format!(
            "http://{}/slow",
            listener
                .local_addr()
                .unwrap()
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener
                .accept()
                .await
                .unwrap();
            let mut buf = [0; 4096];
            socket
                .read(&mut buf)
                .await
                .unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\npartial")
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                qualify_config(&cfg, &track(), remote(&url), None)
            )
            .await
            .is_err()
        );
        assert_eq!(
            std::fs::read_dir(
                dir.path()
                    .join("cache/music")
            )
            .unwrap()
            .count(),
            0
        );
        server.abort();
    }

    #[test]
    fn reservations_bound_concurrent_writes_and_do_not_delete_open_fill() {
        let dir = tempfile::tempdir().unwrap();
        let temp = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        let reservation = reserve(dir.path(), temp.path(), 80, 100).unwrap();
        assert!(
            temp.path()
                .exists()
        );
        let other = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        assert!(reserve(dir.path(), other.path(), 30, 100).is_err());
        drop(reservation);
        assert!(reserve(dir.path(), other.path(), 30, 100).is_ok());
    }
}
