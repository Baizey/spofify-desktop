#!/usr/bin/env python3
"""Check the real Slint Lyrics reader against a disposable remote playback fixture.

Build with SLINT_EMIT_DEBUG_INFO=1 cargo build --locked --features slint/mcp.
Run with Python + Pillow, Cage and wlr-randr; no Go server, live account,
audio output, ambient profile, or application test backend is used.

Check synchronized line paint during playback, bounded automatic recovery and
track isolation, without following the current line or persisting lyric text.
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
from threading import Event, Lock, Thread
from time import monotonic, sleep

sys.dont_write_bytecode = True
from audio_smoke import admission
from audio_smoke_wire import Peer
from fixture import Failure, check, private_env
from native import DESKTOP, bounds, desktop, element_id, element_type, wait
from playback import CONNECT, PROTOCOL, SOCKET, WS_GUID


DURATION = 38
TIMED = ("synthetic-timed", "Synthetic Timed Recording")
PLAIN = ("synthetic-plain", "Synthetic Plain Recording")
STALE = ("synthetic-stale", "Synthetic Delayed Recording")
EMPTY = ("synthetic-empty", "Synthetic Missing Recording")
RECOVERY = ("synthetic-recovery", "Synthetic Recovered Recording")
TRACKS = (TIMED, PLAIN, STALE, EMPTY, RECOVERY)
RETRY_WINDOW = 2.0  # Longer than the 250 ms + 750 ms backoffs; fixture failures are immediate.
LINES = ("Quiet morning", "Open water", "Evening light")
STARTS = (2000, 9000, 18000)
PLAIN_TEXT = ("Words without a clock", "Only words remain")
TIMED_SOURCE, PLAIN_SOURCE = "Timed archive", "Plain booklet"
STALE_TEXT, STALE_SOURCE = "Synthetic late verse must stay hidden", "Delayed lyric credit"
RECOVERED_TEXT, RECOVERED_SOURCE = "Synthetic recovered verse stays in memory", "Recovered lyric credit"
OLD_TEXT = (*LINES, TIMED_SOURCE, *PLAIN_TEXT, PLAIN_SOURCE, STALE_TEXT, STALE_SOURCE)
MISSING, UNAVAILABLE = "No lyrics for this track.", "Lyrics unavailable."
REMOTE = "synthetic-remote-output"


def lyrics_path(track):
    return f"/api/library/tracks/{track[0]}/lyrics"


def payload(track, state, *, source=None, ownership="library", generation="local-v1", lines=(), **fields):
    result = dict(version=1, trackId=track[0], recordingVersion="recording-v1",
                  sourceGeneration=generation, offlineTransferAllowed=False,
                  state=state, lines=list(lines), **fields)
    if source:
        result["attribution"] = dict(name=source, ownership=ownership)
    return result


LYRICS = {
    lyrics_path(TIMED): [(200, payload(TIMED, "timed", source=TIMED_SOURCE, provenance="local_lrc",
                                      plainText="\n".join(LINES),
                                      lines=[dict(startMs=ms, text=line) for ms, line in zip(STARTS, LINES)]))],
    lyrics_path(PLAIN): [(200, payload(PLAIN, "plain", source=PLAIN_SOURCE, plainText="\n".join(PLAIN_TEXT)))],
    lyrics_path(STALE): [(503, None),
                         (200, payload(STALE, "plain", source=STALE_SOURCE, plainText=STALE_TEXT))],
    lyrics_path(EMPTY): [
        (404, None),
        (200, payload(EMPTY, "not_found", reason="no_match")),
        (503, None), (None, None), (503, None),  # Exhaust both retries on the third reader opening.
    ],
    lyrics_path(RECOVERY): [
        (503, None),
        (None, None),  # Close without sending HTTP headers to exercise a transport error.
        # Older Go responses could carry the cache's source generation instead of local-v1.
        (200, payload(RECOVERY, "plain", source=RECOVERED_SOURCE, ownership="provider",
                      provenance="provider", generation="lrclib-signature-v2", plainText=RECOVERED_TEXT)),
    ],
}


class RemotePeer(Peer):
    def __init__(self, handler):
        super().__init__(handler)
        self.closed = Event()

    def roster(self):
        return super().roster() + [dict(id=REMOTE, deviceId="synthetic-remote-device",
                                        name="Synthetic remote player", canPlay=True)]

    def open_remote(self):
        self.owner = REMOTE  # Never grant the desktop execution authority.
        self.player["queue"] = [dict(id=id_, name=name, duration=DURATION, key=i, order=i, intent="low")
                                for i, (id_, name) in enumerate(TRACKS, 1)]
        self.player["cursor"].update(key=1, visit=1, run=1, chain="synthetic-lyrics-chain")
        self.snapshot()


@contextmanager
def fixture():
    ready = dict(username="synthetic-lyrics", password=token_hex(16))
    csrf, cookie = token_hex(16), "spofify_session=" + token_hex(24)
    session = dict(accountId="synthetic-lyrics-account", username=ready["username"], role="user", csrfToken=csrf)
    ticket = Queue(maxsize=1)
    admitted = Queue(maxsize=1)
    requests, lyrics_gets, audio_reads = deque(maxlen=24), [], set()
    http_writes, socket_actions = [], []
    lyrics_reads, lyrics_lock = Counter(), Lock()
    connections = set()
    media_gate, media_expired = Event(), Event()
    stale_gate, stale_expired, stale_sent = Event(), Event(), Event()
    gated_media = {resource: Event() for resource in ("artwork", "waveform")}

    def release_media():
        wait("both timed artwork and waveform in flight", lambda: all(seen.is_set() for seen in gated_media.values()), 2)
        check(not media_expired.is_set(), "artwork/waveform gate expired before lyrics rendered")
        media_gate.set()

    def release_stale():
        stale_gate.set()
        wait("delayed old-track retry released", stale_sent.is_set, 2)
        check(not stale_expired.is_set(), "old-track retry gate expired before the recording changed")

    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(5)

        def log_message(self, *_args):
            pass

        def log_request(self, code="-", size="-"):
            # Do not print query strings or any request headers/body.
            entry = (self.command, self.path.split("?", 1)[0], code)
            requests.append(entry)
            if self.command not in ("GET", "HEAD", "OPTIONS"):
                http_writes.append(entry)

        def reply(self, result=None, status=200, headers=()):
            data = json.dumps(result).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json; charset=utf-8")
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
            path = self.path.split("?", 1)[0]
            if path.startswith("/api/library/tracks/") and path.endswith("/lyrics"):
                return self.get_lyrics(path)
            match = re.fullmatch(r"/api/library/tracks/([^/]+)/(artwork|waveform|audio)", self.path)
            if match:
                track, resource = match.groups()
                if resource == "audio":
                    audio_reads.add(track)
                elif track == TIMED[0] and resource in gated_media:
                    gated_media[resource].set()
                    if not media_gate.wait(8):
                        media_expired.set()
            self.reply(status=404)  # No artwork/waveform in this fixture.

        def get_lyrics(self, path):
            with lyrics_lock:
                index = lyrics_reads[self.path]
                lyrics_reads[self.path] += 1
                replies = LYRICS.get(self.path, ())
                status, body = replies[index] if index < len(replies) else (500, None)
                lyrics_gets.append((self.command, path if path == self.path else path + "?<redacted>", status))
            if status is None:
                self.close_connection = True
                return
            stale = self.path == lyrics_path(STALE) and status == 200
            if stale and not stale_gate.wait(8):
                stale_expired.set()
            self.reply(body, status=status)
            if stale:
                stale_sent.set()

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
                    kind = message.get("type")
                    if kind not in ("presence", "sync", "pong"):
                        socket_actions.append(kind)
                    peer.incoming.put(message, timeout=1)
            except (EOFError, BrokenPipeError, ConnectionResetError):
                peer.closed.set()  # Work offline (or fixture teardown) retires the socket.
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
            yield ready, admitted, requests, lyrics_gets, release_media, release_stale
        finally:
            media_gate.set()
            stale_gate.set()
            server.shutdown()
            worker.join(timeout=5)
            for conn in tuple(connections):
                try:
                    conn.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
    check(not audio_reads, f"unowned desktop unexpectedly fetched audio: {audio_reads}")
    check(http_writes == [("POST", "/api/auth/login", 200), ("POST", CONNECT, 200)],
          f"unexpected HTTP writes: {http_writes}")
    expected_actions = ["pause", "seek"] + ["next"] * (len(TRACKS) - 1)
    check(socket_actions == expected_actions,
          f"expected only intentional playback commands {expected_actions}; saw {socket_actions}")
    check(lyrics_reads == Counter({path: len(replies) for path, replies in LYRICS.items()}),
          f"unexpected final lyrics GETs: {lyrics_gets}")
    print("PASS no unexpected playback commands, HTTP writes, or audio reads", flush=True)


def text(win, elements, match):
    return next((e for e in elements if e.get("accessibleRole") == "Text" and win.matches(e, match)), None)


def containing(phrases):
    return r"[\s\S]*(?:" + "|".join(map(re.escape, phrases)) + r")[\s\S]*"


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


def run(win, ready, admitted, requests, lyrics_gets, release_media, release_stale, image_module, captures):
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
    expected_gets = []

    def exact_gets():
        check(lyrics_gets == expected_gets, f"expected exact lyrics GETs {expected_gets}; saw {lyrics_gets}")

    def fetched(track, *statuses):
        path = lyrics_path(track)
        expected_gets.extend(("GET", path, status) for status in statuses)

        def arrived():
            seen = lyrics_gets[:]
            check(seen == expected_gets[:len(seen)], f"expected lyrics GETs {expected_gets}; saw {seen}")
            return len(seen) == len(expected_gets)

        wait(f"GET {path} responses {statuses}", arrived, 6)

    def next_track(track, **cursor):
        exact_gets()
        win.click("Next track")
        peer.command("next")
        peer.cursor(key=TRACKS.index(track) + 1, **cursor)
        now_playing_title(track)

    def now_playing_title(track):
        def title():
            page = element_type(win.elements(), "NowPlaying")
            return page and text(win, win.subtree(page), re.escape(track[1]))
        wait("Now Playing: " + track[1], title)

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
            if pane and all(rows) and credit and current == expected:
                check(not text(win, elements, re.escape("\n".join(LINES))),
                      "timed response also rendered its joined plainText fallback")
                return pane, rows, credit

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
    fetched(TIMED, 200)
    win.absent("Retry lyrics", roles=("Button",))
    peer.cursor(elapsed=4.0)
    win.title("Elapsed 0:04")
    timed("02-first-line", 0, clock_only=True)
    # A new visit of the same queue row resets the marker without another
    # lyrics request, even while the unrelated media requests are held.
    peer.cursor(elapsed=0.0, visit=2, run=2)
    win.title("Elapsed 0:00")
    timed("03-repeat-before-first-line", None, clock_only=True)
    sleep(0.2)  # Allow the presentation observer to see the new visit.
    exact_gets()
    release_media()
    print("PASS timed JSON with plainText fallback, repeat without refetch while artwork and waveform were held", flush=True)
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

    # Start after all old authored starts: a leaked timed model could still be active.
    next_track(PLAIN, run=6, visit=3, elapsed=22.0, playing=True, desired=True)
    win.find("Pause", roles=("Button",))
    fetched(PLAIN, 200)

    def plain():
        view = reader(win)
        if not view:
            return None
        pane, elements = view
        patterns = (*PLAIN_TEXT, "Lyrics source: " + PLAIN_SOURCE)
        content = [text(win, elements, containing((phrase,))) for phrase in patterns]
        return (pane, elements, content) if pane and all(content) else None

    pane, elements, content = wait("plain lyrics and their new attribution", plain)
    check(not text(win, elements, containing((*LINES, TIMED_SOURCE))),
          "old timed lyrics or attribution survived the track transition")
    check(not any(e["handle"] in row_handles or e.get("accessibleDescription") == "Current line" for e in elements),
          "timed row identity or current-line state survived replacement with plain lyrics")
    win.absent("Retry lyrics", roles=("Button",))
    image = capture(win, image_module)
    for i, element in enumerate(content):
        if element not in content[:i]:  # Plain text and its source may be in a single Text element.
            check(paint(image, scale, pane, element, multiline=True) == (False, background),
                  "plain lyrics or attribution gained highlighting")
    save(image, "07-plain-track")
    print("PASS plain track during playback: no stripe or tint, new attribution, no previous lyrics", flush=True)

    def current_elements():
        view = reader(win)
        if not view:
            return None
        elements = view[1]
        check(not text(win, elements, containing(OLD_TEXT)), "previous recording's lyrics or credit appeared")
        check(not any(win.matches(e, "Retry lyrics") for e in elements), "unexpected Retry lyrics control")
        return elements

    def shows(label):
        elements = current_elements()
        return elements and text(win, elements, re.escape(label))

    def stays_quiet(scenario, label):
        peer.ping(scenario)  # Keep the same account socket alive across the quiet windows.
        until = monotonic() + RETRY_WINDOW

        def unchanged():
            exact_gets()
            check(shows(label), f"{scenario}: Lyrics reader did not stay at {label!r}")
            return monotonic() >= until

        wait(scenario + ": no further lyrics GETs", unchanged, RETRY_WINDOW + 2)
        exact_gets()

    def reopen(track):
        win.click("Back")
        wait("Now Playing closed", lambda: not element_type(win.elements(), "NowPlaying"))
        exact_gets()
        win.click("Now playing: " + re.escape(track[1]))
        now_playing_title(track)

    next_track(STALE, run=7, visit=4, elapsed=0, playing=False, desired=False)
    fetched(STALE, 503, 200)  # The automatic retry arrived; hold its success until after the switch.
    check(current_elements(), "missing Lyrics reader during delayed retry")
    next_track(EMPTY, run=8, visit=5)
    fetched(EMPTY, 404)
    wait("404: " + MISSING, lambda: shows(MISSING), 6)
    release_stale()
    stays_quiet("lyrics-http-404", MISSING)
    print("PASS HTTP 404 settles without retries; the gated previous track retry never renders", flush=True)

    reopen(EMPTY)
    fetched(EMPTY, 200)
    wait("200 not_found: " + MISSING, lambda: shows(MISSING), 6)
    stays_quiet("lyrics-json-not-found", MISSING)
    print("PASS reopening revalidates; HTTP 200 not_found still settles without retries", flush=True)

    reopen(EMPTY)
    fetched(EMPTY, 503, None, 503)
    wait("transient failures exhaust retries: " + UNAVAILABLE, lambda: shows(UNAVAILABLE), 6)
    stays_quiet("lyrics-attempts-exhausted", UNAVAILABLE)
    print("PASS persistent transient failures stop after exactly three GETs", flush=True)

    next_track(RECOVERY, run=9, visit=6)
    fetched(RECOVERY, 503, None, 200)
    recovered = RECOVERED_TEXT + "\n\nLyrics source: " + RECOVERED_SOURCE
    wait("automatic same-track recovery renders alternate generation and attribution", lambda: shows(recovered), 6)
    now_playing_title(RECOVERY)
    stays_quiet("lyrics-recovered", recovered)
    if captures:
        save(capture(win, image_module), "08-recovered-track")
    print("PASS automatic recovery on the same track after exactly three GETs, with no retry control", flush=True)

    # Settings closes the reader before the mode change, so this verifies the
    # visible transition, not cancellation of an in-flight retry.
    win.click("Settings")
    win.section("Settings")
    win.click("Work offline")
    win.find("Go online", roles=("Button",))
    win.section("Settings")
    wait("Work offline retires the account socket", peer.closed.is_set, 5)
    win.absent(containing((RECOVERED_TEXT, RECOVERED_SOURCE)), roles=("Text",))
    exact_gets()
    print("PASS switching to Work offline closes the account socket without displaying or refetching lyrics", flush=True)


def no_lyrics_on_disk(root):
    needles = tuple(phrase.encode() for phrase in (*OLD_TEXT, RECOVERED_TEXT, RECOVERED_SOURCE))
    for path in root.rglob("*"):
        if not path.is_symlink() and path.is_file():
            data = path.read_bytes()
            check(not any(needle in data for needle in needles), f"lyric text persisted in {path.relative_to(root)}")
    print("PASS no synthetic lyric text or attribution in the isolated profile files", flush=True)


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
            no_lyrics_on_disk(root)
    except (ImportError, OSError, RuntimeError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f"FAIL synced lyrics: {error or type(error).__name__}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    def interrupted(_signum, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    sys.exit(main())
