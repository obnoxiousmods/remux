# Gateway refusal and probe recovery

The configured probe fallback count applies to both resolution-restricted and
group-cascade selection. A resolution change does not reset the retry budget.

HTTP 429/5xx responses with `Retry-After` are returned immediately rather than
being retried after a shorter local backoff. Preserve `Retry-After`,
`Cache-Control`, and `X-ObnoxiousTV-Error` through the stream proxy.

A 503 explicitly tagged `X-ObnoxiousTV-Error: provider_budget_exhausted` establishes
a bounded, in-memory origin cooldown from its numeric Retry-After (up to one
hour). Subsequent HTTP streams on that gateway receive the same structured
refusal locally; probes skip those sources. This is only valid for a global
gateway budget refusal. Ordinary title, user, or provider errors do not establish
an origin cooldown. Expired entries are removed and the map is bounded.

This does not enable ObnoxiousTV's bandwidth cap. The operator disabled that cap;
the protocol prevents retry cascades if a gateway explicitly refuses playback.

Local validation:

```sh
cargo test -p remux-server --lib gateway_backoff_tests
cargo test -p remux-server --lib playback::probe::probe_tests
```

The HTTP tests use loopback mock gateways and assert actual upstream request
counts. No provider bandwidth is consumed. Deploy production only through
`deploy/remux-canonical-deploy.sh` after committing the canonical checkout.
