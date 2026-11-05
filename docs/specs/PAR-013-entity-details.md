# PAR-013 — Real entity navigation, contents and artwork

**Status:** specified, unassigned. **Prerequisite:** 012 typed grouped results. No queue/device prerequisite for useful browsing. Follow [delivery guide](README.md).

## Contract / API

Open real artist/album/track details from results and already-landed bottom metadata/credits; navigate bounded child contents and back to preserved query/page/focus. D API/route actor/native row seams from 012; G `static/library.js::{selection,loadSlot,renderHeading,trackActions}`, `internal/web/listening.go::listeningItemRoute`.

- `GET /api/library/{artists|albums|tracks}/{id}` provides safe positive details. Artist releases use `GET /api/library?kind=albums&artistId=…&scope=all&releaseGroup=albums|singles|appearances&limit=40&offset=…`; album tracks use `kind=tracks&albumId=…&scope=all&limit=40&offset=…`. Child browsing is not filtered by sidebar query/Include missing. Retain release/disc/track order, exact IDs and name-only nonlinked credits.
- Deliberate route state/back navigation prevents abandoned detail resurrection; bound history in memory, cancel late route/account work and preserve still-playing queue. Each pane/page has loading/empty/error/retry/not-found; partial parent/children remain useful. No invented album/artist destination for missing ID.
- Implement bounded exact-origin artwork through the existing entity `.../{id}/artwork` route after checking `listening.go` contract at assigned base. Decode with finite byte/pixel limits and cancellation, deterministic existing placeholder on missing/invalid content; no external image URLs or persistent cache. Reuse the same artwork/credits on bottom current track without a popup. Keep paths/private metadata out of UI.
- Ordinary track Play works through existing selected-output contract. If 004 landed, attach full artist/album Play/Shuffle and selected album-track context through playback endpoints, never visible pages; otherwise omit those collection controls honestly. 004 integrates these surfaces if it lands later. 007/010/014 provide shared priority/Add/Like when available; no unrelated prerequisite.
- No admin provenance/files/jobs/import/download controls, fake discovery/recommendations, separate topbar or now-playing/Close player button.

## Tests / native acceptance

Test encoded IDs, child query independence, release groups, paging beyond40, name-only credits, missing/deleted entity, old route response after Back/account switch, bounded history/artwork bytes/pixels/redirects, safe placeholders and no private paths. When 004 exists, prove full context and selected row beyond first page, not visible-page replacement; otherwise verify absent unsupported controls.

Native actual navigation from each group through credits/release tabs/album tracks and Back restores query/page/focus; long Unicode text, empty/missing/error child and artwork failure are readable at both sizes. Bottom metadata opens real destination without stopping playback or spawning a second player. Capture and inspect actual artwork/placeholder/keyboard behavior. Browsing can complete independently; call out collection transport coverage honestly in receipt.
