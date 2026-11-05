# PAR-003 — Seekable, bounded local single-track playback

**Status:** specified, unassigned. **Prerequisites:** none. Follow [delivery guide](README.md); coordinate bottom UI with 001/002 without a shared mega-branch.

## Useful delivery / seams

Improve ordinary single-track Play end-to-end: large original files no longer require whole-file RAM buffering, seek works while playing/paused, actual times and local volume/mute live in the bottom dock. D `api.rs::media`, `audio.rs::activate/Fenced`, `engine.rs` media tasks currently preload ≤32 MiB before output. G `internal/web/library_audio.go::{libraryAudio,serveLibraryAudio}` uses `http.ServeContent` Range/HEAD; no transcoding endpoint exists.

## Contract

- Pick/document a bounded authenticated seekable/incremental architecture before code: finite buffer/cache bytes, concurrent requests, total/read/seek deadlines and acceptable first-output/seek targets measured with synthetic media. It must improve the production Play path, not deliver an unused source abstraction. No disk/offline persistence or arbitrary media URL support. Memory-budget changes remain explicit and bounded, not unlimited downloads.
- Use the existing once-encoded track-ID audio route at the exact authenticated origin; support/validate Range/206, Content-Range and safe 200 fallback within chosen bounds. HEAD is optional; never leak paths/cookies to redirect targets. Reject inconsistent ranges, overlarge responses, disallowed MIME, corrupt/unsupported originals with safe errors. Preserve TLS/LAN/CSRF policy, cancellation and no ambient proxy/retry.
- Fetch/decode/seek asynchronously so UI and coordinator polls continue. Every continuation/output sample is fenced by session, actor/request, authority epoch and monotonic lease. Seek publishes actual completed position, not requested time; paused seek stays paused. Cancelled old fetch/seek never overwrites a successor. Local Pause during eligible loading cancels a pending start.
- Bottom elapsed/duration/seek is keyboard operable with pending/error feedback. Volume/mute changes application-local gain only, not system routes/volume; remote-selected gain is unavailable with explanation. Metadata follows actual current track. **No Close player/reset Stop/dismiss or expand popup**; retain internal/incoming `close` and authority-loss silence.
- Name supported container/codec combinations from real decoder evidence (MP3/FLAC/WAV baseline; do not infer AAC/MP4/Ogg/Opus/AIFF/WebM support from MIME). Unsupported files fail honestly, not skip loops. No multi-track, receiver advertisement or handoff claim yet.

## Checks / native acceptance

Test malformed/ignored ranges, partial reads, slow/disconnected/redirected media, corrupt/truncated decodes, finite memory/request budgets and cancellation during fetch/seek/sign-out/lease loss. Existing WAV sample-fence tests remain. Measure native output and nonzero-time seek paused/playing on owned MP3/FLAC/WAV fixtures, including a synthetic file >32 MiB and slow delivery; record peak buffers, first-output/seek times and tolerances. Verify actual elapsed and paused state, local mute/gain without system changes, truthful unavailable-output/media retry, keyboard seek and long-title dock at standard sizes. Freeze evidence for this complete single-track capability; transferred activation remains 005, not an incomplete implementation handoff.
