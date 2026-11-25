# Shared state on desktop

Desktop, Mobile and Go upgrade in lockstep. Desktop and Go use v3 playback JSON on the account WebSocket, without a `Sec-WebSocket-Protocol` header. The session cookie authenticates a same-origin ticket request; the single-use ticket admits the socket in the first JSON frame. There is no legacy playback fallback.

| State | Authority |
| --- | --- |
| Search results, album/artist details, media, artwork, lyrics and offline manifests | Direct authenticated HTTP requests. Search uses the server index, not an eager catalog snapshot. |
| Queue, desired cursor, shuffle, repeat, selected output, connected devices | The account socket's confirmed snapshot and ordered intent updates. Initiators reproject pending choices locally; only the owning device executes audio and reports measured playback. |
| Playlist summaries, scoped playlist contents, recent history | Personal snapshot and changes on the same socket; durable last-good projections render first. A successful snapshot is complete even at 100 playlists; omission invalidates the saved normal playlist. A playlist detail is assembled by token and offset; the scoped `missing` reply retires the current admitted read without closing the socket. Mutation targets are stable playlist/entry identities. |
| Offline queue and downloaded files | This computer can play independently while disconnected. Reconnection restores network services without publishing the private queue or replaying offline playback commands or history. |

A successful local write is **not** an acceptance or settlement. Terminal playback results carry a revision watermark; pending commands are not replayed across bindings. Device ownership and a separate socket binding fence reports. Owners refresh every two seconds; a lost socket leaves local audio independent while the ten-second server lease runs out. Desktop preserves independent local playback during reconnect; it neither takes over a different device’s output nor sends private cursor changes.

The on-wire contract is in the server's `docs/playback-sync-v3.md`. [Interoperability review](account-protocol-review.md) records historical v1 findings, not the current wire.
