#!/usr/bin/env python3
"""Drive the account-free Settings example on a private native Wayland display.

SLINT_EMIT_DEBUG_INFO=1 cargo build --locked --features slint/mcp --example settings_acceptance
python3 scripts/acceptance/settings.py --shell target/debug/examples/settings_acceptance --capture-dir /path/to/new-directory
Requires Cage, wlr-randr, ImageMagick, and access to the owned Slint MCP localhost port.
"""
import argparse
from base64 import b64decode
from contextlib import suppress
from pathlib import Path
import shutil
import signal
import subprocess
import sys
from tempfile import TemporaryDirectory

sys.dont_write_bytecode = True
from fixture import Failure, check, private_env
from native import RETURN, bounds, desktop, element_id, element_type, verify, wait


TYPICAL, WIDE, MINIMUM = (1280, 800), (1740, 920), (480, 440)
PANELS = ("Listening", "Appearance on this device", "Account")
PALETTES = tuple(f"{name}, (?:choose|selected) palette" for name in ("Jade", "Ember", "Violet"))
DISPLAY = ("Dark", "Light", "Compact", "Standard", "Large", "Reset display defaults")
ACTIONS = ("Work offline", "Crossfade", "Manage downloads", *DISPLAY, *PALETTES, "Sign out")


def resize(window, size):
    width, height = size
    if window.dimensions() != size:
        # Probe above the app minimum to measure the Wayland decoration height.
        trial = height + 64
        measured = window.resize((width, trial))["height"]
        if measured != height:
            window.resize((width, trial + height - measured))
    check(window.dimensions() == size, f"expected exact client {size}; got {window.dimensions()}")


def contained(outer, inner):
    x, y, r, b = outer
    a, c, d, e = inner
    return x - 1 <= a < d <= r + 1 and y - 1 <= c < e <= b + 1


class View:
    def __init__(self, window, magick):
        self.window, self.magick = window, magick

    def root(self):
        return element_type(self.window.elements(), "Settings")

    def elements(self):
        root = self.root()
        return self.window.subtree(root) if root else []

    def find(self, name, role="Button", elements=None):
        elements = self.elements() if elements is None else elements
        matches = [e for e in elements if e.get("accessibleRole") == role and self.window.matches(e, name)]
        check(len(matches) <= 1, f"duplicate Settings {role}: {name}")
        return matches[0] if matches else None

    def viewport(self):
        scroll = element_id(self.window.elements(), "catalog-scroll")
        check(scroll, "missing Settings scroll viewport")
        return bounds(scroll)

    def scroll(self, delta):
        x, y, r, b = self.viewport()
        self.window.mcp.call("dispatch_pointer_scroll", {"windowHandle": self.window.handle,
            "position": {"x": (x + r) / 2, "y": (y + b) / 2}, "deltaX": 0, "deltaY": delta})

    def scan(self):
        # Slint omits fully clipped controls from inspection. Visit the page in
        # half-viewport increments, independent of the keyboard assertions.
        self.scroll(1600)
        _, y, _, b = self.viewport()
        previous_y = None
        for _ in range(20):
            elements = self.elements()
            check(elements, "Settings is not open")
            page_y = bounds(elements[0])[1]
            yield elements, page_y
            if page_y == previous_y:
                return
            previous_y = page_y
            self.scroll(-(b - y) / 2)

    def locate(self, name, role="Button"):
        match = self.find(name, role)
        if match:
            return match
        offsets = []
        for elements, page_y in self.scan():
            offsets.append(round(page_y))
            match = self.find(name, role, elements)
            if match:
                return match
        raise Failure(f"missing Settings {role}: {name} (page y during scroll: {offsets})")

    def visible(self, name):
        def revealed():
            match = self.find(name)
            return match if match and contained(self.viewport(), bounds(match)) else None
        return wait(f"fully revealed Settings action: {name}", revealed, timeout=2)

    def reveal(self, name, role="Button"):
        _, y, _, b = bounds(self.locate(name, role))
        _, top, _, bottom = self.viewport()
        if y < top or b > bottom:
            self.scroll((top + bottom - y - b) / 2)

    def click(self, name):
        self.reveal(name)
        element = self.visible(name)
        check(element.get("accessibleEnabled"), f"Settings control disabled: {name}")
        self.window.click_element(element)

    def state(self, name, *, enabled=True, checked=False):
        element = self.locate(name)
        actual = tuple(bool(element.get(key)) for key in ("accessibleEnabled", "accessibleCheckable", "accessibleChecked"))
        check(actual == (enabled, True, checked), f"wrong enabled/checkable/checked state for {name}: {actual}")

    def geometry(self, *, compact):
        roles = {name: "Groupbox" for name in PANELS} | {name: "Button" for name in ACTIONS}
        boxes = {}
        for elements, page_y in self.scan():
            for name, role in roles.items():
                match = self.find(name, role, elements)
                if match:
                    x, y, r, b = bounds(match)
                    boxes[name] = x, y - page_y, r, b - page_y
        self.scroll(1600)
        check(len(boxes) == len(roles), f"missing Settings groups or controls: {sorted(roles.keys() - boxes.keys())}")
        listening, appearance, account = (boxes[name] for name in PANELS)
        if compact:
            check(abs(listening[0] - appearance[0]) <= 1 and listening[3] <= appearance[1], "panels did not stack")
        else:
            check(listening[2] <= appearance[0] and abs(listening[1] - appearance[1]) <= 1, "panels did not sit side by side")
        check(abs(listening[0] - account[0]) <= 1 and abs(appearance[2] - account[2]) <= 1 and
              account[1] >= max(listening[3], appearance[3]), "Account must span both panels and follow them")
        left, _, right, _ = self.viewport()
        previous = []
        for name in ACTIONS:
            x, y, r, b = boxes[name]
            check(left <= x < r <= right and r - x >= 40 and b - y >= 32, f"clipped or undersized control: {name}")
            for other, (a, c, d, e) in previous:
                check(r <= a + 1 or d <= x + 1 or b <= c + 1 or e <= y + 1, f"controls overlap: {other} and {name}")
            previous.append((name, boxes[name]))
        rows = {round(boxes[name][1]) for name in PALETTES}
        check((len(rows) > 1) == compact, "palette grid did not adapt to width")
        check(boxes["Sign out"][1] >= max(boxes[name][3] for name in PALETTES), "Sign out should follow palette grid")
        return appearance[0] - listening[0]

    def focused(self, name, color=b"\x91\xbf\xff"):
        # Slint MCP does not expose the focused item. Read the actual visible
        # outline: two matching straight edges, excluding corners.
        match = self.find(name)
        if not match or not contained(self.viewport(), bounds(match)):
            return False
        x, y, r, b = map(round, bounds(match))
        left, width = x + (r - x) // 4, (r - x) // 2
        parts = self.window.mcp.call("take_screenshot", {"windowHandle": self.window.handle})["content"]
        png = b64decode(next(p["data"] for p in parts if p["type"] == "image"), validate=True)
        rgb = subprocess.run([self.magick, "png:-", "-crop", f"{width}x{b-y}+{left}+{y}",
                             "+repage", "-alpha", "off", "-depth", "8", "rgb:-"], input=png,
                             capture_output=True, check=True, timeout=5).stdout
        stride = width * 3
        check(len(rgb) == stride * (b - y), "focus crop differs from action bounds")
        def edge(row):
            start = row * stride
            return sum(rgb[i:i + 3] == color for i in range(start, start + stride, 3)) >= width * .8
        return any(edge(row) for row in (0, 1)) and any(edge(row) for row in (b - y - 1, b - y - 2))

    def tab(self, name, *, backwards=False):
        if backwards:
            self.window.shortcut("\x10", "\t")  # Slint Shift+Tab
        else:
            self.window.key("\t")
        self.visible(name)
        check(self.focused(name), f"{'Shift+Tab' if backwards else 'Tab'} did not visibly focus {name}")


def layouts(view):
    resize(view.window, TYPICAL)
    view.locate(r"12 downloads · 164\.7 MB on this device", "Text")
    view.locate("SyntheticSettingsUserWithLongUnbrokenName", "Text")
    for name in ("Dark", "Standard", "Jade, selected palette"):
        view.state(name, checked=True)
    for name in ("Light", "Large", "Compact", "Crossfade", "Ember, choose palette", "Violet, choose palette"):
        view.state(name)
    gap = None
    for name, size in (("01-typical", TYPICAL), ("02-wide", WIDE), ("03-minimum", MINIMUM)):
        resize(view.window, size)
        view.scroll(1600)
        view.window.pixels(name, size[0])
        if size == MINIMUM:
            check(not view.find("Sign out"), "expected Sign out below the fold")
        measured = view.geometry(compact=size == MINIMUM)
        if size == TYPICAL:
            gap = measured
        elif size == WIDE:
            check(measured > gap + 30, "wider window did not give Settings more room")
    print("PASS Settings hierarchy and adaptive geometry at all widths", flush=True)


def keyboard(view):
    window = view.window
    window.click("Settings")
    view.scroll(1600)
    for _ in range(16):
        window.key("\t")
        if view.focused("Work offline"):
            break
    else:
        raise Failure("Tab never visibly focused Work offline from the Settings rail")
    view.tab("Crossfade")
    window.key(" ")
    view.state("Crossfade", checked=True)
    window.key(" ")
    view.state("Crossfade")
    for name in ("Manage downloads", *DISPLAY[:-1]):
        view.tab(name)
    window.key(RETURN)
    view.state("Large", checked=True)
    check(view.focused("Large"), "density reflow lost visible keyboard focus")
    window.pixels("04-minimum-large-focus", MINIMUM[0])
    for name in (DISPLAY[-1], *PALETTES, "Sign out"):
        view.tab(name)
    window.pixels("05-minimum-sign-out-focus", MINIMUM[0])
    view.tab(PALETTES[-1], backwards=True)
    # Inspect every control at Large only after keyboard evidence: wheel
    # scanning before the next Tab could scroll focus completely out of view.
    view.geometry(compact=True)
    print("PASS forward/backward Tab, offscreen reveal, visible focus, and density reflow", flush=True)


def wheel(view):
    # A focused control must not keep pulling itself into view as the user
    # wheels away. Reproduce the large one-event jump before any other input.
    view.click("Crossfade")
    view.click("Ember, choose palette")
    check(view.focused("Ember, selected palette", color=bytes.fromhex("c4b0ff")),
          "expected visible Ember focus before wheel input")
    before = bounds(view.root())[1]
    view.scroll(1600)
    after = bounds(view.root())[1]
    check(before < 0 < after and view.find("Work offline") and not view.find("Ember, selected palette"),
          f"large wheel did not reach Listening from focused Ember (Settings y: {before:g} → {after:g})")
    view.window.pixels("06-minimum-wheel-top", MINIMUM[0])
    print(f"PASS compact Large wheel +1600 from focused Ember (Settings y: {before:g} → {after:g})", flush=True)
    view.click("Jade, choose palette")
    view.click("Crossfade")


def controls(view):
    window = view.window
    # Compact keyboard reveal is checked separately; keep pointer callback
    # assertions independent of wheel scrolling between the control groups.
    resize(window, TYPICAL)
    view.scroll(1600)
    view.click("Crossfade")
    view.state("Crossfade", checked=True)
    view.click("Ember, choose palette")
    view.state("Ember, selected palette", checked=True)
    view.state("Jade, choose palette")
    view.click("Light")
    view.state("Light", checked=True)
    view.click("Compact")
    view.state("Compact", checked=True)
    view.click("Reset display defaults")
    view.state("Dark", checked=True)
    view.state("Standard", checked=True)
    view.state("Ember, selected palette", checked=True)
    view.state("Crossfade", checked=True)
    view.click("Manage downloads")
    window.section("Saved offline")
    window.click("Settings")
    window.section("Settings")
    view.click("Work offline")
    view.locate("Offline", "Text")
    view.scroll(1600)
    window.pixels("07-typical-offline", TYPICAL[0])
    view.locate("Go online to change colors", "Text")
    for name in PALETTES:
        view.state(name, enabled=False, checked=name == PALETTES[1])
    view.click("Go online")
    view.locate("Online", "Text")
    view.state("Ember, selected palette", checked=True)
    view.click("Sign out")
    window.find("Sign in", roles=("Button",))
    check(not view.root(), "Settings remained after sign-out")
    print("PASS synthetic Settings callbacks for display, Crossfade, palette, downloads, offline and sign-out", flush=True)


def errors(view):
    resize(view.window, MINIMUM)
    view.reveal(r"Couldn’t save Crossfade on this desktop\. The setting hasn’t changed\.", "Text")
    view.state("Crossfade")
    view.window.pixels("08-minimum-error-crossfade", MINIMUM[0])
    view.state("Light", checked=True)
    view.state("Large", checked=True)
    view.reveal(r"Account palettes are unavailable right now\.", "Text")
    for name in PALETTES:
        view.state(name, enabled=False, checked=name == PALETTES[0])
    view.reveal(r"Account palettes are unavailable right now\.", "Text")
    view.window.pixels("09-minimum-error-palette", MINIMUM[0])
    print("PASS light/Large error states and disabled palette", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--shell", type=Path, required=True, help="trusted built settings_acceptance example, not the product app")
    parser.add_argument("--capture-dir", type=Path, required=True, help="new directory for PNG evidence")
    parser.add_argument("--scratch-dir", type=Path, help="existing base for disposable Wayland/profile state")
    args = parser.parse_args()
    binary = args.shell.resolve(strict=True)
    magick = shutil.which("magick") or shutil.which("convert")
    check(magick, "ImageMagick is required for focus checks")
    output = args.capture_dir.resolve()
    output.mkdir(mode=0o700, exist_ok=False)
    print(f"Settings evidence: {output}", flush=True)
    failures = []
    try:
        for error_mode in (False, True):
            with TemporaryDirectory(prefix="spofify-settings-", dir=args.scratch_dir) as name:
                root = Path(name)
                env = private_env(root)
                env.update(DBUS_SESSION_BUS_ADDRESS=f"unix:path={root}/no-session-bus",
                           DBUS_SYSTEM_BUS_ADDRESS=f"unix:path={root}/no-system-bus")
                if error_mode:
                    env["SETTINGS_FIXTURE_ERRORS"] = "1"
                with desktop(binary, root, env, output, multisize=True) as window:
                    view = View(window, magick)
                    window.section("Settings")
                    if error_mode:
                        with verify(failures, "error state"):
                            errors(view)
                    else:
                        layouts(view)
                        with verify(failures, "keyboard"):
                            keyboard(view)
                        if failures:
                            window.pixels("98-keyboard-failure", MINIMUM[0])
                        else:
                            with verify(failures, "wheel"):
                                wheel(view)
                        before_controls = len(failures)
                        with verify(failures, "controls"):
                            controls(view)
                        if len(failures) > before_controls and view.root():
                            with suppress(Failure):
                                window.pixels("99-controls-failure", int(window.dimensions()[0]))
        check(not failures, "; ".join(failures))
    finally:
        with suppress(OSError):
            output.rmdir()  # Only remove an empty evidence directory.


def interrupt(_signum, _frame):
    raise KeyboardInterrupt


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupt)
    try:
        main()
    except (RuntimeError, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        sys.exit(f"FAIL Settings: {error or type(error).__name__}")
