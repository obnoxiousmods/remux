-- Complete the request telemetry dimensions used by the admin explorer.
-- Existing rows remain valid; new rows are enriched from the authenticated
-- device/user record without retaining access tokens, query strings, or IPs.
ALTER TABLE telemetry_request_events ADD COLUMN user_name TEXT;
ALTER TABLE telemetry_request_events ADD COLUMN item_name TEXT;

CREATE INDEX IF NOT EXISTS idx_telemetry_request_client_time
    ON telemetry_request_events(client_name, created_at);
CREATE INDEX IF NOT EXISTS idx_telemetry_request_user_time
    ON telemetry_request_events(user_id, created_at);
CREATE INDEX IF NOT EXISTS idx_telemetry_request_status_time
    ON telemetry_request_events(status, created_at);

CREATE TABLE IF NOT EXISTS telemetry_hourly_rollups (
    bucket_start TEXT NOT NULL,
    route_template TEXT NOT NULL,
    method TEXT NOT NULL,
    device_name TEXT NOT NULL DEFAULT '',
    client_name TEXT NOT NULL DEFAULT '',
    user_name TEXT NOT NULL DEFAULT '',
    item_name TEXT NOT NULL DEFAULT '',
    status_class TEXT NOT NULL,
    sample_reason TEXT NOT NULL,
    request_count INTEGER NOT NULL,
    error_count INTEGER NOT NULL,
    total_latency_ms REAL NOT NULL,
    max_latency_ms REAL NOT NULL,
    latency_lt_100 INTEGER NOT NULL DEFAULT 0,
    latency_lt_500 INTEGER NOT NULL DEFAULT 0,
    latency_lt_1000 INTEGER NOT NULL DEFAULT 0,
    latency_lt_2500 INTEGER NOT NULL DEFAULT 0,
    latency_lt_5000 INTEGER NOT NULL DEFAULT 0,
    latency_lt_10000 INTEGER NOT NULL DEFAULT 0,
    latency_ge_10000 INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (
        bucket_start, route_template, method,
        device_name, client_name, user_name, item_name,
        status_class, sample_reason
    )
);
CREATE INDEX IF NOT EXISTS idx_telemetry_rollup_time
    ON telemetry_hourly_rollups(bucket_start);

-- Seed the explorer from any raw events captured before this migration so the
-- 7/14-day views are useful immediately after upgrade.
INSERT INTO telemetry_hourly_rollups (
    bucket_start, route_template, method, device_name, client_name, user_name,
    item_name, status_class, sample_reason, request_count, error_count,
    total_latency_ms, max_latency_ms, latency_lt_100, latency_lt_500,
    latency_lt_1000, latency_lt_2500, latency_lt_5000, latency_lt_10000,
    latency_ge_10000
)
SELECT
    strftime('%Y-%m-%dT%H:00:00Z', created_at), route_template, method,
    COALESCE(device_name, ''), COALESCE(client_name, ''), COALESCE(user_name, ''),
    COALESCE(item_name, ''), CAST(status / 100 AS INTEGER) || 'xx', sample_reason,
    COUNT(*), SUM(status >= 400), SUM(latency_ms), MAX(latency_ms),
    SUM(latency_ms < 100), SUM(latency_ms >= 100 AND latency_ms < 500),
    SUM(latency_ms >= 500 AND latency_ms < 1000),
    SUM(latency_ms >= 1000 AND latency_ms < 2500),
    SUM(latency_ms >= 2500 AND latency_ms < 5000),
    SUM(latency_ms >= 5000 AND latency_ms < 10000), SUM(latency_ms >= 10000)
FROM telemetry_request_events
GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9;
