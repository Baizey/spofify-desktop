# PAR-010 — Playlist occurrences and Add to existing playlist

**Status:** specified, unassigned. **Prerequisite:** 009. No device/full-queue dependency to deliver useful entry management. Follow [delivery guide](README.md).

## Contract / seams

Extend 009 detail with bounded manual occurrence pages and exact remove; provide shared Add-to-existing-manual from existing search rows and bottom current track (and other already-landed row surfaces). G `personal.go`, `personal_playlists.go`, `static/personal-library.js::{openAdd,removeEntry,conflictOrFailure}`; D shared typed personal API/actor/UI from 009.

- `GET /api/me/playlists/{id}/entries?limit=40&offset=…` yields `{id,position,track}`. Key row/focus by **entry ID**, not track ID. Duplicates are valid; missing-audio entries remain inspectable/addable but not falsely playable. Empty/deleted/unavailable and page errors are explicit.
- Picker pages `GET /api/me/playlists?kind=manual&limit=40&offset=…`, checks real `capabilities.addTracks`, no derived targets. Read target summary/revision before `POST /{id}/entries {trackId,expectedRevision}`. Add cap 1,000; no implicit track dedup.
- Confirm removal of this occurrence using `DELETE /{id}/entries/{entryId} {expectedRevision}`. Revision conflicts/unknown completion retain selection and demand explicit verification; no auto-retry, count/track-match inference or silent background unlock. Refresh summary/affected page on known success; prepared player queue remains immutable.
- **No Create in the shared picker, including Home/player entry points.** If no eligible target, offer navigation to Playlists, never inline creation or deferred automatic add. Focus returns to invoking row/control on picker dismissal.
- Until 004, ordinary row Play uses the landed single-track transport and makes no whole-playlist/entry-context claim. Once 004 exists, wire exact selected `entryId`/manual context to whole prepared playback, never visible-page replacement; 007 menus only when supported/landed. Whichever unit lands later owns that integration.
- No saved-playlist reorder (011), rules entries mutation, queue engine or popup/Close player. Bottom Save is an ordinary metadata action, not a second player surface.

## Tests / native acceptance

Test >40 entries and >40 targets, two equal track IDs with distinct entries, exact delete, target capability/null/derived, cap, concurrent revision change, lost add/delete responses and explicit verification, stale account/picker lifetime, no inline-create callback. Include correct page-two selected entry when full context path is available.

Native synthetic fixture: browse duplicate occurrences, add from search and bottom current track to an existing playlist, remove the intended duplicate, empty picker navigation, missing audio, error/retry/conflict and lost-response guidance. Inspect keyboard opening/selection/dismiss/focus, page transitions and long names at standard sizes. Verify Home entry-point picker cannot create and playlist edits do not mutate playing prepared queue. Record useful management completion independent of remote playback.
