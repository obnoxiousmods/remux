ALTER TABLE telemetry_recommendation_events ADD COLUMN feed_id TEXT;
ALTER TABLE telemetry_recommendation_events ADD COLUMN algorithm_version INTEGER;
ALTER TABLE telemetry_recommendation_events ADD COLUMN profile_mode TEXT;
ALTER TABLE telemetry_recommendation_events ADD COLUMN personalization TEXT;

CREATE INDEX IF NOT EXISTS idx_telemetry_recommendation_feed
    ON telemetry_recommendation_events(feed_id, created_at);
