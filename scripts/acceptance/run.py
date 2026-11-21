#!/usr/bin/env python3
"""Run isolated spofify-go acceptance; optionally drive one prebuilt native Slint window.

No Cargo, live origin, ambient account, external download or global installation.
See --help for offline fixture and native display requirements.
"""
import argparse
from pathlib import Path
import shutil
import signal
import subprocess
import sys
from tempfile import TemporaryDirectory

sys.dont_write_bytecode = True
from fixture import Api, Failure, api_checks, build, check, fixture, private_env


def interrupted(signum, _frame):
    raise KeyboardInterrupt


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--go-source", type=Path, default=Path.home() / "repos" / "spofify-go",
                        help="clean spofify-go checkout with cmd/offline-fixture (default: ~/repos/spofify-go)")
    parser.add_argument("--go-module-cache", type=Path, help="prepopulated Go module cache; no network fetches")
    parser.add_argument("--fixture-binary", type=Path, help="trusted prebuilt cmd/offline-fixture, instead of Go build")
    parser.add_argument("--desktop", type=Path,
                        help="prebuilt debug desktop with Slint MCP and debug info; needs cage and wlr-randr for multisize checks")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--window-only", action="store_true", help="check empty sign-in, allowing only the Remember Login checkbox, without starting Go")
    mode.add_argument("--synthetic-wire-ui", action="store_true",
                      help="native navigation, fixed-theme Settings, density migration and track intent "
                           "against synthetic wire data; no Go or audio execution")
    mode.add_argument("--playlist-actions-only", action="store_true",
                      help="seed large playlists and verify native Play publishes the full Go queue; no audio output check")
    mode.add_argument("--playlist-offline-only", action="store_true",
                      help="only native playlist save and offline unpin against the disposable Go fixture; no large seeding or Play")
    parser.add_argument("--scratch-dir", type=Path,
                        help="existing base for disposable build/cache/DB/profile state (e.g. worker-private /dev/shm)")
    parser.add_argument("--capture-dir", type=Path,
                        help="optionally retain disposable fixture PNGs in a new direct child of ~/repos/spofify-desktop-temporary")
    parser.add_argument("--pcm", action="store_true",
                        help="also verify online and offline Go fixture audio on a private PipeWire null sink")
    args = parser.parse_args()
    playlist_only = args.playlist_actions_only or args.playlist_offline_only
    if args.pcm and (not args.desktop or args.window_only or args.synthetic_wire_ui or playlist_only):
        parser.error("--pcm requires the full real Go desktop run")
    if (args.window_only or args.synthetic_wire_ui or playlist_only or args.capture_dir) and not args.desktop:
        parser.error("window-only, synthetic-wire, playlist-only and capture modes require --desktop")
    artifact_root = (Path.home() / "repos" / "spofify-desktop-temporary").resolve()
    if not args.scratch_dir or args.capture_dir:
        artifact_root.mkdir(mode=0o700, exist_ok=True)
    scratch = args.scratch_dir.resolve(strict=True) if args.scratch_dir else artifact_root
    check(scratch.is_dir(), "scratch base must be an existing directory")
    captures = None
    if args.capture_dir:
        captures = args.capture_dir.absolute()
        check(captures.parent.resolve() == artifact_root, "capture directory must be a direct child of ~/repos/spofify-desktop-temporary")
        captures.mkdir(mode=0o700, exist_ok=False)
    with TemporaryDirectory(prefix="spofify-acceptance-", dir=scratch) as name:
        root = Path(name)
        env = private_env(root)
        if args.desktop:
            desktop = root / "spofify-desktop"
            shutil.copy2(args.desktop.resolve(strict=True), desktop)
            desktop.chmod(0o700)
        if args.window_only or args.synthetic_wire_ui:
            from native import native_checks
            if args.synthetic_wire_ui:
                from wire_ui import wire_fixture
                print("SYNTHETIC WIRE UI ONLY — includes density migration; no Go server or audio execution", flush=True)
                with wire_fixture() as ready:
                    native_checks(desktop, root, env, ready, pixels=captures)
            else:
                native_checks(desktop, root, env, pixels=captures)
            return
        if args.fixture_binary:
            binary, revision = args.fixture_binary.resolve(strict=True), None
        else:
            binary, revision = build(args.go_source.resolve(strict=True), root, env,
                                     args.go_module_cache.resolve(strict=True) if args.go_module_cache else None)
        failure = None
        with fixture(binary, root, env, revision) as ready:
            print(f"Go fixture source {ready['sourceRevision']}", flush=True)
            api = Api(ready, ready) if playlist_only else api_checks(ready)
            if args.desktop:
                from native import native_checks
                try:
                    if args.pcm:
                        from audio_smoke import output
                        with output(root, env) as pcm:
                            native_checks(desktop, root, env, ready, api, pixels=captures, pcm=pcm)
                    else:
                        native_checks(desktop, root, env, ready, api, pixels=captures,
                                      playlist_actions_only=args.playlist_actions_only,
                                      playlist_offline_only=args.playlist_offline_only)
                except (Failure, TimeoutError) as error:
                    # Check the fixture's shutdown receipt even after a UI regression.
                    failure = error
            else:
                from playback import Peer
                with Peer(api):
                    print("PASS real account v1 ticket, upgrade and initial player/personal snapshots", flush=True)
                print("NOT RUN native desktop (provide --desktop)", flush=True)
        if failure is not None:
            raise failure


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except (Failure, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f"FAIL: {error or type(error).__name__}", file=sys.stderr)
        sys.exit(1)
