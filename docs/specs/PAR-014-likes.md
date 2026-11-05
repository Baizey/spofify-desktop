# PAR-014 — Likes and Liked songs

**Status:** specified, unassigned. **Prerequisites:** none; existing track rows/current track provide identities. Add a working Liked songs collection destination in the current shell; coordinate 001/navigation, not a mandatory shell dependency. Follow [delivery guide](README.md).

## Contract

G `internal/web/personal.go`, `static/personal-library.js::{createLikeButton,recordPlayback}`, `library-collections.js`; D typed personal API, account-scoped actor/results and reusable native track actions.

- `POST /api/me/likes/status {trackIds}` max100 is a CSRF-protected read-like mutation-method request. Batch only bounded visible/current IDs, cache in memory per account with explicit invalidation. Unknown is distinct from false; check unknown status before deciding a toggle, never guess.
- `PUT /api/me/likes {trackId,liked}` sends explicit desired state. No blind mutation retry after timeout/uncertain reply; read status explicitly to verify before another gesture. Coalesce overlapping visible reads, retain keyed focus and bounded per-track pending/error rather than blocking all rows.
- `GET /api/me/likes?limit=40&offset=…` creates a real paginated Liked songs view, newest-first, with empty/loading/retained-refresh/error/retry and missing-audio rows. Like/unlike works for listener/admin on own account; it is not a profile setting. Add shared actions to existing Search, bottom metadata and other already-landed track surfaces; later surfaces reuse them.
- Row Play uses actual current controller/local contract. If 004 exists, whole liked context uses `GET /api/me/likes/playback`, never displayed page; otherwise omit whole-context affordance and 004 wires it later. Add-to-existing/priority actions appear only if their actual units landed. No create shortcut, fake liked-count Home read, history clear, popup or Close player.

## Checks / native acceptance

Test unknown/false/true, status batch max100, interleaved toggles and stale rows, lost accepted PUT followed by explicit read verification, error/401/403/404, two-account isolation and cancellation. Liked paging/empty/missing audio and count/order update do not remount unaffected row controls.

Native fixture: Like from search and bottom, open Liked songs, page/unlike, verify cross-surface consistency, uncertain/error retry and account switch. Actual keyboard/pointer/focus and standard-size screenshots include unknown/pending states rather than false success. Existing playback survives updates. No native-history or device-parity gate for this complete personal feature.
