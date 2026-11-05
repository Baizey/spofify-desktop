# Desktop listening parity — delivery guide

**Specs, not implemented features.** All units below are unassigned, ready for scoped lead assignment subject to their landed prerequisites. This documentation delivery runs no builds or apps and establishes no feature/native acceptance. Each future lead owns its own spec, branch/worktree, tests, one independent review, local integration and cleanup under [roles](../../AGENT_BEHAVIOUR.md) and [workflow](../worktree-review.md). No final campaign/megamerge holds finished units hostage.

## TL;DR / start here

Assign **[PAR-001 unified header/profile](PAR-001-header-profile.md)** and **[PAR-009 manual playlist browse/create](PAR-009-manual-playlists.md)** first as independently useful UI deliveries; coordinate shared `ui/app.slint` / engine changes, not a combined branch. For device parity, start **[PAR-002 selected-output transport](PAR-002-selected-output.md)** and **[PAR-003 seekable single-track media](PAR-003-seekable-media.md)** as separate complete controller/media improvements. **[PAR-012 grouped search](PAR-012-grouped-search.md)** is also independently assignable. Do not assign all units concurrently or make library work wait for a device megaspec.

| Spec | Complete deliverable | Genuine landed prerequisites |
| --- | --- | --- |
| [001](PAR-001-header-profile.md) | Unified Search header, top-right real account menu; remove reset-style dock Stop | None |
| [002](PAR-002-selected-output.md) | Control an already selected output; authoritative bottom transport | None |
| [003](PAR-003-seekable-media.md) | Bounded seekable/incremental local single-track playback, seek and local gain | None |
| [004](PAR-004-local-context-queue.md) | Local album-context queue, progression and in-place queue editing | 003 |
| [005](PAR-005-native-handoff.md) | Receive and return complete browser queues with asynchronous media activation | 002, 004 |
| [006](PAR-006-device-panel.md) | Idle/active dock Devices, rename and guarded explicit selection | 002; **not** 005 (restricted native receiving remains honest until 005) |
| [007](PAR-007-priority-controller.md) | Negotiated Low/High/Next/Now against selected capable outputs | 002; **not** 004/005/008 |
| [008](PAR-008-priority-receiver.md) | Complete native priority receiver, then capability advertisement | 005, 007 |
| [009](PAR-009-manual-playlists.md) | Browse/create/manual metadata/delete with real Playlists route | None |
| [010](PAR-010-playlist-entries.md) | Exact playlist occurrences, add-to-existing/remove | 009 |
| [011](PAR-011-playlist-reorder.md) | Revision-safe cross-page saved-playlist move up/down | 010 |
| [012](PAR-012-grouped-search.md) | Independently paged Artists/Albums/Songs, Include missing | None |
| [013](PAR-013-entity-details.md) | Artist/album/track navigation, contents and bounded artwork | 012 |
| [014](PAR-014-likes.md) | Likes and Liked songs destination/shared track actions | None (existing track rows suffice) |
| [015](PAR-015-qualified-history.md) | Best-effort actual-playback history reporting | 004; handoff regressions with 005 when landed |
| [016](PAR-016-home.md) | Bounded independent recent/playlists Home shelves | 009; **not** 015 or device work |
| [017](PAR-017-derived-consumption.md) | Rules evaluation, signed occurrence playback, complete editable copy | 004, 010 |
| [018](PAR-018-rules-editor.md) | Rules create/edit/preview | 017 |

001 is not a central shell prerequisite: a new destination adds its working route in its own unit using the current native shell; whoever lands later integrates the already-landed routes. 004 supplies complete context playback from the existing search row's album identity, so it does not wait on entity-detail UI. Details, playlist entries, Likes and Home use whichever real playback contract has landed; hide unavailable collection/priority affordances rather than fake completion. The later playback unit attaches its newly supported action to already-landed relevant surfaces (and later surfaces reuse it). This is an explicit acceptance obligation, not an unfinished-interface handoff. Full context integration across manual/likes/album/artist is owned by 004; signed derived selection by 017. 007 similarly attaches its four actions as each selection kind becomes available.

Milestone **core listening** includes 001–016, including controller **and** receiver, not merely a picker. Manual-only playlist delivery is useful but not all playlist parity. 017–018 are a named **later rules-parity milestone**, real listener functionality, not misclassified administration. Root assigns implementation separately; these specs are not permission to launch all work.

## Authority, references and known starting coverage

Source inspected: desktop product baseline `9899a1a824f5633230291916ca8243a3eae9fa5c`; workflow-integrated spec base `8a9a4c4dfa1d4bebf15f6a96b92c7cb4e850150d` (product unchanged); current Go/browser `b6eb54ffdc91e07e2ce7824b8be5f90afa8b05eb`; mobile workflow/context `5b2422f645bc461af3e8419cd6af3399b33fc940`. These are source provenance, **not deployed capability guarantees**. Reverify the assigned implementation base/reference, negotiate actual player support, and show unsupported endpoints honestly. Browser is the product reference; mobile supplies current small-feature/review lessons, not Android product/platform policy.

Discovery reports `.tmp/parity/{REQUEST,DEVICE-AUDIT,LIBRARY-AUDIT}.md` were fully read; they are local discovery, not runtime evidence or required distributed docs. These specs supersede their proposed slicing/dependencies and older Stop advice. Concrete reference paths below use **G** = read-only `spofify-go`, **D** = this repository. Mobile examples read: `AGENT_BEHAVIOUR.md`, `docs/worktree-review.md`, `docs/specs/FEAT-112-device-refresh-jitter.md`, `FEAT-114-explicit-queue-default-device.md`.

D currently provides in-memory sign-in, 350 ms debounced first-20-track query search, guarded single-track local playback, sample-level lease/session fences and immediate local sign-out. It does not yet support full queues/contexts, remote control, device picker, priority intents, playlists, Home or entity pages. Relevant seams: `src/api.rs` (`Item`, `Session`, bounded exact-origin transport), `src/engine.rs` (`Command`, `ResultMessage`, `publish/reset/tick`), `src/player.rs` (`Endpoint`, `valid_snapshot`, authority/directives), `src/audio.rs` (preload/activate/Fenced), `src/main.rs` callbacks, `ui/{app,theme}.slint`. Shared files require coordination, not per-file ownership branches.

## Binding UX and scope for every spec

- One native navigation/search header, top-right Spotify-like **profile button/menu**, actual username/role/account identity and sign-out only. No separate browser topbar, fake account editing/admin APIs or decorative destinations.
- One persistent bottom player. **No now-playing popup, upper-right expand/up-arrow, Close player, Stop-listening, dismiss-player or disguised reset/Stop button.** Remove the MVP reset-style Stop when touching the dock; do not recreate it elsewhere as a parity control. Panel/menu dismissal is not player dismissal. Pause stays available to interrupt eligible loading. Internal stop/fencing on sign-out/window close/authority loss and canonical incoming `close` remains mandatory.
- Queue **Clear upcoming** keeps current/history/player; it is not MVP reset-style `clear`/Stop. Missing popup functionality belongs in bottom transport, metadata/actions, or in-place Queue/Devices panels: seek/times, previous/next, shuffle/repeat, local gain/mute, status/retry, context/credits, Like/Add. No second state owner.
- Preserve native palette/font, keyboard focus/labels, disabled-handler guards, scroll and stable keyed controls; see [visual vocabulary](../visual-alignment.md). No web technology replacement.
- Create playlist **only in Playlists**, never Home, player or shared Add-to-playlist picker. An empty picker may navigate to Playlists; it must not auto-create or later auto-add.
- Search stays query-driven; empty query never becomes match-all. Home now deliberately authorizes bounded personal reads. Existing server history is useful before native reports new events.
- No admin/import/acquisition/jobs/settings/concerts/new backend, remote volume, absolute remote drag API, credential/session/queue persistence or platform-policy changes. Existing HTTPS verification, explicit private-IP trusted-LAN consent, no public HTTP/HTTP hostnames, exact origin/CSRF/redirect refusal and in-memory secrets remain binding.
- Current browser rejects priority intents when no output is selected. **Default desktop does too.** Mobile's fresh unselected Queue High self-admission is an optional separately authorized follow-up after 008, not a prerequisite, permission or hidden alternate implementation in this plan.

## Common implementation acceptance and completion receipt

Each brief's tests below are mandatory focused cases in addition to applicable [testing](../testing.md), [native UI](../ui-validation.md), [storage](../storage-hygiene.md) and [review/integration](../worktree-review.md) procedures. Write meaningful regression/contract tests, reuse unchanged evidence, and exercise production paths rather than test-only success adapters. All personal listening routes work for listener/admin, remain per-account, and never gain admin permissions by role display.

Requests/results/metadata/media remain bounded, cancellable and session/origin/route-generation fenced. Mutation POST/PATCH/PUT/DELETE (including read-like POST) uses exact Origin + CSRF; opaque IDs are encoded once. No automatic mutation replay on timeout/conflict; distinguish initial loading, known-empty, retained refresh, forbidden/unavailable, not-found, auth loss and uncertain completion. Error text is bounded/safe, never raw server diagnostics or private paths. Known identity changes clear old account data; transient network failure alone is not revocation.

For each affected UI, use owned synthetic fixtures/accounts and actual native after-screenshots opened by implementer and the **same** read-only reviewer, with pointer/keyboard interaction at 900×640 and 1100×760 plus relevant minimum 800×600/large-text/long-Unicode cases. Capture empty/loading/error/retry/success and stable focus during refresh; preserve unrelated player state while navigating. Playback acceptance additionally measures real native output or actual remote receiver effects; screenshot/model “Playing” is not output proof or independently measured audibility. Source/mock tests never substitute for native acceptance.

Each feature lead records full base/candidate/reviewer/main SHAs, changed paths, exact commands/results, binary/fixture hashes, actual screenshot/interaction paths inspected, security/error cases and outstanding limitations; then preserves bounded source/review evidence and normally removes only its completed owned worktree/ref. If native is blocked, label **native acceptance blocked / feature not complete**, even if source-reviewed landing is permitted. No combined smoke substitutes for individual completion. This spec-only unit requires docs/link/diff checks, not Cargo/native launches.
