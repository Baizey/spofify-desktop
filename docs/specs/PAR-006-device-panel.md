# PAR-006 — Devices in the idle and active bottom dock

**Status:** specified, unassigned. **Prerequisite:** 002. 005 full native receiving is not required to deliver selection among supported outputs; never label the restricted native target fully compatible before 005. Follow [delivery guide](README.md).

## Useful delivery / contract

One reachable Devices control in the persistent bottom dock, even idle, opens an in-place native panel with discovery, rename, explicit selection and retry. No header/Profile duplicate or expanded-player window. Reference G `static/{playback-devices,playback-sync}.js`, `playback.go::guardedTransfer/startTransfer`; D controller model from 002 and UI/main callbacks.

- Render session-bound exact device IDs, name, This desktop, availability, active vs selected/selecting destination and Playing/Paused/Unavailable/needs-local-retry. Duplicate labels receive distinct accessible identity; never dedup/delete by name. Opening/dismissing/rendering the panel does not register/select/prepare/play (actor lifecycle belongs to 002).
- `POST /api/me/player` `rename{deviceId,name}` supports same-account devices; trim and enforce 1–64 Unicode characters/noncontrol plus server byte bounds. Preserve keyed draft/cursor/focus across polling; server error/uncertain save is not success or automatic retry. No friendly-name persistence policy.
- Explicit `transfer{deviceId,targetDeviceId,epoch,commandId,selectionSeq}` uses increasing issuer selection sequence. This is distinct from `requireUnselected:true` self admission. Serialize one in flight plus newest unsent choice; allow supersession/reselection during switching/quarantine without early audio grant. A selected row or HTTP 200 is not terminal activation success.
- Honest connecting/switching/quarantine/drain/error states and remaining wait; unavailable targets disabled with handler guards; failed prepare leaves healthy source active. Connection retry remains usable during uncertain music outcomes, with no replay. If 005 absent, unsupported native prepare rejects clearly rather than discarding queue; selection between capable browser endpoints still works.
- Retained populated **and known-empty** views stay geometrically stable during routine verification: no heartbeat loading-strip flash, scroll/rename reset or repeated busy announcement. Null first load, genuine recovery and failures remain visible/accessibly actionable. This stability belongs in this unit, not a mandatory separate polish dependency.
- **No Close player/Stop listening/reset Stop/dismiss-player button or expand popup.** Dismissing Devices only closes that panel; internal stop/close/fences remain. No remote gain control or crash-proof durable broker identity promise.

## Checks / native acceptance

Production controller tests cover exact-ID rename/Unicode limits, stale save, retained empty versus null, refresh geometry/focus, rapid A→B→C, selected≠active, quarantine reselection, unavailable/expired devices, unknown registration and independent retry. Verify selection never uses unguarded self-claim fallback.

Owned synthetic account with browser peers and native: select A/B/C rapidly, rename identical labels, lose source/target and recover paused, show unsupported native-target failure before 005 or exact receiving after it. Assert actual terminal receiver/output effects and only one authority. Inspect idle/local/remote/loading/error panels and keyboard/pointer selection, rename/save/cancel, scroll over many refreshes at both native sizes. Device discovery itself must produce no audio/media GET or selected-output change.
