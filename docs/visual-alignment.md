# Native visual alignment

The native shell follows the current Spofify web vocabulary, not the full web feature set. Reference: read-only `spofify-go` commit `5b6be7a8f7d597575cec060613ee53617d4344df`, `internal/web/static/{app,library,player}.css`. The user's historical web screenshot informed composition only; its old logo/navigation are not restored.

## Mapping

| Web source | Native interpretation |
| --- | --- |
| `app.css` bg/panel/raised/line | `Theme` #0b0d10 / #12151a / #181c22 / #292f38; near-black page, restrained charcoal fields and controls |
| text/muted/green/green-dark | #f2f5f7 / #98a2b1 / #5ce89b / #123c29; light titles, quiet metadata and small green actions |
| red/amber/focus | #ff7884 / #ffc968 / #94f7bf; honest error, HTTP warning and visible keyboard focus |
| 6px control radius, 58px topbar | Shared radius token, compact header with first-party logo, wordmark and native platform label |
| JetBrains Mono 14px base | Existing bundled unmodified font; 14px row titles, 11px metadata, 12–13px controls |
| Library pill search and flat song rows | 44px-high/24px-radius native search, 66px rows with 46px local monogram art and thin separators; no cards or catalog preload |
| `player.css` #111716 / 106px / 58px art | Bottom dock with local art, compact light play/pause, green small controls and status; no fake progress, seek or volume |

`ui/theme.slint` defines reusable native Action, Field, Consent and Cover components. Actions have semantic button roles, accessible labels, disabled state, focus forwarding, visible focus and Space/Return activation; checkbox supports Space. Fields use native Slint TextInput for keyboard editing, password masking, selection and accessibility. No control is a mouse-only TouchArea. Stable result models/callbacks and all Rust/API/player/audio code remain unchanged. ScrollView retains native scroll behavior.

The sign-in form is centered and width-bounded, with explicit field/status heights rather than the old interdependent split-column layout. Empty, searching, error/retry, disabled and player states are live engine state, not demonstration placeholders. The artwork is deliberately a local monogram, not fetched album art. There are no decorative navigation tabs or unavailable product actions.

## First-party asset provenance

`assets/spofify-logo.svg` is an unmodified byte-for-byte copy of `spofify-go/internal/web/static/spofify-logo.svg` at the pinned reference above. Its embedded provenance identifies the mobile vector (`spofify-mobile/app/src/main/res/drawable/ic_spofify.xml`, server spec `docs/specs/FEAT-021-mobile-logo-branding.md`). It is a first-party Spofify brand asset, not a third-party icon/font. Existing JetBrains Mono bytes and full SIL OFL license are unchanged; see README.

## Validation scope

Native Wayland captures and focused real-Go synthetic interaction are retained under `.tmp/web-vibe/` outside Git. They cover 1100×760 and 900×640 sign-in/error, empty/results, playing/paused/stopped and search error/retry, keyboard focus and transport, and credential-free screenshots. Testing is separate from the user's existing MVP instance, which is never inspected or replaced. Backend/protocol recovery evidence in `docs/validation.md` remains applicable because those source files are unchanged. This is a Linux-native visual alignment, not a Windows/macOS support claim.
