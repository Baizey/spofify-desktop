# Native listener implementation

`src/main.rs` installs Slint callbacks. The GUI thread never waits for a library read, decoder, artwork request, or playlist write. Account changes advance the shared epoch. Player controls, browser events, MPRIS intents and completed reads are checked against their scope before use.

The sidebar switches between independent sections. History is the initial section, not the parent of Playlists, Saved offline or Settings. Back retraces a drill-down within the current section, including a search and its results. The bottom player is present only while there is a current track, including a paused one.

## Data ownership

| Owner | Data / contract |
| --- | --- |
| `engine`, `socket`, `audio`, `player` | Go’s `spofify.account-state.v1`. Authenticated admission and same-origin upgrade, canonical queue edits, output presence/grants, and observed playback facts. The Rodio output consumes bounded bytes and a revocable execution grant. Displaying a queue never authorizes rendering it. |
| `desk`, `pages`, `replica`, `personal`, `personal_bridge`, `actions` | Bounded, account-local browser views. Playlists and Recent come from account snapshots and live changes. Playlist members are installed from complete, scoped snapshot parts; playlist edits travel over the same account socket. Indexed search uses `/search` and its opaque continuation, not the older track-only search. Album and artist views use the corresponding library detail and child-page endpoints. |
| `presentation`, `media`, `artwork` | Real authenticated artwork, waveform and lyrics. The catalog artwork worker loads the hero and up to 12 leading rows, coalesces duplicates and caches misses. The mini-player requests cover art for its current track; lyrics and waveform are fetched when Now Playing opens. No external artwork/lyrics URL is followed and no lyric is stored in downloads. |
| `preferences` | Device-local density, crossfade and output volume. Density loads from `appearance.json` without applying an old theme value; crossfade and volume keep their separate files `playback.json` and `local-volume.json`. |
| `offline`, `collections`, `saved`, `playlist_pins`, `playlist_pin_store`, `playlist_browser` | Explicit downloads, durable playlist intent and ordered collection membership. Verified media are separate from ephemeral room state and network credentials. |

Online catalog views are capped at 500 rows. Saved views show all local rows; the cache retains at most 24 views and 1,400 rows total; the navigation trail has 40 entries. Navigation cancels superseded reads. Last-good rows survive transient failures and local collection invalidations. A stale search continuation rebuilds from a new result set through the requested depth. Reads and pending rules evaluations have bounded automatic recovery; writes and playback commands are not blindly replayed. Only server-observed qualification contributes to shared Recent. The desktop does not synthesize History rows or upload offline listens. See `shared-state.md` for the account-socket boundary.

## Remembered sign-in

At launch, the desktop validates a remembered session with its original server before admitting the account. It backs off on transient read failures and stops after at most three attempts. Cancel interrupts the restore. A rejected credential is retired; other failures leave it saved and show the password sign-in form. Password sign-in and sign-out are not automatically repeated.

## Manual saved music

Save a track or collection while online. In **Settings**, use **Work offline** to switch from the server-owned room to the independent local queue; **Go online** first validates the same account through `GET /api/auth/session`. While an explicit reconnect is pending the local queue remains usable, and the validation probe is the only network operation permitted. A transient failure backs off; Stay offline cancels. A successful validation retires the local queue before normal online features resume, while a revoked/expired session needs a fresh sign-in. Play on Saved is deliberately local-only so neither order nor repeated occurrences silently change. Audio is fetched through Go's versioned offline endpoints, checked against metadata, size and digest, and stored privately under the exact origin/account scope. Each selected occurrence is verified when read; the desktop does not preload all audio into memory. The per-track buffer is limited to 32 MiB. Total download storage is limited by the filesystem, not by a track count or aggregate byte quota. Collection manifests are validated page by page without truncating membership, including repeat occurrences. Removing a collection does not delete the shared audio files.

Manual and rule playlist pins persist separately under `spofify-playlist-pins-v1`. The worker starts again with the same authenticated account, rereads the fenced manifest, and verifies and reuses audio already on disk. Only a complete membership replaces the displayed snapshot; a matching legacy collection stays playable while its first pin is pending. Removing a pin works offline and never removes audio or rewrites the legacy index. Completed pins can refresh after reconnect; there is no periodic polling after success. Album and artist saves still use the collection index.

Downloads persist across restarts. This is **not** a credential vault or disk encryption. After process exit, reopen its files by signing in to the same server/account or by allowing the app to validate a remembered session with that server. An already-admitted explicit offline session can outlast its network cookie, but cannot make requests with an expired cookie. Sign-out revokes local access. No passwords, cookies, command receipts or socket tickets are written to the offline index.

## Verification environment

Use the normal locked Cargo test command. Filesystem tests require a private Linux directory that supports owner/mode checks, file locking, hard links and symlinks. On FUSE-mediated build workers use a fresh `TMPDIR` on private tmpfs and remove it afterwards. The MPRIS session-bus round-trip test is intentionally opt-in (`dbus-run-session -- cargo test --locked desktop_controller_round_trip -- --ignored`). Native acceptance uses a separately supervised debug inspection build and disposable Go account; production does not enable inspection.
