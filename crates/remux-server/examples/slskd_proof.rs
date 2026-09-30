//! Live, isolated end-to-end proof. Config JSON is supplied as a file (never logged).
//! cargo run -p remux-server --example slskd_proof -- /path/to/proof.json
use anyhow::{Context, Result};
use remux_server::{Config, db};
use serde::Deserialize;
use serde_json::json;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Deserialize)]
struct Proof {
    data_dir: PathBuf,
    output: PathBuf,
    addon: serde_json::Value,
    tracks: Vec<Track>,
    #[serde(default)]
    prefetch_wait_secs: u64,
}
#[derive(Deserialize)]
struct Track {
    title: String,
    artist: String,
    duration: i64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();
    let path = std::env::args()
        .nth(1)
        .context("provide proof config JSON path")?;
    let proof: Proof = serde_json::from_slice(&std::fs::read(path)?)?;
    std::fs::create_dir_all(&proof.data_dir)?;
    let marker = proof
        .data_dir
        .join(".slskd-proof");
    anyhow::ensure!(
        marker.exists()
            || std::fs::read_dir(&proof.data_dir)?
                .next()
                .is_none(),
        "proof data directory must be empty or belong to a previous proof run"
    );
    std::fs::write(marker, b"isolated slskd proof database")?;
    let config = Config {
        data_dir: proof
            .data_dir
            .clone(),
        music_fallback_stream_addon_timeout_secs: 60,
        disable_dht: true,
        torrent_http_port: None,
        torrent_peer_port: None,
        ..Default::default()
    }
    .resolve();
    let (router, ctx) = remux_server::init_app_with_ctx(config).await?;
    sqlx::query("UPDATE addons SET enabled = 0")
        .execute(&ctx.db)
        .await?;
    let now = chrono::Utc::now().naive_utc();
    sqlx::query("INSERT INTO addons (id, name, preset, resources, types, enabled, priority, created_at, updated_at, system, is_default, http_redirect_stream, service_filter) VALUES (?1, 'slskd proof', ?2, '[\"stream\"]', '[\"track\"]', 1, 0, ?3, ?3, 0, 1, 0, '[]') ON CONFLICT(id) DO UPDATE SET preset=excluded.preset, enabled=1")
        .bind(Uuid::new_v5(&Uuid::NAMESPACE_URL, b"slskd-proof"))
        .bind(json!({"kind": "slskd", "config": proof.addon}).to_string())
        .bind(now).execute(&ctx.db).await?;
    ctx.addons
        .reload(&ctx.db, &ctx.config)
        .await?;
    let mut user = db::User::new_with_password(
        String::new(),
        "slskd-proof".into(),
        &Uuid::new_v4().to_string(),
        None,
    )?;
    user.is_admin = true;
    user.save_by_username(&ctx.db)
        .await?;
    let key = db::ApiKey::create(&ctx.db, "slskd-proof").await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .unwrap()
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(75))
        .build()?;
    let mut results = vec![];
    let mut queue = vec![];
    for track in &proof.tracks {
        let id = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("slskd-proof:{}:{}", track.artist, track.title).as_bytes(),
        );
        let mut media = db::Media {
            id,
            title: track
                .title
                .clone(),
            kind: db::MediaKind::Track,
            runtime: Some(track.duration),
            ..Default::default()
        };
        media
            .external_ids
            .artist_name = Some(
            track
                .artist
                .clone(),
        );
        media
            .external_ids
            .custom_stremio_id = Some(format!("slskd-proof:{id}"));
        db::Media::upsert(&ctx.db, &[media]).await?;
        queue.push(json!({"Id":id,"PlaylistItemId":id.to_string()}));
    }
    let track_count = proof
        .tracks
        .len();
    for (track_index, track) in proof
        .tracks
        .into_iter()
        .enumerate()
    {
        let id = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("slskd-proof:{}:{}", track.artist, track.title).as_bytes(),
        );
        let mut media = db::Media::get_by_id(&ctx.db, &id)
            .await?
            .context("proof media")?;
        let present_before_request = !media
            .streams(&ctx.db)
            .await?
            .is_empty();
        let mut record = json!({"id": id, "title": track.title, "artist": track.artist, "warm_ms": []});
        record["source_present_before_request"] = json!(present_before_request);
        let outcome: Result<()> = async {
            for iteration in 0..6 {
                let start = Instant::now();
                let playback = client
                    .post(format!("{base}/items/{id}/playbackinfo"))
                    .header(
                        "X-Emby-Token",
                        key.access_token
                            .expose(),
                    )
                    .json(&json!({}))
                    .send()
                    .await?;
                anyhow::ensure!(
                    playback
                        .status()
                        .is_success(),
                    "playbackinfo HTTP {}",
                    playback.status()
                );
                let body: serde_json::Value = playback
                    .json()
                    .await?;
                anyhow::ensure!(
                    body["MediaSources"]
                        .as_array()
                        .is_some_and(|s| !s.is_empty()),
                    "no media sources"
                );
                let response = client
                    .get(format!("{base}/items/{id}/file"))
                    .header(
                        "X-Emby-Token",
                        key.access_token
                            .expose(),
                    )
                    .header("Range", "bytes=0-65535")
                    .send()
                    .await?;
                anyhow::ensure!(
                    response.status() == 206,
                    "range HTTP {}",
                    response.status()
                );
                let bytes = response
                    .bytes()
                    .await?;
                anyhow::ensure!(!bytes.is_empty(), "empty audio");
                let elapsed = start
                    .elapsed()
                    .as_secs_f64()
                    * 1000.0;
                if iteration == 0 {
                    record["initial_ms"] = json!(elapsed);
                } else {
                    record["warm_ms"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!(elapsed));
                }
            }
            let response = client
                .get(format!("{base}/items/{id}/file"))
                .header(
                    "X-Emby-Token",
                    key.access_token
                        .expose(),
                )
                .send()
                .await?;
            anyhow::ensure!(
                response.status() == 200,
                "full file HTTP {}",
                response.status()
            );
            let bytes = response
                .bytes()
                .await?;
            let audio = proof
                .data_dir
                .join(format!("verify-{id}.audio"));
            tokio::fs::write(&audio, &bytes).await?;
            let output = tokio::process::Command::new("ffmpeg")
                .args(["-nostdin", "-v", "error", "-xerror", "-i"])
                .arg(&audio)
                .args(["-map", "0:a:0", "-f", "null", "-"])
                .output()
                .await?;
            anyhow::ensure!(
                output
                    .status
                    .success(),
                "full response decode failed"
            );
            tokio::fs::remove_file(audio).await?;
            record["bytes"] = json!(bytes.len());
            record["full_decode"] = json!(true);
            Ok(())
        }
        .await;
        if let Err(error) = outcome {
            record["error"] = json!(format!("{error:#}"));
        }
        if proof.prefetch_wait_secs > 0
            && track_index + 1 < track_count
            && record
                .get("error")
                .is_none()
        {
            let report = json!({"ItemId": id, "PlaySessionId": "slskd-proof-queue", "PlaylistItemId": id.to_string(), "NowPlayingQueue": queue});
            let response = client
                .post(format!("{base}/sessions/playing"))
                .header(
                    "X-Emby-Token",
                    key.access_token
                        .expose(),
                )
                .json(&report)
                .send()
                .await?;
            anyhow::ensure!(
                response
                    .status()
                    .as_u16()
                    == 204,
                "queue start report HTTP {}",
                response.status()
            );
            record["queue_start_status"] = json!(204);
            let response = client
                .post(format!("{base}/sessions/playing/progress"))
                .header(
                    "X-Emby-Token",
                    key.access_token
                        .expose(),
                )
                .json(&report)
                .send()
                .await?;
            anyhow::ensure!(
                response
                    .status()
                    .as_u16()
                    == 204,
                "queue progress report HTTP {}",
                response.status()
            );
            record["queue_progress_status"] = json!(204);
            tokio::time::sleep(Duration::from_secs(proof.prefetch_wait_secs)).await;
        }
        println!("{}", record);
        results.push(record);
        std::fs::write(&proof.output, serde_json::to_vec_pretty(&results)?)?;
    }
    db::ApiKey::delete(
        &ctx.db,
        key.access_token
            .expose(),
    )
    .await?;
    server.abort();
    ctx.shutdown()
        .await;
    Ok(())
}
