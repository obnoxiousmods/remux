use anyhow::Result;
use axum_test::TestServer;
use chrono::Utc;
use http::header::HeaderValue;
use remux_sdks::remux::{MediaSourceInfo, MediaStream, MediaStreamType};
use serde_json::json;
use uuid::Uuid;

use crate::{AppContext, Config, db, init_app_with_ctx};

pub const AUTH_HEADER: &str = "MediaBrowser Client=\"Test\", Device=\"Test\", DeviceId=\"test-device\", Version=\"1.0.0\"";

pub fn auth_header_with_token(token: &str) -> String {
    format!(
        "MediaBrowser Client=\"Test\", Device=\"Test\", DeviceId=\"test-device\", Version=\"1.0.0\", Token=\"{}\"",
        token
    )
}

/// RAII guard that shuts down the `AppContext` (releases torrent/DHT sockets)
/// when the test ends. Hold this for the lifetime of the test.
pub struct TestGuard(pub AppContext, Option<tempfile::TempDir>);

impl Drop for TestGuard {
    fn drop(&mut self) {
        let ctx = self
            .0
            .clone();
        // Fire-and-forget shutdown: releases sockets so the next test (or a
        // server restart) can bind the same ports without "address in use" errors.
        tokio::spawn(async move {
            ctx.shutdown()
                .await;
        });
    }
}

/// Creates a test server from the given config, seeds an admin user "test"/"test",
/// and returns the server alongside a [`TestGuard`].
pub async fn new_test_server_with_config(
    mut config: Config,
) -> Result<(TestServer, TestGuard)> {
    // Test servers never need a stable peer-listening port. Using the runtime
    // default (6881..6891) makes parallel integration tests exhaust that range.
    config.torrent_peer_port = None;
    let (app, ctx) = init_app_with_ctx(config).await?;

    let server = TestServer::builder()
        .save_cookies()
        .expect_success_by_default()
        .mock_transport()
        .build(app)?;

    // Seed admin user via startup wizard (no auth required)
    server
        .post("/startup/user")
        .json(&json!({ "Name": "test", "Password": "test" }))
        .await;

    server
        .post("/startup/complete")
        .await;

    Ok((server, TestGuard(ctx, None)))
}

/// Creates a test server with a temporary SQLite DB, seeds an admin user
/// "test"/"test", and returns the server alongside a [`TestGuard`] (which
/// carries the `AppContext` and shuts down background services on drop).
pub async fn new_test_server() -> Result<(TestServer, TestGuard)> {
    let temp_dir = tempfile::tempdir()?;
    let database_path = temp_dir
        .path()
        .join("remux-test.sqlite");
    let (server, mut guard) = new_test_server_with_config(Config {
        database_url: Some(format!("sqlite://{}?mode=rwc", database_path.display())),
        torrent_http_port: None, // OS picks a free ephemeral port
        disable_dht: true,       // no DHT needed in tests; avoids socket conflicts
        torrent_peer_port: None,
        ..Default::default()
    })
    .await?;
    guard.1 = Some(temp_dir);
    Ok((server, guard))
}

/// Spins up a test server and authenticates as the seeded "test" user.
/// Returns `(server, guard, access_token)`.
pub async fn authenticated_server() -> (TestServer, TestGuard, String) {
    let (server, guard) = new_test_server()
        .await
        .unwrap();

    let resp = server
        .post("/users/authenticatebyname")
        .add_header(
            http::header::AUTHORIZATION,
            HeaderValue::from_static(AUTH_HEADER),
        )
        .json(&json!({ "Username": "test", "Pw": "test" }))
        .await;

    let body: serde_json::Value = resp.json();
    let token = body["AccessToken"]
        .as_str()
        .unwrap()
        .to_string();
    (server, guard, token)
}

/// Inserts a test video source with pre-populated probe data (container="mp4",
/// bitrate=8_000_000, 1920×1080 h264). No ffprobe or network needed — the
/// fields are set directly so playbackinfo tests behave identically in CI and
/// locally.
pub async fn insert_test_source(ctx: &AppContext) -> db::Media {
    let now = Utc::now().naive_utc();

    // Build minimal probe_data so playbackinfo can make transcode decisions
    // without needing ffprobe or a live network connection.
    let probe = MediaSourceInfo {
        container: Some("mp4".to_string()),
        bitrate: Some(8_000_000),
        run_time_ticks: Some(100_000_000),
        media_streams: vec![
            MediaStream {
                codec: Some("h264".to_string()),
                type_: Some(MediaStreamType::Video),
                index: 0,
                width: Some(1920),
                height: Some(1080),
                ..Default::default()
            },
            MediaStream {
                codec: Some("aac".to_string()),
                type_: Some(MediaStreamType::Audio),
                index: 1,
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let mut media = db::Media {
        title: "Test Source".to_string(),
        kind: db::MediaKind::Stream,
        stream_info: Some(crate::stream::StreamInfo {
            descriptor: crate::stream::StreamDescriptor::Local(
                "test-fixture.mp4".into(),
            ),
            ..Default::default()
        }),
        probe_data: Some(probe),
        created_at: now,
        updated_at: now,
        ..Default::default()
    };
    media
        .save(&ctx.db)
        .await
        .expect("insert_test_source failed");
    media
}

/// Registers an in-process media-tracker addon: inserts the `addons` row and
/// installs a runtime carrying `tracker`, so `AddonService::media_tracker_for`
/// resolves it. Preset instantiation cannot build a stub, hence the direct
/// runtime injection.
pub async fn register_media_tracker(
    ctx: &AppContext,
    name: &str,
    tracker: std::sync::Arc<dyn crate::addons::media_tracker::MediaTrackerAddon>,
) -> crate::addons::Addon {
    let now = Utc::now().naive_utc();
    let addon = crate::addons::Addon {
        id: Uuid::new_v4(),
        name: name.to_string(),
        preset: remux_sdks::remux::AddonPresetRef {
            kind: "test-media-tracker".to_string(),
            config: Default::default(),
        },
        resources: vec![],
        types: vec![],
        enabled: true,
        priority: 0,
        created_at: now,
        updated_at: now,
        system: false,
        is_default: true,
        http_redirect_stream: false,
        service_filter: vec![],
    };
    addon
        .insert(&ctx.db)
        .await
        .expect("register_media_tracker: addon insert failed");

    ctx.addons
        .push_runtime(crate::addons::AddonRuntime {
            row: addon.clone(),
            caps: crate::addons::AddonCapabilities {
                media_tracker: Some(tracker),
                ..Default::default()
            },
        });
    addon
}

/// Inserts a minimal saved Movie row. Use when a test needs a real `media_id`
/// to reference and does not care about playback fields.
pub async fn seed_movie(ctx: &AppContext) -> db::Media {
    let now = Utc::now().naive_utc();
    // `Media::validate` requires a Movie to carry a canonical external id AND
    // to use the stable UUID derived from it, so the id cannot be random.
    let mut media = db::Media {
        title: "Test Movie".to_string(),
        kind: db::MediaKind::Movie,
        external_ids: db::ExternalIds {
            imdb: db::NonEmptyString::try_new(format!("tt{:08}", rand_suffix())).ok(),
            ..Default::default()
        },
        created_at: now,
        updated_at: now,
        ..Default::default()
    };
    media.id = Uuid::from(&media.media_id_raw());
    media
        .save(&ctx.db)
        .await
        .expect("seed_movie failed");
    media
}

/// Cheap per-call entropy for unique test identifiers.
fn rand_suffix() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
