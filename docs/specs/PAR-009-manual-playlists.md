# PAR-009 — Playlists browse, manual creation and metadata

**Status:** specified, unassigned. **Prerequisites:** none; add a real Playlists route in the current native shell, reuse 001 if landed. No device/queue/history dependency. Follow [delivery guide](README.md).

## Useful delivery / contract

List personal playlists, create a manual playlist, open metadata, edit name/description and confirm delete. This usable management feature deliberately excludes entries editing (010), reorder (011) and rules authoring (018). G `internal/web/personal.go::personalSummary`, `internal/store/personal_playlists.go`, `static/{library-collections,personal-library}.js`; D `api.rs`, account actor/results in `engine.rs`, native UI/main route callbacks.

- `GET /api/me/playlists?limit=40&offset=…` returns bounded pages newest-updated first. `GET /api/me/playlists/{id}` returns summary/revision/capabilities. Detail is real metadata, not fake empty entries; label membership editing as not part of this release until 010.
- Manual create `POST /api/me/playlists {name,description}` → 201; **do not send `kind:"manual"`** (supplied kind enters derived union). Name 1–100 Unicode code points, description≤500; validate controls/encoding and show server quota safely (100 playlists). `PATCH /{id} {name,description,expectedRevision}` → summary; `DELETE /{id} {expectedRevision}` →204 with confirmation: playlist only, not music.
- Listener/admin both own private account-scoped playlists; never send owner ID or expose others/admin management. Respect real `kind,countState,capabilities`; derived counts may be null/not evaluated, not zero. Derived summary can open truthful metadata; no entries/add/order calls or claim of rules parity. `SOURCE_REFERENCED` delete refusal is safe, actionable and not bypassed.
- Creation exists **only inside Playlists**. Home/profile/player/Add picker cannot create, including empty-state shortcuts. No auto-add after navigation/creation.
- Bounded initial/refresh/page loading, retained data, empty/error/retry/not-found/deleted states, stable focus and draft on conflicts. On 409 or uncertain create/update/delete, do not auto-resubmit; explicit verification/reload retains draft. Same-name match/count is not proof a create succeeded; make ambiguity visible. Account switch/sign-out cancels and clears drafts/results.

## Tests / native acceptance

Test exact 201/204 bodies, encoded IDs, Unicode limits, controls, quota, strict summary/capabilities/null counts, two-account isolation/both roles, cross-page list, revision conflict, referenced delete, delayed/lost create response, explicit reload and stale-session results. No POST retry on timeout or guessed duplicate reconciliation.

Native fixture: empty/list>40, create/edit/delete with keyboard and pointer, conflict with second synthetic session, uncertainty and retained draft, derived card not evaluated, deleted route/error retry. Inspect real route/header/profile coexistence at standard sizes and long titles; existing playback survives navigation. Confirm no creation control on Home or elsewhere and no now-playing/Close player UI. This unit completes management without waiting for queue parity; receipt must call it manual metadata/create coverage, not all playlists.
