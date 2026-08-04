# Unified home recommendations

`GET /remux/home/recommendations` is a Remux extension for clients that render a
ranked home feed. Jellyfin's `/movies/recommendations` and
`/shows/recommendations` contracts remain unchanged.

Accepted query parameters use Jellyfin-style names:

- `UserId` (optional; defaults to the authenticated user)
- `RowLimit` (1-12; default 12)
- `ItemLimit` (5-12; default 12)
- `Seed` (unsigned integer used for deterministic daily variation)
- `IncludeMovies` and `IncludeSeries` (booleans; both default true)

The response contains `AlgorithmVersion`, `FeedId`, `Seed`, `ProfileMode`,
`Confidence`, and ranked `Rows`. A row includes its stable category ID, title,
recommendation type, generic/personalized classification, media kind, optional
baseline, rank, and Jellyfin `BaseItemDto` items.

Profile modes are `cold_start`, `blended`, and `personalized`. Cold-start feeds
contain only popular, recent, and tag-balanced catalog rows. Blended feeds
progressively introduce existing personalized recommendation categories;
personalized feeds retain two generic exploration rows. Feed ordering is stable
for the same user, seed, and algorithm version.

Recommendation telemetry accepts the optional `feedId`, `algorithmVersion`,
`profileMode`, and `personalization` fields. These observations are not ranking
inputs.
