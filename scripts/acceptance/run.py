#!/usr/bin/env python3
"""Check isolated Spofify Go HTTP APIs or an independent desktop UI scenario.

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
    # Recognize retired flags so old invocations do not silently run a weaker check.
    for flag in ("--synthetic-wire-ui", "--playlist-actions-only", "--pcm"):
        parser.add_argument(flag, action="store_true", help=argparse.SUPPRESS)
    mode.add_argument("--playlist-offline-only", action="store_true",
                      help="only native playlist save and offline unpin against the disposable Go fixture; no large seeding or Play")
    parser.add_argument("--scratch-dir", type=Path,
                        help="existing base for disposable build/cache/DB/profile state (e.g. worker-private /dev/shm)")
    parser.add_argument("--capture-dir", type=Path,
                        help="optionally retain disposable fixture PNGs in a new direct child of ~/repos/spofify-desktop-temporary")
    args = parser.parse_args()
    if (args.synthetic_wire_ui or args.playlist_actions_only or args.pcm or
            (args.desktop and not (args.window_only or args.playlist_offline_only))):
        parser.error("the old v1 wire scenarios do not work with the v3 socket; see docs/native-listener.md "
                     "for the Rust playback fixture. With --desktop use --window-only or --playlist-offline-only.")
    if (args.window_only or args.playlist_offline_only or args.capture_dir) and not args.desktop:
        parser.error("window-only, playlist-offline-only and capture modes require --desktop")
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
        if args.window_only:
            from native import native_checks
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
            if args.playlist_offline_only:
                from native import native_checks
                try:
                    native_checks(desktop, root, env, ready, Api(ready, ready), pixels=captures,
                                  playlist_offline_only=True)
                except (Failure, TimeoutError) as error:
                    # Check the fixture's shutdown receipt even after a UI regression.
                    failure = error
            else:
                api_checks(ready)
                print("NOT RUN native desktop or playback socket (HTTP checks only)", flush=True)
        if failure is not None:
            raise failure


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except (Failure, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f"FAIL: {error or type(error).__name__}", file=sys.stderr)
        sys.exit(1)
