# PAR-016 — Bounded personal Home shelves

**Status:** specified, unassigned. **Prerequisite:** 009 real playlist list/metadata destination. No 015 native-history or device/queue prerequisite. Follow [delivery guide](README.md).

## Contract / reference

Add a real Home destination in the single native nav/search header (reuse 001 if landed). Modest browser parity: **Recently played + Your playlists**, not a dashboard/recommendations feed. G `static/library-collections.js::{home,loadHome,publishHome}`, `internal/store/personal_history.go::personalHistoryPage`; D personal account actor and navigation from 009.

- Deliberate Home entry authorizes independent bounded `GET /api/me/recent?limit=8` and `GET /api/me/playlists?limit=6&offset=0`. Recent dedupes by track ID/latest event; not albums or complete history. Playlist cards open 009 metadata (010 entries if landed), View all opens Playlists. Derived cards say Rules/not evaluated, never zero or quick Play before evaluated support.
- Existing history from browser/other devices is sufficient and useful. **Do not wait for native reporter 015.** Native-only listening may not yet populate recent; no fabricated local history card. Once 015 lands, server-qualified history refresh appears once. Do not issue browser's unused likes-count read unless a separately authorized real rendered feature needs it.
- Each shelf settles independently; first exposed ordering/height/focus remains stable during visit. Retain successful content on refresh failure; show per-shelf retry/empty/unavailable with safe notices. Local personal events invalidate affected shelf, coalesce one trailing read. No invented server personal push or catalog-revision-as-personal-revision guarantee; bounded explicit refresh suffices for external writes.
- Navigation/logout cancels and fences old publications; preserve query and prepared playback. Current-track recent Play works through existing real transport; missing audio has no fake Play. Reuse Like/Add/priority when landed, never placeholder actions. Whole manual-playlist quick Play only after 004, from prepared endpoint not shelf items; later unit owns integration.
- Empty Home links to real Search or Playlists. Do not copy browser Browse albums as a hidden blank-query all-catalog request. **No Create playlist on Home or in shared Add picker**, no automatic add after navigation, no recommendations, admin shortcuts or extra topbar. **No now-playing popup or Close player/Stop/dismiss affordance.**

## Checks / native acceptance

Test exact limits, independent delayed/failed shelves, known-empty vs null, retained content, first-use order stability, coalesced trailing refresh, external writes followed by explicit refresh, late route/account results and no blank catalog/unused likes request. Creation callback cannot be reached from Home/picker.

Seed owned server recent history **before native reporting exists**, open Home and use recent Play plus playlist navigation. Hold/fail each shelf while the other stays interactive, empty/retry, derived cards, long names and keyboard focus through refresh. Capture actual native Home/header/dock at standard and 800×600 sizes. If 015 landed, verify one qualified new native play appears through actual server read; otherwise explicitly record reporter not covered, without blocking this complete bounded-read feature.
