# PAR-015 — Record qualified actual native listening

**Status:** specified, unassigned. **Prerequisite:** 004 occurrence/progression; test 005 handoff when landed. **Not a prerequisite for 016 Home or playlist reads.** Follow [delivery guide](README.md).

## Contract / scope

Native listening contributes truthful recent history through existing `POST /api/me/history {eventId,trackId}`. G `static/player.js::observeProgress`, `personal-library.js::recordPlayback`, `internal/store/personal_history.go::PersonalRecordPlay`; D actual audio progress/current-occurrence model from 004 and bounded authenticated API tasks.

- Track current occurrence `{key,trackId,eventId,progress,reported}`. Count ≥1 second of **qualified actual local output progression**, excluding seek jumps, waiting/loading, paused time, imported elapsed, remote projection and wall-clock-only interpolation. Only authority-owning native receiver reports its actual progress. Unknown output progress is no listening credit.
- Attempt once per occurrence, best-effort and asynchronous; publish attempted/reported continuity consistently with browser semantics. Server deduplicates retained event IDs per account. Preserve event identity/progress/reported through supported handoff, repeated track entries have distinct events. No retry after uncertain response, passive restore, account reset or history clear; no falsely claiming a successful write on failure.
- Navigate freely without resetting current occurrence; new queue/current/close/auth/session changes fence pending work. This reporting never grants authority, starts media, blocks transport or replaces the queue. Respect exact-origin/CSRF and bounded safe errors; no telemetry, private-path logs, new history schema, persistent event backlog or extra clear-history UI.
- Home can already show browser/other-device history. Without 015, describe native-only recent population as missing; with it, verify actual server read after qualified output, not a local optimistic card.

## Checks / acceptance

Deterministic production progress tests: 0.99 versus≥1 second, paused/waiting, seek forward/back, repeated samples, same-track new occurrence, delayed response/lost accepted POST, no auto-retry, auth revocation and remote-only projection. Assert one event/attempt and no old-session publication. Handoff vectors preserve exact event/progress/reported with at-most-once client attempts across receivers; tolerate server best-effort retention honestly.

Owned real-Go/native: play actual output past threshold, `GET /api/me/recent?limit=8` contains track once; seek/pause/remote control add no false event; repeated true occurrence and browser↔native transfer preserve expected history. If Home landed, inspect its refreshed shelf using real server response; otherwise API verification plus native playback interaction is enough for this reporting feature. Record actual output measurement, fixture result and UI impact; no physical audibility claim or new popup/Close player.
