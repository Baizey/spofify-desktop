# Desktop acceptance tools

Use the [Rust cross-native tests](../../docs/native-listener.md) with the disposable Go fixture to verify v3 playback. `run.py` runs HTTP checks by default and also has independent `--window-only` and `--playlist-offline-only` desktop checks. The seeded offline relaunch mode and the `reorder_preview` example do not use a playback socket peer.

## Historical v1 wire scenarios (not working with v3)

These Python scenarios are kept for their test designs. They still speak or inspect v1 JSON; a successful HTTP upgrade does not make them compatible with the current socket. Do not use them to verify v3 playback or account state.

- The old full real-Go UI/PCM run, `--synthetic-wire-ui`, and `--playlist-actions-only` are disconnected from `run.py`. Their old playback observers and synthetic fixture remain in `native.py`, `playback.py`, and `wire_ui.py`.
- `audio_smoke.py`, `delayed_local_play.py`, `synced_lyrics.py`, and `reorder.py` use v1 playback fixtures.
- Connected `offline_session_relaunch.py`, `catalog_continuity.py`, `manual_add.py`, `offline_catalog_parity.py`, `quick_add_concurrency.py`, `search_bulk_add.py`, and `search_entity_menus.py` use the old synthetic account fixture. This does not affect `offline_session_relaunch.py --seeded-offline`.
- Connected assertions in `remember_login.py`, `remember_login_kwallet.py`, and `remember_login_output.py` use the v1 recorder or observer. Their independent keyring helpers do not define a playback contract.
