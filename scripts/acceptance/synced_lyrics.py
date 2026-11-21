#!/usr/bin/env python3
"""Check the real Slint Lyrics reader against a disposable remote playback fixture.

Build with SLINT_EMIT_DEBUG_INFO=1 cargo build --locked --features slint/mcp.
Run with Python + Pillow, Cage and wlr-randr; no Go server, live account,
audio output, ambient profile, or application test backend is used.

This reader highlights the current line without following it: long lyrics still
scroll manually, so playback does not move the reader or controls. This case
checks position and identity of visible rows, not manual scrolling.
"""
import argparse
from base64 import b64decode, b64encode
from collections import Counter, deque
from contextlib import contextmanager
from hashlib import sha1
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from io import BytesIO
import json
import os
from pathlib import Path
from queue import Full, Queue
import re
from secrets import token_hex
import signal
import socket
import subprocess
import sys
from tempfile import TemporaryDirectory
from threading import Event, Thread
from time import sleep

sys.dont_write_bytecode = True
from audio_smoke import admission
from audio_smoke_wire import Peer
from fixture import Failure, check, private_env
from native import DESKTOP, bounds, desktop, element_id, element_type, wait
from playback import CONNECT, PROTOCOL, SOCKET, WS_GUID


DURATION = 38
TIMED = ("synthetic-timed", "Synthetic Timed Recording")
PLAIN = ("synthetic-plain", "Synthetic Plain Recording")
LINES = ("Quiet morning", "Open water", "Evening light")
STARTS = (2000, 9000, 18000)
PLAIN_TEXT = ("Words without a clock", "Only words remain")
TIMED_SOURCE, PLAIN_SOURCE = "Timed archive", "Plain booklet"
REMOTE = "synthetic-remote-output"


def payload(track, source, **fields):
    return dict(version=1, trackId=track[0], recordingVersion="recording-v1",
                sourceGeneration="local-v1", offlineTransferAllowed=False,
                attribution=dict(name=source, ownership="library"), **fields)


LYRICS = {
    TIMED[0]: payload(TIMED, TIMED_SOURCE, state="timed", provenance="local_lrc",
                      lines=[dict(startMs=ms, text=line) for ms, line in zip(STARTS, LINES)]),
    PLAIN[0]: payload(PLAIN, PLAIN_SOURCE, state="plain", lines=[], plainText="\n".join(PLAIN_TEXT)),
}


class RemotePeer(Peer):
    def roster(self):
        return super().roster() + [dict(id=REMOTE, deviceId="synthetic-remote-device",
                                        name="Synthetic remote player", canPlay=True)]

    def open_remote(self):
        self.owner = REMOTE  # Never grant the desktop execution authority.
        self.player["queue"] = [dict(id=id_, name=name, duration=DURATION, key=i, order=i, intent="low")
                                for i, (id_, name) in enumerate((TIMED, PLAIN), 1)]
        self.player["cursor"].update(key=1, visit=1, run=1, chain="synthetic-lyrics-chain")
        self.snapshot()


@contextmanager
def fixture():
    ready = dict(username="synthetic-lyrics", password=token_hex(16))
    csrf, cookie = token_hex(16), "spofify_session=" + token_hex(24)
    session = dict(accountId="synthetic-lyrics-account", username=ready["username"], role="user", csrfToken=csrf)
    ticket = Queue(maxsize=1)
    admitted = Queue(maxsize=1)
    requests, lyrics_reads, audio_reads = deque(maxlen=24), Counter(), set()
    connections = set()
    media_gate, gate_expired = Event(), Event()
    gated_media = {resource: Event() for resource in ("artwork", "waveform")}

    def release_media():
        wait("both timed artwork and waveform in flight", lambda: all(seen.is_set() for seen in gated_media.values()), 2)
        check(not gate_expired.is_set(), "artwork/waveform gate expired before lyrics rendered")
        media_gate.set()

    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(5)

        def log_message(self, *_args):
            pass

        def log_request(self, code="-", size="-"):
            # Do not print query strings or any request headers/body.
            requests.append((self.command, self.path.split("?", 1)[0], code))

        def reply(self, result=None, status=200, headers=()):
            data = json.dumps(result).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(data)))
            for name, value in headers:
                self.send_header(name, value)
            try:
                self.end_headers()
                self.wfile.write(data)
            except (BrokenPipeError, ConnectionResetError):
                pass  # Visibility changes can cancel an in-flight resource.

        def trusted(self, *, write=False):
            return (self.headers.get("Host") == ready["origin"].removeprefix("http://") and
                    self.headers.get("Origin") in ((ready["origin"],) if write else (None, ready["origin"])))

        def do_POST(self):
            if not self.trusted(write=True):
                return self.reply(status=403)
            try:
                size = int(self.headers.get("Content-Length", "0"))
                check(0 < size <= 1024, "invalid request body length")
                body = json.loads(self.rfile.read(size))
            except (Failure, ValueError):
                return self.reply(status=400)
            if (self.path == "/api/auth/login" and self.headers.get("X-Spofify-Login") == "1" and
                    body == {key: ready[key] for key in ("username", "password")}):
                return self.reply(session, headers=(("Set-Cookie", cookie + "; Path=/; HttpOnly; Max-Age=3600; SameSite=Lax"),))
            if self.headers.get("Cookie") != cookie or self.headers.get("X-CSRF-Token") != csrf:
                return self.reply(status=401)
            if self.path == CONNECT and body == {}:
                token = token_hex(32)
                try:
                    ticket.put_nowait(token)
                except Full:
                    return self.reply(status=429)
                return self.reply(dict(ticket=token))
            self.reply(status=404)

        def do_GET(self):
            if not self.trusted() or self.headers.get("Cookie") != cookie:
                return self.reply(status=401)
            if self.path == SOCKET:
                return self.upgrade()
            if self.path == "/api/auth/session":
                return self.reply(session)
            match = re.fullmatch(r"/api/library/tracks/([^/]+)/(lyrics|artwork|waveform|audio)", self.path)
            if match:
                track, resource = match.groups()
                if resource == "audio":
                    audio_reads.add(track)
                elif resource == "lyrics" and track in LYRICS:
                    lyrics_reads[track] += 1
                    return self.reply(LYRICS[track])
                elif track == TIMED[0] and resource in gated_media:
                    gated_media[resource].set()
                    if not media_gate.wait(8):
                        gate_expired.set()
            self.reply(status=404)  # No artwork/waveform in this fixture.

        def upgrade(self):
            key = self.headers.get("Sec-WebSocket-Key", "")
            if (not key or not self.trusted(write=True) or self.headers.get("Upgrade", "").lower() != "websocket"
                    or self.headers.get("Sec-WebSocket-Version") != "13"
                    or self.headers.get("Sec-WebSocket-Protocol") != PROTOCOL):
                return self.reply(status=400)
            accept = b64encode(sha1((key + WS_GUID).encode()).digest()).decode()
            self.protocol_version = "HTTP/1.1"
            self.send_response(101)
            for name, value in (("Upgrade", "websocket"), ("Connection", "Upgrade"),
                                ("Sec-WebSocket-Protocol", PROTOCOL), ("Sec-WebSocket-Accept", accept)):
                self.send_header(name, value)
            self.end_headers()
            peer = RemotePeer(self)
            connections.add(self.connection)
            try:
                stream = peer.messages()
                hello = next(stream)
                check(hello.get("type") == "hello" and hello.get("ticket") == ticket.get(timeout=1)
                      and hello.get("deviceId") and hello.get("name") and hello.get("canPlay") is True,
                      "invalid synthetic account hello")
                peer.hello = hello
                self.connection.settimeout(40)
                admitted.put(peer, timeout=1)
                for message in stream:
                    peer.incoming.put(message, timeout=1)
            except (EOFError, BrokenPipeError, ConnectionResetError):
                pass  # The owned window exits first.
            except Exception as error:
                target = peer.incoming if peer.hello else admitted
                if not target.full():
                    target.put_nowait(error)
            finally:
                connections.discard(self.connection)

    class Server(ThreadingHTTPServer):
        daemon_threads = False

    with Server(("127.0.0.1", 0), Handler) as server:
        ready["origin"] = f"http://127.0.0.1:{server.server_port}"
        worker = Thread(target=server.serve_forever, name="synthetic-lyrics-server")
        worker.start()
        try:
            yield ready, admitted, requests, lyrics_reads, release_media
        finally:
            media_gate.set()
            server.shutdown()
            worker.join(timeout=5)
            for conn in tuple(connections):
                try:
                    conn.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
    check(not audio_reads, f"unowned desktop unexpectedly fetched audio: {audio_reads}")


def text(win, elements, match):
    return next((e for e in elements if e.get("accessibleRole") == "Text" and win.matches(e, match)), None)


def reader(win):
    panel = element_type(win.elements(), "LyricsReader")
    if panel is None:
        return None
    elements = win.subtree(panel)
    return (element_id(elements, "reader"), elements) if text(win, elements, "Lyrics") else None


def capture(win, image_module):
    image = next((p for p in win.mcp.call("take_screenshot", {"windowHandle": win.handle})["content"]
                  if p["type"] == "image"), None)
    check(image and image["mimeType"] == "image/png", "missing native PNG")
    with image_module.open(BytesIO(b64decode(image["data"], validate=True))) as png:
        size = win.window_properties()["size"]
        check(png.size == (size["width"], size["height"]), "PNG and window dimensions differ")
        return png.convert("RGB")


def paint(image, scale, pane, band, *, multiline=False):
    """Probe the left margin and the blank right half, away from the fixture's short text."""
    left, _, right, _ = bounds(pane)
    _, top, _, bottom = bounds(band)
    width = right - left
    check(width >= 200 and bottom > top, "lyrics are not in the wide reader")
    x0, x1 = round(left * scale), round((left + 32) * scale)
    x2, x3 = round((left + 0.72 * width) * scale), round((left + 0.85 * width) * scale)
    inset = 0 if multiline else (bottom - top) * 0.28
    y0, y1 = round((top + inset) * scale), round((bottom - inset) * scale)
    check(0 <= x0 < x1 < x2 < x3 <= image.width and 0 <= y0 < y1 <= image.height,
          "sampled lyric line is not visible")
    accents = ((102, 227, 180), (20, 97, 75), (20, 115, 87))  # Default Jade accent variants.
    pixels = image.load()

    def accent(rgb):
        return any(max(abs(a - b) for a, b in zip(rgb, color)) <= 4 for color in accents)

    def stripe_at(x):
        run = 0
        for y in range(y0, y1):
            run = run + 1 if accent(pixels[x, y]) else 0
            if run >= min(round(6 * scale), y1 - y0):
                return True
        return False

    stripe = any(stripe_at(x) for x in range(x0, x1))
    colors = image.crop((x2, y0, x3, y1)).getcolors((x3 - x2) * (y1 - y0))
    count, fill = max(colors)
    check(count >= 0.95 * (x3 - x2) * (y1 - y0), "lyric background sample was not a solid fill")
    return stripe, fill


def run(win, ready, admitted, requests, lyrics_reads, release_media, image_module, captures):
    # Keep the default open queue and still allocate the split Lyrics reader.
    if win.window_properties()["size"]["width"] != DESKTOP[0]:
        win.resize(DESKTOP)
    win.fill("Server( origin| URL| address)?", ready["origin"])
    win.fill("Username", ready["username"])
    win.fill("Password", ready["password"])
    win.click("Sign in")
    peer = admission(admitted, requests, "lyrics sign-in")
    peer.open_remote()
    win.click("Lyrics")
    win.absent("Username|Password", roles=("TextInput",))
    win.title(TIMED[1])
    scale = win.window_properties()["scaleFactor"]
    background = None
    row_boxes = row_handles = initial_pane_top = None

    def save(image, name):
        if captures:
            with (captures / (name + ".png")).open("xb") as file:
                image.save(file, format="PNG")

    def timed(name, active, *, clock_only=False):
        nonlocal background, row_boxes, row_handles, initial_pane_top

        def content():
            view = reader(win)
            if not view:
                return None
            pane, elements = view
            rows = [text(win, elements, re.escape(line)) for line in LINES]
            credit = text(win, elements, "Lyrics source: " + TIMED_SOURCE)
            current = [e.get("accessibleLabel") for e in elements if e.get("accessibleDescription") == "Current line"]
            expected = [] if active is None else [LINES[active]]
            return (pane, rows, credit) if pane and all(rows) and credit and current == expected else None

        pane, rows, credit = wait(name + " Lyrics heading, rows, source and current-line state", content, timeout=6)
        image = capture(win, image_module)
        pane_top = bounds(pane)[1]
        boxes = [tuple(y - pane_top for y in bounds(row)[1::2]) for row in rows]
        handles = [row["handle"] for row in rows]
        if row_boxes is None:
            row_boxes, row_handles, initial_pane_top = boxes, handles, pane_top
        if clock_only:
            check(pane_top == initial_pane_top, "clock update moved the Lyrics reader within the window")
        check(boxes == row_boxes, "playback moved the timed lyrics within the reader")
        check(handles == row_handles, "playback remounted the timed lyric rows")
        for i, row in enumerate(rows):
            stripe, fill = paint(image, scale, pane, row)
            if background is None:
                background = fill  # The pre-first-line frame is all inactive.
            check(stripe == (i == active) and (fill != background) == (i == active),
                  f"{name}: line {i + 1} expected {'stripe and tint' if i == active else 'plain background'}; "
                  f"got stripe={stripe}, fill={fill}, inactive={background}")
        check(paint(image, scale, pane, credit) == (False, background), "timed attribution acquired a highlight")
        save(image, name)
        print(f"PASS {name}: expected stripe/tint, stable row positions/handles and source attribution", flush=True)

    timed("01-before-first-line", None)
    peer.cursor(elapsed=4.0)
    win.title("Elapsed 0:04")
    timed("02-first-line", 0, clock_only=True)
    # A new visit of the same queue row resets the marker without another
    # lyrics request, even while the unrelated media requests are held.
    peer.cursor(elapsed=0.0, visit=2, run=2)
    win.title("Elapsed 0:00")
    timed("03-repeat-before-first-line", None, clock_only=True)
    sleep(0.2)  # Allow the presentation observer to see the new visit.
    check(lyrics_reads == {TIMED[0]: 1}, f"same-track repeat refetched accepted lyrics: {lyrics_reads}")
    release_media()
    print("PASS timed JSON lyric delivery and repeat without refetch while artwork and waveform were held", flush=True)
    # Publish just one playing sample. The transition at 9s must come from the
    # desktop's elapsed projection, not a second fixture update or a lyric fetch.
    peer.cursor(elapsed=8.0, desired=True, playing=True, run=3)
    win.find(r"Elapsed 0:(09|10|11)", roles=("Text",), timeout=6)
    timed("04-projected-second-line", 1, clock_only=True)

    peer.cursor(observed=True, elapsed=16.25)
    win.find(r"Elapsed 0:(16|17)", roles=("Text",))
    win.click("Pause")
    peer.command("pause")
    peer.cursor(run=4, elapsed=16.25, playing=False, desired=False)
    win.find("Play", roles=("Button",))
    sleep(2.2)  # A still-advancing cursor would cross the next line's 18s boundary.
    win.title("Elapsed 0:16", timeout=2)
    timed("05-paused-second-line", 1)

    win.drag_slider(0.56)
    seek = peer.command("seek")["value"]
    check(abs(seek - 0.56 * DURATION) < 0.6, f"unexpected native seek target: {seek!r}")
    peer.cursor(run=5, elapsed=seek)
    win.find(r"Elapsed 0:2[01]", roles=("Text",))
    timed("06-seek-third-line", 2)

    win.click("Next track")
    peer.command("next")
    # Start after all old authored starts: a leaked timed model could still be active.
    peer.cursor(run=6, key=2, visit=3, elapsed=22.0, playing=True, desired=True)
    win.title(PLAIN[1])
    win.find("Pause", roles=("Button",))

    def plain():
        view = reader(win)
        if not view:
            return None
        pane, elements = view
        patterns = (*PLAIN_TEXT, "Lyrics source: " + PLAIN_SOURCE)
        content = [text(win, elements, r"[\s\S]*" + re.escape(phrase) + r"[\s\S]*") for phrase in patterns]
        return (pane, elements, content) if pane and all(content) else None

    pane, elements, content = wait("plain lyrics and their new attribution", plain)
    check(not text(win, elements, r"[\s\S]*(" + "|".join((*LINES, TIMED_SOURCE)) + r")[\s\S]*"),
          "old timed lyrics or attribution survived the track transition")
    check(not any(e["handle"] in row_handles or e.get("accessibleDescription") == "Current line" for e in elements),
          "timed row identity or current-line state survived replacement with plain lyrics")
    check(lyrics_reads == {TIMED[0]: 1, PLAIN[0]: 1}, f"unexpected track-scoped lyric requests: {lyrics_reads}")
    image = capture(win, image_module)
    for i, element in enumerate(content):
        if element not in content[:i]:  # Plain text and its source may be in a single Text element.
            check(paint(image, scale, pane, element, multiline=True) == (False, background),
                  "plain lyrics or attribution gained highlighting")
    peer.ping("lyrics-check-complete")
    save(image, "07-plain-track")
    print("PASS plain track during playback: no stripe or tint, new attribution, no previous lyrics", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--desktop", required=True, type=Path, help="trusted prebuilt Slint MCP debug binary")
    parser.add_argument("--scratch-dir", type=Path, default=Path("/dev/shm"),
                        help="existing parent directory for a disposable profile (default: /dev/shm)")
    parser.add_argument("--capture-dir", type=Path, help="optional new directory for synthetic screenshots")
    args = parser.parse_args()
    try:
        from PIL import Image
        binary = args.desktop.resolve(strict=True)
        check(binary.is_file() and os.access(binary, os.X_OK), "desktop must be an executable file")
        if args.capture_dir:
            args.capture_dir.mkdir(mode=0o700, exist_ok=False)
        with TemporaryDirectory(prefix="synced-lyrics-", dir=args.scratch_dir.resolve(strict=True)) as temp:
            root = Path(temp)
            env = private_env(root)
            runtime = env["XDG_RUNTIME_DIR"]
            env.update(DBUS_SESSION_BUS_ADDRESS=f"unix:path={runtime}/no-bus",
                       DBUS_SYSTEM_BUS_ADDRESS=f"unix:path={runtime}/no-bus")
            with fixture() as server, desktop(binary, root, env, multisize=True) as win:
                run(win, *server, Image, args.capture_dir)
    except (ImportError, OSError, RuntimeError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f"FAIL synced lyrics: {error or type(error).__name__}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    sys.exit(main())
