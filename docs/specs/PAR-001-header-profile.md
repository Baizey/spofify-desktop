# PAR-001 — Unified native Search header and real profile

**Status:** specified, unassigned; no implementation/native evidence. **Prerequisites:** none. Read the binding [delivery guide](README.md), workflow and common acceptance there.

## Useful delivery / seams

Put the existing working search in one coherent native header; top-right Spotify-like profile button opens real account details and Sign out. Preserve working destinations already landed; add no Home/Playlists placeholders. `ui/app.slint` currently separates branding/Sign out and the search field; `src/main.rs` callbacks and `engine.rs::publish/reset` project only signed-in state. `api.rs::Session` already has account ID/username/role. Reference G `internal/web/auth.go::authSessionSummary` and `static/{listen.html,shell.js}`; there is no self-profile edit API.

## Contract

- Search navigation focuses the native query field. Preserve query, results, debounce/cancellation and keyboard editing across opening/closing profile. Empty query stays a prompt, not an all-library fetch. A menu is not a second page/topbar.
- Display actual username and listener/admin role; immutable account ID may appear under account details. No fake avatar identity, email/password editing, saved credentials, Settings or admin user endpoints.
- Add bounded authenticated `GET /api/auth/session` revalidation, returning `{accountId,username,role,csrfToken}`. Keep CSRF private/in memory. Use existing login/logout routes and origin/session rules. Explicit profile refresh and normal session-expiry handling suffice; no new background platform policy.
- Late refresh cannot install another/old identity. Account change/401 fences audio and clears account-scoped requests, rows, drafts and profile; network failure keeps truthful unverified state, not invented logout. Sign out immediately revokes local output/work; attempt leave/logout once and distinguish local sign-out from unconfirmed server logout.
- **Remove the MVP dock reset-style Stop button/callback exposure. No Close player, Stop listening, dismiss-player or upper-right expand/now-playing popup.** Keep internal cancellation/stop and incoming `close`; Pause must still cancel eligible pending local playback rather than allow a late start. Do not delete safety behavior merely because its old button disappears. Queue Clear upcoming is not part of this unit.

## Focused checks / native acceptance

Automate actual account projection, wrong/late session generation, 401 versus transient failure, once-only logout failure, empty-query no request, query focus/restoration and no Stop/Close UI callback. Preserve trusted-LAN consent, secret clearing and source fences. In an owned native window exercise search typing/IME, profile open/dismiss with keyboard and pointer, long username/role, revalidation failure/retry and sign-out during delayed playback. Inspect native header/dock at both standard sizes and 800×600: one search/header, no hidden duplicate topbar or player-close affordance, reachable menu/focus and no credentials in captures. Receipt per guide; no new navigation or playback engine is owed by this unit.
