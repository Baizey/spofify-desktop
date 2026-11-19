# Account socket interoperability review

Server reference: `08bb99e` (`spofify.account-state.v1`). The desktop talks to the authenticated Go service; it does not connect to Typesense. The findings below refer to this server revision.

## Partial playlist creation is not published — reproduced

On a disposable Go fixture I connected two clients to the same account and waited for both initial personal snapshots. One sent `createPlaylist` with a new name and a no-longer-available `trackId`, as can happen from stale search results. Its socket closed. The second client received **zero** playlist changes; after an explicit `sync`, its fresh snapshot contained the new playlist with zero tracks.

In `internal/accountpersonal/state.go`, `Source.Apply` creates the playlist and adds the initial track in separate store operations. It returns the committed summary *and* the add error. In `internal/web/account_personal_channel.go`, `personalWork` takes the error branch before publishing the change. Prefer a single atomic store operation. If partial success is intentional, broadcast the committed change and send a scoped refusal for the failed part without dropping the player's socket. Do not replay an ambiguous create or add.

## Bounded frames do not bound server-side work — source finding

`Source.SyncPlaylist` in `internal/accountpersonal/wire.go` requests a complete `Source.Playlist`. That call reads all eligible rows and hashes the marshalled playlist before returning a part. Continuations do the same work again, including when checking an unchanged token. With N rows and K parts this can revisit O(N × K) rows. The work shares a per-account FIFO with personal edits and a 15-second deadline. This is about the size of **one playlist**, not the size of the indexed search corpus. No production latency or capacity figure was measured here.

Consider immutable, short-lived snapshot materializations keyed by account, playlist and version, with bounded slices or keyset reads and cheap known-token validation. Keep the existing atomic install on the client; a mixture of different playlist versions must never become visible.

## Unavailable and oversized playback selections look alike — source finding

`internal/accountplayer/player.go` (`Player.Resolved`) keeps the existing selection and sends the requesting source’s unchanged cursor when resolution fails, yields no eligible tracks, or the selection cannot fit. Preserving the existing queue is good, but the desktop cannot tell “use a smaller selection” from “this track is no longer available.” A short, scoped reason on refusal would let the listener recover without introducing command replay or requiring success acknowledgments.
