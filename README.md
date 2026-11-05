# Spofify Desktop — native search + local playback

Rust + Slint, Linux first. Connect to an existing Spofify Go server, explicitly sign in, search for a track, and play it on this desktop. **Search-first replaces the initial album-browsing/read-only bootstrap brief.** No empty-query catalog fetch, album grid, catalog preload, web view, remembered credentials, playlists, offline cache or remote-output takeover.

## Current status

**Linux-first MVP:** sign in to an existing server, search, and play a track locally with Pause, Resume, Stop and Sign out. Native Wayland screenshots, Slint MCP interaction and actual Rodio/PipeWire output progress have been verified against an owned synthetic fixture built from the existing Go server. This is not a web view or a no-pixel testing backend.

Validation includes guarded used-room admission, retained paused-queue recovery, expired-device recovery, delayed media cancellation and authentication loss. Output progress was measured from the native sink and player snapshots; physical audibility was not independently measured. See [validation notes](docs/validation.md). Windows/macOS are not supported targets, and release packaging is not provided.

## Build and run

Requires Rust >=1.92, a working Linux Wayland/X11 display, audio output, and native development libraries for fontconfig/ALSA (plus the normal C toolchain). Dependencies are pinned in `Cargo.lock`.

```sh
cargo build --locked -j 2
cargo run --locked -j 2
cargo test --locked -j 2
```

On a Linuxbrew installation where pkg-config does not see already-installed libraries:

```sh
export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
```

Do not install global packages merely to bypass a sandbox display/audio limitation. The application uses the system's normal default audio output. It never launches a browser or external media player.

Enter an **origin**, e.g. `https://music.example.com` (no path, query, credentials or fragment). TLS verification is always on. Trusted-LAN HTTP requires a separate visible confirmation and a concrete RFC1918, loopback, Tailscale `100.64.0.0/10`, or IPv6 ULA/loopback literal. Public HTTP and HTTP hostnames are rejected. HTTP credentials travel unencrypted; use only a trusted network.

## Search and playback boundaries

- Search debounces for 350ms, cancels previous work, requests at most 20 tracks, and discards stale session/query results. Empty search makes no catalog request. Refine the query instead of loading more pages.
- Sign-in/session cookies/CSRF tokens remain in memory. Redirects, ambient HTTP proxies and automatic retries are disabled. Requests, response sizes, metadata and media bytes are bounded. Server error bodies and private library paths are never displayed or logged.
- Media is fetched only by an encoded track ID from the exact authenticated origin, at most **32 MiB**, in memory; this initial slice buffers one bounded original-format file before decoding. It is not a streaming/offline cache and can reject larger tracks. Supported formats follow Rodio's MP3/FLAC/Vorbis/WAV/AAC/MP4 decoder features. Unsupported files fail honestly.
- The desktop registers a local playback device lazily on Play, polls the canonical player protocol and makes only a **guarded self-claim when no output is selected**. It does not seize another output. Exact-epoch acknowledgments, conservative lease deadlines and source-level session/lease fencing precede decoded output. Local Pause/Resume/Stop publish actual state. No remote picker, handoff queue, shuffle/repeat, history mutation or seek UI is implemented.
- Sign out/server changes/window close immediately fence the output source and cancel stale work. Device leave and logout are attempted once; local clearing does not depend on a successful server response. Normal shutdown awaits worker/logout completion. Audio already submitted to the hardware is subject to the output device's finite buffer latency.
- Protocol conflicts stop output and use an observational player GET before normal polling; they never replay the failed write or automatically restart audio. A definitively expired device registration is discarded; select a track again to register afresh. Recovery of supported retained single-track state is paused and metadata-only until explicit Play.
- The MVP has no seek or volume UI. It uses the normal default output and does not change system volume/routes. Large tracks can take time to download fully; the 32 MiB cap is a deliberate initial limitation, not a claim of instant playback or production large-catalog performance.

## Opt-in native inspection

Slint **1.18.1's published embedded MCP** is used, not an application-specific mock bridge:

```sh
SLINT_EMIT_DEBUG_INFO=1 cargo build --locked -j 2 --features slint/mcp
SLINT_MCP_PORT=18471 cargo run --locked -j 2 --features slint/mcp
```

The upstream server binds **127.0.0.1** and exposes `/mcp`. No inspection feature is enabled in the default dependency features. Release builds refuse inspection environment opt-ins before creating a window. Never inspect a session containing real credentials: MCP can inspect/control the UI. Use only disposable accounts; do not retain login trees or screenshots while credentials are entered.

```sh
curl --fail -s http://127.0.0.1:18471/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

For repeatable development-only layout checks, `SPOFIFY_DEV_SIZE=900x640` (or `1100x760`) constrains the app's own window only when `SLINT_MCP_PORT` is also set in a debug build. Ordinary launches remain resizable. `scripts/inspect-native.py --port 18471 --output /absolute/new/capture-directory` saves only this app's native window/tree and refuses populated login inputs.

Published tools include `list_windows`, `get_window_properties`, `get_element_tree`, `take_screenshot`, `click_element`, `set_element_value`, and `dispatch_key_event`. Obtain real window/element handles from discovery; do not guess handles. A Slint testing backend without pixels is **not** native visual evidence.

## Bundled font

Unmodified JetBrains Mono v2.304 variable TTF, copyright 2020 The JetBrains Mono Project Authors, under the complete SIL OFL 1.1 in `assets/JetBrainsMono-OFL.txt`. Copied byte-for-byte from the existing Spofify server's verified asset, originally from upstream commit `cd5227bd1f61dff3bbd6c814ceaf7ffd95e947d9`:

- [Font source](https://raw.githubusercontent.com/JetBrains/JetBrainsMono/cd5227bd1f61dff3bbd6c814ceaf7ffd95e947d9/fonts/variable/JetBrainsMono%5Bwght%5D.ttf), SHA-256 `662a196d58f1183bf2d77428b6d5283fe3f45161ab021bea4036bc98e5cac016` (303,144 bytes).
- [License source](https://raw.githubusercontent.com/JetBrains/JetBrainsMono/cd5227bd1f61dff3bbd6c814ceaf7ffd95e947d9/OFL.txt), SHA-256 `30f0c136e3c88e422d0791acd97238870f9054a9729bc34cf2ff0d4ed8cac4ad`.

No artwork or fonts are fetched from the network at runtime. Placeholder art is intentional.
