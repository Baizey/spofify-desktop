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


WIDE = (1280, 720)
COMPACT = (640, 640)
DOCK_TITLE = r"Now playing: Synthetic Tone \d{3}"
ESCAPE, CONTROL, ALT, LEFT = "\x1b", "\x11", "\x12", "\uf702"


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
    raise Failure(f"timed out waiting for {label}")


class Window:
    def __init__(self, mcp, handle, output, resize_output=None):
        self.mcp, self.handle, self.output = mcp, handle, output
        self.resize_output = resize_output
        self.properties = self.window_properties()
        self.pictures = set()

    def window_properties(self):
        return self.mcp.value("get_window_properties", {"windowHandle": self.handle})

    def resize(self, mode):
        before = self.window_properties()["size"]
        self.resize_output(mode)
        def resized():
            size = self.window_properties()["size"]
            return size if size["width"] == mode[0] and size != before else None
        return wait(f"same window resized to {mode[0]} physical px (was {before})", resized)

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

    def key(self, text, event="PressAndRelease"):
        self.mcp.call("dispatch_key_event", {"windowHandle": self.handle, "text": text, "eventType": event})

    def shortcut(self, modifier, key):
        self.key(modifier, "Press")
        try:
            self.key(key)
        finally:
            self.key(modifier, "Release")

    def fill(self, pattern, value):
        self.click_element(self.find(pattern, roles=("TextInput",)))
        # Type through the real input rather than assigning a model property.
        self.key(value)

    def scroll_catalog(self, delta):
        width = self.window_properties()["size"]["width"] / self.properties["scaleFactor"]
        self.mcp.call("dispatch_pointer_scroll", {"windowHandle": self.handle,
            "position": {"x": width / 2, "y": 350}, "deltaX": 0, "deltaY": delta})

    def drag_slider(self, fraction):
        slider = self.find("Playback position", roles=("Slider",))
        check(slider.get("accessibleEnabled"), "playback slider is disabled")
        # Drag the visible rail, not a model value or the hidden Slider's center.
        rail = wait("rendered seek rail", lambda: element_type(self.elements(), "SeekBar"), 3)
        x, y, right, bottom = bounds(rail)
        check(right - x >= 32 and bottom > y, f"rendered seek rail has no usable pointer target: {bounds(rail)}")
        self.mcp.call("drag_element", {"elementHandle": rail["handle"],
            "target": {"x": x + (right - x) * fraction, "y": (y + bottom) / 2}})

    def pixels(self, name, expected_width=None):
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
        size = self.window_properties()["size"]
        check((width, height) == (size["width"], size["height"]), "PNG and native window dimensions differ")
        if expected_width is not None:
            check(width == expected_width, f"{name}: expected {expected_width} px, got {width} px")
        digest = hashlib.sha256(data).hexdigest()
        check(digest not in self.pictures, f"native frame unchanged at {name}")
        self.pictures.add(digest)
        if self.output:
            with (self.output / (name + ".png")).open("xb") as file:
                file.write(data)
        print(f"PASS native pixels {name}: {width}x{height}", flush=True)


def element_type(elements, name):
    return next((e for e in elements if any(t.get("typeName") == name for t in e.get("typeNamesAndIds", []))), None)


def bounds(element):
    pos, size = element.get("absolutePosition", {}), element.get("size", {})
    x, y = pos.get("x", 0), pos.get("y", 0)
    return x, y, x + size.get("width", 0), y + size.get("height", 0)


@contextmanager
def verify(failures, label):
    """Continue independent evidence collection; every failed assertion still fails the run."""
    try:
        yield
    except (Failure, TimeoutError) as error:
        failures.append(f"{label}: {error}")
        print(f"FAIL {failures[-1]}", flush=True)


def assert_dock_layout(window):
    elements = window.elements()
    def named(pattern):
        return next((e for e in elements if window.matches(e, pattern) and e.get("accessibleRole") == "Button"), None)
    dock = element_type(elements, "PlayerBar")
    catalog = next((e for e in elements if any(t.get("id", "").split("::")[-1] == "catalog-scroll"
                   for t in e.get("typeNamesAndIds", []))), None)
    check(dock and catalog, "missing catalog viewport or dock in the Slint tree")
    left, top, right, bottom = bounds(dock)
    props = window.window_properties()
    width, height = (props["size"][key] / props["scaleFactor"] for key in ("width", "height"))
    check(abs(bounds(catalog)[3] - top) <= 1, "catalog viewport and dock do not meet without overlap")
    check(left == 0 and abs(right - width) <= 1 and abs(bottom - height) <= 1 and 48 < bottom - top <= 100,
          f"dock not anchored to client bottom or unexpected height: {bounds(dock)}")
    transport = element_type(elements, "Transport")
    check(transport, "missing dock transport")
    midpoint = sum(bounds(transport)[::2]) / 2
    check(abs(midpoint - width / 2) <= 1, "dock transport is not centered")
    rail = element_type(elements, "SeekBar")
    check(rail, "populated dock must render a seek rail at both widths")
    x, y, x2, y2 = bounds(rail)
    check(left <= x < x2 <= right and bounds(transport)[3] <= y < y2 <= bottom,
          "dock seek rail overlaps transport or leaves the dock")
    previous_right = left
    for label, control in ((DOCK_TITLE, named(DOCK_TITLE)), ("Transport", transport),
                           ("Play queue", named("Play queue")), ("Playback devices", named("Playback devices"))):
        check(control, f"missing dock control {label}")
        x, y, x2, y2 = bounds(control)
        check(previous_right <= x < x2 <= right and top <= y < y2 <= bottom,
              f"dock control {label} is clipped or overlaps another group: {bounds(control)}")
        previous_right = x2
    print(f"PASS dock geometry: {width:g}×{height:g} logical, height {bottom - top:g}, controls disjoint", flush=True)


def full_player(window, title, name, width, failures):
    window.click(title)  # Strict pointer activation: no keyboard or accessibility fallback.
    try:
        window.find("NOW PLAYING", timeout=3, roles=("Text",))
    except Failure:
        window.pixels(name + "-pointer-failed", width)
        raise
    print(f"PASS {width}px dock identity opens full player by pointer", flush=True)
    window.find("Open play queue", roles=("Button",))
    window.find(r"Synthetic Tone \d{3}", roles=("Text",))
    window.pixels(name, width)
    with verify(failures, name + " slider hit target"):
        slider = window.find("Playback position", roles=("Slider",))
        x, y, right, bottom = bounds(slider)
        check(right - x >= 32 and bottom > y, f"full-player slider has no usable pointer target: {bounds(slider)}")
    if not any(window.matches(e, "Lyrics") for e in window.elements()):
        window.scroll_catalog(-360)
        window.find("Lyrics", roles=("Text",), timeout=3)
        window.pixels(name + "-details", width)
        window.scroll_catalog(360)


@contextmanager
def desktop(binary, root, env, pixels=None, *, multisize=False):
    cage = shutil.which("cage")
    check(cage, "native acceptance needs cage (headless Wayland only)")
    randr = shutil.which("wlr-randr") if multisize else None
    check(not multisize or randr, "native multisize acceptance needs wlr-randr")
    # The reserved port is closed just before launch. Only connect to the spawned
    # debug process while it is alive, and run this in a private worker if available.
    with socket.socket() as reserved:
        reserved.bind(("127.0.0.1", 0))
        port = reserved.getsockname()[1]
    env = dict(env, WLR_BACKENDS="headless", WLR_RENDERER="pixman", WLR_HEADLESS_OUTPUTS="1",
               SLINT_BACKEND="winit-software", SLINT_MCP_PORT=str(port))
    if "ALSA_CONFIG_PATH" not in env:
        alsa = root / "asound-null.conf"
        alsa.write_text('pcm.!default { type null hint { show on description "Disposable acceptance sink" } }\n')
        env["ALSA_CONFIG_PATH"] = str(alsa)
    check("DISPLAY" not in env and "WAYLAND_DISPLAY" not in env and "WAYLAND_SOCKET" not in env,
          "native acceptance requires a private display environment")
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

            def resize_output(mode):
                check(process.poll() is None, "owned Cage not running")
                runtime = Path(env["XDG_RUNTIME_DIR"])
                displays = [path for path in runtime.glob("wayland-*") if path.is_socket()]
                check(len(displays) == 1, "expected exactly one owned Wayland socket")
                display_env = dict(env, WAYLAND_DISPLAY=str(displays[0]))
                listing = subprocess.run([randr], env=display_env, cwd=root, capture_output=True,
                                         text=True, timeout=5)
                check(listing.returncode == 0, "cannot read owned Cage output: " + listing.stderr.strip())
                outputs = re.findall(r"^(HEADLESS-\d+)\s", listing.stdout, re.MULTILINE)
                check(len(outputs) == 1, "expected exactly one owned headless output")
                width, height = mode
                changed = subprocess.run([randr, "--output", outputs[0], "--custom-mode", f"{width}x{height}@60Hz"],
                                         env=display_env, cwd=root, capture_output=True, text=True, timeout=5)
                check(changed.returncode == 0, "cannot resize owned Cage output: " + changed.stderr.strip())

            yield Window(mcp, handle, pixels, resize_output if multisize else None)
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


def native_checks(binary, root, env, ready=None, api=None, pixels=None, pcm=None):
    failures = []
    with desktop(binary, root, env, pixels, multisize=api is not None) as window:
        window.find("Sign in")
        check(not any(e.get("accessibleRole") == "Checkbox" for e in window.elements()), "unexpected sign-in gate")
        window.pixels("01-login-empty")
        if ready is None:
            print("NOT RUN native sign-in/browsing/player (window-only run)", flush=True)
            return
        window.fill("Server( origin| URL| address)?", ready["origin"])
        window.fill("Username", ready["username"])
        window.fill("Password", ready["password"])
        window.click("Sign in")
        window.section("History")
        check(not any(window.matches(e, "Liked|Like|Unlike|Home|Search") and e.get("accessibleRole") == "Button" for e in window.elements()), "obsolete navigation entry")
        window.pixels("02-signed-in")

        window.click("Playlists")
        window.section("Playlists")
        window.click("Open Synthetic Manual Duplicates")
        wait("distinct visible duplicate playlist rows", lambda: sum(window.matches(element, "Open Synthetic Tone 001")
             for element in window.elements()) >= 2)
        window.pixels("03-manual-playlist")
        print("PASS native sign-in, playlist navigation and duplicate rows", flush=True)
        if api is None:
            with verify(failures, "Ctrl+F focuses the live search field"):
                window.shortcut(CONTROL, "f")
                window.key("Synthetic Tone")
                window.find("Clear search", timeout=2, roles=("Button",))
            if failures:
                window.fill("Search music", "Synthetic Tone")
            window.find("SEARCH RESULTS", roles=("Text",))
            window.find("Open Synthetic Tone 001")
            before_back = len(failures)
            with verify(failures, "Escape returns from Search to the playlist"):
                window.key(ESCAPE)
                window.find("TRACKS", timeout=2, roles=("Text",))
            if len(failures) != before_back:
                window.click("Back")
            window.find("Open Synthetic Tone 001")
            before_back = len(failures)
            with verify(failures, "Alt+Left returns from the playlist to the listing"):
                window.shortcut(ALT, LEFT)
                window.find("Open Synthetic Manual Duplicates", timeout=2)
            if len(failures) != before_back:
                window.click("Back")
            window.find("Open Synthetic Manual Duplicates")
            window.click("Open Synthetic Manual Duplicates")
            window.find("Open Synthetic Tone 001")
            print("PASS typed search and repeated playlist open (synthetic wire only)", flush=True)
            window.click("Show actions for Synthetic Tone 001")
            window.find("Hide actions for Synthetic Tone 001")
            window.click("Play queue")
            window.find("The queue is empty")
            window.pixels("04-empty-queue")
            window.click("Back")
            window.find("Open Synthetic Tone 001")
            check(not any(window.matches(node, "Hide actions for Synthetic Tone 001") for node in window.elements()),
                  "playlist row actions stayed open after navigating away and back")
            check(not any(window.matches(e,"Now playing: Nothing playing") for e in window.elements()), "idle dock offers an empty player")
            window.click("Settings")
            window.find("Reset display defaults")
            window.pixels("05-appearance")
            print("PASS closed row actions after Back, empty queue, idle dock and appearance (UI only)", flush=True)
            check(not failures, "; ".join(failures))
            return
        with verify(failures, "playlist menu partition"):
            options = r"Rename|Delete|Edit rules"
            check(not any(window.matches(e, options) for e in window.elements()),
                  "playlist management is visible before opening Playlist options")
            window.click("Playlist options")
            for option in ("Rename", "Delete"):
                window.find(option, timeout=3, roles=("Button",))
            window.click("Playlist options")
            check(not any(window.matches(e, options) for e in window.elements()),
                  "playlist management stayed open after toggling Playlist options")
            print("PASS playlist management is behind Playlist options", flush=True)
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

            if pcm is not None:
                from audio_smoke import heard_tone
                baseline = pcm.stat().st_size
            window.click("Play Synthetic Tone 001")
            try:
                observer.wait(canonical_track, timeout=20)
            except TimeoutError as error:
                raise Failure("desktop Play did not update the canonical online queue") from error
            if pcm is not None:
                with verify(failures, "real Go online PCM"):
                    wait("220 Hz Go fixture tone at the private monitor", lambda: heard_tone(pcm, hz=220, start=baseline), 5)
                    print("PASS real Go online track rendered 220 Hz PCM", flush=True)
            window.find(DOCK_TITLE)
            print("PASS native Play updated canonical online queue and dock", flush=True)
            window.pixels("04-dock-wide", WIDE[0])
            with verify(failures, "wide geometry"):
                assert_dock_layout(window)
            window.resize(COMPACT)
            window.find(DOCK_TITLE)
            window.pixels("05-dock-compact", COMPACT[0])
            with verify(failures, "compact geometry"):
                assert_dock_layout(window)
            with verify(failures, "compact full player"):
                full_player(window, DOCK_TITLE, "06-player-compact", COMPACT[0], failures)
                window.click("Open play queue")
                window.find("Shuffle", roles=("Button",))
                window.find("Repeat: (off|one|all)", roles=("Button",))
                window.click("Back")
                window.find("Open play queue", roles=("Button",))
            if not any(window.matches(e, "Open Synthetic Tone 001") for e in window.elements()):
                window.click("Back")
                window.find("Open Synthetic Tone 001")
            window.resize(WIDE)

            window.click("Playback devices")
            window.find("Acceptance observer")
            peer_button = window.find("Select output")
            check(not peer_button.get("accessibleEnabled"), "read-only remote peer was selectable for audio")
            window.pixels("07-devices")
            print("PASS native device panel shows another connection without selecting an incapable peer", flush=True)

        window.click("Albums")
        window.section("Albums")
        window.find("Open Synthetic Paging Album", roles=("Button",))
        window.pixels("08-album-grid", WIDE[0])
        window.click("Open Synthetic Paging Album")
        window.find("Open Synthetic Tone 001")
        window.pixels("09-album-detail", WIDE[0])
        print("PASS native album browsing through the real Go catalog", flush=True)

        window.click("Show actions for Synthetic Tone 001")
        for option in ("Album", "Artist", "Add to queue", "Add to playlist", "Save offline"):
            window.find(option + " · Synthetic Tone 001", roles=("Button",))
        check(not any(window.matches(e, r"(?:Like|Unlike)(?: · .*)?") for e in window.elements()),
              "obsolete Like action in track menu")
        window.click("Save offline · Synthetic Tone 001")
        window.find(r"Saved on this device\.", 15)
        window.section("Downloads", retry_navigation=True)
        window.find("Open Synthetic Tone 001")
        window.click("Work offline")
        window.find("Go online")
        window.find("No song selected")
        window.find("Play Synthetic Tone 001", 15)
        window.pixels("10-offline-ready")
        if pcm is not None:
            baseline = pcm.stat().st_size
        window.click("Play Synthetic Tone 001")
        window.find("Now playing: Synthetic Tone 001")
        window.pixels("11-offline-dock")
        if pcm is not None:
            with verify(failures, "real Go saved/offline PCM"):
                wait("220 Hz saved Go fixture tone at the private monitor", lambda: heard_tone(pcm, hz=220, start=baseline), 5)
                print("PASS saved Go track rendered 220 Hz PCM while offline", flush=True)
        print("PASS native save, manual offline switch and saved-audio dock", flush=True)
        with verify(failures, "wide full player"):
            full_player(window, "Now playing: Synthetic Tone 001", "12-now-playing-wide", WIDE[0], failures)
    check(not failures, "; ".join(failures))
