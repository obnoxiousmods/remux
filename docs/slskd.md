# Soulseek acquisition and fast music playback

The `slskd` addon acquires missing tracks through slskd's batch API. Each candidate
gets a unique `remux-acquire/<UUID>` destination, so racing peers cannot overwrite
each other. The server checks the advertised byte count, decodes the entire audio
file, and checks duration before atomically publishing the file and probe manifest
under `<data_dir>/music/slskd/`. Subsequent playback reuses the persisted stream or
manifest, including after a restart and when Soulseek is offline.

These files are a permanent acquired library, separate from the evictable HTTP
music cache. Provision disk space accordingly. `music_cache_entry_bytes` limits
each acquisition. The configured completed-download directory is staging space;
do not index its `remux-acquire` subdirectory with another music provider.

## Setup

Add **Soulseek (slskd)** in the Remux dashboard. Configure:

- `url`: slskd's base URL, reachable by Remux.
- `api_key`: a slskd readwrite API key.
- `download_dir`: slskd's completed downloads directory as mounted in Remux.
  Both services need access to this shared volume.
- `formats`: preferred formats, default `flac,m4a,mp3,ogg,opus`.
- `min_bitrate`: minimum reported lossy bitrate, default 192 kbps.
- `acquire_timeout_secs`: total acquisition budget, default 40 seconds.
- `hedge_delay_ms`: delay before additional candidates start, default 150 ms.
- `max_peers`: maximum concurrent candidates per track, default 3.

Remux tries local music providers first. Its global
`music_fallback_stream_addon_timeout_secs` also bounds acquisition; set it to 60
if cold Soulseek transfers need that long. Warm playback does not wait this budget.
`music_prefetch_tracks` defaults to 2 (maximum 8; 0 disables). Playback start and
progress reports containing `NowPlayingQueue` warm the next tracks through the
same user-scoped stream resolver. Prefetch is bounded to two concurrent jobs and
repeated reports are deduplicated for 60 seconds. Clients omitting their queue
cannot benefit from lookahead.

## Fork

The fork starts at upstream `e42a525d700d6dc343f316447803138b8ea2fbe3`.
`deploy/slskd/acquisition-concurrency.patch` preserves the complete changes and tests;
`deploy/slskd/build.sh` runs the React UI tests/build and the focused .NET tests, then
publishes the executable with its matching `wwwroot` assets. It requires Node/npm and
the .NET 10 SDK, and keeps build files on disk rather than `/tmp`. Its opt-in settings are:

```text
SLSKD_API_DOWNLOAD_CONCURRENCY=16
SLSKD_API_SEARCH_CONCURRENCY=8
SLSKD_API_CONCURRENCY_WAIT_MS=2000
```

Unset values retain upstream limits of 2 download operations, 1 search operation,
and immediate rejection when busy. Waiting API callers honor request cancellation.
Increasing these limits alone does not establish a playback latency guarantee.

## Verification

`examples/slskd_proof.rs` creates a separate Remux database, registers only the
slskd addon, and tests actual HTTP playback endpoints. Supply a private JSON file:

```json
{
  "data_dir": "/path/to/isolated-proof-data",
  "output": "/path/to/results.json",
  "addon": {
    "url": "http://127.0.0.1:5929",
    "api_key": "YOUR-KEY",
    "download_dir": "/path/to/slskd/completed",
    "acquire_timeout_secs": 60,
    "max_peers": 4
  },
  "tracks": [{"title": "Nude", "artist": "Radiohead", "duration": 255}]
}
```

Run `cargo run -p remux-server --example slskd_proof -- /path/to/private-proof.json`.
Use a separate slskd test account: signing in with the production account would
disconnect its existing session. The proof reports initial and five repeat times
for `PlaybackInfo` plus a 64 KiB range request, and fully downloads and decodes each
successful track. Failed acquisitions remain in the report. A fresh data directory
is required for cold measurements; reruns measure persisted reuse. These are
server-side startup timings, not measurements of a phone's speaker output.

Filename/artist matching and duration checks are heuristics, not acoustic
fingerprinting. They cannot establish recording identity with certainty. Cold
acquisition still includes remote-peer and full-transfer latency. Sub-500 ms
average playback depends on how often the requested music is already local or
successfully prefetched.
