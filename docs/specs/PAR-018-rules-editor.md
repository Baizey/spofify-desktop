# PAR-018 — Rules playlist create, edit and read-only preview

**Status:** specified, unassigned; **later rules-parity milestone** completing the explicitly deferred listener editor. **Prerequisite:** 017 evaluation/derived model. Follow [delivery guide](README.md).

## Contract / bounded scope

Implement the existing browser rules editor, not a new query language/backend. G `static/playlist-recipe.js::{create,createPreview,pipeline}`, `internal/web/derived.go`, existing store definition validators. Native form/state extends 009/017; only Playlists owns creation.

- `POST /api/me/playlists {name,description,kind:"derived",definition}` creates; `GET /{id}/definition` reads; `PUT /{id}/definition {definition,expectedRevision}` saves. Here `/{id}` is `/api/me/playlists/{id}`. Reuse metadata Unicode/error/revision policy; distinguish create/edit authority and real capabilities, listener/admin same private scope.
- Model up to four ordered sources: Liked songs, all saved music, own playlists; per-source first/last windows; include/exclude AND/OR/NOT conditions for genre/knownness, audio availability, artist, album and membership; duplicate retention/dedup; source/title ordering and final first/last window. Follow actual schema/bounds, no arbitrary expression execution or live provider search. Source pickers are bounded and account-scoped; source-reference/kind/cycle validation belongs to server, never bypassed.
- `POST /api/me/playlists/preview {definition,limit?,cursor?,targetPlaylistId?,expectedRevision?}` is read-only, CSRF-protected; target/revision appear together. Render bounded cursor results/count/notices, pending/empty/invalid/busy/timeout states. Preview does **not** save/select/queue/play. Editing draft invalidates prior preview and late preview response; explicit Preview has no hidden mutation.
- Preserve draft on validation/conflict/network failure. Unknown create/save outcome requires explicit verification, never automatic retry or same-name success inference. Saved definition revision invalidates old evaluation/selection; user explicitly refreshes via 017. Existing prepared playback remains unchanged.
- Create/control entry points only in Playlists. No Home/Add-picker creation, source-admin/import/settings/provider controls, new persistence, popup or Close player.

## Tests / native acceptance

Schema-vector tests cover all listed source/filter/order/window modes, four-source cap, nested conditions/invalid bounds, duplicate policy, unknown/deleted/referenced source, preview target/revision pair, cursor paging, stale draft/revision/session responses and no preview mutation/audio. Concurrent definition save/unknown accepted reply preserves draft and blocks blind resend; complete definition round-trip retains semantics.

Native owned fixtures: create a rules playlist in Playlists, assemble/reorder sources and conditions with keyboard, Preview then edit (old preview invalid), save and evaluate/play through 017, reopen/edit, conflict/timeout recovery without losing draft. Inspect long/Unicode labels, form scrolling/focus/disabled/error at standard and minimum sizes; no preview changes the playing queue. Receipt separates actual editor/evaluation/native acceptance from earlier manual-only scope. This closes later rules parity without reopening unrelated admin/backend work.
