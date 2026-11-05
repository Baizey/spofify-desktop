# PAR-004 — Local context queues and real progression

**Status:** specified, unassigned. **Prerequisite:** 003 seekable local media. Follow [delivery guide](README.md). This is a complete local album-listening/queue delivery, not the whole device foundation.

## Scope / source seams

From an existing search track with a real album ID, offer an explicit working album-context Play action; add a bounded in-place Queue panel, Previous/Next, shuffle and repeat to the persistent bottom dock. Preserve single-track Play. D `api.rs::Item` must retain safe structured artist/album identity; extend `player.rs` full musical model, `audio.rs` queue execution and `engine.rs` async context work. Reference G `static/player.js`, `internal/{web/library_playback.go,store/library_playback.go,store/personal_playback.go}`.

## Contract

- Prepare complete bounded context, never the visible 20/40-row page: `GET /api/library/albums/{id}/playback`, `/artists/{id}/playback`, `/api/me/likes/playback`, `/api/me/playlists/{id}/playback` (manual). Preserve total/limit/truncated and explicit selected track/manual `entryId`. Artist own releases exclude appearances. Only expose context actions for already-landed browsing surfaces; this unit wires all those surfaces, and later surfaces use this landed path. Album action from existing search makes this independently demonstrable without 013.
- Maintain positive v1 state: queue≤1,000, stable positive keys, baseline order, position, context, structured metadata, seconds, shuffle/repeat, elapsed/duration, playing/waiting/loading/error/truncated/notice and current history occurrence. Duplicate track IDs remain distinct entries. Incoming prepared derived identities must not be erased; signed derived selection is 017. No selectionToken/path/media URL in published prepared state.
- Real EOF advances; repeat-one/all and shuffle preserve baseline restoration; Previous restarts after >3 seconds, otherwise selects prior/wrap according to mode. Fetch next audio asynchronously, publish actual loading/media/blocked/output state. Never credit a synthetic elapsed timer as output; no endless skip/retry loop on failed media. Retire invalidated media on queue/current/session/lease changes.
- In-place Queue shows current/upcoming and count/truncation; page/virtualize bounded rows. Exact-key jump/remove/move up/down/next and **Clear upcoming** are complete actions; clear keeps current, played history, context and player. Preserve focused occurrence/scroll through publications. No absolute remote drag promise.
- Bottom transport has no reset Stop/Close player/Stop listening/dismiss/expand popup. Canonical incoming `close` and safety shutdown still stop/fence; Pause interrupts eligible pending start. Receiver remains unadvertised; a full handoff may still be rejected until 005, explicitly not silently converted.
- Create history event/progress/reported identity and preserve it, but **no history POST is owed here** (015). Existing server Home reads do not wait on this.

## Checks / native acceptance

Use complete album/manual/likes/artist projection vectors with >one page, duplicate playlist entries, no-audio membership, cap/truncation and selected row beyond first page. Test EOF, paused seek/next, Previous threshold, repeat modes, shuffle restore, exact key operations, clear-vs-close, stale context/media replies and immutable prepared queue despite later playlist edits. Full snapshots round-trip without dropping supported identities; unsupported receiver prepares remain rejected, not truncated.

Native owned fixture: play an album from search, hear no claim beyond measured native output, observe actual multi-track progression, selected-row context start, queue edits/clear while paused/playing, corrupt-next/media retry, keyboard focus and large queue scrolling at both sizes. New context actions work on any already-landed manual/likes/details surfaces. No all-features merge is needed to land this local listening feature.
