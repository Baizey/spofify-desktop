# PAR-012 — Query-driven Artists, Albums and Songs

**Status:** specified, unassigned. **Prerequisites:** none; existing query/search rows suffice. Coordinate with 001, do not wait for shell/queue/playlists. Follow [delivery guide](README.md).

## Useful delivery / contract

Replace first-20-tracks-only results with independently useful bounded Artists→Albums→Songs groups and Include missing. D `api.rs::search/Item/WireItem`, engine request/result generations, `main.rs` search callbacks, native row model; G `static/{library-search,library}.js`, `internal/web/library.go::libraryQuery`.

- Nonempty query issues independent `GET /api/library?kind=artists|albums|tracks&scope=all&availability=in-library&q=…&limit=10&offset=…`. Include missing uses `availability=all`. Query≤200 characters; preserve debounce (350 ms native acceptable) with IME composition guard. No blank match-all, catalog preload, playlist search or provider calls.
- Each group has own total/page/loading/empty/error/retry and can settle before siblings. A stale query/filter/page/session result cannot paint; cancel obsolete work. Preserve query/paging/focus through deliberate navigation; clearing cancels and shows prompt or already-landed Home without an unrequested catalog read.
- Retain safe artist/album/track identities, structured credits (name-only manual artists have no fabricated links), availability and artwork identity, never private paths. Missing-audio items remain inspectable/available to later personal actions; Play disabled with explanation.
- Tracks keep the real existing Play action, routed through whichever controller is landed. Artist/album cards are honest result summaries until 013 adds working open/detail; no dead navigation or collection Play placeholders. This unit is a complete search improvement, not a DTO-only detail handoff. When 013 is present, reconcile its existing links. 004/007/personal actions are reused when landed, not mandatory to search.
- Keep one native search/header, stable keyed rows and keyboard access, no new browser topbar/popup or Close player. Native listener defaults apply even for admin; exclude tracked/manual admin diagnostics, jobs/downloads.

## Tests / native acceptance

Test independently delayed/failed groups, each page/retry/total, Include missing, max Unicode query/encoding, IME, stale same-query ABA, fast new query/clear/logout, strict bounded DTOs and no blank request. Keep security and existing latest-query/session-win regressions. Search-query text stays memory-only and out of logs/settings.

Native owned fixture: each entity group useful while another is held/fails, empty versus missing tracks, paging, filter changes mid-request, long credits, keyboard focus/editing and track Play continuity. Inspect standard and minimum-size group layouts, independent retry and no fake artist/album controls. Do not claim full entity browsing before 013 or native audio merely from a Play button.
