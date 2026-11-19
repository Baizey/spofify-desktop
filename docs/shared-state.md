# Shared state on desktop

This client and Go `08bb99e` use `spofify.account-state.v1`. The session cookie authenticates a same-origin ticket request; the single-use ticket admits an account socket. It is a matched cutover, not an attempted v2 connection followed by a downgrade.

| State | Authority |
| --- | --- |
| Search results, album/artist details, media, artwork, lyrics and offline manifests | Direct authenticated HTTP requests. Search uses the server index, not an eager catalog snapshot. |
| Queue, playback cursor, shuffle, repeat, selected output, connected devices | The account socket’s player snapshot and ordered player/output updates. Commands express choices; the output reports facts. Receiving metadata is not permission to render audio. |
| Playlist summaries, scoped playlist contents, recent history | Personal snapshot and changes on the same socket. A playlist detail is assembled by token and offset, not pieced together from different revisions. Mutation targets are stable playlist/entry identities, not list indexes. |
| Manual offline queue and downloaded files | This computer only. The explicit Settings switch ends shared output; reconnect validates the account and rebuilds shared state. Local clicks and offline plays are not replayed to Go. |

Reconnection replaces authoritative state. A successful local write to the socket is **not** a server receipt. For a choice whose result is ambiguous, observe the next authoritative state; do not automatically replay Play, Next, create, add, or delete. A private output handle and a short-lived grant govern local playback, not a cache hit or the selected-device label.

The on-wire contract is in the server’s `docs/library-playback-session.md`. [Interoperability review](account-protocol-review.md) describes observed limitations.
