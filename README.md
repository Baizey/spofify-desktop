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
- Play a whole collection or start from a particular occurrence. Use a track’s `…` menu to queue it, save it offline, or add it to a playlist. Open **Saved offline** to play downloads on this computer, whether connected or offline. A Saved queue stays on this device. When this desktop plays an account queue, it uses a verified download before requesting the audio from the server.
- Select the current track’s artwork or title to open **Now playing**. Queue and output controls stay at the right of the mini-player. The selected output is shared with other connected listeners on your account. The desktop can control another output or play through its own default audio device. Linux desktop environments can use MPRIS media keys for play/pause, next and previous.
- **Work offline** is a Settings choice, not a playlist action. Switch to it when you want to browse and listen without server traffic. Going offline does not create an online queue or upload local button presses later. Saved music survives restarting the application. A fresh launch still requires a connection to validate your remembered session or sign in; saved files alone cannot unlock an account.
- Enable **Crossfade** in Settings to blend up to five seconds at natural track endings on this desktop. It is off by default and stored in `playback.json` alongside `appearance.json`. It does not affect another selected output or change the shared queue. If the next track cannot be prepared in time or either track is too short, playback advances normally.
- **Settings → Music storage** shows the current account’s downloaded audio on this device. Desktop has no persistent automatic music cache. **Settings** also separates this computer’s theme and density from the account palette. The local appearance file is `$XDG_CONFIG_HOME/spofify-desktop/appearance.json` (default `~/.config`). Private saved audio lives under `$XDG_DATA_HOME/spofify-downloads-v1` (default `~/.local/share`). Total downloads are limited by available disk space, with a 32 MiB limit per track.

This build targets the account WebSocket introduced in Go `08bb99e`. It does not negotiate the retired playback protocol or fall back to personal-library HTTP endpoints. See [shared state](docs/shared-state.md) for the transport boundary and the [protocol review](docs/account-protocol-review.md) for known server limitations.

## Install in the desktop menu

```sh
make build
make install PREFIX="$HOME/.local"
```

Ensure `$HOME/.local/bin` is on the desktop session's `PATH`. To remove the binary and launcher, use `make uninstall PREFIX="$HOME/.local"`. This does not delete personal data. Distributors can stage the same files with `DESTDIR`; `BINARY` can point to an already-built executable. No root access or user service is required for a per-user install.

## Install a GitHub release

On Linux x86-64 (Ubuntu 24.04 or newer, or an equivalent desktop with glibc 2.39+, ALSA and fontconfig), download the installer archive from [official GitHub Releases](https://github.com/Baizey/spofify-desktop/releases), extract it, and run `bash install.sh`. Then open **Spofify** from the app menu. No Rust installation is needed. If you used `make install` before, use the app menu rather than an older `spofify-desktop` on your shell `PATH`.

When an update is available, select **Update** at the top of the app. When it’s ready, quit and reopen Spofify to run the new version. To update manually, close Spofify and rerun `install.sh` from the latest official release. Either way, your settings, sign-in, and saved music stay in place.

See [desktop releases](docs/desktop-release.md) for how updates are verified or how to publish a package.

## Development

```sh
cargo fmt --all -- --check
cargo test --locked
cargo build --locked
```

With a provisioned Cargo cache you can add `--offline` to the build/test commands. If native libraries were installed with Linuxbrew rather than the system package manager, expose them to the build with `export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"`.

The default build does not enable Slint's inspection server. For development with **disposable accounts only**, build with `SLINT_EMIT_DEBUG_INFO=1 cargo build --locked --features slint/mcp`, then launch that **debug binary** with `SLINT_MCP_PORT` on an unused unprivileged loopback port. Never expose an inspection session containing real credentials. `scripts/inspect-native.py` captures only the opt-in Slint app window and accessibility tree.

For a disposable acceptance run, see the options in `python3 scripts/acceptance/run.py --help`. Point `--desktop` at that exact MCP-enabled debug binary. With `--desktop`, the default harness builds the Go checkout’s test fixture and drives a private Cage Wayland session, using `wlr-randr` to exercise both wide and compact windows. Linux workers with a mediated filesystem can use `--scratch-dir /dev/shm` for the disposable fixture, including the Go linker and SQLite. `--synthetic-wire-ui` instead exercises native rendering and navigation against a synthetic search and account-socket fixture; it is **not** a real-Go playback test. `scripts/acceptance/audio_smoke.py` independently checks PCM output, pause/resume, natural completion, and lease loss against a synthetic account-state peer and private null sink.

## Credits

The waveform icon follows the Spofify mobile mark. [JetBrains Mono 2.304](https://github.com/JetBrains/JetBrainsMono/releases/tag/v2.304) is bundled unmodified under the [SIL Open Font License](assets/JetBrainsMono-OFL.txt).
