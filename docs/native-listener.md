# Native listener implementation

`src/main.rs` installs Slint callbacks. The GUI thread never waits for a library read, decoder, artwork request, or playlist write. Account changes advance the shared epoch. Player controls, browser events, MPRIS intents and completed reads are checked against their scope before use.

## Data ownership

| Owner | Data / contract |
| --- | --- |
| `engine`, `socket`, `audio`, `player` | Go's **current** `spofify.library-playback.v2`. Authenticated ticket and WebSocket upgrade, room snapshot, canonical queue and device updates, instructions, execution leases and reports. The Rodio output consumes bounded bytes and a revocable execution grant. Displaying a queue never authorizes rendering it. |
| `desk`, `pages`, `replica`, `actions` | Bounded, account-local browser views. History, playlists/entries, rules and changes use Go's personal HTTP API. Indexed search uses `/search` and its opaque continuation, not the older track-only search. Album and artist views use the corresponding library detail and child-page endpoints. |
| `presentation`, `media`, `artwork` | Real authenticated artwork, waveform and lyrics. The catalog artwork worker loads the hero and up to 12 leading rows, coalesces duplicates and caches misses. The mini-player requests cover art for its current track; lyrics and waveform are fetched when Now Playing opens. No external artwork/lyrics URL is followed and no lyric is stored in downloads. |
| `preferences` | Local theme and scale; the palette is read and changed using the authenticated account palette contract. A lost write response is reconciled by reading, not by repeating the write. |
| `offline`, `collections`, `saved` | Explicit downloads and ordered collection membership. Verified media are separate from ephemeral room state and network credentials. |

Browser models are capped at 500 rows per view, 24 cached views and 1,400 retained rows total; the navigation trail has 40 entries. Navigation cancels superseded reads. Last-good rows survive transient failures and local collection invalidations. A stale search continuation rebuilds from a new result set through the requested depth. Reads and pending rules evaluations have bounded automatic recovery; writes and playback commands are not blindly replayed. Qualification hints can invalidate Recent but never synthesize a history row. Current Go does **not** provide shared playlist/Recent WebSocket deltas. See `shared-state.md` for the future boundary.

## Manual saved music

Save a track or collection while online. Use **Work offline** to switch from the server-owned room to the independent local queue; **Go online** first validates the same account through `GET /api/auth/session`. While an explicit reconnect is pending the local queue remains usable, and the validation probe is the only network operation permitted. A transient failure backs off; Stay offline cancels. A successful validation retires the local queue before normal online features resume, while a revoked/expired session needs a fresh sign-in. Play on Saved is deliberately local-only so neither order nor repeated occurrences silently change. Audio is fetched through Go's versioned offline endpoints, checked against metadata, size and digest, and stored privately under the exact origin/account scope. Each selected occurrence is verified when read; the desktop does not preload all audio into memory. Limits include 32 MiB per track, 256 stored tracks and 1 GiB total track storage. Collection membership preserves occurrences; removing a collection does not delete the shared audio files.

Downloads persist across restarts. This is **not** a credential vault or disk encryption. After process exit, sign in to the same server/account to reopen its files. An already-admitted explicit offline session can outlast its network cookie, but cannot make requests with an expired cookie. Sign-out revokes local access. No passwords, cookies, command receipts or socket tickets are written to the offline index.

## Verification environment

Use the normal locked Cargo test command. Filesystem tests require a private Linux directory that supports owner/mode checks, file locking, hard links and symlinks. On FUSE-mediated build workers use a fresh `TMPDIR` on private tmpfs and remove it afterwards. The MPRIS session-bus round-trip test is intentionally opt-in (`dbus-run-session -- cargo test --locked desktop_controller_round_trip -- --ignored`). Native acceptance uses a separately supervised debug inspection build and disposable Go account; production does not enable inspection.
