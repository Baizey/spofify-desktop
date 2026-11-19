#!/usr/bin/env python3
"""Exercise the prebuilt production desktop against disposable native KDE Secret Service.

Requires ksecretd with its PAM-launch protocol, dbus-run-session, busctl, Cage,
a Slint MCP debug desktop and the prebuilt Go fixture. No real PAM login, host
keyring, display, input, sound device, build, or retained captures.
"""
import argparse
from contextlib import ExitStack, contextmanager
import json
import os
from pathlib import Path
import selectors
import shutil
import signal
import socket
import subprocess
import sys
from tempfile import TemporaryDirectory
from time import monotonic, sleep
from xml.sax.saxutils import escape

sys.dont_write_bytecode = True
from fixture import Api, Failure, check, fixture, private_env, stop
from native import REMEMBER_LOGIN, bounds, desktop, element_id, login_options, verify, wait
from remember_login import SERVER, assert_idle, disabled_login, interrupted, no_profile_secrets, store_metadata
from remember_login_wire import recorded_origin
from workspace_checks import open_manual, open_playlists

DBUS = "org.freedesktop.DBus"
NAME = "org.freedesktop.secrets"
SERVICE_PATH = "/org/freedesktop/secrets"
SERVICE = "org.freedesktop.Secret.Service"
APPLICATION = "spofify-desktop-remembered-login-v1"


class Wallet:
    def __init__(self, busctl, root, env):
        self.busctl, self.root, self.env = busctl, root, env

    def call(self, path, interface, method, *args, destination=NAME):
        result = subprocess.run(
            [self.busctl, "--address=" + self.env["DBUS_SESSION_BUS_ADDRESS"], "--json=short",
             "--timeout=2", "call", destination, path, interface, method, *args],
            env=self.env, cwd=self.root, capture_output=True, timeout=3,
        )
        check(result.returncode == 0, f"private D-Bus call failed: {interface}.{method}")
        return json.loads(result.stdout)["data"]

    def owner_pid(self):
        pid, = self.call("/org/freedesktop/DBus", DBUS, "GetConnectionUnixProcessID", "s", NAME,
                         destination=DBUS)
        return pid

    def items(self, slot=None):
        args = ["a{ss}", "1" if slot is None else "2", "application", APPLICATION]
        if slot is not None:
            args += ["slot", slot]
        unlocked, locked = self.call(SERVICE_PATH, SERVICE, "SearchItems", *args)
        check(not locked, "disposable KDE wallet unexpectedly returned locked items")
        return unlocked


@contextmanager
def native_wallet(root, env):
    runner, busctl, kde = (shutil.which(name, path=env["PATH"])
                           for name in ("dbus-run-session", "busctl", "ksecretd"))
    check(runner and busctl and kde, "KDE acceptance needs dbus-run-session, busctl and ksecretd")
    runtime = Path(env["XDG_RUNTIME_DIR"])
    bus_path, bootstrap = runtime / "bus", runtime / "bootstrap.sock"
    check(len(os.fsencode(bootstrap)) < 104, "choose a shorter --scratch-dir for private Unix sockets")
    address = f"unix:path={bus_path}"
    config = root / "bus.conf"
    # No service-activation dirs, host includes or systemd activation.
    config.write_text(f"""<busconfig><type>session</type><listen>{escape(address)}</listen>
        <auth>EXTERNAL</auth><policy context="default"><allow send_destination="*"/>
        <allow receive_sender="*"/><allow own="*"/></policy></busconfig>""")
    env = dict(env, DBUS_SYSTEM_BUS_ADDRESS=f"unix:path={runtime}/no-system-bus",
               XDG_CONFIG_DIRS=env["XDG_CONFIG_HOME"], XDG_DATA_DIRS=env["XDG_DATA_HOME"])
    with subprocess.Popen([runner, "--config-file=" + str(config), "--", "/bin/sh", "-c",
                           'printf "%s\\n" "$DBUS_SESSION_BUS_ADDRESS"; read -r _ || :'],
                          cwd=root, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE) as bus:
        try:
            with selectors.DefaultSelector() as sel:
                sel.register(bus.stdout, selectors.EVENT_READ)
                check(sel.select(5), "private D-Bus did not publish an address")
                published = bus.stdout.readline().decode().strip()
            check(published == address or published.startswith(address + ",guid="),
                  "private D-Bus did not publish its expected Unix address")
            env = dict(env, DBUS_SESSION_BUS_ADDRESS=published)
            with socket.socket(socket.AF_UNIX) as listener, (root / "ksecretd.log").open("wb") as log:
                listener.bind(str(bootstrap))
                listener.listen()
                r, w = os.pipe()
                try:
                    # This is KDE's 56-byte PAM-derived key input, but no actual
                    # PAM/authentication is used, just a random disposable key.
                    try:
                        os.write(w, os.urandom(56))
                    finally:
                        os.close(w)
                    daemon = subprocess.Popen([kde, "--pam-login", str(r), str(listener.fileno())],
                        pass_fds=(r, listener.fileno()), cwd=root, stdout=log, stderr=log,
                        env=dict(env, PAM_KWALLET5_LOGIN="1", QT_QPA_PLATFORM="offscreen"))
                finally:
                    os.close(r)
                try:
                    with socket.socket(socket.AF_UNIX) as client:
                        client.settimeout(2)
                        client.connect(str(bootstrap))
                    wallet = Wallet(busctl, root, env)

                    def ready():
                        check(daemon.poll() is None, "ksecretd exited during disposable wallet bootstrap")
                        try:
                            return wallet.call(SERVICE_PATH, SERVICE, "ReadAlias", "s", "default")
                        except Failure:
                            return None

                    collection, = wait("ksecretd native Secret Service", ready, timeout=5)
                    check(wallet.owner_pid() == daemon.pid, "the spawned ksecretd does not own Secret Service")
                    check(collection != "/", "ksecretd did not create a default disposable wallet")
                    locked, = wallet.call(collection, "org.freedesktop.DBus.Properties", "Get", "ss",
                                          "org.freedesktop.Secret.Collection", "Locked")
                    check(locked == {"type": "b", "data": False}, "disposable KDE wallet is not unlocked")
                    yield env, wallet
                finally:
                    stop(daemon)
        finally:
            bus.stdin.close()
            stop(bus, graceful=True)


def sign_out(window):
    window.click("Settings")
    window.title("Settings")

    def visible_sign_out():
        elements = window.elements()
        viewport = element_id(elements, "catalog-scroll")
        check(viewport, "missing Settings scroll viewport")
        left, top, right, bottom = bounds(viewport)
        for item in elements:
            if window.matches(item, "Sign out") and item.get("accessibleRole") == "Button":
                x, y, x2, y2 = bounds(item)
                if left <= x < x2 <= right and top <= y < y2 <= bottom:
                    return item
        # Settings can place Account below the fold. Use the actual viewport
        # rather than a fixed bottom margin based on an assumed dock height.
        window.mcp.call("dispatch_pointer_scroll", {"windowHandle": window.handle,
            "position": {"x": (left + right) / 2, "y": (top + bottom) / 2}, "deltaX": 0, "deltaY": -360})
        return None

    window.click_element(wait("visible Account / Sign out in Settings", visible_sign_out))
    window.find("Sign in", roles=("Button",))


def play_pause(window, receipt, restore_mark, pcm):
    from audio_smoke import heard_tone
    open_playlists(window)
    open_manual(window)
    check(("owner", True) not in receipt.events(restore_mark), "browsing acquired a Go output owner")
    mark = len(receipt.events())
    baseline = pcm.stat().st_size
    window.click("Play")
    wait("owned canonical Go playlist selection", lambda: ("owned-playlist-selection",) in receipt.events(mark),
         timeout=15)
    events = receipt.events(mark)
    check(events.count(("client", "play")) == 1 and ("playlist-play", True) in events,
          "one native Playlist Play did not send one correctly scoped command")
    print("PASS explicit native Playlist Play acquired the Go output and selected the canonical queue", flush=True)

    # Do not accept a second Play or a protocol-injected Pause: the native
    # transport must become active with real Go media and its private null sink.
    wait("Go media request after explicit Play", lambda: ("media",) in receipt.events(mark))
    wait("220 Hz from restored desktop", lambda: heard_tone(pcm, hz=220, start=baseline), 5)
    pause_mark = len(receipt.events())
    window.click("Pause")
    wait("Go Pause command", lambda: ("client", "pause") in receipt.events(pause_mark))
    print("PASS native Pause reached real Go (private null output)", flush=True)


def checks(binary, root, env, go, ready, receipt, wallet, *, playback, socket_ready_at):
    check(not wallet.items(), "disposable KDE wallet was not empty at startup")
    with desktop(binary, root, env) as window:
        disabled_login(window)
        window.fill(SERVER, ready["origin"])
        window.fill("Username", ready["username"])
        window.fill("Password", ready["password"])
        window.click(REMEMBER_LOGIN)
        check(login_options(window, required=True).get("accessibleChecked"), "native opt-in was not checked")
        window.click("Sign in")
        window.section("History")
        check([e for e in receipt.events() if e[0] == "login"] == [("login", "POST", 200, "rolling-v1")],
              "expected one successful Go rolling password login")
        slot = wait("desktop to publish a KDE-backed active slot", lambda: store_metadata(env).get("active"))
        items = wait("production item in native KDE wallet", lambda: wallet.items(slot))
        check(len(items) == 1 and wallet.items() == items, "expected exactly one KDE credential for the active slot")
        no_profile_secrets(env, ready, receipt)
    print("PASS native opt-in created one slot-matched item in the real KDE wallet", flush=True)

    sleep(max(0, socket_ready_at - monotonic()))
    receipt.allow_socket.set()
    failures = []
    mark = len(receipt.events())
    with desktop(binary, root, env) as window:
        window.section("History")
        wait("Go session validation on desktop restart", lambda: ("session", "GET", 200) in receipt.events(mark))
        # Observe beyond the opening snapshot: Go can also assign ownership
        # in response to a playback-capable client's first presence pulse.
        wait("Go output state and two post-hello presence pulses",
             lambda: ("owner", False) in receipt.events(mark)
             and receipt.events(mark).count(("client", "presence")) >= 2)
        events = receipt.events(mark)
        check(("client", "hello") in events and not any(e[0] == "login" for e in events),
              "desktop did not rejoin Go without password authentication")
        check(not any(e in events for e in (("owner", True), ("client", "select"), ("client", "play"), ("media",))),
              "desktop restart selected an audio output or started playback")
        check(wallet.items() == wallet.items(slot) == items and store_metadata(env).get("active") == slot,
              "desktop restart did not retain only the same credential")
        no_profile_secrets(env, ready, receipt)
        print("PASS desktop restart recovered from KDE and validated with Go without password or automatic audio", flush=True)
        if playback is not None:
            with verify(failures, "optional native Play/Pause"):
                play_pause(window, receipt, mark, playback)
        api = Api(go)
        api.cookie = receipt.rolling_cookie()
        check(api.cookie, "Go did not issue a rolling cookie")
        logout_mark = len(receipt.events())
        sign_out(window)
        wait("real Go logout", lambda: ("logout", "POST", 204) in receipt.events(logout_mark))
        api.request("/api/auth/session", status=401)
        wait("logout to remove the KDE item and local restore markers",
             lambda: not wallet.items() and not {"active", "cleanup"} & store_metadata(env).keys())
        no_profile_secrets(env, ready, receipt)
    print("PASS native logout deleted the KDE item, disarmed metadata and revoked the Go session", flush=True)

    mark = len(receipt.events())
    with desktop(binary, root, env) as window:
        disabled_login(window, ready["origin"], ready["username"])
        assert_idle(window, receipt, mark)
    check(not wallet.items(), "post-logout restart recreated a KDE credential")
    print("PASS post-logout restart stayed signed out with only prefill", flush=True)
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--desktop", type=Path, required=True, help="prebuilt Slint MCP debug desktop")
    parser.add_argument("--fixture-binary", type=Path, required=True, help="prebuilt Go offline-fixture")
    parser.add_argument("--scratch-dir", type=Path, help="existing disposable scratch base (e.g. /dev/shm)")
    parser.add_argument("--play-pause", action="store_true", help="also exercise explicit native Play/Pause")
    args = parser.parse_args()
    check(shutil.which("cage"), "native acceptance needs Cage's headless Wayland compositor")
    binary, go_binary = (p.resolve(strict=True) for p in (args.desktop, args.fixture_binary))
    scratch = args.scratch_dir.resolve(strict=True) if args.scratch_dir else None
    with TemporaryDirectory(prefix="spofify-kde-", dir=scratch) as name:
        root = Path(name).resolve()
        with native_wallet(root, private_env(root)) as (env, wallet), fixture(go_binary, root, env) as go:
            socket_ready_at = monotonic() + 16  # Match the Go fixture's startup output drain fence.
            print(f"EVIDENCE native Slint + real Go {go['sourceRevision']} + private native KDE Secret Service", flush=True)
            with ExitStack() as stack:
                pcm = None
                if args.play_pause:
                    from audio_smoke import output
                    audio_env = dict(env)
                    pcm = stack.enter_context(output(root, audio_env))
                    # ALSA/PipeWire are isolated; the app still needs the private wallet bus.
                    env.update({key: audio_env[key] for key in ("ALSA_CONFIG_PATH", "PIPEWIRE_RUNTIME_DIR", "PULSE_SERVER")})
                ready, receipt = stack.enter_context(recorded_origin(go))
                failures = checks(binary, root, env, go, ready, receipt, wallet,
                                  playback=pcm, socket_ready_at=socket_ready_at)
    check(not failures, "; ".join(failures))


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except (RuntimeError, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f"FAIL: {error or type(error).__name__}", file=sys.stderr)
        sys.exit(1)
