# Native listener implementation

`src/main.rs` installs Slint callbacks. The GUI thread never waits for a library read, decoder, artwork request, or playlist write. Account changes advance the shared epoch. Player controls, browser events, MPRIS intents and completed reads are checked against their scope before use.

The sidebar switches between independent sections. History is the initial section, not the parent of Playlists, Saved offline or Settings. Back retraces a drill-down within the current section, including a search and its results. The bottom player is present only while there is a current track, including a paused one.

## Data ownership

| Owner | Data / contract |
| --- | --- |
| `engine`, `socket`, `playback_sync`, `audio`, `player` | Go's account WebSocket with v3 playback JSON. Authenticated admission, confirmed intent plus pending command projection, a device-owned native executor, and measured observations. The Rodio output consumes bounded sources. Displaying a queue never authorizes rendering it. |
| `desk`, `pages`, `replica`, `personal`, `personal_bridge`, `actions` | Bounded, account-local browser views. Playlists and Recent come from account snapshots and live changes. Playlist members are installed from complete, scoped snapshot parts; playlist edits travel over the same account socket. Indexed search uses `/search` and its opaque continuation, not the older track-only search. Album and artist views use the corresponding library detail and child-page endpoints. |
| `presentation`, `media`, `artwork` | Real authenticated artwork, waveform and lyrics. The catalog artwork worker loads the hero and up to 12 leading rows, coalesces duplicates and caches misses. The mini-player requests cover art for its current track; lyrics and waveform are fetched when Now Playing opens. No external artwork/lyrics URL is followed and no lyric is stored in downloads. |
| `preferences` | Device-local density, crossfade and output volume. Density loads from `appearance.json` without applying an old theme value; crossfade and volume keep their separate files `playback.json` and `local-volume.json`. |
| `offline`, `collections`, `saved`, `playlist_pins`, `playlist_pin_store`, `playlist_browser` | Explicit downloads, durable playlist intent and ordered collection membership. Verified media are separate from ephemeral room state and network credentials. |

Online catalog views are capped at 500 rows. Saved views show all local rows; the cache retains at most 24 views and 1,400 rows total; the navigation trail has 40 entries. Navigation cancels superseded reads. Last-good rows survive transient failures and local collection invalidations. A stale search continuation rebuilds from a new result set through the requested depth. Reads and pending rules evaluations have bounded automatic recovery; writes and playback commands are not blindly replayed. Only server-observed qualification contributes to shared Recent. The desktop does not synthesize History rows or upload offline listens. See `shared-state.md` for the account-socket boundary.

## Remembered sign-in

A typed remembered login admits the same account locally before checking the network. The private Secret Service record binds the previously authenticated origin and account to the startup preference; disk prefill and the Downloads index are never authority. A legacy online-only credential must validate with its original server once before it can be migrated. A definitive rejection retires the remembered capability. Password sign-in and remote sign-out are not automatically repeated.

An auto-offline startup or a lost playback link gets four bounded socket-and-handshake attempts without extra backoff. Exhausting the batch saves manual-offline for remembered sessions; **Go online** starts a new batch. Manual-offline startup creates no account socket. Credentials are revalidated independently. A single process per private config root owns remembered auth.

## Manual saved music

Save a track or collection while online. Use **Work offline** to play a Saved queue in its stored order, including duplicate entries. The local queue remains usable during revalidation and the socket handshake. A winning unowned claim publishes that queue; a live-owner conflict silences local output and adopts the shared snapshot. If the former owner reconnects, only compatible detached cursor progress may be imported while its lease is live. Audio is fetched through Go's versioned offline endpoints, checked against metadata, size and digest, and stored privately under the exact origin/account scope. Each selected occurrence is verified incrementally and played from the same opened file. Download storage is limited by the filesystem, not by a per-track, aggregate byte, or track-count quota. Collection manifests are validated page by page without truncating membership, including repeat occurrences. Removing a collection does not delete the shared audio files.

Online audio uses one authenticated response per loaded source and a temporary private file, not Saved storage. Playback can start once the decoder has the bytes it needs. Seeking ahead of the downloaded portion, or opening a format that reads a tail index, may wait for more data. Desktop’s transfer and PCM buffers are bounded independently of track size. Codecs may retain format-specific metadata such as MP4 sample tables; Desktop does not buffer the complete encoded track.

Manual and rule playlist pins persist separately under `spofify-playlist-pins-v1`. The worker starts again with the same authenticated account, rereads the fenced manifest, and verifies and reuses audio already on disk. Only a complete membership replaces the displayed snapshot; a matching legacy collection stays playable while its first pin is pending. Removing a pin works offline and never removes audio or rewrites the legacy index. Completed pins can refresh after reconnect; there is no periodic polling after success. Album and artist saves still use the collection index.

Downloads persist across restarts. This is **not** disk encryption or protection from someone who controls the system user account and its keyring. An installed Work offline grant can outlast its server cookie. While disconnected, Desktop cannot learn about remote revocations; a definitive rejection on Go online retires the grant, and local sign-out revokes local access. No passwords, cookies, CSRF tokens, command receipts or socket tickets are written to the offline index.

## Verification environment

Use the normal locked Cargo test command. Filesystem tests require a private Linux directory that supports owner/mode checks, file locking, hard links and symlinks. On FUSE-mediated build workers use a fresh `TMPDIR` on private tmpfs and remove it afterwards. The MPRIS session-bus round-trip test is intentionally opt-in (`dbus-run-session -- cargo test --locked desktop_controller_round_trip -- --ignored`). The opt-in `engine::cross_native_tests::desktop_cross_native_play_and_handoff` and `engine::cross_native_tests::desktop_cross_native_conditional_seek` run with the Go/Android `pair.py` runner, a disposable Go account, the real v3 transport and a deterministic Rodio mixer. The [historical Python v1 socket scenarios](../scripts/acceptance/README.md) are incompatible with the current socket. Production builds do not enable inspection.
