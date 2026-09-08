# Remux music-client compatibility audit

This is the durable record of Remux/Jellyfin contract mismatches found while testing real
music clients. Keep schema and routing defects separate from temporary source-resolution
failures: they can produce similar client errors but require different fixes.

Status values: `fixed`, `implemented`, `investigating`, `deferred`, and `not a contract bug`.

## Findings

### Generic playlist browse ignores `IncludeItemTypes`

- **Status:** fixed, deployed, and live-verified 2026-07-15.
- **Clients observed:** Discrete.
- **Request:** `GET /Items?ParentId={playlistId}&IncludeItemTypes=Audio` (Discrete sends
  `fields` more than once, which is unrelated).
- **Expected contract:** only audio playlist members are returned, and filtering happens
  before `StartIndex`/`Limit` pagination and `TotalRecordCount` calculation.
- **Previous Remux behavior:** the generic `/Items` playlist branch returned every relation.
  The live `car time` response contained 132 `Audio` items and one `MusicArtist` (`sadeyes`)
  despite the explicit audio filter. A typed client rejects the complete response.
- **User-visible symptom:** playlist data fails to load with a generic loader error.
- **Change:** reuse the playlist item-type filter for both `/Playlists/{id}/Items` and the
  generic `/Items?ParentId={playlistId}` path. A mixed artist/audio regression test verifies
  filtering before a one-item page is selected.
- **Files:** `crates/remux-server/src/api/items.rs`,
  `crates/remux-server/src/api/playlists.rs`.
- **Verification:** the live Discrete-form query now returns `TotalRecordCount=132`, 132
  returned items, and only `Type=Audio`. The regression test
  `playlist_browse_filters_item_types_before_pagination` passes.

### Dedicated playlist route previously ignored `IncludeItemTypes`

- **Status:** fixed and live before this audit.
- **Clients observed:** Finamp.
- **Request:** `GET /Playlists/{id}/Items?IncludeItemTypes=Audio`.
- **Previous Remux behavior:** the route leaked non-audio playlist members and Finamp threw
  `Wrong BaseItemDto type: MusicArtist`.
- **Change:** resolve member kinds in one batch, filter before pagination, and preserve all
  members for unfiltered requests.
- **Detail:** `PR-DRAFT-finamp-music-compat.md`, section E.

### Missing versus empty music DTO fields

- **Status:** handled in the current worktree; re-verify final wire shapes before upstreaming.
- **Clients observed:** Manet and Finamp.
- **Contract issue:** Remux historically omitted empty optional properties or returned the
  wrong empty shape. Jellyfin clients may distinguish an absent property, `null`, and `[]`,
  and some typed clients assume Jellyfin's exact shape.
- **Fields already audited:** `Artists`, `ArtistItems`, `AlbumArtists`, `MediaSources`, and
  `MediaStreams`. The current implementation intentionally emits empty artist arrays and
  synthesizes a minimal audio stream when probe data is absent; this supersedes older notes
  that proposed `null` for artist arrays.
- **Rule for future fixes:** compare serialized JSON from stock Jellyfin, not only Rust model
  optionality or OpenAPI declarations, and add wire-format assertions for empty values.

### Manet reports "Not allowed to transcode audio"

- **Status:** fixed, deployed, and live-verified 2026-07-15. Confirmed not a user-policy
  denial.
- **Client request:** Manet 1704 uses `GET /Items/{id}/File` directly. It does not request
  `PlaybackInfo` or a transcoding URL for the observed attempts.
- **Evidence:** the `tv` user has `EnableAudioPlaybackTranscoding=true`. During one queue run,
  `/File` returned a mix of `200`, `410`, and `500`. The `500` response logged
  `no playable sources`; the same track succeeded later after sources became available.
- **Interpretation:** Manet maps a generic legacy file-endpoint failure to a misleading
  transcoding-permission message. Advertising fake permissions or transcode URLs would hide
  the real source-resolution failure and is not appropriate.
- **Root cause:** unlike `PlaybackInfo`, `StreamService::lookup` did not call
  `refresh_streams`. Manet could therefore read an empty source list or an expired signed URL
  indefinitely because it never calls the route that refreshes those sources.
- **Change:** root item lookups now invoke the same TTL-governed, per-item-locked source refresh
  used by `PlaybackInfo` before selecting a cached source. Explicit media-source selections
  remain untouched. This preserves the resolver's existing 60-second TTL, addon ordering,
  concurrency limits, and fallback behavior.
- **File:** `crates/remux-server/src/services/stream_service.rs`.
- **Verification:** a track that previously returned `500 no playable sources` refreshed from
  Monochrome in 647 ms on a direct `/File` request and returned HTTP `206`. A second request
  inside the TTL returned `206` in 96 ms without another addon refresh.

### Legacy file route accepts unauthenticated requests

- **Status:** investigating; do not conflate with the Manet compatibility fix.
- **Observed behavior:** `GET /Items/{id}/File` returned audio bytes without an authorization
  header. The route handler has no `AuthSession` extractor.
- **Risk:** anyone who knows or obtains an item UUID may be able to stream or download it.
- **Next check:** verify stock Jellyfin's accepted authentication mechanisms for `/File`,
  `/Download`, and audio stream routes, then enforce the equivalent policy without breaking
  clients that send a token in the query string instead of the authorization header.

### Intermittent playback resource failures

- **Status:** investigating; not currently classified as a Jellyfin schema mismatch.
- **Clients observed:** Discrete and Manet.
- **Evidence:** live requests sometimes fail after Monochrome returns HTTP 429 or all addons
  return no usable stream. `PlaybackInfo` then returns `500`, or `/Items/{id}/File` returns
  `500`; signed upstream URLs can return `410`. Some identical item IDs succeed on a later
  attempt.
- **User-visible symptoms:** iOS `(-1008) resource unavailable`, Discrete loader failures,
  and Manet's misleading transcoding alert.
- **Existing mitigation:** worker concurrency limiting, retry/backoff, and HTTP timeouts are
  documented in `PR-DRAFT-finamp-music-compat.md`, section B. Current logs show that the
  mitigation does not eliminate burst failures and needs another measured pass.

### Music addon fallback resolution

- **Status:** implemented, deployed, and live-verified 2026-07-16.
- **Previous behavior:** every matching stream addon ran concurrently and Remux waited for all
  of them before returning any result. Addon priority only affected result ordering, so slow
  fallback providers extended successful primary lookups and were contacted unnecessarily.
- **Change:** track resolution now treats addon priority as fallback tiers. Equal-priority
  providers race concurrently and the first non-empty result wins; unfinished peers are
  cancelled. Lower-priority tiers are started only when every provider in the preceding tier
  returns empty or errors. Movie and episode multi-source aggregation is unchanged.
- **Live configuration:** archive music providers and Monochrome are priority `-30` primary
  providers. SpotiFLAC and yt-dlp remain priority `0` fallbacks.
- **File:** `crates/remux-server/src/addons/mod.rs`.
- **Verification:** a primary archive hit selected priority `-30` in 458 microseconds and
  returned HTTP `206` without entering the fallback tier. A known missing track exhausted
  priority `-30`, then yt-dlp supplied four candidates at priority `0`; the request returned
  HTTP `206`. Unit tests verify that a successful provider does not wait for an unfinished
  peer and that empty results allow another provider to win.
- **Remaining issue:** the external Monochrome worker still returns `429` during large client
  prefetch bursts. Fallback tiers now recover tracks supported elsewhere, but they cannot make
  an unsupported track playable and do not replace resolver-side rate-limit work.

### Finamp `(-1100) The requested URL was not found`

- **Status:** fixed, deployed, and live-verified 2026-07-15.
- **Evidence:** affected `/Items/{id}/File` requests returned an actual HTTP `404`. The
  Monochrome resolver response duplicated an absolute signed Tidal URL inside itself, for
  example `https://.../mediatracks/.../https://.../mediatracks/.../0.mp4`.
- **Root cause:** the Eclipse-compatible addon adapter persisted the resolver response without
  validating this known malformed shape. Both `ffprobe` and direct playback requested the
  duplicated path and received `404` from the CDN.
- **Change:** normalize duplicated absolute URLs at the addon boundary and persist only the
  final signed CDN URL. Correctly formed URLs are unchanged.
- **File:** `crates/remux-server/src/addons/eclipse.rs`.
- **Verification:** the exact duplicated shape is covered by
  `normalizes_duplicated_absolute_media_url`. After deployment, the previously failing Finamp
  track returned HTTP `206` for a byte-range request. Upstream resolver `429` failures remain a
  separate availability issue.

### Playlist and item artwork gaps

- **Status:** fixed, deployed, and live-verified 2026-07-15 for playlist child tracks.
- **Evidence:** clients successfully load text metadata while many primary-image requests
  return `404`. Some playlist DTOs have empty `ImageTags`; track and album artwork succeeds
  when a valid tag is present.
- **Root causes:** playlist routes converted each member independently without preloading its
  album/artist records; parent-image preloading consumed a shared album image after the first
  track; and `/Items/{track}/Images/Primary` only checked artwork stored directly on the track.
- **Change:** both playlist browse routes now batch-preload parents. Track DTOs inherit the
  album primary image tag (artist as a final fallback), and the track image route serves the
  same inherited image. Direct track artwork still takes precedence.
- **Files:** `crates/remux-server/src/api/items.rs`,
  `crates/remux-server/src/api/playlists.rs`, `crates/remux-server/src/api/models.rs`,
  `crates/remux-server/src/api/images.rs`, and `crates/remux-server/src/db/media.rs`.
- **Verification:** `playlist_tracks_inherit_and_serve_shared_album_art` verifies that two
  tracks sharing one album both expose identical `ImageTags.Primary` and
  `AlbumPrimaryImageTag`, and that the advertised track image route returns bytes. Live client
  traffic returned HTTP `200` for a previously missing track primary image after deployment.

### Provable 1:1 audio response parity (diff harness)

- **Status:** fixed, deployed, and harness-proven 2026-07-16. A local Audio track item is
  now byte-for-byte identical to stock Jellyfin's — `RESULT: PASS — 0 real gaps`, and the
  `--value-diff` pass shows zero remaining value divergences either.
- **Method:** a standalone diff harness (`tools/parity/`, see its `README.md`) replays the
  union of the Fields real clients (Finamp + Jellify) request against **two live servers** —
  Remux and a throwaway Docker Jellyfin (`jellyfin/jellyfin`) seeded with a **copy of the same
  physical files** — pairs items by file (not Id), normalizes a server-specific ignore-list
  (Ids, tokens, timestamps, image-tag *values*), and buckets every remaining field difference
  into `MISSING` / `NULL_VS_VALUE` / `EMPTY_ARRAY_VS_POPULATED` / `TYPE_MISMATCH` (hard fails)
  plus `VALUE_DIFF` (informational). It re-ran after every fix batch as the regression gate.
- **Progression (20 identical-file track pairs):** 280 → 120 → 80 → 40 → **0** hard gaps.
- **Divergences closed, each proven by a harness re-run:**
  - `MediaSources[].Container` / `Bitrate` / `Size` — carried through from `probe_data`
    (`conversions.rs`).
  - Per-stream `BitRate` (overall-bitrate fallback for the first audio stream), `BitDepth`,
    `Level`, `TimeBase` — captured in `playback/probe.rs`.
  - `MediaStream.Level` numeric wire format — Jellyfin's `.NET` writer prints a whole-valued
    `double` with no decimal (`0`, not `0.0`); `serialize_option_whole_f64`
    (`remux-sdks/src/lib.rs`) matches it byte-for-byte while keeping the OpenAPI `number` type.
  - `IsAVC` key casing — `#[serde(rename = "IsAVC")]` (`remux-sdks/src/remux/mod.rs`).
  - `VideoType` — now `Option`, omitted for audio, retained (`VideoFile`) for video sources.
  - Item-level `MediaStreams`, `Container`, `ProductionYear`/`PremiereDate`,
    `ParentIndexNumber` (disc), `AlbumPrimaryImageTag` (folder cover adopted at group time).
  - `DisplayTitle` — dropped the non-Jellyfin " - Default" suffix (`playback/probe.rs`).
  - `Genres` / `GenreItems` — local tracks now parse the `genre` tag into `MusicGenre`
    entities + `media_relations` (`tasks/group_local_music.rs`). A **batch relation loader**
    (`Media::load_relations_for_many`, `db/media.rs`) fixes empty genres on the multi-id
    `/Items?Ids=` browse path, where relations were previously loaded only for single-item
    fetches — this was the actual reason genres appeared on `/Items/{id}` but not in browse.
  - `HasLyrics` — gated on real availability: streaming tracks keep `true` (an addon resolves
    lyrics on demand); local tracks claim lyrics only when they carry a lyric stream, matching
    Jellyfin's `false` for a bare audio file (`api/models.rs::track_has_lyrics`).
  - `MediaSources[0].Name` — the file stem (e.g. `Chief Keef - Bang - 01 - …`), not the track
    title. Local tracks have no `stream_info`, so the stem is stashed on `probe_data.name` at
    group time and preferred by the serializer.
- **Tests:** colocated `#[cfg(test)]` guards in `conversions.rs` (audio MediaSource shape incl.
  file-stem Name; whole-number `Level` serialization; video keeps `VideoType`) and
  `group_local_music.rs` (`parse_genres` delimiter/dedupe/comma handling).
- **Rollout:** a one-time full-library `GroupLocalMusic` re-run backfills genres, folder art,
  release year, disc, and file-stem source names across all local tracks (the task reprocesses
  rows whose `probe_data` is null; the audio ffprobe itself is unchanged).

### `/Audio/{id}/universal` redirected audio into the video pipeline

- **Status:** fixed, deployed, and harness-proven 2026-07-16.
- **Clients affected:** any music client that uses Jellyfin's standard adaptive audio
  endpoint (web client, Finamp in some modes, others).
- **Evidence:** `GET /Audio/{id}/universal` returned `307 →
  /videos/{id}/master.m3u8?VideoCodec=copy&AudioCodec=aac`, i.e. the **video** HLS
  pipeline, for an audio-only track. Following it yielded a 192-byte
  `application/vnd.apple.mpegurl` video manifest. The underlying file was perfect
  (a local FLAC decoded cleanly to 44.1 kHz/16-bit/199 s), and `/Audio/{id}/stream`
  and `/Items/{id}/File` both returned `206 audio/flac`. This is the most likely
  cause of "a track isn't playing well or at all" reports (e.g. Hayley Williams –
  *Ego Death at a Bachelorette Party*).
- **Change:** `audio_universal` now redirects to the range-capable direct audio
  stream (`/audio/{id}/stream?static=true`) — the exact source `PlaybackInfo`
  advertises as `SupportsDirectPlay=true`. That handler already resolves and
  refreshes addon-backed sources via `StreamService::lookup`, so streaming tracks are
  unaffected. Clients that cannot decode the source negotiate a transcode through
  `PlaybackInfo`'s `TranscodingUrl`, not through this convenience redirect.
- **File:** `crates/remux-server/src/api/playback.rs`.
- **Verification:** the playback harness (`tools/playback/`) asserts universal never
  redirects into `/videos/` and resolves to an `audio/*` stream.

### Single-item `/Items/{id}` mislabeled local tracks as remote

- **Status:** fixed, deployed, and harness-proven 2026-07-16.
- **Evidence:** the batch `/Items?Ids=…` path returned a correct `Protocol:File`
  source for a local track (this is what the parity harness exercised, hence green),
  but the single-item `item()` path — used by `/Users/{uid}/Items/{id}` and a
  one-id `/Items?Ids=` — returned `Protocol:Http`, `IsRemote:true`, `Container:null`
  with a *video* transcoding URL. `PlaybackInfo` itself was correct, so direct
  playback still worked, but any client reading the item's own `MediaSources`
  (for display or source selection) saw a bogus remote source for every local file.
- **Root cause:** `item()` wrapped **every** Track in an HLS/`Http` MediaSource. That
  wrap is correct only for streaming/addon tracks, whose CDN URLs are IP-locked.
- **Change:** gate the wrap to non-local tracks (`custom_stremio_id` not starting
  with `opendal:`); local files keep the direct `File` source that `db_media_to_item`
  already builds (identical to the batch path and the parity harness).
- **File:** `crates/remux-server/src/api/items.rs`.
- **Verification:** playback harness — Hayley album **19/19**, a 40-track random local
  sample **40/40**, all decode-verified; parity harness stays `PASS — 0 gaps`.

### Streaming tracks heal to a local copy (strict content match)

- **Status:** implemented, deployed, and harness-proven 2026-07-16.
- **Evidence:** the `car time` playlist is **entirely** streaming-backed (all 133
  members are Deezer rows). ~27% failed at test time — the signed upstream URL is dead
  (HTTP `410`) or the provider no longer serves the track (empty `PlaybackInfo`), and
  re-resolution cannot recover an unavailable track. **111 of the 133 members have an
  identical local file** in the library that plays reliably.
- **Root cause of the missed heals:** the opendal-local addon already had a fallback
  for non-local tracks, but it matched `LOWER(opendal_files.title)` — a filename-derived
  column that is unreliable (it frequently holds the *artist* name), so it both missed
  real local twins and matched unrelated songs sharing a title.
- **Change:** the fallback now matches strictly against the `media` table on
  **track title + album + artist** (the fields GroupLocalMusic populates reliably) and
  only trusts an **unambiguous single candidate** — two different songs sharing a title
  are never conflated, and duplicate local rips fall through to no-heal. opendal-local
  is a primary (`priority -30`) provider, so a matched local file becomes the reliable
  source and the streaming provider is never contacted. Tracks with no local copy are
  unchanged (still resolved via the streaming addons).
- **File:** `crates/remux-server/src/addons/opendal.rs`.
- **Verification (playback harness):** `car time` climbed **97/133 → 125/133** with the
  28 healed tracks all decode-backed by the correct local FLACs and **zero regressions**
  on the 97 that already passed. `Victorious` (Deezer `410`) now serves the correct
  Panic! At The Disco local FLAC (decode-verified, 179 s). The 8 residual failures are
  genuine data gaps: 7 have no local copy and are gone upstream, and 1 matched a local
  file that has since been deleted from disk (stale opendal index — a scan-refresh
  concern, not a resolution bug).

### One dead provider made all music playback fail

- **Status:** fixed, deployed, and live-verified 2026-09-07 (commits `72ddfcfa`, `208e771c`,
  `25ab71c4`).
- **Clients observed:** Feishin 1.15.1; the same shape reaches Finamp, Manet and Discrete.
- **Evidence:** user `tv`, 2026-09-03 14:04. Seven local opendal addons answered
  `no streams` in under 10 ms, then `stream addon timed out addon=Monochrome
  timeout_secs=10`, then `500 no playable sources`, then four client retries each
  answered instantly from the 45-second negative cache.
- **Root cause, layered:**
  - **Monochrome is gone.** `MONOCHROME_URL` (`addons/eclipse.rs`) returns HTTP 404 with
    the Cloudflare body `error code: 1042`. Per-day success across retained logs: Aug 28
    3/3, Aug 30 0/1, Aug 31 0/2, Sep 1 0/2, Sep 3 0/4. 919 `no playable sources` in total.
  - **The priority tiering documented above did not exist in the code.** `get_streams` was
    a flat `futures::future::join_all` over every stream addon, so every resolution waited
    for the slowest provider — a track a local addon answered in 3 ms still cost the full
    10 s.
  - **A permanent 404 was retried five times** at 1.5 s behind `WORKER_CONCURRENCY` and the
    750 ms rate gate, which is precisely what consumed that budget.
  - **`make_http_client` set no timeouts at all**, leaving `Config::addon_http_timeout_secs`
    a field nothing read.
  - **No fallback could fire.** SpotiFLAC's `addons.resources` omits `stream`, so
    `PickCap::<dyn StreamAddon>::pick` rejects it before any capability check; yt-dlp's
    `StreamCap::supports` required an `external_ids.youtube_id` that a Deezer-sourced track
    never carries.
- **Change:** music resolves through priority fallback tiers (equal-priority providers race,
  first non-empty wins, unfinished peers dropped, next tier only on exhaustion); movies and
  episodes still aggregate every source. A per-addon circuit breaker
  (`addons/health.rs`) opens after three consecutive failures for a doubling window capped
  at 15 minutes, with a single half-open probe — an `Empty` result is explicitly *not* a
  failure, since local addons answer empty for most tracks. Music gets a 3 s primary budget;
  fallback tiers get 20 s, because yt-dlp needs ~2.5 s just to search and is only reached
  when the alternative is silence. yt-dlp now accepts any track and resolves by search,
  guarded by `youtube_search_matches` (title, artist across title/uploader/channel/artist,
  duration within 5 s) and carrying yt-dlp's per-format `http_headers` into the descriptor.
- **Verification:** against a copy of the production library, PlaybackInfo for a local track
  fell from ~10 s to **49 ms**; a 30-track local sample went 29/30 → **30/30** and the
  Panic! At The Disco tracks that have a local file are **31/31**, all decode-verified.

### `no playable sources` was a 500

- **Status:** fixed, deployed, and live-verified 2026-09-07.
- **Evidence:** clients mistranslate a 500 — Manet reports "Not allowed to transcode audio",
  Finamp `-1008` — sending users after the wrong problem. Two distinct paths produced it:
  `StreamService::load` (919 occurrences) and `probe_candidates` exhausting every candidate
  (**2,998** occurrences, more than three times the first).
- **Change:** a typed `NoPlayableSources` error. `PlaybackInfo` now answers `200` with
  `ErrorCode: NoCompatibleStream`, as stock Jellyfin does; the byte endpoints answer 404.
  `log_api_error` drops 4xx to `debug`, so the spurious ERROR lines disappear on their own.

### `/Audio/{id}/universal` rejected stock Jellyfin clients and re-encoded everything

- **Status:** fixed, deployed, and live-verified 2026-09-07. Supersedes the 2026-07-16 entry
  below: the 307 redirect it describes is no longer in the code, and the behaviour that
  replaced it had regressed further.
- **Evidence:** `400 unsupported audio download combination: container=mp4, codec=aac` —
  the pair stock clients send — **59 times across 6 distinct users** (lakmani 24, maddog 14,
  Legendmax 10, timiditit 8, silly 2, Willy7683 1).
- **Root cause:** `audio_download_format` was a strict whitelist missing `mp4`/`aac`, and the
  handler never read Jellyfin's `Container` (the client's supported-container list), so
  `TranscodingContainer` fell through to its `mp3` default and every lossless file was
  re-encoded — served `Accept-Ranges: none` and, worse, `Content-Disposition: attachment`.
- **Change:** the full Jellyfin parameter set is parsed; direct play delegates to
  `videos_stream_inner`'s range-capable byte path when the source matches what the client
  declared and stays inside its caps; the format map is infallible and degrades to mp3;
  the attachment disposition is gone from the streaming endpoint, and
  `/items/{id}/download` gained the RFC 5987 `filename*` form.

### Audio served as `application/octet-stream`

- **Status:** fixed, deployed, and harness-proven 2026-09-07.
- **Evidence:** `Daft Punk - … - Too Long.MP3` failed the harness on two endpoints.
  `mime_from_path` matched extensions case-sensitively, so an uppercase extension fell
  through to `application/octet-stream`, which strict music clients refuse.
- **Change:** lowercased (plus m4b/wma/wv/aiff), and the byte path now normalises an audio
  source's Content-Type from its probe. Note the gate is `probe_is_audio_only`, not "has no
  video stream": a tagged music file carries its cover art as an mjpeg *video* stream, so
  the naive test rejects most of the library's music.

### Unrelated production bugs found while fixing the above

- **The media INSERT listed 46 columns against 47 value placeholders** (left over from
  `d9e53201`). It broke playlist creation in production — user `shadow`, three attempts on
  2026-09-05 — and 80 unit tests. The lib test target now compiles: 537 → 655 passing,
  92 → 12 failing, the residual 12 pre-existing and unrelated.
- **A 70-second full-table scan.** `is_channel_query` used `kinds.iter().all(...)`, which is
  vacuously true on an empty vec, so `kind: Some(vec![])` took the channel `ORDER BY` while
  contributing no `WHERE` predicate: `SELECT * FROM media WHERE 1=1` ordered by channel
  number, observed returning **1,514,646 rows in 70.4 s** during an ordinary music browse.

## Verification checklist

- Capture the exact request and serialized response from Remux.
- Capture the equivalent stock Jellyfin response for the same media shape.
- Distinguish omitted, `null`, `[]`, and populated fields.
- Verify filtering before pagination and counts.
- Test at least one strict typed client and one web client.
- For playback failures, correlate client time, item ID, endpoint status, addon resolution,
  and upstream status before changing the API contract.
