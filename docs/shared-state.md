# Shared state on desktop

Desktop and Go use `spofify.account-state.v3`. The session cookie authenticates a same-origin ticket request; the single-use ticket admits an account socket. Desktop does not negotiate or fall back to the old playback protocol.

| State | Authority |
| --- | --- |
| Search results, album/artist details, media, artwork, lyrics and offline manifests | Direct authenticated HTTP requests. Search uses the server index, not an eager catalog snapshot. |
| Queue, desired cursor, shuffle, repeat, selected output, connected devices | The account socket's confirmed snapshot and ordered intent updates. Initiators reproject pending choices locally; only the owning device executes audio and reports measured playback. |
| Playlist summaries, scoped playlist contents, recent history | Personal snapshot and changes on the same socket. A playlist detail is assembled by token and offset, not pieced together from different revisions. Mutation targets are stable playlist/entry identities, not list indexes. |
| Offline queue and downloaded files | This computer can play independently while disconnected. On reconnect, a live owner wins an ordinary claim; absent one, a nonempty local queue takes precedence over the retained shared snapshot. Offline actions are not replayed as commands or history. |

A successful local write is **not** an acceptance or settlement. Terminal playback results carry a revision watermark; pending commands are not replayed across bindings. Device ownership and a separate socket binding fence reports. Owners refresh every two seconds; a lost socket leaves local audio independent while the ten-second server lease runs out. Rebinding the live owner with the exact confirmed basis can accept a compatible detached cursor without borrowing its offline history. One cursor update can reconcile a local Pause, Resume, Seek, or compatible advance between hello and snapshot; it never imports queue edits or settings.

The on-wire contract is in the server's `docs/playback-sync-v3.md`. [Interoperability review](account-protocol-review.md) records historical v1 findings, not the current wire.
