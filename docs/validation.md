# MVP validation and limits

## Actual native evidence

The owned fixture used an offline local build of `spofify-go` at `5b6be7a8f7d597575cec060613ee53617d4344df`, fresh synthetic accounts and imported MP3/FLAC tones. No working server, real account, real library or other application's screen was used. Slint 1.18.1's embedded localhost MCP inspected, clicked and captured the actual Wayland window. Rodio used the normal PipeWire-backed default ALSA output. Fixture tone files alone were attenuated to 10%; host volume/routes were unchanged.

Evidence is retained outside Git under `/var/home/lsm/repos/spofify-desktop/.tmp/mvp/`:

| Evidence | Source | Actual coverage |
| --- | --- | --- |
| `early-fcb04cc-before/`, `early-fcb04cc-after-click/` | `fcb04cc7` | Native pixels, structured tree and pointer checkbox interaction |
| `native-candidate-05-evidence/` | `be175c824306996c421ad871d163e2e673d887a9` | 1100×760 login, zero catalog GETs on empty search, bounded search, guarded used-empty claim, authenticated media, advancing native sink position, Pause/Resume/Stop, stable row handles across periodic publication, focused keyboard search, search failure/retry |
| `native-candidate-06-evidence/` | `4d8657e` | Explicit native Sign out while playing issued leave/logout and retained the paused room queue; its subsequent recovery failure was preserved and fixed |
| `native-focused-09-evidence/` | `243d72a6be4c946317a4f4021e72bb4f2fe632b3` | PASS at 900×640: retained nonempty paused-state admission, actual Go missing-registration response and explicit fresh registration, advancing output, delayed-media Stop cancellation, media failure, conflict reconciliation without auto-restart, actual server session revocation |
| `final-layout-12-900x640/`, `final-layout-12-1100x760/` | `acac86f` | PASS for the subsequent sign-in-only layout correction: full control/footer bounds with a three-line validation error and a real checkbox click at both sizes. Playback code is unchanged. |

The combined core/recovery evidence is intentional: unchanged successful cases were reused, not repeatedly rerun for documentation or error-message matcher changes. Earlier failed campaigns remain marked failures. In particular, candidate-05's media-error matcher timed out although the native UI correctly reported the unavailable media; the focused run corrected that assertion and passed.

The focused campaign uses a localhost-only observing/fault relay in front of the real owned Go server. It rewrites Host/Origin on that internal test hop, records only synthetic request classifications/state, and injects delayed media, a media 404 and a protocol 409. Registration recovery used the owned desktop session to leave its registration immediately before forwarding its poll, obtaining a real Go 404. Authentication revocation used the real admin password-reset API. No takeover flag, unselected-picker workaround or fabricated audio clock was used.

Player `elapsed` comes from Rodio sink progress, not a UI timer. Increasing decoded/output progress is proven; independently measured speaker/headphone audibility is not. Following a conflict, the observational server snapshot may still describe the last pre-fence state until a new explicit action; native output is stopped and that snapshot does not advance.

## Automated checks

At `243d72a`, **36 Rust unit/mock tests passed** offline with locked dependencies and two build jobs; the same 36 passed after the final sign-in-only layout change at `acac86f`. Coverage includes origin/TLS/redirect/auth/body limits; positive search projection (including actual Go name-only artists); latest-query/session suppression; abort/drain; pending-media Stop; real WAV decoding and per-sample epoch/lease/session fencing; stale-epoch observations; guarded claims/directive acknowledgments; retained paused-state admission without cached media; and expired registration retirement. Pure decoder tests do not themselves prove an output device; the native campaign supplies that separate evidence.

## Honest MVP boundaries

- Search only, at most 20 results, 350 ms debounce; no empty-query catalog load or automatic browse/pagination.
- One original media file buffered in memory, maximum 32 MiB. No streaming, seek, offline cache or instant-play/large-catalog performance claim.
- Only the supported single-track, context-free room snapshot subset. Unsupported multi-track/context handoffs are rejected rather than silently rewritten.
- No remote-output takeover, output picker, playlist/history/shuffle/repeat, volume control, credentials persistence or release packaging.
- Explicit Play is required after authority loss/recovery. Hardware already-buffered samples remain subject to finite device latency.
- MCP is an explicit debug-build/runtime opt-in, localhost only. Ordinary builds have no MCP feature; release entry rejects inspection environment opt-ins.

All campaign cleanup paths awaited owned native, Go and browser processes, removed exact owned fixture/scratch roots, and recorded primary failures separately from cleanup failures. The resumable `target.tar` is build cache, not evidence or a source archive.
