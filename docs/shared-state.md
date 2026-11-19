# Shared state on desktop

The distinction is **shared, asynchronous state vs. a point-to-point exchange**. It is not reads vs. writes. Play a track and add an occurrence to the queue are asynchronous commands to the account's shared player. They belong on its WebSocket. Login, search, fetching a particular audio file and similar requests with a direct answer fit HTTP.

## Wire compatibility

The current Go server documents playback v2 in `spofify-go/docs/library-playback-session.md`. Admission starts with an authenticated HTTP request for a one-use ticket; the socket carries musical commands, their results, server state, executor instructions and fenced output reports. A command result is not evidence that an audio device has started. Session, occurrence, connection and executor identities serve different purposes; a matching track ID does not establish ownership of an output.

Today the Go server still provides personal playlists, Likes, and listening history over HTTP. Do not send playlist mutations to the playback socket or silently fall back to a retired player API. Search uses `GET /search` and requires the server's search index. It is not a substitute for the local copy of live player state.

## Direction for the next protocol

Player state, the queue, playlists and Recently played belong in an **account-scoped replica**. The intended protocol supplies initial state on connection and then sends smaller updates. A screen observes the replica; navigating back to a playlist should not replace its usable rows with a spinner. Reconnect and repair are application responsibilities, not buttons. Version information helps establish which state needs repair; it is not a universal precondition for an ordinary Play command.

- If state is behind, retain the last usable view while repairing it. Bind row-specific actions to occurrences, not positions: a tap on row three must not operate on tomorrow's row three.
- A command known **not** to have been sent can be applied after a short interruption if its target and user intent are still current. A command that **may** have run is different: settle by its original identity where supported. If the result cannot be established, adopt authoritative state; do not resend Next or replay old Play commands.
- A client that renders another device's progress does not own its audio. Only continue output through interruption while the actual executor grant permits it. A cache hit is not a lease.
- Manual offline is an explicit change of source, not a symptom of network failure. Local playback must not send commands or retroactively turn offline actions into online actions. Retire old account/source workers and data before publishing for a successor.

The proposed startup snapshots and playlist/Recent delta delivery are not promises about the current server. Keep transport changes behind the data owners so implementing a future contract does not require new screen-specific recovery logic.
