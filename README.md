# Spofify Desktop

Spofify Desktop plays music from a running, self-hosted Spofify Server. Ask whoever hosts it for a server address this computer can reach and a Spofify account, not a Spotify login.

## Install

The prebuilt release is for **Linux x86-64**. The installer is built and smoke-tested on Ubuntu 24.04 (glibc 2.39 baseline). Use a Wayland or X11 desktop with ALSA, fontconfig, and working audio.

1. Download `spofify-desktop-VERSION-linux-x86_64.tar.gz` from [GitHub Releases](https://github.com/Baizey/spofify-desktop/releases). The asset named `spofify-desktop-linux-x86_64` is the bare executable used by the updater, not the installer.
2. Extract the archive and run `bash install.sh` inside the extracted folder. It installs for your user; no root access or Rust is needed.
3. Open **Spofify** from your app menu.

While online, release installs check for updates even before sign-in. The app checks the new executable against GitHub's SHA-256 checksum and installs it in the background. When an update is ready, quit and reopen Spofify. To update manually, close the app and run the installer from a newer release. See [Desktop releases](docs/desktop-release.md) for what the updater trusts.

## Sign in

Enter the **Server address**, such as `https://music.example.com`, and sign in with your Spofify username and password. Include the scheme and any non-default port, but no path. Prefer HTTPS with a publicly trusted certificate; HTTP sends credentials unencrypted.

Online playback and search require a working [playback sync v3](docs/shared-state.md) connection. If you use a reverse proxy, it must preserve the configured Host and pass WebSocket upgrades. Online search also needs a configured Typesense index on the server.

**Keep me signed in** stores a session in an unlocked Linux keyring that supports Secret Service, not your password. Enable it while online if you want to reopen the app offline later.

## Listen

- Use **Search** to find tracks, albums, and artists in the server's library, or pick up from **History**. Create and edit manual and rule-based playlists.
- Reorder the queue, control playback on another connected device signed in to the same account, or transfer playback with **Playback devices**. Desktop supports Linux media keys and optional crossfade for local playback.
- Select the mini-player's artwork or player icon to open **Now playing** for waveforms and lyrics, including synced lyrics where available.

[See the album and queue view](docs/images/native-album.png).

## Listen offline

While connected, choose **Save offline** on tracks, albums, artists, or playlists. Open **Saved offline** to see saved music and check playlist download progress. Streaming alone doesn't save a track.

Choose **Settings → Work offline** to search and play downloaded music without connecting to the server. To reopen offline later, make sure **Keep me signed in** succeeds while online, finish your downloads, and switch to **Work offline** before quitting. The keyring must be unlocked when you reopen Spofify. Downloads stay on this computer and are tied to your account and server address. Lyrics aren't available offline, and offline listens aren't added to server history. Choose **Go online** to reconnect.

## Build and test

On Linux, use stable Rust with `rustfmt` (see the minimum version in [`Cargo.toml`](Cargo.toml)), a native toolchain, `pkg-config`, and ALSA and fontconfig development headers. On Ubuntu or Debian, install `build-essential`, `pkg-config`, `libasound2-dev`, and `libfontconfig1-dev`.

From the checkout:

```sh
cargo fmt --all -- --check
cargo test --locked
cargo run --locked
```

To add a launcher from source, run `make build` and `make install PREFIX="$HOME/.local"`. Keep `$HOME/.local/bin` on your desktop session's `PATH`. This install doesn't use the release updater.

See the [desktop implementation and test notes](docs/native-listener.md) and the [HTTP and desktop UI checks](scripts/acceptance/README.md).

The bundled [JetBrains Mono font](assets/JetBrainsMono-OFL.txt) is distributed under the SIL Open Font License.
