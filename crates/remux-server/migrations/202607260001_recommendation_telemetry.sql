-- Observational recommendation telemetry. This table is intentionally isolated
-- from playback telemetry and is never read by recommendation ranking.
CREATE TABLE IF NOT EXISTS telemetry_recommendation_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    session_key TEXT NOT NULL,
    event TEXT NOT NULL,
    category_id TEXT NOT NULL,
    recommendation_type TEXT NOT NULL,
    baseline_item_id TEXT,
    baseline_item_name TEXT,
    media_kind TEXT NOT NULL,
    shelf_position INTEGER NOT NULL,
    item_id TEXT,
    item_position INTEGER,
    shuffle_seed TEXT NOT NULL,
    user_id TEXT NOT NULL,
    device_id TEXT,
    device_name TEXT,
    client_name TEXT,
    client_version TEXT
);

CREATE INDEX IF NOT EXISTS idx_telemetry_recommendation_time
    ON telemetry_recommendation_events(created_at);
CREATE INDEX IF NOT EXISTS idx_telemetry_recommendation_user_event
    ON telemetry_recommendation_events(user_id, event, created_at);
CREATE INDEX IF NOT EXISTS idx_telemetry_recommendation_category
    ON telemetry_recommendation_events(category_id, created_at);
