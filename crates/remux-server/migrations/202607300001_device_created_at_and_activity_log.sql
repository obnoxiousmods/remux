ALTER TABLE devices ADD COLUMN created_at TEXT;

ALTER TABLE activity_log RENAME TO activity_log_legacy_20260730;

CREATE TABLE activity_log (
    id          TEXT    PRIMARY KEY,
    timestamp   TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    user_id     TEXT    NOT NULL,
    user_name   TEXT    NOT NULL DEFAULT '',
    action      TEXT    NOT NULL,
    target_user_id   TEXT,
    target_user_name TEXT,
    device_id   TEXT,
    device_name TEXT,
    details     TEXT
);

INSERT INTO activity_log (id, timestamp, user_id, user_name, action, details)
SELECT CAST(id AS TEXT), date, COALESCE(user_id, ''), '', type,
       COALESCE(overview, short_overview)
FROM activity_log_legacy_20260730;

DROP TABLE activity_log_legacy_20260730;

CREATE INDEX IF NOT EXISTS idx_activity_log_timestamp ON activity_log (timestamp DESC);
