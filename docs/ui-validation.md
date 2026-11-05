# Native desktop UI validation

**UI completion requires actual native-window screenshots, inspection and interaction for the affected feature.** Compilation, unit/mock tests, Slint element trees and no-pixel testing backends are not visual acceptance. Read [AGENTS](../AGENTS.md), [visual vocabulary](visual-alignment.md), [testing](testing.md) and [resource/storage ownership](worktree-review.md#build-and-native-resource-ownership).

## Isolate, reuse, capture

1. Use only a new owned native application/window and isolated synthetic fixture. Never inspect, capture, kill, replace or reuse the user's running window, retained binaries or real account/server. Record candidate/base, default versus MCP features, binary SHA-256, fixture source/hash, run owner, output paths and finite time/log/storage budgets before launch.
2. Build under BUILD, stop/await build writers and release BUILD. Acquire standalone NATIVE before native resources/ports; reuse the exact hashed binary with no implicit build. A build plus native run may stay in one invocation for ephemeral scratch, but never hold both resources concurrently. Check ports/ownership before launch; no device/volume/route changes or discovery of personal sessions.
3. For the existing embedded Slint MCP route, use an explicitly opt-in debug binary and an owned unprivileged loopback port (18471 is the documented example, not permission to use an occupied port). With a working provisioned Wayland display, a launch fragment is:

   ```sh
   SLINT_BACKEND=winit-software WINIT_UNIX_BACKEND=wayland \
     SLINT_MCP_PORT=18471 SPOFIFY_DEV_SIZE=900x640 "$OWNED_MCP_BINARY"
   ```

   This fragment alone does not supervise children or perform acceptance. `SPOFIFY_DEV_SIZE` is a debug-only app-window constraint when MCP is enabled; ordinary windows remain resizable. Use the installed display backend appropriate to the environment, not a no-pixel substitute or global reconfiguration. MCP remains localhost-only and absent from ordinary builds; release inspection opt-ins are rejected.
4. Exercise affected flows using actual pointer/keyboard interaction or discovered MCP tools/handles, never guessed handles. Within the same supervising invocation, a credential-free capture of that owned window can use:

   ```sh
   python3 scripts/inspect-native.py --port 18471 --output "$NEW_CAPTURE_DIRECTORY"
   ```

   The helper requires exactly one app window, complete inspection metadata and a **new** output directory. It refuses populated Username/Password inputs and removes accessible values from the retained tree. It saves `native.png`, `tree.json`, `window.json`; it does not launch/login/interact/shut down or run a journey. Do not capture or log credential-populated login trees/screens, private library paths or secrets even with synthetic testing.
5. Preserve captures outside ephemeral scratch, **open and actually inspect the PNGs**, then fix/re-capture regressions. The same independent peer inspects affected after-renders and interaction records; before-only images or passing MCP calls are not approval. Stop/await owned app/fixture/monitor children before releasing NATIVE and cleaning exact owned scratch; record primary and cleanup errors separately.

## Proportionate desktop journey

Exercise the changed feature and affected adjacent behavior, not every old campaign by habit. Use 900×640 and 1100×760 as established baseline sizes, plus relevant resize/larger text/long Unicode content where the change warrants it. Record actual viewport/scale/backend, not an unsupported cross-platform claim.

- Check real loading, empty, error/retry, disabled and successful states; retain useful content/focus during refresh. Inspect truncation, contrast, row/action geometry, bottom-player clearance and reachable last rows. Before/after comparisons use matching size/scale when a baseline exists; do not regenerate baselines just to approve regressions.
- Exercise keyboard Tab/Shift-Tab, visible focus, Space/Return activation, text editing, scrolling and affected menus/dialog dismissal. Preserve semantic labels and usable pointer targets; do not import phone-only Back/IME/thumb-navigation conventions.
- For authentication changes, preserve trusted-LAN HTTP confirmation and literal-origin restrictions, normal TLS, exact-origin/redirect refusal, in-memory credentials/session and clear sign-out. Verify changed error/retry paths without collecting secrets.
- For search/navigation/playlist/Home/profile work, test the assigned real capabilities and relevant loading/empty/partial/error states. Current parity requests authorize those features; the shipped search-only baseline is not a permanent exclusion. Preserve the native visual language: coherent header/search/navigation, top-right account menu, creation in Playlists, player capabilities in the bottom player rather than a duplicate now-playing popup.
- For playback/device changes, distinguish local native sink progress from remote selected-output state. Exercise relevant authority, queue/occurrence, cancellation, logout and recovery behavior with owned synthetic peers; browsing/refresh/connection recovery must not fabricate playback. A picker or enabled button alone proves neither interoperability nor transfer. Record exactly what the real fixture versus mocks established.

## Completion receipt

Record full source SHA, binary/fixture hashes, viewport/backend/scale, steps actually exercised, screenshot paths **actually inspected**, reviewer/findings/fixes, automated/native results and remaining limitations. Native Rodio/PipeWire progress is output evidence, not independently measured physical audibility; screenshots alone prove neither audio nor device handoff. No Windows/macOS or mobile acceptance is implied.

If display, fixture, interaction or screenshot inspection is blocked, report **UI validation blocked / not complete** with the concrete current reason. Historical native failures are not standing prohibitions, and healthy historical runtime is not current-candidate acceptance. A source-reviewed landing never converts missing native evidence into feature completion. Documentation-only changes do not need a launch or new screenshots.
