# PAR-017 — Consume rules playlists and copy complete results

**Status:** specified, unassigned; **later rules-parity milestone**, real listener feature, not admin. **Prerequisites:** 004 full context queue and 010 playlist occurrence UI. Follow [delivery guide](README.md). Rules authoring stays 018.

## Contract

Reference G `internal/web/derived.go::{derivedHTTP,derivedEntriesHTTP}`, `static/{playlist-recipe,library-collections,personal-library,player}.js`. Extend native typed personal/selection/queue seams; do not call manual entries/order endpoints for derived membership.

- `GET /api/me/playlists/{id}/evaluation?limit=…&cursor=…` gives bounded cursor pages, complete-result/count state, revision/fence, target/occurrence identity and selectionToken. Use real capabilities (`editEntries/addTracks:false`, copy-to-manual support); unknown/not evaluated counts are null, not zero. Loading/evaluation-busy/empty/timeout/stale errors are distinct and bounded.
- Explicit Refresh reevaluates stale saved results; ordinary catalog/personal changes invalidate signed actions and visible evaluation readiness, **not the already prepared queue**. Do not silently refresh a held selection/token to newer content. Bound page/cursor memory and cancel account/route work.
- `POST /api/me/playlists/{id}/playback {expectedRevision,fence,selectionToken?}` prepares bounded complete playback. Row Play preserves exact target/occurrence/token through preparation; no fallback to bare track/album or first duplicate. Validate response revision/fence/start occurrence/limit/notices. Prepared published state never contains selectionToken. CSRF required despite read-like preparation.
- Use existing selected-output routing and full queue. If 007 landed, expose its four intents with exact signed selection; otherwise ordinary supported Play is sufficient. Stale/expired/tampered signed authority requires explicit refresh/new gesture, never retry/rebind. No acquisition or backend schema changes.
- `POST /api/me/playlists/{id}/copy {name,description,expectedRevision,fence}` makes a complete editable manual copy (≤1,000), preserving duplicates and unavailable tracks, not just shown page. Treat copy as **creation: expose only inside Playlists**, never Home/player/Add picker. Use 009 name/description rules, conflict/unknown-result verification, and no automatic replay. Successful copy opens real manual destination.
- No direct entries editing/reorder of derived results, rules editor, provider calls, popup or Close player. Explicitly report rules consumption complete while authoring remains 018.

## Tests / native acceptance

Test multiple cursor pages, null/not-evaluated counts, stale catalog/personal fence, expired/tampered selection, exact same-track duplicate occurrence, revision conflict, busy/timeout/kind errors and no fallback selection. Full copy includes undisplayed/unavailable occurrences or fails atomically at cap; lost accepted copy cannot be retried blindly. Verify prepared queue stable after later reevaluation and tokens absent from snapshots/logs/settings.

Owned synthetic fixture: inspect evaluated derived list, Refresh stale state, play an exact duplicate from later page with actual native progression/selected-output effect, copy full result and inspect manual IDs/count/order. Keyboard/focus/loading/error native captures in Playlists at both sizes, no creation on Home. This separate complete consumption feature lands before authoring, without waiting for a final rules megamerge.
