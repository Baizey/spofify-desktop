"""Drive the published Slint MCP against a real, isolated Wayland window."""
from base64 import b64decode
from contextlib import contextmanager
import hashlib
from importlib.util import module_from_spec, spec_from_file_location
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import struct
import subprocess
from time import monotonic, sleep
from urllib.error import URLError

from fixture import Failure, check, stop
from playback import Peer


def mcp_class():
    # Reuse the repository's upstream Slint MCP transport, not an app mock.
    helper = Path(__file__).resolve().parents[1] / "inspect-native.py"
    spec = spec_from_file_location("inspect_native", helper)
    module = module_from_spec(spec)
    spec.loader.exec_module(module)
    return module.Mcp


def wait(label, fn, timeout=10):
    end = monotonic() + timeout
    while monotonic() < end:
        result = fn()
        if result:
            return result
        sleep(0.15)
    raise Failure(f"native window did not reach: {label}")


class Window:
    def __init__(self, mcp, handle, output):
        self.mcp, self.handle, self.output = mcp, handle, output
        self.properties = mcp.value("get_window_properties", {"windowHandle": handle})
        self.pictures = set()

    def elements(self):
        tree = self.mcp.value("get_element_tree", {
            "elementHandle": self.properties["rootElementHandle"], "maxElements": 1000})
        check(not tree.get("debugInfoMissing"), "desktop requires SLINT_EMIT_DEBUG_INFO=1 at build time")
        # A long page may be truncated; the visible header and first visible rows come first.
        return tree["elements"]

    @staticmethod
    def matches(element, pattern):
        return re.fullmatch(pattern, element.get("accessibleLabel") or "", re.IGNORECASE) is not None

    def find(self, pattern, timeout=10, roles=None):
        def matches():
            return next((element for element in self.elements() if self.matches(element, pattern)
                         and (roles is None or element.get("accessibleRole") in roles)), None)
        return wait(pattern, matches, timeout)

    def section(self, label, retry_navigation=False):
        def current():
            element = next((node for node in self.elements() if self.matches(node, label)
                            and node.get("accessibleRole") == "Button"), None)
            if element and element.get("accessibleDescription") == "Current section":
                return element
            if retry_navigation and element:
                # A write may temporarily defer route changes. Retry only navigation,
                # never the underlying mutation.
                self.click_element(element)
            return None
        return wait(f"current section {label}", current)

    def click_element(self, element):
        self.mcp.call("click_element", {"elementHandle": element["handle"]})

    def click(self, pattern):
        element = self.find(pattern, roles=("Button", "Checkbox"))
        check(element.get("accessibleEnabled", True), f"native control disabled: {pattern}")
        self.click_element(element)

    def fill(self, pattern, value):
        element = self.find(pattern, roles=("TextInput",))
        self.click_element(element)
        # Dispatch text rather than assigning a model property: edits must fire the UI callback.
        self.mcp.call("dispatch_key_event", {"windowHandle": self.handle, "text": value})

    def pixels(self, name):
        for element in self.elements():
            check(not (self.matches(element, "Username|Password") and element.get("accessibleValue")),
                  "refusing to capture populated login fields")
        image = next((part for part in self.mcp.call("take_screenshot", {
            "windowHandle": self.handle})["content"] if part["type"] == "image"), None)
        check(image and image["mimeType"] == "image/png", "native screenshot did not return a PNG")
        data = b64decode(image["data"], validate=True)
        check(data[:8] == b"\x89PNG\r\n\x1a\n" and data[12:16] == b"IHDR" and len(data) > 4096,
              "native screenshot was empty or malformed")
        width, height = struct.unpack("!II", data[16:24])
        check(width >= 480 and height >= 440, "unexpected native window pixel dimensions")
        digest = hashlib.sha256(data).hexdigest()
        check(digest not in self.pictures, f"native frame unchanged at {name}")
        self.pictures.add(digest)
        if self.output:
            with (self.output / (name + ".png")).open("xb") as file:
                file.write(data)
        print(f"PASS native pixels {name}: {width}x{height}", flush=True)


@contextmanager
def desktop(binary, root, env, pixels, cage):
    # The reserved port is closed just before launch. Only connect to the spawned
    # debug process while it is alive, and run this in a private worker if available.
    with socket.socket() as reserved:
        reserved.bind(("127.0.0.1", 0))
        port = reserved.getsockname()[1]
    alsa = root / "asound.conf"
    alsa.write_text('pcm.!default { type null hint { show on description "Disposable acceptance sink" } }\n')
    env = dict(env, WLR_BACKENDS="headless", WLR_RENDERER="pixman", WLR_HEADLESS_OUTPUTS="1",
               SLINT_BACKEND="winit-software", SLINT_MCP_PORT=str(port), SPOFIFY_DEV_SIZE="1100x760",
               ALSA_CONFIG_PATH=str(alsa))
    mcp = mcp_class()(port)
    with (root / "desktop.log").open("w+b") as log:
        process = subprocess.Popen([cage, "--", str(binary)], env=env, cwd=root,
                                   stdout=log, stderr=log, start_new_session=True)
        try:
            def discover():
                if process.poll() is not None:
                    log.seek(0)
                    # No login inputs have been entered at this point.
                    tail = log.read(16384).decode(errors="replace").splitlines()[-5:]
                    raise Failure("owned Wayland compositor/desktop exited before readiness: " + " | ".join(tail))
                try:
                    windows = mcp.value("list_windows", {})["windowHandles"]
                except URLError:
                    return None
                check(len(windows) <= 1, "expected only the owned Slint desktop window")
                return windows[0] if windows else None
            handle = wait("Slint MCP on owned Wayland display", discover, 15)
            yield Window(mcp, handle, pixels)
        finally:
            # Cage and the desktop are in a fresh owned process group. Stop both
            # even if startup failed before an app window could be discovered.
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                stop(process)
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass


def native_checks(binary, root, env, ready=None, api=None, pixels=None):
    cage = shutil.which("cage")
    check(cage, "native acceptance requires an installed cage with headless Wayland (no host display used)")
    with desktop(binary, root, env, pixels, cage) as window:
        window.find("Sign in")
        window.pixels("01-login-empty")
        if ready is None:
            consent = r"Allow trusted.LAN HTTP"
            window.click(consent)
            check(window.find(consent, roles=("Checkbox",)).get("accessibleChecked"), "native HTTP consent click did not toggle")
            window.pixels("02-consent-checked")
            print("NOT RUN native sign-in/browsing/player (window-only run)", flush=True)
            return
        window.fill("Server( origin| URL| address)?", ready["origin"])
        window.fill("Username", ready["username"])
        window.fill("Password", ready["password"])
        window.click(r"Allow trusted.LAN HTTP")
        window.click("Sign in")
        window.find("Home", 15)
        window.section("Home")
        window.pixels("02-signed-in")

        window.click("Playlists")
        window.section("Playlists")
        window.click("Open Synthetic Manual Duplicates")
        wait("distinct visible duplicate playlist rows", lambda: sum(window.matches(element, "Open Synthetic Tone 001")
             for element in window.elements()) >= 2)
        window.pixels("03-manual-playlist")
        print("PASS native sign-in, playlist navigation and duplicate rows", flush=True)
        if api is None:
            window.click("Search")
            window.section("Search")
            window.fill("Search music", "Synthetic Tone")
            window.find("Open Synthetic Tone 001")
            window.click("Back")
            window.find("Open Synthetic Tone 001")
            window.click("Back")
            window.find("Open Synthetic Manual Duplicates")
            window.click("Open Synthetic Manual Duplicates")
            window.find("Open Synthetic Tone 001")
            print("PASS typed search, Back navigation and repeated playlist open (synthetic wire only)", flush=True)
            window.click("Show actions for Synthetic Tone 001")
            window.find("Hide actions for Synthetic Tone 001")
            window.click("Play queue")
            window.find("The queue is empty")
            window.pixels("04-empty-queue")
            window.click("Back")
            window.find("Open Synthetic Tone 001")
            check(not any(window.matches(node, "Hide actions for Synthetic Tone 001") for node in window.elements()),
                  "playlist row actions stayed open after navigating away and back")
            window.click("Now playing: Nothing playing")
            window.find(r"No lyrics are available for this track\.")
            window.click("Back")
            window.find("Open Synthetic Tone 001")
            window.click("Settings")
            window.find("Reset display defaults")
            window.pixels("05-appearance")
            print("PASS closed row actions after Back, empty queue, expanded player and appearance (UI only)", flush=True)
            return
        # Join only after the desktop: an observer must not become the initial
        # selected logical output or change how the desktop is admitted.
        with Peer(api) as observer:
            def selected_desktop(envelope):
                snapshot = envelope.get("snapshot", {})
                return snapshot.get("transfer") is None and any(
                    device.get("name") == "Spofify Desktop" and device.get("canPlay")
                    and device.get("deviceId") == snapshot.get("selectedDeviceId")
                    for device in snapshot.get("devices", []))

            # The initial output transfer can still be draining while the
            # catalog is already interactive. This case exercises Play on a
            # settled output, not rejection of actions during a handoff.
            if not selected_desktop(observer.snapshot):
                observer.wait(selected_desktop, timeout=20)
            window.click("Playback devices")
            window.find("Deselect")
            window.click("Back")
            window.find("Open Synthetic Tone 001")

            def canonical_track(envelope):
                state = envelope.get("snapshot", {}).get("state", {})
                queue, position = state.get("queue", []), state.get("position", -1)
                return (0 <= position < len(queue) and queue[position].get("id") == ready["duplicateTrackId"])

            window.click("Play Synthetic Tone 001")
            try:
                observer.wait(canonical_track, timeout=20)
            except TimeoutError as error:
                raise Failure("desktop Play did not update the canonical online queue") from error
            window.find(r"Now playing: Synthetic Tone \d{3}")
            print("PASS native Play updated canonical online queue and dock", flush=True)

            window.click("Playback devices")
            window.find("Acceptance observer")
            peer_button = window.find("Select output")
            check(not peer_button.get("accessibleEnabled"), "read-only remote peer was selectable for audio")
            window.pixels("04-devices")
            print("PASS native device panel shows another connection without selecting an incapable peer", flush=True)

        window.click("Home")
        window.section("Home")
        window.click("Albums")
        window.click("Open Synthetic Paging Album")
        window.find("Open Synthetic Tone 001")
        window.pixels("05-album")
        print("PASS native album browsing through the real Go catalog", flush=True)

        window.click("Show actions for Synthetic Tone 001")
        window.click("Save offline · Synthetic Tone 001")
        window.find(r"Saved on this device\.", 15)
        window.section("Offline", retry_navigation=True)
        window.find("Open Synthetic Tone 001")
        window.click("Work offline")
        window.find("Go online")
        window.find("Now playing: Nothing playing")
        window.find("Play Synthetic Tone 001", 15)
        window.pixels("06-offline-ready")
        window.click("Play Synthetic Tone 001")
        window.find("Now playing: Synthetic Tone 001")
        window.pixels("07-offline-dock")
        print("PASS native save, manual offline switch and saved-audio dock", flush=True)
