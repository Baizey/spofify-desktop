# Selected-output playback (PAR-002)

The desktop is both a **controller** for an already selected output and a restricted **single-track native receiver**. These are separate capabilities. Full browser state can be displayed without granting local audio authority or claiming full native queue/handoff support.

## User behavior

- Sign-in registers one in-memory, session-bound device and polls serially. It does not send selection or music intent. Search stays query-driven.
- Search Play controls the active browser output when one is selected. The dock shows that output’s name, actual current track/artists, state and elapsed/duration. Pause/Resume, Previous/Next and seek in **seconds** share one bounded command path.
- HTTP success is not command success. Pending, refused, blocked/media, timeout/cancelled and unknown outcomes are distinguished. A deliberate Pause can interrupt one pending music operation; both receipts are accounted for. Only **unsent** seeks coalesce. Accepted or uncertain music commands are never replayed automatically.
- An unavailable selected output remains unavailable; ordinary Play does not take it over. A genuinely unselected explicit Play uses the guarded self-claim, matching terminal activation acceptance and fresh lease before fetching local music. A self-owner recovered by registration/poll may use its valid lease only for fresh explicit intent; admission is bounded.
- Pause also cancels a pending local media load, including incoming remote Pause. The dock has no Close, Stop, dismiss or expand affordance. Internal lifecycle/incoming close still resets. Incoming clear means **clear upcoming**, preserving the current track/history and existing error.
- Retry connection never retries music. A definite missing registration is retired. Lost registration acceptance is uncertain: no automatic re-registration/name-based deletion; explicit registration recovery is spaced by at least 30 seconds and explained in the dock. This timer is not proof of server expiry.
- Sign-out and normal window shutdown retain immediate session/sample fencing and single-attempt known-device leave. No credentials, device IDs or queues are persisted.

## Bounds and authority

Player requests are at most **2 MiB**, matching Go’s request ceiling; only the player endpoint admits **36 MiB** responses. A response may contain room state and up to 16 directive snapshots/commands: 17 × 2 MiB plus bounded metadata/outcome overhead. The 1,000-entry regression vector is approximately 1.2 MiB per publication and over 2 MiB for state plus one directive; the old 32 KiB/256 KiB limits rejected legitimate observations. Other API and media limits are unchanged.

Player networking has a four-second deadline. Local leases still use send time, subtract the two-second safety margin, and are independently checked per decoded sample. GET/reconciliation and command/transfer responses never grant or renew output authority. Stale responses and malformed/duplicate-key envelopes fail closed. Full positive state validation preserves queue keys/order, duplicate occurrences, context and history occurrence; paths/media URLs/selection tokens are not retained in snapshots.

Remote elapsed interpolation adds at most five seconds of bounded state age/monotonic time, only while playing and not waiting/loading. It never becomes local output evidence.

The native receiver continues to reject unsupported full prepared queues before replacing the source. Local media remains bounded whole-file acquisition (32 MiB), not streaming, local seek or full queue parity. No picker/rename/transfer UI, priority receiver advertisement, remote volume, queue editor or additional library surfaces are introduced.

## Validation and limits

Focused Rust tests exercise full observation versus restricted admission, request/response bounds, duplicate JSON keys, terminal receipt correlation, Pause interruption, seek coalescing, observation-only authority, claim readiness, session/status failures, clear/close and pending-load cancellation. Existing lease/sample/session regressions remain in the suite; historical tests are not relabeled as new native cases.

Owned Linux Wayland/Slint acceptance used current-Go synthetic fixtures and an actual browser receiver at 900×640 and 1100×760. Actual browser audio/state changes were checked for native Play/Pause/Resume/Previous/Next/seek, delayed loading/Pause cancellation, injected blocked and refused outcomes, lost accepted replies, unavailable output and unsupported native prepare. The remote-controller trace contains no native media GET, self-transfer or owner-state publication. Separate guarded local acceptance checks terminal claim before media and advancing native sink publication, plus cancellation without late autoplay.

The green seek paint retains the standard Slider’s pointer, keyboard, focus and accessibility implementation. Focused after-renders and pointer/RightArrow effects validate the presentation change separately from unchanged protocol cases. Final source/binary/fixture hashes, failed attempts, reviewed captures and cleanup receipts are retained locally under `.tmp/parity/PAR-002/`; early failed aggregate campaigns remain failures even where their independently attributable successful steps are reused.

These are Linux-native and actual-browser effects, **not independently measured physical audibility**, Windows/macOS support, or full native receiving/queue parity. Normal window-shutdown behavior retains the existing production fence/join path; test-run process termination is not claimed as an interactive window-close test.
