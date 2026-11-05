# PAR-011 — Safe cross-page saved-playlist moves

**Status:** specified, unassigned. **Prerequisite:** 010. Follow [delivery guide](README.md). This is saved membership ordering, not playback queue ordering.

## Contract

Add native Move up/down to manual entry rows, keyed by exact occurrence. On explicit move only, read all bounded entry pages (max1,000), establish consistent summary/revision, validate a complete entry-ID permutation and send **one** `PUT /api/me/playlists/{id}/order {entryIds:[complete order],expectedRevision}`. Reference G `store/personal_playlists.go::PersonalReorderPlaylist`, `static/personal-library.js::moveEntry`; extend D 010 API/actor/detail rows.

Never reorder just the visible page, collapse duplicate tracks, send one command per crossed row, mutate derived lists or change the prepared playback queue. If reads disagree with revision/membership, refuse with explicit refresh rather than guessing. Bounds at first/last entry are real disabled states with accessible labels.

Conflict, malformed/incomplete page and lost mutation reply leave a persistent verification-needed state. Keep the user's intended occurrence/focus; no automatic retry, rollback claim, background refresh unlocking or success based on count. Only explicit successful reload/verification permits a fresh move. Session/route change cancels pending work; an old move cannot apply to a new account or replacement playlist.

No drag API, queue moves, rules editing or creation location changes. No Close player/reset Stop/dismiss affordance or now-playing popup.

## Checks / native acceptance

Production tests: 81+ entries across pages, duplicate track IDs, moving entry40↔41, exact full permutation/expectedRevision, first/last boundaries, deleted entry between reads, revision conflict, incomplete/oversize pages, accepted PUT with lost reply, quiet refresh unable to clear recovery and explicit verified reload. Assert one mutation, no retries and unchanged prepared queue.

Native actual UI: keyboard/pointer cross-page Move, visible order and intended occurrence/focus retained, loading disables unsafe repeat, conflict/unknown completion→explicit verification→fresh action. Inspect list/error/disabled states at both standard sizes; no accidental create/add or popup. This incremental improvement lands independently after 010; no device/history gates.
