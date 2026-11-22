# Spofify Desktop

A native Linux music player for your [Spofify server](../spofify-go). Rust owns the session, music library and playback; Slint renders the desktop window and Rodio sends audio to the default output. The server and its music library run separately.

![Album detail and the playing mini-player, captured through Slint MCP against the Go test library](docs/images/native-album.png)

[Compact Now Playing screenshot](docs/images/native-player-compact.png)

## Run

Install stable Rust (at least the `rust-version` in `Cargo.toml`), a C/C++ toolchain, `pkg-config`, and development headers for fontconfig and ALSA. On Debian/Ubuntu the native packages include `build-essential pkg-config libfontconfig1-dev libasound2-dev`. For playback you need a working audio output and a Wayland or X11 desktop session.

```sh
cargo run --release --locked
```

Use the root URL of a Spofify server implementing `spofify.account-state.v1`, such as `https://music.example.com`, and sign in with an existing listener or admin account. The desktop client is for listening; server administration stays in the Go dashboard. Both `http://` and `https://` addresses work directly, including hostnames with a port. Use HTTPS on untrusted networks; HTTP sends credentials unencrypted. Do not enter a path, credentials, query, or fragment in the server field.

After a successful sign-in the app remembers the server and username. Select **Keep me signed in** to reopen the app without typing your password. This uses an unlocked Linux desktop keyring that provides the Secret Service interface and a server session, not a saved password. Sign out to stop automatic sign-in on this computer.

## Listening

- Start in **History**, or open your **Playlists** from the library navigation. Search the catalog with `Ctrl+F`. Results come from the server’s unified search index, in ranked order; the desktop doesn’t download a catalog to search it. Select an artist or album to open its page, then use Back to return to your results.
- Play a whole collection or start from a particular occurrence. Use a track’s `…` menu to queue it, save it offline, or add it to a playlist. Use a manual or rule playlist’s **Save offline** action to download it in the background. **Saved offline** shows progress and resumes pending downloads when you’re back online with the same account. Open **Saved offline** to play downloads on this computer, whether connected or offline. A Saved queue stays on this device. When this desktop plays an account queue, it uses a verified download before requesting the audio from the server.
- Use **Edit order** on the queue or an editable manual playlist. In the queue, press and drag a song in Up Next to its new position, then release to place it. In a playlist, hold the row briefly before dragging. With the keyboard, use ↑/↓ to select a row, Space or Enter to lift it, ↑/↓ or Home/End to choose its destination, and Space or Enter to place it. Escape cancels a lift. GNOME and KDE animation preferences are read at launch; `SPOFIFY_REDUCED_MOTION=1` also disables reorder animations.
- Select the current track’s artwork or title to open **Now playing**. Queue and output controls stay at the right of the mini-player. The selected output is shared with other connected listeners on your account. The desktop can control another output or play through its own default audio device. Linux desktop environments can use MPRIS media keys for play/pause, next and previous.
- **Work offline** switches the same browsing, Search, and playback controls to music available on this device. Known playlists open normally and keep their downloaded tracks in order, including repeats. Local queue changes are not uploaded later. Saved music survives restarting the application. If you signed in with **Keep me signed in** and left Desktop in **Work offline**, it can reopen offline with your unlocked keyring, without contacting the server. **Go online** validates the same account first. Otherwise, a fresh launch needs an online sign-in or session check; saved files alone cannot unlock an account.
- Enable **Crossfade** in Settings to blend up to five seconds at natural track endings on this desktop. It is off by default and stored in `playback.json` alongside `appearance.json`. It does not affect another selected output or change the shared queue. If the next track cannot be prepared in time or either track is too short, playback advances normally.
- **Settings → Music storage** shows the current account’s downloaded audio on this device. Desktop has no persistent automatic music cache. **Settings → Density** applies to this computer only. The local density file is `$XDG_CONFIG_HOME/spofify-desktop/appearance.json` (default `~/.config`). Private saved audio lives under `$XDG_DATA_HOME/spofify-downloads-v1` (default `~/.local/share`). Downloads use available disk space without a per-track size limit. Online playback uses temporary private disk space; it does not add tracks to Saved.

This build targets the account WebSocket introduced in Go `08bb99e`. It does not negotiate the retired playback protocol or fall back to personal-library HTTP endpoints. See [shared state](docs/shared-state.md) for the transport boundary and the [protocol review](docs/account-protocol-review.md) for known server limitations.

## Install in the desktop menu

```sh
make build
make install PREFIX="$HOME/.local"
```

Ensure `$HOME/.local/bin` is on the desktop session's `PATH`. To remove the binary and launcher, use `make uninstall PREFIX="$HOME/.local"`. This does not delete personal data. Distributors can stage the same files with `DESTDIR`; `BINARY` can point to an already-built executable. No root access or user service is required for a per-user install.

## Install a GitHub release

On Linux x86-64 (Ubuntu 24.04 or newer, or an equivalent desktop with glibc 2.39+, ALSA and fontconfig), download the installer archive from [official GitHub Releases](https://github.com/Baizey/spofify-desktop/releases), extract it, and run `bash install.sh`. Then open **Spofify** from the app menu. No Rust installation is needed. If you used `make install` before, use the app menu rather than an older `spofify-desktop` on your shell `PATH`.

Spofify checks for and installs verified updates in the background when it starts, even before sign-in. It does not check in **Work offline**. When the top notice says an update is ready, quit and reopen Spofify to use it. To update manually, close Spofify and rerun `install.sh` from the latest official release. Either way, your settings, sign-in, and saved music stay in place.

See [desktop releases](docs/desktop-release.md) for how updates are verified or how to publish a package.

## Development

```sh
cargo fmt --all -- --check
cargo test --locked
cargo build --locked
```

With a provisioned Cargo cache you can add `--offline` to the build/test commands. If native libraries were installed with Linuxbrew rather than the system package manager, expose them to the build with `export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"`.

The default build does not enable Slint's inspection server. For development with **disposable accounts only**, build with `SLINT_EMIT_DEBUG_INFO=1 cargo build --locked --features slint/mcp`, then launch that **debug binary** with `SLINT_MCP_PORT` on an unused unprivileged loopback port. Never expose an inspection session containing real credentials. `scripts/inspect-native.py` captures only the opt-in Slint app window and accessibility tree.

For a disposable acceptance run, see the options in `python3 scripts/acceptance/run.py --help`. Point `--desktop` at that exact MCP-enabled debug binary. With `--desktop`, the default harness builds the Go checkout’s test fixture and drives a private Cage Wayland session, using `wlr-randr` to exercise both wide and compact windows. Linux workers with a mediated filesystem can use `--scratch-dir /dev/shm` for the disposable fixture, including the Go linker and SQLite. `--synthetic-wire-ui` instead exercises native rendering and navigation against a synthetic search and account-socket fixture; it is **not** a real-Go playback test. `scripts/acceptance/audio_smoke.py` independently checks PCM output, pause/resume, natural completion, and lease loss against a synthetic account-state peer and private null sink. Run `python3 scripts/acceptance/delayed_local_play.py --desktop /path/to/mcp-enabled/spofify-desktop` to additionally measure first PCM before delayed Play resolution and output grant, verify continuous playback across confirmation, and check that Desktop stays silent or yields on authority corrections.

To verify offline relaunch with a private synthetic server and keyring, run `python3 -B scripts/acceptance/offline_session_relaunch.py --desktop /path/to/mcp-enabled/spofify-desktop --scratch-dir /dev/shm`. Add `--seeded-offline` to exercise only native cold admission and local sign-out without using the HTTP test peer.

To capture native reorder motion and exact move commands against a disposable wire peer, run `python3 -B scripts/acceptance/reorder.py target/debug/spofify-desktop` with the MCP-enabled build above. Without loopback, build `cargo build --locked --example reorder_preview` and run `target/debug/examples/reorder_preview scripts/acceptance/reorder-motion.events <new-frame-directory> soft` in a private Cage session on a 1280×720 output. Use `line` to compare the alternative cue or `reorder-edges.events` to exercise the long list and keyboard. The offline fixture drives the same native window with synthetic projections; it does not test the wire.

## Credits

The waveform icon follows the Spofify mobile mark. [JetBrains Mono 2.304](https://github.com/JetBrains/JetBrainsMono/releases/tag/v2.304) is bundled unmodified under the [SIL Open Font License](assets/JetBrainsMono-OFL.txt).
