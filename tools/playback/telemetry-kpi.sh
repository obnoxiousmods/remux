#!/usr/bin/env bash
set -euo pipefail

DB_PATH="${1:-${REMUX_DB:-/opt/remux/data/db.sqlite}}"
WINDOW_HOURS="${2:-24}"
STARTUP_WINDOW_SECONDS="${3:-60}"

if ! [[ "$WINDOW_HOURS" =~ ^[0-9]+$ ]] || [ "$WINDOW_HOURS" -lt 1 ]; then
  echo "WINDOW_HOURS must be a positive integer (got: $WINDOW_HOURS)" >&2
  exit 1
fi

if ! [[ "$STARTUP_WINDOW_SECONDS" =~ ^[0-9]+$ ]] || [ "$STARTUP_WINDOW_SECONDS" -lt 1 ]; then
  echo "STARTUP_WINDOW_SECONDS must be a positive integer (got: $STARTUP_WINDOW_SECONDS)" >&2
  exit 1
fi

if [ ! -f "$DB_PATH" ]; then
  echo "Database not found: $DB_PATH" >&2
  exit 1
fi

WINDOW_EXPR="datetime('now','-${WINDOW_HOURS} hours')"
TITLE_TS="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

echo "# Remux playback KPI snapshot"
echo "# at: $TITLE_TS UTC"
echo "# window: last ${WINDOW_HOURS}h"
echo "# startup window: ${STARTUP_WINDOW_SECONDS}s"
echo "# db: $DB_PATH"
echo

sqlite3 -readonly "$DB_PATH" -header -column <<SQL
WITH scope AS (
  SELECT *
  FROM telemetry_request_events
  WHERE created_at >= $WINDOW_EXPR
),
route_agg AS (
  SELECT
    route_template,
    count(*) AS n,
    sum(CASE WHEN status NOT BETWEEN 200 AND 299 THEN 1 ELSE 0 END) AS errs,
    round(sum(CASE WHEN status NOT BETWEEN 200 AND 299 THEN 1 ELSE 0 END) * 100.0 / count(*), 2) AS err_pct,
    round(avg(latency_ms), 2) AS avg_ms,
    round(min(latency_ms), 2) AS min_ms,
    round(max(latency_ms), 2) AS max_ms
  FROM scope
  WHERE route_template IN (
    '/items/{id}/playbackinfo',
    '/videos/{id}/main.m3u8',
    '/videos/{id}/{segment_file}',
    '/videos/{id}/master.m3u8',
    '/videos/{id}/stream'
  )
  GROUP BY route_template
)
SELECT 'request route kpis' AS section, * FROM route_agg
ORDER BY route_template;

WITH scope AS (
  SELECT *
  FROM telemetry_request_events
  WHERE created_at >= $WINDOW_EXPR
),
route_quantile AS (
  SELECT
    route_template,
    latency_ms,
    row_number() OVER (PARTITION BY route_template ORDER BY latency_ms) AS rn,
    count(*) OVER (PARTITION BY route_template) AS n
  FROM scope
  WHERE route_template IN (
    '/items/{id}/playbackinfo',
    '/videos/{id}/main.m3u8',
    '/videos/{id}/{segment_file}'
  )
)
SELECT
  'latency quantiles (ms)' AS section,
  route_template AS route,
  'p50' AS metric,
  (SELECT latency_ms FROM route_quantile WHERE route_template = q.route_template AND rn = CAST((n * 0.50) + 0.5 AS INT)) AS latency_ms
FROM (SELECT DISTINCT route_template, n FROM route_quantile) q
UNION ALL
SELECT
  'latency quantiles (ms)' AS section,
  route_template AS route,
  'p90' AS metric,
  (SELECT latency_ms FROM route_quantile WHERE route_template = q.route_template AND rn = CAST((n * 0.90) + 0.5 AS INT))
FROM (SELECT DISTINCT route_template, n FROM route_quantile) q
UNION ALL
SELECT
  'latency quantiles (ms)' AS section,
  route_template AS route,
  'p95' AS metric,
  (SELECT latency_ms FROM route_quantile WHERE route_template = q.route_template AND rn = CAST((n * 0.95) + 0.5 AS INT))
FROM (SELECT DISTINCT route_template, n FROM route_quantile) q
UNION ALL
SELECT
  'latency quantiles (ms)' AS section,
  route_template AS route,
  'p99' AS metric,
  (SELECT latency_ms FROM route_quantile WHERE route_template = q.route_template AND rn = CAST((n * 0.99) + 0.5 AS INT))
FROM (SELECT DISTINCT route_template, n FROM route_quantile) q
ORDER BY route, metric;

-- PlaybackInfo error concentration by item
WITH scope AS (
  SELECT *
  FROM telemetry_request_events
  WHERE created_at >= $WINDOW_EXPR
    AND route_template = '/items/{id}/playbackinfo'
),
error_items AS (
  SELECT
    item_id,
    item_name,
    count(*) AS requests,
    sum(CASE WHEN status NOT BETWEEN 200 AND 299 THEN 1 ELSE 0 END) AS errors,
    round(sum(CASE WHEN status NOT BETWEEN 200 AND 299 THEN 1 ELSE 0 END) * 100.0 / count(*), 1) AS error_pct,
    min(created_at) AS first_seen,
    max(created_at) AS last_seen
  FROM scope
  GROUP BY item_id
  HAVING errors > 0
)
SELECT 'playbackinfo errors by item' AS section, * FROM error_items
ORDER BY errors DESC, requests DESC
LIMIT 20;

-- Segment 404 concentration by item
WITH scope AS (
  SELECT *
  FROM telemetry_request_events
  WHERE created_at >= $WINDOW_EXPR
    AND route_template = '/videos/{id}/{segment_file}'
),
segment_items AS (
  SELECT
    item_id,
    item_name,
    count(*) AS segment_reqs,
    sum(CASE WHEN status = 404 THEN 1 ELSE 0 END) AS segment_404,
    round(sum(CASE WHEN status = 404 THEN 1 ELSE 0 END) * 100.0 / count(*), 1) AS segment_404_pct,
    min(created_at) AS first_seen,
    max(created_at) AS last_seen
  FROM scope
  GROUP BY item_id
  HAVING segment_404 > 0
)
SELECT 'segment 404 by item' AS section, * FROM segment_items
ORDER BY segment_404 DESC, segment_reqs DESC
LIMIT 20;

-- Segment miss concentration by playback key (high blast-radius bad sources)
WITH scope AS (
  SELECT playback_key, item_name, item_id, status
  FROM telemetry_request_events
  WHERE created_at >= $WINDOW_EXPR
    AND route_template = '/videos/{id}/{segment_file}'
    AND playback_key IS NOT NULL
),
key_stats AS (
  SELECT
    playback_key,
    item_id,
    max(item_name) AS item_name,
    count(*) AS total_segments,
    sum(CASE WHEN status NOT BETWEEN 200 AND 299 THEN 1 ELSE 0 END) AS segment_errors
  FROM scope
  GROUP BY playback_key, item_id
  HAVING total_segments >= 25
)
SELECT
  'segment errors by playback_key' AS section,
  playback_key,
  item_name,
  item_id,
  total_segments,
  segment_errors,
  round(segment_errors * 100.0 / total_segments, 1) AS error_pct
FROM key_stats
WHERE segment_errors > 0
ORDER BY segment_errors DESC, total_segments DESC
LIMIT 20;

-- Startup chain approximation: playbackinfo -> main.m3u8
WITH playbackinfo AS (
  SELECT item_id, user_id, device_id, created_at AS pi_at
  FROM telemetry_request_events
  WHERE created_at >= $WINDOW_EXPR
    AND route_template = '/items/{id}/playbackinfo'
    AND status BETWEEN 200 AND 299
),
main AS (
  SELECT item_id, user_id, device_id, created_at AS main_at, playback_key
  FROM telemetry_request_events
  WHERE created_at >= $WINDOW_EXPR
    AND route_template = '/videos/{id}/main.m3u8'
    AND status BETWEEN 200 AND 299
),
matched AS (
  SELECT
    m.item_id,
    m.user_id,
    m.device_id,
    m.main_at,
    (
      SELECT p.pi_at
      FROM playbackinfo p
      WHERE p.item_id = m.item_id
        AND p.user_id = m.user_id
        AND (p.device_id IS NULL OR m.device_id IS NULL OR p.device_id = m.device_id)
        AND p.pi_at <= m.main_at
      ORDER BY p.pi_at DESC
      LIMIT 1
    ) AS pi_at
  FROM main m
),
deltas AS (
  SELECT
    (julianday(main_at) - julianday(pi_at)) * 86400000.0 AS delay_ms
  FROM matched
  WHERE pi_at IS NOT NULL
    AND (julianday(main_at) - julianday(pi_at)) * 86400000.0 BETWEEN 0 AND 30000
),
ranked AS (
  SELECT delay_ms, row_number() OVER (ORDER BY delay_ms) AS rn, count(*) OVER () AS n
  FROM deltas
)
SELECT
  'pi -> main (<=30s match)' AS section,
  count(*) AS samples,
  round(avg(delay_ms), 2) AS avg_ms,
  round((SELECT delay_ms FROM ranked WHERE rn = CAST((n * 0.50) + 0.5 AS INT)), 2) AS p50_ms,
  round((SELECT delay_ms FROM ranked WHERE rn = CAST((n * 0.90) + 0.5 AS INT)), 2) AS p90_ms,
  round((SELECT delay_ms FROM ranked WHERE rn = CAST((n * 0.95) + 0.5 AS INT)), 2) AS p95_ms
FROM ranked;

-- Startup chain approximation: main.m3u8 -> first segment on same playback_key
WITH main AS (
  SELECT playback_key, item_id, created_at AS main_at
  FROM telemetry_request_events
  WHERE created_at >= $WINDOW_EXPR
    AND route_template = '/videos/{id}/main.m3u8'
    AND status BETWEEN 200 AND 299
),
segments AS (
  SELECT playback_key, item_id, created_at AS segment_at
  FROM telemetry_request_events
  WHERE created_at >= $WINDOW_EXPR
    AND route_template = '/videos/{id}/{segment_file}'
    AND status BETWEEN 200 AND 299
),
matched AS (
  SELECT
    m.playback_key,
    m.item_id,
    m.main_at,
    (
      SELECT s.segment_at
      FROM segments s
      WHERE s.playback_key = m.playback_key
        AND s.item_id = m.item_id
        AND s.segment_at >= m.main_at
      ORDER BY s.segment_at ASC
      LIMIT 1
    ) AS segment_at
  FROM main m
),
deltas AS (
  SELECT
    (julianday(segment_at) - julianday(main_at)) * 86400000.0 AS delay_ms
  FROM matched
  WHERE segment_at IS NOT NULL
    AND (julianday(segment_at) - julianday(main_at)) * 86400000.0 BETWEEN 0 AND 30000
),
ranked AS (
  SELECT delay_ms, row_number() OVER (ORDER BY delay_ms) AS rn, count(*) OVER () AS n
  FROM deltas
)
SELECT
  'main -> first segment (<=30s match)' AS section,
  count(*) AS samples,
  round(avg(delay_ms), 2) AS avg_ms,
  round((SELECT delay_ms FROM ranked WHERE rn = CAST((n * 0.50) + 0.5 AS INT)), 2) AS p50_ms,
  round((SELECT delay_ms FROM ranked WHERE rn = CAST((n * 0.90) + 0.5 AS INT)), 2) AS p90_ms,
  round((SELECT delay_ms FROM ranked WHERE rn = CAST((n * 0.95) + 0.5 AS INT)), 2) AS p95_ms
FROM ranked;

-- Startup-impact segment misses by playback window after first segment request
WITH segment_scope AS (
  SELECT
    playback_key,
    item_id,
    item_name,
    created_at,
    status,
    julianday(created_at) AS ts_jd
  FROM telemetry_request_events
  WHERE created_at >= $WINDOW_EXPR
    AND route_template = '/videos/{id}/{segment_file}'
    AND playback_key IS NOT NULL
),
first_segment AS (
  SELECT
    playback_key,
    min(ts_jd) AS first_seg_ts_jd
  FROM segment_scope
  GROUP BY playback_key
),
startup_miss_window AS (
  SELECT
    s.playback_key,
    s.item_id,
    max(s.item_name) AS item_name,
    count(*) AS segment_requests,
    sum(CASE WHEN s.status = 404 THEN 1 ELSE 0 END) AS segment_404s,
    sum(
      CASE
        WHEN s.status = 404
          AND (s.ts_jd - f.first_seg_ts_jd) * 86400.0 <= $STARTUP_WINDOW_SECONDS
          AND (s.ts_jd - f.first_seg_ts_jd) * 86400.0 >= 0
        THEN 1 ELSE 0
      END
    ) AS startup_404s
  FROM segment_scope s
  INNER JOIN first_segment f ON f.playback_key = s.playback_key
  GROUP BY s.playback_key, s.item_id
),
startup_summary AS (
  SELECT
    sum(segment_requests) AS total_segment_reqs,
    sum(segment_404s) AS total_segment_404s,
    sum(startup_404s) AS startup_window_404s,
    round(sum(startup_404s) * 100.0 / nullif(sum(segment_requests), 0), 2) AS startup_window_404_pct,
    round(sum(segment_404s) * 100.0 / nullif(sum(segment_requests), 0), 2) AS all_window_404_pct
  FROM startup_miss_window
)
SELECT
  'startup segment 404 concentration (first ' || $STARTUP_WINDOW_SECONDS || 's)' AS section,
  total_segment_reqs AS segment_requests,
  total_segment_404s AS segment_404s,
  startup_window_404s AS startup_window_404s,
  startup_window_404_pct,
  all_window_404_pct
FROM startup_summary;

WITH segment_scope AS (
  SELECT
    playback_key,
    item_id,
    item_name,
    created_at,
    status,
    julianday(created_at) AS ts_jd
  FROM telemetry_request_events
  WHERE created_at >= $WINDOW_EXPR
    AND route_template = '/videos/{id}/{segment_file}'
    AND playback_key IS NOT NULL
),
first_segment AS (
  SELECT
    playback_key,
    min(ts_jd) AS first_seg_ts_jd
  FROM segment_scope
  GROUP BY playback_key
),
startup_blast_radius AS (
  SELECT
    s.playback_key,
    max(s.item_name) AS item_name,
    s.item_id,
    count(*) AS segment_requests,
    sum(CASE WHEN s.status = 404 THEN 1 ELSE 0 END) AS segment_404s,
    sum(
      CASE
        WHEN s.status = 404
          AND (s.ts_jd - f.first_seg_ts_jd) * 86400.0 <= $STARTUP_WINDOW_SECONDS
          AND (s.ts_jd - f.first_seg_ts_jd) * 86400.0 >= 0
        THEN 1 ELSE 0
      END
    ) AS startup_404s
  FROM segment_scope s
  INNER JOIN first_segment f ON f.playback_key = s.playback_key
  GROUP BY s.playback_key, s.item_id
)
SELECT
  'playback-key startup misses' AS section,
  playback_key,
  item_name,
  item_id,
  segment_requests,
  segment_404s,
  startup_404s
FROM startup_blast_radius
WHERE startup_404s > 0
ORDER BY startup_404s DESC, segment_requests DESC
LIMIT 20;

-- Client first-frame instrumentation health
SELECT
  'client-first-frame count' AS section,
  count(*) AS events,
  min(created_at) AS first_seen,
  max(created_at) AS last_seen,
  round(avg(elapsed_ms), 2) AS avg_elapsed_ms,
  round((SELECT elapsed_ms FROM (
    SELECT elapsed_ms, row_number() OVER (ORDER BY elapsed_ms) AS rn, count(*) OVER () AS n
    FROM telemetry_playback_events
    WHERE created_at >= $WINDOW_EXPR
      AND event = 'client-first-frame'
  ) WHERE rn = CAST((n * 0.50) + 0.5 AS INT)), 2) AS median_elapsed_ms
FROM telemetry_playback_events
WHERE created_at >= $WINDOW_EXPR
  AND event = 'client-first-frame';
SQL
