#!/usr/bin/env python3
"""Real Slint relaunches + prebuilt Go fixture + in-memory Secret Service double.

Requires a debug desktop built with SLINT_EMIT_DEBUG_INFO=1 and
--features slint/mcp; cage, dbus-daemon, and Python dbus-python and GLib
(PyGObject). --output-relaunch additionally uses the private ALSA/PipeWire
null monitor and Cage's wlr-foreign-toplevel-management close protocol.
No build, real keyring, host display/input, live origin, or retained captures.
"""
import argparse
from contextlib import contextmanager, ExitStack
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import subprocess
import sys
from tempfile import TemporaryDirectory
from time import monotonic, sleep
from xml.sax.saxutils import escape

sys.dont_write_bytecode = True
from fixture import Api, check, fixture, private_env, stop
from native import REMEMBER_LOGIN, bounds, desktop, login_options, wait
from remember_login_wire import recorded_origin, vacant_output
from workspace_checks import open_manual, open_playlists


SERVER = "Server( origin| URL| address)?"
AUTH = {"login", "session", "logout"}


@contextmanager
def secret_service(root, env, python):
    daemon = shutil.which("dbus-daemon")
    check(daemon, "dbus-daemon is required")
    address = "unix:path=" + env["XDG_RUNTIME_DIR"] + "/bus"
    check(len(os.fsencode(address.removeprefix("unix:path="))) < 104,
          "choose a shorter --scratch-dir for the private Unix bus socket")
    # Unlike the default session config, this bus reads no service activation
    # directories, includes or host D-Bus settings.
    config = root / "bus.conf"
    config.write_text(f"""<busconfig><type>session</type><listen>{escape(address)}</listen>
        <auth>EXTERNAL</auth><policy context="default"><allow send_destination="*"/>
        <allow receive_sender="*"/><allow own="*"/></policy></busconfig>""")
    env = dict(env, DBUS_SESSION_BUS_ADDRESS=address,
               DBUS_SYSTEM_BUS_ADDRESS="unix:path=" + env["XDG_RUNTIME_DIR"] + "/no-system-bus",
               XDG_CONFIG_DIRS=env["XDG_CONFIG_HOME"], XDG_DATA_DIRS=env["XDG_DATA_HOME"])
    receipt = root / "secret-receipt"

    def events():
        return receipt.read_text().splitlines()[1:] if receipt.exists() else []

    with subprocess.Popen([daemon, "--nofork", "--print-address=1", "--config-file=" + str(config)],
                          cwd=root, env=env, stdout=subprocess.PIPE) as bus:
        try:
            with selectors.DefaultSelector() as sel:
                sel.register(bus.stdout, selectors.EVENT_READ)
                check(sel.select(5) and bus.stdout.readline().decode().startswith(address + ",guid="),
                      "private D-Bus did not publish its expected Unix address")
            helper = Path(__file__).resolve().with_name("remember_login_secret_service.py")
            with subprocess.Popen([str(python), "-I", str(helper), str(receipt)], cwd=root, env=env) as service:
                try:
                    def ready():
                        check(service.poll() is None, "private Secret Service double exited during startup "
                              "(install dbus-python and PyGObject for --dbus-python)")
                        return receipt.exists() and receipt.read_text() == "ready\n"
                    wait("private Secret Service double", ready, timeout=5)
                    yield env, events
                finally:
                    stop(service)
        finally:
            stop(bus)


def disabled_login(window, origin="", username=""):
    def matching_form():
        fields = [window.find(label, roles=("TextInput",)) for label in (SERVER, "Username", "Password")]
        return tuple(field.get("accessibleValue", "") for field in fields) == (origin, username, "")
    wait("expected server/username and blank password in native sign-in", matching_form)
    sign_in = window.find("Sign in", roles=("Button",))
    # Slint omits false booleans from the inspection response.
    check(not sign_in.get("accessibleEnabled", False), "sign-in enabled with a blank password")
    option = login_options(window, required=True)
    check(not option.get("accessibleChecked"), "Keep me signed in was not initially unchecked")


def store_metadata(env):
    path = Path(env["XDG_CONFIG_HOME"]) / "spofify-desktop-login-v1" / "login.json"
    return json.loads(path.read_bytes()) if path.exists() else {}


def no_profile_secrets(env, ready, receipt):
    forbidden = {secret.encode() for secret in {ready["password"], *receipt.tokens()}}
    for key in ("HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "TMPDIR"):
        for path in Path(env[key]).rglob("*"):
            check(not path.is_symlink(), "unexpected link in disposable desktop profile")
            if path.is_file():
                content = path.read_bytes()
                check(not any(secret in content for secret in forbidden), "secret persisted in desktop profile")


def assert_idle(window, receipt, mark):
    # Allow the initial worker to run; then verify the actual window did not
    # leave sign-in and no authentication request was received.
    sleep(1)
    window.find("Sign in", roles=("Button",))
    check(not any(event[0] in AUTH for event in receipt.events(mark)), "unremembered launch contacted auth")


def sign_out(window):
    window.click("Settings")
    window.title("Settings")

    def visible_sign_out():
        for item in window.elements():
            if not window.matches(item, "Sign out") or item.get("accessibleRole") != "Button":
                continue
            _, top, _, bottom = bounds(item)
            if 0 <= top < bottom <= window.dimensions()[1] - 100:
                return item
        # Settings shares the catalog's scroll viewport. The Account card
        # can start below the fold.
        window.scroll_catalog(-360)
        return None

    window.click_element(wait("visible Account / Sign out in Settings", visible_sign_out))
    window.find("Sign in", roles=("Button",))


def checks(binary, root, env, go, ready, receipt, keyring, first_socket_after, pcm):
    for remember in (False, True):
        mark = len(receipt.events())
        with desktop(binary, root, env) as window:
            if remember:
                disabled_login(window, ready["origin"], ready["username"])
                assert_idle(window, receipt, mark)
                check(not keyring(), "unchecked sign-in touched the private keyring")
                print("PASS unremembered relaunch prefills server and username only; no authentication or secret", flush=True)
                window.click(REMEMBER_LOGIN)
                check(login_options(window, required=True).get("accessibleChecked"), "opt-in click did not check the control")
            else:
                disabled_login(window)
                window.fill(SERVER, ready["origin"])
                window.fill("Username", ready["username"])
            window.fill("Password", ready["password"])
            window.click("Sign in")
            window.section("History")
            expected = ("login", "POST", 200, "rolling-v1" if remember else None)
            check([e for e in receipt.events(mark) if e[0] == "login"] == [expected],
                  "Go did not observe exactly one successful password login in the expected session mode")
            wait("committed login profile", lambda: store_metadata(env).get("username") == ready["username"]
                 and bool(store_metadata(env).get("active")) == remember)
            check(keyring() == (["store"] if remember else []), "unexpected keyring operations on password sign-in")
            no_profile_secrets(env, ready, receipt)
    print("PASS explicit opt-in stores one cookie-only credential in the private Secret Service double", flush=True)

    expected_keyring = ["store"]
    for statuses in ((503, 503, 503), (404,)):
        receipt.fail_next_sessions(*statuses)
        mark = len(receipt.events())
        expected_keyring.extend(["lookup"] * len(statuses))
        with desktop(binary, root, env) as window:
            window.find(r"Could not restore the saved session\. Sign in with your password\.")
            disabled_login(window, ready["origin"], ready["username"])
            window.absent(".*retry.*", roles=("Button",))
            check([e for e in receipt.events(mark) if e[0] in AUTH]
                  == [("session", "GET", status) for status in statuses],
                  "saved-session restore did not finish after the expected read attempts")
            check(keyring() == expected_keyring and store_metadata(env).get("active"),
                  "non-auth restore failure unexpectedly changed the saved credential")
    print("PASS transient restore stops after three reads; 404 stops after one; failures show Sign in without Retry",
          flush=True)

    # During the password launches only ticket/upgrade routes were held back.
    # After Go's startup drain, the first playable hello gets the vacant output.
    sleep(max(0, first_socket_after - monotonic()))
    receipt.allow_socket.set()
    receipt.fail_next_sessions(503)
    expected_keyring.extend(["lookup", "lookup"])
    mark = len(receipt.events())

    def on_restore():
        wait("Go rolling session validation", lambda: ("session", "GET", 200) in receipt.events(mark))
        check([event for event in receipt.events(mark) if event[0] == "session"]
              == [("session", "GET", 503), ("session", "GET", 200)],
              "remembered relaunch did not recover through one automatic read")
        check(keyring() == expected_keyring, "remembered relaunch did not revalidate the saved slot")
        print("PASS remembered relaunch recovered from 503 and entered History after real Go validation", flush=True)

    with ExitStack() as launches:
        if pcm is None:
            window = launches.enter_context(desktop(binary, root, env))
            window.section("History")
            on_restore()
            vacant_output(receipt, mark)
            print("PASS vacant Go output elected the playable desktop without automatic media or commands", flush=True)
            open_playlists(window)
            open_manual(window)
            play_mark = len(receipt.events())
            window.click("Play")
            wait("canonical playlist selection", lambda: ("owned-playlist-selection",) in receipt.events(play_mark))
            wait("post-Play presence", lambda: ("client", "presence") in receipt.events(play_mark))
            events = receipt.events(play_mark)
            check(events.count(("client", "play")) == 1 and ("playlist-play", True) in events,
                  "one native Playlist Play did not send exactly one correctly scoped command")
            print("PASS one native Playlist Play selected the expected canonical queue", flush=True)
        else:
            from remember_login_output import output_relaunches
            window = launches.enter_context(output_relaunches(
                binary, root, env, receipt, pcm, mark, on_restore=on_restore, remote_api=Api(go, go)))
            check([event for event in receipt.events(mark) if event[0] == "session"] ==
                  [("session", "GET", 503)] + [("session", "GET", 200)] * 4,
                  "remembered relaunches did not each validate a saved session")
            expected_keyring.extend(["lookup"] * 3)
            check(keyring() == expected_keyring, "output relaunches did not revalidate the same saved slot")
        check(not any(event[0] == "login" for event in receipt.events(mark)),
              "remembered relaunch sent a desktop password login")
        sign_out(window)
        wait("real Go logout", lambda: ("logout", "POST", 204) in receipt.events(mark))
        api = Api(go)
        api.cookie = receipt.rolling_cookie()
        check(api.cookie, "real Go rolling cookie was not observed")
        api.request("/api/auth/session", status=401)
        expected_keyring.append("clear")
        wait("private credential removal", lambda: keyring() == expected_keyring
             and not {"active", "cleanup"} & store_metadata(env).keys())
        no_profile_secrets(env, ready, receipt)
    print("PASS native Sign out retired the saved keyring item and Go session", flush=True)

    mark = len(receipt.events())
    with desktop(binary, root, env) as window:
        disabled_login(window, ready["origin"], ready["username"])
        assert_idle(window, receipt, mark)
    check(keyring() == expected_keyring, "post-sign-out relaunch touched Secret Service")
    print("PASS second relaunch stays signed out with only server and username prefilled", flush=True)

    # Re-enroll using only the normal password Sign in. A later definitive
    # denial must stop a sequence that began with a recoverable failure.
    with desktop(binary, root, env) as window:
        disabled_login(window, ready["origin"], ready["username"])
        window.click(REMEMBER_LOGIN)
        window.fill("Password", ready["password"])
        window.click("Sign in")
        window.section("History")
        expected_keyring.append("store")
        wait("new remembered credential", lambda: keyring() == expected_keyring
             and store_metadata(env).get("active"))
    receipt.fail_next_sessions(503, 401)
    mark = len(receipt.events())
    with desktop(binary, root, env) as window:
        window.find(r"The saved session is no longer valid\. Sign in again\.")
        disabled_login(window, ready["origin"], ready["username"])
        window.absent(".*retry.*", roles=("Button",))
        expected_keyring.extend(["lookup", "lookup", "clear"])
        wait("rejected credential retired", lambda: keyring() == expected_keyring
             and not {"active", "cleanup"} & store_metadata(env).keys())
        check([e for e in receipt.events(mark) if e[0] in AUTH]
              == [("session", "GET", 503), ("session", "GET", 401)],
              "401 did not stop automatic restore immediately")
    print("PASS restore-time 401 ends backoff, retires the credential and leaves manual Sign in", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--desktop", type=Path, required=True, help="prebuilt debug Slint desktop")
    parser.add_argument("--fixture-binary", type=Path, required=True, help="prebuilt Go cmd/offline-fixture")
    parser.add_argument("--scratch-dir", type=Path, help="disposable existing scratch base (e.g. /dev/shm)")
    parser.add_argument("--output-relaunch", action="store_true",
                        help="also check graceful playing, paused and remote-owner relaunches with private PCM monitoring")
    parser.add_argument("--dbus-python", type=Path, default=Path("/usr/bin/python3"),
                        help="installed Python with dbus-python and PyGObject (default: /usr/bin/python3)")
    args = parser.parse_args()
    binary, go_binary, python = (p.resolve(strict=True) for p in (args.desktop, args.fixture_binary, args.dbus_python))
    scratch = args.scratch_dir.resolve(strict=True) if args.scratch_dir else None
    with TemporaryDirectory(prefix="spofify-relaunch-", dir=scratch) as name:
        root = Path(name).resolve()
        env = private_env(root)
        with secret_service(root, env, python) as (env, keyring), fixture(go_binary, root, env) as go:
            first_socket_after = monotonic() + 16
            print(f"EVIDENCE native Slint + real Go {go['sourceRevision']} + private in-memory Secret Service double", flush=True)
            with ExitStack() as stack:
                pcm = None
                if args.output_relaunch:
                    from audio_smoke import output
                    from remember_login_output import prepare_tone
                    prepare_tone(root, go)
                    audio_env = dict(env)
                    pcm = stack.enter_context(output(root, audio_env))
                    # Keep Secret Service on its own bus, not the audio stack's absent bus.
                    env.update({key: audio_env[key] for key in ("ALSA_CONFIG_PATH", "PIPEWIRE_RUNTIME_DIR", "PULSE_SERVER")})
                ready, receipt = stack.enter_context(recorded_origin(go))
                checks(binary, root, env, go, ready, receipt, keyring, first_socket_after, pcm)


def interrupted(_signal, _frame):
    raise KeyboardInterrupt


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except (RuntimeError, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f"FAIL: {error or type(error).__name__}", file=sys.stderr)
        sys.exit(1)
