#!/usr/bin/env python3
"""Native cold offline relaunch acceptance with synthetic wire and Secret Service.

Requires a debug desktop built with SLINT_EMIT_DEBUG_INFO=1 and --features
slint/mcp, headless Cage with wlr-foreign-toplevel-management, dbus-daemon,
and Python dbus-python/PyGObject. No Go binary, host display, keyring or
sound device. Optional --pixels-dir retains authenticated synthetic scenes.
"""
import argparse
from contextlib import contextmanager
from email.utils import formatdate
from http import server as http_server
import json
from pathlib import Path
import selectors
import shutil
import signal
import socket
from socketserver import BaseRequestHandler, TCPServer
import subprocess
import sys
from tempfile import TemporaryDirectory
from threading import Event, Lock, Thread
from time import sleep, time
from types import SimpleNamespace
from unittest.mock import patch
from urllib.error import URLError
from urllib.parse import urlsplit
from xml.sax.saxutils import escape

sys.dont_write_bytecode = True
import wire_ui
from audio_smoke import child
from fixture import Failure, check, private_env, stop
from native import REMEMBER_LOGIN, SAVED, Window, login_options, mcp_class, wait
from remember_login import SERVER, disabled_login, interrupted, sign_out, store_metadata
from remember_login_output import close_toplevel
from workspace_checks import open_manual, open_playlists

LOGIN, SESSION = "/api/auth/login", "/api/auth/session"
MODE = "X-Spofify-Session-Mode"
ROLLING = "rolling-v1"
KNOWN_ROUTES = {LOGIN, SESSION, "/api/auth/logout", wire_ui.TICKET_PATH, wire_ui.SOCKET_PATH,
                "/api/library", "/search"}
SEEDED_SLOT = "00000000000040008000000000000001"
SEEDED_USER = "synthetic-wire"
SEEDED_ACCOUNT = "wire-account-01"


class Receipt:
    def __init__(self):
        self.log = []
        self.lock = Lock()
        self.probe_pending = Event()
        self.probe_release = Event()
        self.probe_release.set()

    def record(self, *row):
        with self.lock:
            self.log.append(row)

    def events(self, mark=0):
        with self.lock:
            return self.log[mark:]


@contextmanager
def recorded_wire(port=0):
    """Observe fixture ingress, including failed HTTP attempts and socket writes."""
    receipt = Receipt()

    def route(handler):
        path = urlsplit(handler.path).path
        return path if path in KNOWN_ROUTES else "<other route>"

    class Server(http_server.ThreadingHTTPServer):
        def __init__(self, address, fixture_handler):
            class Handler(fixture_handler):
                def parse_request(self):
                    result = super().parse_request()
                    if result:
                        receipt.record("http", self.command, route(self))
                    return result

                def send_response(self, code, message=None):
                    receipt.record("status", route(self), code)
                    super().send_response(code, message)

                def reply(self, value=None, status=200, headers=None):
                    if (status == 200 and self.path == LOGIN and self.headers.get(MODE) == ROLLING) or (
                            status == 200 and self.path == SESSION):
                        check(headers and "Set-Cookie" in headers, "missing fixture bearer")
                        headers = dict(headers, **{MODE: ROLLING})
                        headers["Set-Cookie"] += "; Expires=" + formatdate(time() + 3600, usegmt=True)
                        if self.path == LOGIN:
                            value = dict(value, sessionMode=ROLLING)
                    super().reply(value, status, headers)

                def do_GET(self):
                    if self.path != SESSION:
                        return super().do_GET()
                    if not self.trusted() or not self.owned():
                        return
                    # The inherited fixture's session GET is authenticated but
                    # nonrolling; use the same summary and echo the same bearer.
                    receipt.probe_pending.set()
                    check(receipt.probe_release.wait(8), "test did not release the held session probe")
                    outer_reply = self.reply

                    def rolling_reply(value=None, status=200, headers=None):
                        if status == 200:
                            headers = {"Set-Cookie": self.headers["Cookie"] + "; Path=/; HttpOnly; SameSite=Lax"}
                        outer_reply(value, status, headers)

                    self.reply = rolling_reply
                    super().do_GET()

            super().__init__((address[0], port), Handler)

    class AccountWire(wire_ui.AccountWire):
        def message(self):
            value = super().message()
            kind = value.get("type") if isinstance(value, dict) else None
            if kind == "personal":
                command = value.get("command")
                kind = "syncPlaylist" if isinstance(command, dict) and command.get("type") == "syncPlaylist" else None
            if kind not in {"hello", "presence", "sync", "syncPlaylist", "release"}:
                kind = "<unexpected command>"
            receipt.record("client", kind)
            return value

    # Override only the two construction hooks inside the reused fixture in
    # this process, not the stdlib; keep its credential, ticket and frame checks.
    with patch.object(wire_ui, "http", SimpleNamespace(server=SimpleNamespace(
            BaseHTTPRequestHandler=http_server.BaseHTTPRequestHandler, ThreadingHTTPServer=Server))), \
            patch.object(wire_ui, "AccountWire", AccountWire), wire_ui.wire_fixture() as ready:
        try:
            yield ready, receipt
        finally:
            receipt.probe_release.set()


def run_secret_helper(path, seed_origin=None):
    """Child on the private bus; reuse the in-memory double's read/delete methods."""
    import os
    import re
    import remember_login_secret_service as ss
    import dbus.service
    from dbus.mainloop.glib import DBusGMainLoop
    from gi.repository import GLib

    class Collection(ss.Collection):
        @dbus.service.method(ss.Collection.IFACE, in_signature="a{sv}" + ss.SECRET + "b",
                             out_signature="oo", byte_arrays=True, sender_keyword="sender")
        def CreateItem(self, properties, secret, replace, sender=None):
            attrs = properties.get(ss.ITEM + ".Attributes", {})
            session, params, blob, content_type = secret
            service = self.service
            ss.require(service.sessions.get(session) == sender and not params and not replace
                       and content_type in ("text/plain; charset=utf8", "application/json")
                       and 0 < len(blob) <= 16384 and not service.items)
            ss.require(properties.get(ss.ITEM + ".Label") and set(attrs) == {"application", "slot"}
                       and attrs["application"] == "spofify-desktop-remembered-login-v1"
                       and re.fullmatch(r"[0-9a-f]{32}", attrs["slot"]))
            try:
                value = json.loads(bytes(blob))
            except (ValueError, UnicodeError):
                ss.require(False)
            ss.require(isinstance(value, dict))
            mode = "online"
            if value.get("format") == "local-v1":
                ss.require(set(value) == {"format", "credential", "username", "role"}
                           and value["username"] == SEEDED_USER and value["role"] == "user")
                value, mode = value["credential"], "offline"
            ss.require(isinstance(value, dict) and set(value) == {"origin", "accountId", "cookie"}
                       and value["origin"].startswith("http://127.0.0.1:")
                       and value["accountId"] == SEEDED_ACCOUNT
                       and value["cookie"].startswith("spofify_session="))
            item = service.path("item")
            service.items[item] = ss.Item(service, item, attrs, blob)
            service.note("store " + mode)
            return item, ss.NONE

    address = os.environ["DBUS_SESSION_BUS_ADDRESS"]
    ss.require(address == "unix:path=" + os.environ["XDG_RUNTIME_DIR"] + "/bus")
    ss.Collection = Collection
    DBusGMainLoop(set_as_default=True)
    bus = dbus.bus.BusConnection(address)
    name = dbus.service.BusName("org.freedesktop.secrets", bus=bus, do_not_queue=True)
    service = ss.Service(name, path)
    if seed_origin:
        # A static, never-issued dummy bearer. The receipt-only endpoint
        # cannot issue sessions or recognize it as a network credential.
        grant = dict(format="local-v1", username=SEEDED_USER, role="user",
                     credential=dict(origin=seed_origin, accountId=SEEDED_ACCOUNT,
                                     cookie="spofify_session=seeded-offline-no-server-session"))
        attrs = {"application": "spofify-desktop-remembered-login-v1", "slot": SEEDED_SLOT}
        item = service.path("item")
        service.items[item] = ss.Item(service, item, attrs, json.dumps(grant).encode())
    service.note("ready")
    GLib.MainLoop().run()


@contextmanager
def private_keyring(root, env, python, *, seed_origin=None):
    daemon = shutil.which("dbus-daemon")
    check(daemon, "dbus-daemon is required")
    address = "unix:path=" + env["XDG_RUNTIME_DIR"] + "/bus"
    check(len(address.removeprefix("unix:path=").encode()) < 104,
          "choose a shorter --scratch-dir for the private Unix bus socket")
    config = root / "bus.conf"
    config.write_text(f"""<busconfig><type>session</type><listen>{escape(address)}</listen>
        <auth>EXTERNAL</auth><policy context="default"><allow send_destination="*"/>
        <allow receive_sender="*"/><allow own="*"/></policy></busconfig>""")
    alsa = root / "alsa-null.conf"
    alsa.write_text('pcm.!default { type null hint { show on description "Disposable acceptance sink" } }\n')
    env = dict(env, DBUS_SESSION_BUS_ADDRESS=address, ALSA_CONFIG_PATH=str(alsa),
               DBUS_SYSTEM_BUS_ADDRESS="unix:path=" + env["XDG_RUNTIME_DIR"] + "/no-system-bus",
               XDG_CONFIG_DIRS=env["XDG_CONFIG_HOME"], XDG_DATA_DIRS=env["XDG_DATA_HOME"],
               PIPEWIRE_RUNTIME_DIR=env["XDG_RUNTIME_DIR"], PULSE_SERVER="unix:" + env["XDG_RUNTIME_DIR"] + "/no-pulse")
    path = root / "secret-receipt"

    def keyring():
        return path.read_text().splitlines()[1:] if path.exists() else []

    with subprocess.Popen([daemon, "--nofork", "--print-address=1", "--config-file=" + str(config)],
                          cwd=root, env=env, stdout=subprocess.PIPE) as bus:
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(bus.stdout, selectors.EVENT_READ)
                check(selector.select(5) and bus.stdout.readline().decode().startswith(address + ",guid="),
                      "private D-Bus did not publish its expected Unix address")
            # -I prevents ambient Python paths; explicitly add just this harness.
            helper = ("import sys; from pathlib import Path; sys.path.insert(0, sys.argv[1]); "
                      "from offline_session_relaunch import run_secret_helper; "
                      "run_secret_helper(Path(sys.argv[2]), sys.argv[3] or None)")
            command = [str(python), "-I", "-B", "-c", helper, str(Path(__file__).resolve().parent), str(path), seed_origin or ""]
            with subprocess.Popen(command,
                                  cwd=root, env=env) as service:
                try:
                    def ready():
                        check(service.poll() is None, "private Secret Service helper exited (requires dbus-python/PyGObject)")
                        return path.exists() and path.read_text() == "ready\n"
                    wait("private Secret Service helper", ready, timeout=5)
                    yield env, keyring
                finally:
                    stop(service)
        finally:
            stop(bus)


def quiet(receipt, mark, label):
    # Bounded native idle window, in addition to checking the entire cold
    # launch prefix at the end of each scenario and after process exit.
    sleep(1)
    check(not receipt.events(mark), f"{label}: unexpected wire traffic {receipt.events(mark)[:8]}")


def assert_online(receipt, mark):
    rows = receipt.events(mark)
    http = [r[1:] for r in rows if r[0] == "http"]
    check(http and http[0] == ("GET", SESSION) and http.count(("GET", SESSION)) == 1,
          f"expected exactly one initial session probe, got {http}")
    check(all(r in {("GET", SESSION), ("POST", wire_ui.TICKET_PATH), ("GET", wire_ui.SOCKET_PATH)} for r in http),
          f"unexpected HTTP during reconnect (including audio/output requests): {http}")
    check(all(event in rows for event in (("status", SESSION, 200), ("http", "POST", wire_ui.TICKET_PATH),
                                          ("status", wire_ui.SOCKET_PATH, 101), ("client", "hello"),
                                          ("client", "syncPlaylist"))),
          "missing successful session probe, account connection or normal playlist read")
    check(rows.index(("status", SESSION, 200)) < rows.index(("http", "POST", wire_ui.TICKET_PATH)),
          "account ticket requested before successful session response")
    commands = [r[1] for r in rows if r[0] == "client"]
    check(all(c in {"hello", "presence", "sync", "syncPlaylist"} for c in commands),
          f"unexpected reconnect commands (including output selection or resume): {commands}")


@contextmanager
def closeable_desktop(binary, root, env, pixels, mcp_port):
    # Like remember_login_output.closeable_desktop, but allow one repeatable
    # MCP port for hosts that authorize each worker-loopback endpoint exactly.
    with socket.socket() as reserved:
        if mcp_port:
            reserved.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        reserved.bind(("127.0.0.1", mcp_port))
        port = reserved.getsockname()[1]
    child_env = dict(env, WLR_BACKENDS="headless", WLR_RENDERER="pixman", WLR_HEADLESS_OUTPUTS="1",
                     SLINT_BACKEND="winit-software", SLINT_MCP_PORT=str(port))
    check(not {"DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"} & child_env.keys(),
          "native acceptance requires a private display environment")
    mcp = mcp_class()(port)
    with child(["cage", "--", str(binary)], root, child_env) as process:
        def discover():
            check(process.poll() is None, "owned Cage/desktop exited before readiness")
            try:
                windows = mcp.value("list_windows", {})["windowHandles"]
            except URLError:
                return None
            check(len(windows) <= 1, "expected only the owned Slint desktop window")
            return windows[0] if windows else None

        handle = wait(f"Slint MCP at 127.0.0.1:{port} on owned Wayland display", discover, 15)
        yield Window(mcp, handle, pixels)
        check(process.poll() is None, "owned Cage/desktop exited before the close request")
        displays = [p for p in Path(env["XDG_RUNTIME_DIR"]).glob("wayland-*") if p.is_socket()]
        check(len(displays) == 1, "expected one owned Cage display socket")
        close_toplevel(displays[0])  # Genuine Wayland CloseRequested; signals are cleanup only.
        code = process.wait(timeout=5)
        if code != 0:
            log = (root / "cage.log").read_bytes()[-2048:].decode(errors="replace").splitlines()
            raise Failure(f"owned Cage/desktop exited {code} after CloseRequested: {' | '.join(log[-8:])}")


def scenarios(binary, root, env, ready, receipt, keyring, pixels, mcp_port=0):
    def launch():
        return closeable_desktop(binary, root, env, pixels, mcp_port)

    def active_mode(mode):
        return (store_metadata(env).get("active") and not store_metadata(env).get("cleanup")
                and next((e for e in reversed(keyring()) if e.startswith("store ")), None) == "store " + mode)

    def retired():
        return not {"active", "cleanup"} & store_metadata(env).keys()

    def login(window, remember):
        meta = store_metadata(env)
        disabled_login(window, meta.get("origin", ""), meta.get("username", ""))
        if not meta:
            window.fill(SERVER, ready["origin"])
            window.fill("Username", ready["username"])
        if remember:
            window.click(REMEMBER_LOGIN)
            check(login_options(window, required=True).get("accessibleChecked"), "opt-in was not checked")
        window.fill("Password", ready["password"])
        actual_origin = window.find(SERVER, roles=("TextInput",)).get("accessibleValue")
        check(actual_origin == ready["origin"], "native server field did not retain the exact synthetic origin")
        mark = len(receipt.events())
        window.click("Sign in")
        try:
            window.section("History")
        except Failure as error:
            labels = sorted({e["accessibleLabel"] for e in window.elements()
                             if e.get("accessibleRole") == "Text" and e.get("accessibleLabel")})
            raise Failure(f"{error}; synthetic login receipt: {receipt.events(mark)[:12]}; visible text: {labels}") from error
        wait("synthetic account hello", lambda: ("client", "hello") in receipt.events(mark))
        if remember:
            wait("committed online credential", lambda: active_mode("online"))
        else:
            wait("saved opt-out prefill", lambda: store_metadata(env).get("username") == ready["username"] and retired())

    def work_offline(window, remember):
        window.click("Settings")
        window.click("Work offline")
        window.section("Settings")
        window.find("Go online", roles=("Button",))
        if remember:
            wait("committed explicit offline grant", lambda: active_mode("offline"))

    def offline_settings(window):
        window.click("Settings")
        window.title("Settings")
        window.title(ready["username"])
        window.find("Go online", roles=("Button",))
        window.absent("Work offline", roles=("Button",))

    def signed_out():
        mark, keys = len(receipt.events()), keyring()
        with launch() as window:
            disabled_login(window, ready["origin"], ready["username"])
            window.absent(SAVED, roles=("Button",))
            quiet(receipt, mark, "signed-out cold launch")
            window.find("Sign in", roles=("Button",))
        check(not receipt.events(mark) and keyring() == keys and retired(),
              "signed-out cold launch attempted authentication, authenticated access, or keyring mutation")

    with launch() as window:
        login(window, remember=True)
        work_offline(window, remember=True)
        mark = len(receipt.events())
        quiet(receipt, mark, "explicit Work offline before close")
    check(not receipt.events(mark), "closing explicit offline desktop touched synthetic wire")
    print("PASS explicit Work offline persisted before a clean native close", flush=True)

    mark, keys = len(receipt.events()), keyring()
    with launch() as window:
        offline_settings(window)
        window.absent("Sign in", roles=("Button",))
        check(keyring() == keys + ["lookup"], "cold offline admission did not read exactly one active item")
        if pixels:
            window.pixels("01-cold-offline-account")
        window.click(SAVED)
        window.section(SAVED)
        window.absent("Offline mode", roles=("Text",))
        if pixels:
            window.pixels("02-cold-saved-offline")
        offline_settings(window)
        quiet(receipt, mark, "cold offline admission")
        print("PASS cold offline relaunch shows the same synthetic account, without HTTP/account/audio wire traffic", flush=True)

        receipt.probe_pending.clear()
        receipt.probe_release.clear()
        try:
            window.click("Go online")
            wait("owned same-account session probe", receipt.probe_pending.is_set, timeout=5)
            window.find("Stay offline", roles=("Button",), timeout=3)
            sleep(1)  # Give other workers a chance to issue forbidden early traffic.
            window.absent("Work offline", roles=("Button",))
            pending = receipt.events(mark)
            check(pending == [("http", "GET", SESSION)],
                  f"only the session probe may run before admission, got {pending}")
        finally:
            receipt.probe_release.set()
        window.find("Work offline", roles=("Button",))
        wait("committed online choice", lambda: active_mode("online"))
        open_playlists(window)
        open_manual(window)
        window.find(f"Play track: {wire_ui.TRACK_NAME}", roles=("Button",))
        assert_online(receipt, mark)
        if pixels:
            window.pixels("03-reconnected-playlist")
        print("PASS Go online performs only the saved same-account probe before normal playlist access; no output/play commands or media", flush=True)

    mark = len(receipt.events())
    with launch() as window:
        window.section("History")
        open_playlists(window)
        open_manual(window)
        assert_online(receipt, mark)
        print("PASS successful Go online persists; cold relaunch validates and opens online", flush=True)
        work_offline(window, remember=True)

    mark = len(receipt.events())
    with launch() as window:
        offline_settings(window)
        keys = keyring()
        sign_out(window)
        wait("retired local grant", lambda: retired() and keyring() == keys + ["clear"])
        disabled_login(window, ready["origin"], ready["username"])
        quiet(receipt, mark, "direct offline sign-out")
    check(not receipt.events(mark), "direct offline sign-out or close touched synthetic wire")
    signed_out()
    print("PASS direct offline Sign out clears the active item locally; next launch stays signed out", flush=True)

    keys = keyring()
    with launch() as window:
        login(window, remember=False)
        work_offline(window, remember=False)
    check(retired() and keyring() == keys, "opt-out Work offline created or used remembered credentials")
    signed_out()
    print("PASS opt-out Work offline does not grant cold access", flush=True)


@contextmanager
def offline_receipt(port):
    """A private connection counter, not an HTTP server or authenticated fixture."""
    receipt = Receipt()

    class CountConnection(BaseRequestHandler):
        def handle(self):
            receipt.record("connection")  # No read, reply, or sensitive request logging.

    with TCPServer(("127.0.0.1", port), CountConnection) as listener:
        thread = Thread(target=listener.serve_forever, name="cold-offline-receipt")
        thread.start()
        try:
            yield f"http://127.0.0.1:{listener.server_address[1]}", receipt
        finally:
            listener.shutdown()
            thread.join()


def seeded_offline(binary, root, env, origin, receipt, keyring, pixels, mcp_port):
    profile = Path(env["XDG_CONFIG_HOME"]) / "spofify-desktop-login-v1" / "login.json"
    profile.parent.mkdir(mode=0o700)
    metadata = json.dumps(dict(version=1, origin=origin, username=SEEDED_USER, active=SEEDED_SLOT))
    profile.touch(mode=0o600, exist_ok=False)
    profile.write_text(metadata)

    print("SEEDED OFFLINE: cold admission, visual evidence and observed wire silence only; "
          "not Work offline persistence or Go online", flush=True)
    with closeable_desktop(binary, root, env, pixels, mcp_port) as window:
        window.click("Settings")
        window.title("Settings")
        window.title(SEEDED_USER)
        window.find("Go online", roles=("Button",))
        window.absent("Sign in", roles=("Button",))
        window.absent("Work offline", roles=("Button",))
        check(keyring() == ["lookup"], "cold seeded launch did not read exactly one active grant")
        if pixels:
            window.pixels("01-seeded-cold-offline-account")
        window.click(SAVED)
        window.section(SAVED)
        window.absent("Offline mode", roles=("Text",))
        if pixels:
            window.pixels("02-seeded-cold-saved-offline")
        quiet(receipt, 0, "seeded cold admission")
        print("PASS seeded native cold admission shows same synthetic account; no observed connections", flush=True)
        sign_out(window)
        wait("seeded active slot cleared", lambda: keyring() == ["lookup", "clear"]
             and not {"active", "cleanup"} & store_metadata(env).keys())
        disabled_login(window, origin, SEEDED_USER)
        quiet(receipt, 0, "seeded direct offline sign-out")
    check(not receipt.events(), "seeded offline launch/sign-out/close attempted to reach receipt listener")

    with closeable_desktop(binary, root, env, pixels, mcp_port) as window:
        disabled_login(window, origin, SEEDED_USER)
        quiet(receipt, 0, "seeded sign-out relaunch")
        window.absent(SAVED, roles=("Button",))
    check(not receipt.events() and keyring() == ["lookup", "clear"],
          "signed-out relaunch touched the private keyring or receipt listener")
    print("PASS seeded cold offline Sign out deletes the active item; relaunch stays signed out", flush=True)

    print("CHECK valid active disk slot without a matching private keyring item", flush=True)
    profile.write_text(metadata)  # Valid disk slot, no matching keyring item.
    with closeable_desktop(binary, root, env, pixels, mcp_port) as window:
        disabled_login(window, origin, SEEDED_USER)
        quiet(receipt, 0, "disk-only slot")
        window.absent(SAVED, roles=("Button",))
    print("CHECK corrupt disk profile", flush=True)
    profile.write_text("not a valid login profile")
    with closeable_desktop(binary, root, env, pixels, mcp_port) as window:
        disabled_login(window)
        quiet(receipt, 0, "corrupt disk profile")
        window.absent(SAVED, roles=("Button",))
    check(not receipt.events() and keyring() == ["lookup", "clear"],
          "disk-only or corrupt profile reached the receipt listener or obtained a secret")
    print("PASS missing keyring item and corrupt metadata grant neither access nor observed wire traffic", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--desktop", required=True, type=Path, help="prebuilt debug Slint desktop")
    parser.add_argument("--seeded-offline", action="store_true",
                        help="seed a test grant; only check cold offline UI, local sign-out and observed wire silence")
    parser.add_argument("--scratch-dir", type=Path, help="existing disposable scratch base (e.g. /dev/shm)")
    parser.add_argument("--pixels-dir", type=Path, help="existing directory for new synthetic native PNGs")
    parser.add_argument("--wire-port", type=int, default=0,
                        help="private wire 127.0.0.1 port for an exact worker TCP grant (default: ephemeral)")
    parser.add_argument("--mcp-port", type=int, default=0,
                        help="private Slint MCP 127.0.0.1 port for an exact worker TCP grant (default: ephemeral)")
    parser.add_argument("--dbus-python", default=Path("/usr/bin/python3"), type=Path,
                        help="Python with dbus-python and PyGObject")
    args = parser.parse_args()
    binary = args.desktop.resolve(strict=True)
    python = args.dbus_python.resolve(strict=True)
    scratch = args.scratch_dir.resolve(strict=True) if args.scratch_dir else None
    pixels = args.pixels_dir.resolve(strict=True) if args.pixels_dir else None
    check(all(0 <= port <= 65535 for port in (args.wire_port, args.mcp_port)), "ports must be 0–65535")
    check(not args.wire_port or args.wire_port != args.mcp_port, "wire and MCP require different ports")
    check(not pixels or pixels.is_dir(), "--pixels-dir must be an existing directory")
    check(shutil.which("cage"), "native acceptance needs Cage with wlr-foreign-toplevel-management")
    with TemporaryDirectory(prefix="spofify-cold-", dir=scratch) as name:
        root = Path(name).resolve()
        env = private_env(root)
        if args.seeded_offline:
            with offline_receipt(args.wire_port) as (origin, receipt), \
                    private_keyring(root, env, python, seed_origin=origin) as (env, keyring):
                seeded_offline(binary, root, env, origin, receipt, keyring, pixels, args.mcp_port)
        else:
            with private_keyring(root, env, python) as (env, keyring), \
                    recorded_wire(args.wire_port) as (ready, receipt):
                scenarios(binary, root, env, ready, receipt, keyring, pixels, args.mcp_port)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except (RuntimeError, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f"FAIL: {error or type(error).__name__}", file=sys.stderr)
        sys.exit(1)
