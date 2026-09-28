# Music reliability investigation — 27 September 2026

## Scope and evidence limits

Canonical checkout: `/home/s/remux-server-wip`. Baseline commit:
`5d51ca95113cde883354e024b75d6bc9eb029483`.
The running executable matched its immutable release manifest (SHA-256
`29f57352a0c73cebb88970bb24fc76f4246c3f9adcb818e81161fccd3140f577`).

Reviewed retained application logs from **20 August–27 September** (38 days),
SQLite request/playback telemetry, hourly rollups, current addon configuration,
local file access using the service's UID/GID/supplemental groups, and upstream
HTTP responses. Nginx's retained detailed request evidence covers 13–27 September;
SQLite detailed requests cover approximately 14 days. Older hourly rollups were
sampled along with successful requests. **Their totals are not denominators for
failure-rate estimates.** No claim of a complete 38-day request census is made.

Finamp and Manet are the acceptance clients. Server route tests emulate their
requests; they do not substitute for an actual iOS listening test, interruption,
background playback, or lock-screen controls.

## Findings

* Nginx records **81 Finamp requests to `/Audio/{id}/main.m3u8` returning 404**.
  Another client made six failing audio-master requests. The routes were absent,
  despite Jellyfin exposing both audio master and main playlists.
* Stored Googlevideo locators for failing tracks lacked `valid_until` even when
  their query strings contained expiry timestamps. The expiry inference helper
  existed but was not called before persistence. An old Qobuz locator returned
  410. Direct-file reuse could select cached HTTP locators without qualifying the
  body or recovering to another source.
* A fresh `deadmau5 Strobe` YouTube result matched the recording's title and
  approximately 638-second duration. Its AAC and Opus URLs returned **206 for
  `bytes=0-65535`, then 403 for a full body, `bytes=0-`, and a later range**.
  Native yt-dlp downloading also failed with 403 and zero audio bytes. This
  proves an unusable source despite successful discovery and an initial byte
  probe. It does **not** establish why Google's access check rejected it.
* Application logs contain **75 yt-dlp search mismatches** followed by addon
  errors. A normal recording miss was counted as provider failure. Search took
  the first result and then extracted it again to obtain formats.
* Logs contain **40 Monochrome permanent 404s**. The configured Monochrome worker
  manifest and SpotiFLAC Eclipse manifest are gone (404). SpotiFLAC's row was
  enabled for `catalog,search`, without `stream`; toggling it on would not make
  the endpoint work.
* There are **11,659 explicit 429 retry messages for stremio.obby.ca** and **33
  for torrentio.strem.fun**. Those are video providers. No corresponding music
  worker-429 messages or identified music nginx 429 responses were found in the
  reviewed window. Music rate limiting remains **unproven in production**.
* Of 540 track source-sync log messages, 331 were nonempty and 209 empty. Named
  winners included Monochrome (30), yt-dlp (109), mixed Monochrome/local (1),
  1tb4music (86), slskd (72), and 1tb2music (33). A nonempty URL response did not
  prove that playback succeeded.
* 175 of 233 retained track stop records had zero position; only 25 recorded
  completion. These observations cannot distinguish failed playback from
  intentional skips by themselves.

## Local provider checks

| Provider | Indexed files | Readable as service | Complete decode spot-check |
|---|---:|---:|---|
| 10tbmusic | 524 | 524 | Passed, one file |
| 1tb4music | 14,026 | 14,026 | Passed, one file |
| 1tb2music | 1,282 | 1,282 | Passed, one file |
| slskdcomplete | 2,536 | 2,447 | Passed, one file; 89 indexed paths missing |
| 1tbmusic, 1tb3music, 10tb2music | 0 | 0 indexed | No recording to qualify |

Total readable indexed files: **18,279**. Readability of every path is not proof
that every file completely decodes. Empty providers are not proven playback
sources.

## Implementation

* Local files are selected before remote providers, independently of addon
  priority. Missing paths are skipped. Remote providers race with at most two
  in-flight candidates and a short hedge delay inside a bounded resolution
  deadline. The winner must deliver a complete, decodable recording.
* Remote HTTP music fills an on-demand **10 GiB** disk cache, with a **512 MiB**
  per-object limit and two concurrent fills. Per-key locks coalesce identical
  requests. Keys include item, user scope, URL, and negotiated headers. Partial
  files are temporary; publication happens after complete decode, audio probing,
  size and duration checks. Persisted source rows carry the access scope.
  Subsequent file/range/HLS requests use stable local bytes. This deliberately
  trades cold-start latency for reliable delivery; cold fills may time out.
* Legacy URL expiry is inferred both on persistence and validity checks. A
  cached URL alone is no longer a usable music source. Explicit stale source IDs
  recover through the owning track. Refresh timestamps are written after source
  persistence, rather than marking a failed write fresh.
* Resolver cooldowns are shared per origin and honor numeric and HTTP-date
  `Retry-After`. Permanent 4xx answers fail immediately. Attempts and elapsed
  time are bounded. Half-open breaker probes are leased so concurrent callers
  cannot all probe a recovering provider; cancellation releases the lease.
* yt-dlp checks up to three results, reuses extracted formats, and kills the
  process if its request is cancelled. A recording mismatch is an empty result,
  not a provider outage. Eclipse matching requires artist/title agreement and
  duration agreement when present; it never substitutes the first unrelated
  result. Optional Eclipse metadata and expiry are supported.
* Added an explicit-manifest `eclipse` preset for other compatible sources. This
  is adapter support, not evidence that any arbitrary manifest works.
* Audio HLS uses AAC-only MPEG-TS segments and existing session, seek, cleanup,
  and segment-serving machinery. Both direct-main and master entry points are
  supported. A main request without a session redirects to a stable session URL,
  so playlist reloads cannot restart the recording. Playlist Content-Length is
  preserved through body instrumentation (chunked playlists caused FFmpeg I/O
  failures in a controlled reproduction). The master advertises only an audio
  codec. Universal audio honors
  an HLS transcode request and audio channel/sample-rate constraints.
* Tidal layout discovery coalesces concurrent requests, scopes cache entries to
  negotiated headers, limits probe bursts, and checks segment Content-Range.
* Hourly request rollups now count every request reaching the middleware;
  retained detail may still be sampled. New rollups use `sample_reason=complete`
  so they can be separated from legacy sampled totals. Audio response body
  observations distinguish complete transfer, error, truncation, and
  cancellation. Body delivery is not a claim that the user listened.

## Provider expansion constraints

The old SpotiFLAC Eclipse endpoint cannot currently be qualified. Current
SpotiFLAC Mobile Tidal/Qobuz extensions use a different signed-session and ticket
protocol. Installing their manifest as an Eclipse manifest would not work. Additional public instance discovery checked the Monochrome instance list: `api.monochrome.tf` and `tidal.kinoplus.online` failed DNS lookup; `wolf.qqdl.site` and `maus.qqdl.site` failed TLS negotiation (unexpected EOF). None passed discovery, much less full playback. No
new account, subscription, signed-app impersonation, or alternate-IP workaround
was introduced. Rate-limit avoidance here means fewer requests, shared cooldowns,
coalescing, caching, and independent fallback.

Primary protocol references:

* [Jellyfin v10.11.8 DynamicHlsController](https://raw.githubusercontent.com/jellyfin/jellyfin/v10.11.8/Jellyfin.Api/Controllers/DynamicHlsController.cs)
* [Eclipse addon protocol](https://www.eclipsemusic.app/docs)
* [HTTP Retry-After](https://www.rfc-editor.org/rfc/rfc9110.html#name-retry-after)
* [SpotiFLAC Mobile provider source](https://github.com/spotiflacapp/SpotiFLAC-Extension/tree/main/sources)

## Reproduction

Run Rust tests from the canonical checkout. Full audio tests require ffmpeg and
ffprobe. `music_cache::tests` uses a controlled HTTP origin with valid, invalid,
truncated, and wrong-duration bodies. The ten-track API test starts an isolated
SQLite database and loopback server, generates WAV/FLAC/MP3 recordings, tests
file/range routes, then decodes complete audio HLS through the registered routes.
It does not contact music providers or mutate production data.

For an authorized deployed account, `scripts/verify-music-playback.py` takes a
private token file and a JSON item list. It downloads and fully decodes each
recording, checks exact range bytes, and emits a credential-free JSON report.
Use `--hls` for a complete segmented transfer. Supply `duration_seconds` per item
to reject previews and truncated recordings. Preserve the output alongside the
release identifier. A successful fixture or local-library run does not qualify
an unavailable remote provider.

## Verification evidence

Before deployment, ten real library recordings from the four nonempty local
providers passed complete downloads, strict FFmpeg decoding and **30 exact range
comparisons**. The same baseline server returned **404** for the first audio HLS
playlist. Reports are retained privately under
`/tmp/remux-music-proof-private/` and contain no access tokens.

The isolated ten-track API queue passes after the session handoff fix. It covers
WAV, FLAC and MP3 inputs, direct download, initial/suffix/open byte ranges, audio
master/main/universal HLS, and progressive MP3 conversion constrained to mono and
22.05 kHz. FFmpeg reads the real HTTP HLS endpoints and the output durations are
checked. Scoped sources reject unauthenticated audio/internal-stream requests.

Additional native yt-dlp probes as the production UID/GID, using the deployed
executable and configured extraction helper, attempted videos `dQw4w9WgXc` and
`5NV6Rdv1a3I`. Both exited unsuccessfully with **zero audio bytes**; the latter
reported HTTP 403. Along with the matched Strobe probe, these do not qualify
yt-dlp as a working production music source. A 403 is not a proven 429 rate limit.

Final pre-deployment command:
`CARGO_INCREMENTAL=0 cargo test --locked --offline -p remux-server --lib`
returned **681 passed, 2 failed** (683 total). The two failures are
`api::shows::test::upcoming_collection_parent_id_scopes_to_collection_series`
and `api::shows::test::upcoming_returns_episodes_with_released_at_today_or_future`.
Both reproduce using the unchanged baseline test binary; their assertions were
not weakened. Logs: `/tmp/remux-music-tests-final5.log` and
`/tmp/remux-baseline-shows.log`.

All music tests passed, including ten remote-origin fills followed by ten cache
replays with **zero additional origin requests**, expired locator reuse of
verified cached bytes, concurrent fill coalescing, user isolation, corrupt/
truncated/wrong-duration rejection, cancellation cleanup and quota reservations.
A real controlled HTTP 429 response verifies Retry-After delay before retry.
The API test also proves **40 file requests in complete hourly rollups** while
healthy detailed events are sampled out (sample rate zero). Body observation
checks distinguish complete delivery, body error and cancellation. Formatting
was run for both changed Rust crates; `git diff --check` and Python script syntax
validation passed.

## Deployed release and live verification

Deployed through `deploy/remux-canonical-deploy.sh` from a clean canonical tree:

* Code commit: `e5243dad5b5f499df0e99ba0215c8b443be44410`.
* Release: `/opt/remux/releases/e5243dad5b5f499df0e99ba0215c8b443be44410-2026-09-28T052900Z`.
* Executable SHA-256: `c48e2af4ee547b7ed135993de734bd2d08e56c18ebed9676549fb12fa8d8b6a5`.
* Dashboard index SHA-256: `4629f58715bc83e3dbf487c448d5d9f7dc0dc4f607cb2a2f9e355617cc2159f6`.

The release integrity check confirms that the running executable matches the
immutable manifest. The service is active. Deployment completed on 27 September
Pacific time (28 September UTC).

Loopback HTTP verification on the deployed service passed **20/20 full direct
files**, **60/60 exact range checks**, and **20/20 complete HLS recordings**.
The first queue contains 10.6 minutes of audio across all four nonempty local
providers; the additional queue contains 40.9 minutes of typical-length tracks.
HLS delivered **527 segments** in total. Each concatenated recording passed strict
FFmpeg decoding and a duration comparison. Before the public HTTPS check, the
post-deployment body telemetry window recorded **819 complete transfers and zero
body-error/truncated/cancelled outcomes**. Journal checks found no panics or
transcode failures in that window. These are transfer/decode checks, not a claim
of listening completion or real-device Finamp/Manet acceptance.

The three failing remote music providers were disabled through the admin API:
**SpotiFLAC, Monochrome, and yt-dlp**. All three updates returned HTTP 200 and the
database flags were independently verified false. Their configurations were
preserved; previous enabled states are backed up privately in
`/tmp/remux-music-proof-private/provider-enabled-before.json`. No unqualified
remote source was added. Remote-only tracks therefore remain unavailable with
these upstreams. Re-enabling one requires a fresh complete-recording test; a
working manifest or a first-byte response is insufficient.

The three empty local providers point to existing but empty directories
(`/1TB/music`, `/1TB3/music`, `/10TB2/music`). Their import configurations remain
available for future files; they are not counted as proven playback sources.

The post-deployment evidence is committed separately from the deployed code so
that the audit can contain the actual release hash and live results.

Public HTTPS verification through `https://remux.obnoxious.lol` also passed the
**10-track, 40.9-minute queue**: ten complete files, thirty exact byte-range
comparisons, and ten complete HLS decodes through nginx/TLS. No HTTP 429 occurred
in those probes. Loopback results alone were not used to claim public-route
success. This run originates from the server host; cellular/client-network
conditions remain untested.

Credential-free aggregate results and hashes of the original private reports are
in [music-verification-2026-09-27.json](music-verification-2026-09-27.json).
