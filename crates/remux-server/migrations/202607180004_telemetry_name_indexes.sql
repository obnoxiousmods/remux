-- The telemetry explorer filters on the denormalized *_name columns added by
-- 202607180002_telemetry_dimensions.sql, but the indexes from
-- 202607180001_telemetry.sql only cover the corresponding *_id columns. Filtering
-- by Device, User or Content therefore could not use an index and degraded to a
-- scan of the whole selected time range.
CREATE INDEX IF NOT EXISTS idx_telemetry_request_device_name_time
    ON telemetry_request_events(device_name, created_at);
CREATE INDEX IF NOT EXISTS idx_telemetry_request_user_name_time
    ON telemetry_request_events(user_name, created_at);
CREATE INDEX IF NOT EXISTS idx_telemetry_request_item_name_time
    ON telemetry_request_events(item_name, created_at);
