# Spofify Desktop

Spofify Desktop is a Linux client for listening to music from a running, self-hosted [Spofify Server](https://github.com/Baizey/spofify-server). Use it with your own server or with a listener account from someone who hosts one. Set up and administer the server separately.

## Install

The prebuilt release is for **Linux x86-64**, built and tested on Ubuntu 24.04 (glibc 2.39 baseline). Use a Wayland or X11 desktop with ALSA, fontconfig, and working audio.

1. Download `spofify-desktop-VERSION-linux-x86_64.tar.gz` from [GitHub Releases](https://github.com/Baizey/spofify-desktop/releases). The asset named `spofify-desktop-linux-x86_64` is the bare executable used by the updater, not the installer.
2. Extract the archive and run `bash install.sh` inside the extracted folder. It installs for your user; no root access or Rust is needed.
3. Open **Spofify** from your app menu.

Release installs verify and install updates in the background while online. When an update is ready, quit and reopen Spofify. To update manually, close the app and run the installer from a newer release. See [Desktop releases](docs/desktop-release.md) for update verification and packaging.

## Sign in

Enter the server's configured **Server address**, such as `https://music.example.com`, and sign in with an existing listener or admin account. Include the scheme and any non-default port, but no path. Use the server's [configured LAN or HTTPS address](https://github.com/Baizey/spofify-server#remote-and-local-network-access) to connect from another machine. HTTP sends credentials unencrypted.

Online playback requires [playback sync v3](docs/shared-state.md). If you use a reverse proxy, it must preserve the configured Host and pass WebSocket upgrades.

**Keep me signed in** stores a session in an unlocked Linux keyring that supports Secret Service, not your password. Enable it while online if you want to reopen the app offline later.

## Listen

- Search the server's tracks, albums, and artists, or pick up from **History**. Create and edit manual and rule-based playlists.
- Play albums and playlists, reorder the queue, and switch playback between connected devices on the same account. Desktop also supports Linux media keys and optional crossfade for local playback.
- Select the mini-player's artwork or player icon to open **Now playing** for waveforms and lyrics, including synced lyrics where available.

[See the album and queue view](docs/images/native-album.png).

## Listen offline

While connected, choose **Save offline** on tracks, albums, artists, or playlists. Open **Saved offline** to see saved music and check playlist download progress. Streaming alone doesn't save a track. For offline listening, choose **Settings → Work offline** to search and play only downloaded music. Downloads stay on this computer and are tied to the account that saved them. Lyrics aren't available offline, and offline listens aren't added to server history. Choose **Go online** to reconnect.

## Build and test

On Linux, use stable Rust with `rustfmt` (see the minimum version in [`Cargo.toml`](Cargo.toml)), a native toolchain, `pkg-config`, and ALSA and fontconfig development headers. On Ubuntu or Debian, install `build-essential`, `pkg-config`, `libasound2-dev`, and `libfontconfig1-dev`.

From the checkout:

```sh
cargo fmt --all -- --check
cargo test --locked
cargo run --locked
```

To add a launcher from source, run `make build` and `make install PREFIX="$HOME/.local"`. Keep `$HOME/.local/bin` on your desktop session's `PATH`. This install doesn't use the release updater.

See the [desktop implementation and test notes](docs/native-listener.md) and the [HTTP and desktop UI checks](scripts/acceptance/README.md). Cross-client playback checks need a separate runner and a disposable Go fixture.

The bundled [JetBrains Mono font](assets/JetBrainsMono-OFL.txt) is distributed under the SIL Open Font License.
