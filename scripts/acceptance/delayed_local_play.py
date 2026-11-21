#!/usr/bin/env python3
"""Native local-Play ordering against a delayed synthetic Go-shaped account peer.

Linux: Cage, PipeWire (including its ALSA plugin), pipewire-pulse, WirePlumber
0.5, pactl and parec. Build the desktop with SLINT_EMIT_DEBUG_INFO=1 cargo
build --locked --features slint/mcp. Run with --desktop target/debug/spofify-desktop.
Run --case selected for the quickest acceptance, or omit --case for all cases.
Uses disposable loopback credentials, private profile/compositor/audio and no Go process.
The fake peer holds back Play resolution and attached grant delivery independently.
"""
import argparse
import struct
import subprocess
import sys
import wave
from collections import deque
from io import BytesIO
from math import cos, isfinite, pi, sin
from pathlib import Path
from queue import Empty
from secrets import token_hex
from tempfile import TemporaryDirectory
from time import monotonic, sleep

sys.dont_write_bytecode = True
from audio_smoke import admission, output
from audio_smoke_wire import BINDING, CURSOR, Peer, SECONDS, TITLE, TRACK_ID, audio_fixture
from fixture import Failure, check, private_env
from native import desktop, wait

RATE = 48000
BLOCK_MS = 20
BLOCK = RATE * BLOCK_MS // 1000
MARK_MS = 240
HOLD = 0.600
GRANT_DELAY = 0.180
START_WINDOW = 1.200
DEADLINE_DRAIN_MS = 180
STOP_BUDGET_MS = 360
TONES = (450, 650, 850, 550, 750, 950, 1050, 1150, 1250, 1350, 1450)
COEFFS = tuple(2 * cos(2 * pi * hz / RATE) for hz in TONES)
CASES = ("selected", "vacancy", "startup-drain", "remote", "output", "cursor", "denial", "disconnect", "deadline")
# Only emit known application-owned messages, never arbitrary element text.
LOGIN_STATUS = frozenset((
    "Enter an HTTP or HTTPS server address without a path, credentials, query, or fragment.",
    "Unable to reach the server. Check your connection and server address.",
    "Unable to initialize secure networking.",
    "Invalid username or password.",
    "The request contains invalid or oversized input.",
    "The server returned an incompatible API response.",
    "The server refused this request. Check your account permissions.",
    "This item is no longer available.",
    "The server redirected the request. Enter its direct origin instead.",
    "Too many requests. Please try again later.",
    "The server is temporarily unavailable.",
    "The server could not complete the request.",
    "Sign-in is unavailable. Reopen the desktop and try again.",
    "The desktop is busy. Please try again.",
    "Session expired. Sign in again.",
    "Sign-in canceled.",
))
LOGIN_BUTTONS = frozenset(("Sign in", "Signing in…", "Restoring session…", "Cancel sign in"))
LOGIN_ROUTES = frozenset(("/api/auth/login", "/api/auth/session", "/api/me/player/connect", "/api/me/player/socket"))


def timecoded_wav():
    """A nonrepeating pitch each 240 ms, then a sustained final pitch until EOF."""
    data = BytesIO()
    with wave.open(data, "wb") as wav:
        wav.setparams((1, 2, RATE, 0, "NONE", "not compressed"))
        for i, hz in enumerate(TONES):
            # Each pitch has an integer number of cycles in a 20 ms block.
            cycle = struct.pack(f"<{BLOCK}h", *(int(12000 * sin(2 * pi * j * hz / RATE)) for j in range(BLOCK)))
            blocks = (MARK_MS if i < len(TONES) - 1 else SECONDS * 1000 - i * MARK_MS) // BLOCK_MS
            wav.writeframes(cycle * blocks)
    return data.getvalue()


class Monitor:
    """Read the *recorded sink PCM*, not decoder state or the Slint elapsed label."""
    def __init__(self, pcm):
        self.pcm = pcm
        self.start = self.offset = pcm.stat().st_size & ~1
        self.blocks = []

    def sample(self):
        with self.pcm.open("rb") as stream:
            stream.seek(self.offset)
            raw = stream.read()
        size = len(raw) // (2 * BLOCK) * (2 * BLOCK)
        self.offset += size
        for at in range(0, size, 2 * BLOCK):
            samples = struct.unpack_from(f"<{BLOCK}h", raw, at)
            peak = max(map(abs, samples))
            marker = None
            if peak > 500:
                def power(coeff):
                    last = prev = 0.0
                    for v in samples:
                        last, prev = v + coeff * last - prev, last
                    return last * last + prev * prev - coeff * last * prev

                strengths = list(map(power, COEFFS))
                winner = max(range(len(TONES)), key=strengths.__getitem__)
                if strengths[winner] > 0.33 * BLOCK * sum(s * s for s in samples):
                    marker = winner
            self.blocks.append((monotonic(), peak, marker))

    def mark(self):
        # Fence at the recorder, including a partly buffered block.
        return (self.pcm.stat().st_size - self.start + 2 * BLOCK - 1) // (2 * BLOCK)

    def markers(self):
        # Two adjacent classified blocks constitute a witness. Ignore a single
        # straddling window; never collapse an intro repeated after another pitch.
        found = []
        for i, ((_, _, a), (seen, _, b)) in enumerate(zip(self.blocks, self.blocks[1:])):
            if a is not None and a == b and (not found or found[-1][0] != a):
                found.append((a, i, seen))
        return found

    def quiet(self, label, start=0):
        peak = max((p for _, p, _ in self.blocks[start:]), default=0)
        check(peak <= 32, f"{label}: unexpected PCM peak {peak} after monitor block {start}")

    def continuous(self):
        markers = self.markers()
        heard = [marker for marker, _, _ in markers]
        check(len(heard) >= 6 and heard == list(range(len(heard))),
              f"missing/repeated/reordered PCM pitch markers: {[TONES[i] for i in heard]}")
        gap = longest = 0
        for _, peak, _ in self.blocks[markers[0][1]:]:
            gap = gap + BLOCK_MS if peak <= 500 else 0
            longest = max(gap, longest)
        check(longest <= 100, f"PCM dropped for {longest} ms during confirmation")
        return [TONES[i] for i in heard], longest


class DelayedPeer(Peer):
    remote = dict(id="remote-connection", deviceId="remote-device", name="Synthetic remote output", canPlay=True)

    def __init__(self, handler):
        super().__init__(handler)
        self.grant_ms = None  # no implicit grant via Peer.transport
        self.sent_grant = None
        self.basis = None
        self.facts = []
        self.releases = 0
        self.show_remote = False
        self.play_received = None

    def roster(self):
        return ([self.remote] if self.show_remote else []) + super().roster()

    def outputs(self):
        self.send("outputs", owner=self.owner, devices=self.roster())

    def open_case(self, case):
        self.show_remote = case == "remote"
        self.owner = "" if case == "startup-drain" else (self.remote["id"] if self.show_remote else self.id)
        if self.owner:
            # Go's election runs before Snapshot, incrementing even an empty cursor's Run.
            self.player["cursor"] = dict(CURSOR, run=1)
        self.snapshot()
        self.outputs()

    def messages(self):
        for msg in super().messages():
            if msg["type"] == "play":
                check(self.play_received is None, "Desktop replayed Play on the same connection")
                self.play_received = monotonic()
            elif msg["type"] == "fact":
                check(self.sent_grant is not None, f"Desktop sent a fact before attached grant: {msg}")
            yield msg

    def transport(self, msg):
        check(msg["type"] != "sync", "Desktop rejected the fixture projection and requested repair")
        check(msg["type"] in ("presence", "pong", "fact", "play", "release"),
              f"unexpected Desktop write: {msg['type']}")
        super().transport(msg)
        if msg["type"] == "presence" and self.owner == self.id:
            self.basis = msg["basis"]
        elif msg["type"] == "release":
            self.releases += 1
            check(msg == {"type": "release"} and self.releases == 1, "duplicate/malformed release")
        elif msg["type"] == "fact":
            cursor = self.player["cursor"]
            check(self.owner == self.id and all(msg[k] == cursor[k] for k in BINDING),
                  f"fact did not bind to the confirmed local run: {msg}")
            check(not msg.get("error") and not msg["ended"] and isfinite(msg["elapsed"]),
                  f"unexpected fact before fixture EOF: {msg}")
            self.facts.append(msg)
            self.observe(cursor, msg)

    def assign(self, owner):
        self.owner = owner
        self.basis = None
        self.handle = "synthetic-" + token_hex(16)
        cursor = self.cursor(run=self.player["cursor"]["run"] + 1, playing=False)
        self.outputs()
        return cursor

    def retire_for_remote(self, *, target_disappears):
        # A different connection joined and then requested Select. A retiring
        # lease has no visible owner until the incumbent releases. Target leave
        # can win the race with release; the incumbent is then yielded, not re-elected.
        self.show_remote = True
        self.outputs()
        self.owner = ""
        self.outputs()
        if target_disappears:
            self.show_remote = False
            self.outputs()
        self.expect("retiring Desktop release", lambda m: m["type"] == "release", timeout=3)
        return self.assign("" if target_disappears else self.remote["id"])

    def grant(self):
        check(self.owner == self.id and self.basis is not None, "no local presence basis for delayed grant")
        fields = dict(basis=self.basis, ms=15000, handle=self.handle)
        self.sent_grant = monotonic()
        self.send("grant", **fields)

    def pump(self):
        while True:
            try:
                msg = self.incoming.get_nowait()
            except Empty:
                return
            if isinstance(msg, Exception):
                raise Failure(f"desktop account socket closed: {msg}") from msg
            check(msg["type"] in ("presence", "pong", "fact"), f"unexpected Desktop write: {msg['type']}")
            self.transport(msg)


def tick(peer, monitor, until=None, seconds=0):
    deadline = monotonic() + seconds if until is None else until
    while True:
        peer.pump()
        monitor.sample()
        if monotonic() >= deadline:
            return
        sleep(min(0.010, max(0, deadline - monotonic())))


def selected_in_window(win, name):
    wait(f"projected output {name!r}", lambda: any(
        win.matches(e, "Playback devices") and e.get("accessibleDescription") == name for e in win.elements()), 4)


def login_surface(win):
    try:
        elements = win.elements()
    except (RuntimeError, OSError, ValueError, KeyError) as error:
        return "MCP inspection unavailable (" + type(error).__name__ + ")"
    controls, statuses = set(), set()
    history = False
    for element in elements:
        if element.get("computedOpacity", 1) <= 0:
            continue
        role, label = element.get("accessibleRole"), element.get("accessibleLabel")
        if role == "Button" and label in LOGIN_BUTTONS:
            controls.add(label + (" [enabled]" if element.get("accessibleEnabled") else " [disabled]"))
        elif role == "Button" and label == "History":
            history = True
        elif role == "Text" and label in LOGIN_STATUS:
            statuses.add(label)
    surface = ", ".join(sorted(controls)) or ("History rendered (signed-in surface)" if history else "login controls absent")
    return surface + "; visible login status: " + (" ".join(sorted(statuses)) or "no recognized message")


def login(win, ready, admitted, requests, connections, case):
    stage = "enter server address"
    try:
        win.fill("Server( origin| URL| address)?", ready["origin"])
        stage = "enter username"
        win.fill("Username", ready["username"])
        stage = "enter password"
        win.fill("Password", ready["password"])
        stage = "click Sign in"
        win.click("Sign in")
        stage = "await account hello after clicking Sign in"
        return admission(admitted, requests, case)
    except (RuntimeError, OSError, ValueError, KeyError) as error:
        # No screenshots, tree dumps, field values, or raw exception bodies.
        reason = "no authenticated account hello" if isinstance(error.__cause__, Empty) else type(error).__name__
        routes = [(method if method in ("GET", "POST") else "other",
                   path if path in LOGIN_ROUTES else "<other>",
                   int(code) if isinstance(code, int) and 100 <= code <= 599 else "-")
                  for method, path, code in list(requests)]
        raise Failure(f"{stage}: {reason}; fake peer accepted TCP connections: {len(connections)}; "
                      f"loopback routes/status: {routes}; {login_surface(win)}") from None


def exercise(case, win, ready, admitted, requests, connections, pcm):
    peer = login(win, ready, admitted, requests, connections, case)
    peer.open_case(case)
    peer.expect("opening presence", lambda m: m["type"] == "presence")
    win.section("History")
    selected_in_window(win, peer.hello["name"] if peer.owner == peer.id else
                       peer.remote["name"] if peer.owner else "No output selected")
    if case == "vacancy":
        check(peer.retire_for_remote(target_disappears=True) == dict(CURSOR, run=2) and
              not peer.owner and [device["id"] for device in peer.roster() if device["canPlay"]] == [peer.id],
              "vacancy must be an empty owner/cursor after retirement/release with only local eligible")
        selected_in_window(win, "No output selected")
        peer.ping("retirement-complete")

    win.fill("Search tracks, albums or artists", TITLE)
    play = win.find("Play " + TITLE, roles=("Button",))
    monitor = Monitor(pcm)
    tick(peer, monitor, seconds=0.18)
    monitor.quiet("before explicit Play")
    click_fence = monotonic()
    win.click_element(play)
    command = peer.command("play")
    check(command["selection"] == {"kind": "track", "track": {"id": TRACK_ID}} and
          not command.get("shuffle") and "seed" not in command,
          "expected the original non-shuffle, context-free catalog Track Play")
    commanded = peer.play_received
    if case == "vacancy":
        check(peer.assign(peer.id)["run"] == 3, "Play did not get the pre-resolution owner-change cursor")
    tick(peer, monitor, until=commanded + HOLD)
    check(peer.sent_grant is None and peer.player["cursor"]["key"] == 0, "fixture published confirmation early")
    if case in ("startup-drain", "remote"):
        monitor.quiet("Play waiting on a startup drain or another device")
        cursor = peer.play(command)
        tick(peer, monitor, seconds=0.35 if case == "startup-drain" else 0.8)
        monitor.quiet("resolved Play without local output authority")
        if case == "startup-drain":
            # The virtual startup drain expires: Go bumps Run before Outputs,
            # then answers an eligible local presence with an attached grant.
            current = peer.assign(peer.id)
            check(current["run"] == cursor["run"] + 1, "missing drain-end cursor")
            peer.expect("local presence after startup drain", lambda m: m["type"] == "presence")
            tick(peer, monitor, seconds=GRANT_DELAY)
            monitor.quiet("startup-drain election published, attached grant still withheld")
            peer.grant()
            tick(peer, monitor, seconds=1.0)
            check(monitor.markers() and any(m["playing"] for m in peer.facts),
                  "startup-drain positive control did not play after election and grant")
            print("PASS startup-drain: silent even after Play resolved; PCM/facts only after election and grant", flush=True)
        else:
            print("PASS remote: no Desktop PCM/facts before or after Play resolved under the remote owner", flush=True)
        return

    marks = monitor.markers()
    check(marks and marks[0][0] == 0,
          f"{case}: no synthetic PCM intro before canonical Play cursor and attached grant ({HOLD:.3f}s hold)")
    onset = marks[0][2]
    print(f"EVIDENCE {case}: {TONES[0]} Hz PCM witnessed {(onset-click_fence)*1000:.0f} ms after click; "
          f"at least {(monotonic()-onset)*1000:.0f} ms before correction or Play confirmation; no grant/facts",
          flush=True)

    if case == "deadline":
        # A slow resolver can legitimately take longer than Desktop's 1.2 s
        # provisional window. No correction/confirmation/grant triggers silence.
        tick(peer, monitor, until=commanded + START_WINDOW)
        stop_by = monitor.mark() + DEADLINE_DRAIN_MS // BLOCK_MS
        tick(peer, monitor, seconds=0.68)
        quiet_ms = (len(monitor.blocks) - stop_by) * BLOCK_MS
        check(quiet_ms >= 400, "too little post-deadline PCM captured")
        monitor.quiet("provisional PCM survived its deadline plus sink drain", stop_by)
        peer.ping("still-unconfirmed-after-deadline")
        print(f"EVIDENCE deadline: {quiet_ms}ms of silence after {START_WINDOW:.1f}s + "
              f"{DEADLINE_DRAIN_MS}ms recorder allowance; no cursor, grant or native facts", flush=True)
        late = Monitor(pcm)
        peer.play(command)
        peer.grant()
        tick(peer, late, seconds=1.25)
        check(late.markers() and late.markers()[0][0] == 0 and any(m["playing"] for m in peer.facts),
              "late canonical Play + grant did not start a fresh authorized intro")
        print("PASS deadline: PCM expired without confirmation; late Go resolution played from the intro", flush=True)
        return

    if case in ("selected", "vacancy"):
        cursor_at = monotonic()
        peer.play(command)  # Go resolver: source-tagged replace, new chain, visit/run 0
        tick(peer, monitor, until=cursor_at + GRANT_DELAY)
        # Fence the socket after sending the canonical cursor but before any
        # grant; the reader rejects facts throughout both withheld windows.
        peer.ping("before-first-grant")
        grant_fence = monitor.mark()
        peer.grant()
        tick(peer, monitor, until=peer.sent_grant + 2.9)
        check((len(monitor.blocks) - grant_fence) * BLOCK_MS >= 2400, "too little post-grant PCM captured")
        pitches, gap = monitor.continuous()
        elapsed = [m["elapsed"] for m in peer.facts if m["playing"] and not m["buffering"]]
        check(len(elapsed) >= 2 and elapsed[0] >= 0.30 and elapsed[-1] - elapsed[0] >= 0.50 and
              all(b >= a - 0.05 for a, b in zip(elapsed, elapsed[1:])), f"adopted positions did not continue: {elapsed}")
        check(onset < cursor_at < peer.sent_grant, "PCM/cursor/grant happened out of order")
        print(f"PASS {case}: PCM → Play cursor +{(cursor_at-commanded)*1000:.0f}ms → "
              f"grant +{(peer.sent_grant-commanded)*1000:.0f}ms (from wire Play); "
              f"pitches {pitches}, max gap {gap}ms; only confirmed facts, "
              f"advancing {elapsed[0]:.2f} → {elapsed[-1]:.2f}s", flush=True)
        return

    correction_fence = monitor.mark()
    if case == "output":
        peer.retire_for_remote(target_disappears=False)
        peer.play(command)  # pending catalog lookup can still resolve; remote remains owner
    elif case == "cursor":
        # A remote Close cancels the pending Play and clears traversal, even
        # when empty, without an origin tag on the immediate state change.
        peer.show_remote = True
        peer.outputs()
        peer.player["cursor"] = CURSOR.copy()
        peer.send("player", player=dict(queue=dict(kind="replace"), cursor=CURSOR, shuffle=False))
    elif case == "denial":
        # A failed/stale resolution is NOT an error envelope. It returns the
        # unchanged canonical cursor tagged with the issuing connection.
        peer.send("player", player=dict(source=peer.id, cursor=peer.player["cursor"]))
    else:
        peer.disconnect()
        # There was never a grant or an actual cursor to continue detached.
        # Keep the new hello pending briefly, then show the old, disconnected binding.
        old = peer
        peer = admission(admitted, requests, "post-Play reconnect")
        check(peer.id != old.id and peer.hello["deviceId"] == old.hello["deviceId"], "not a new connection for the same device")
        monitor.sample()
        peer.owner = old.id
        peer.player = old.player
        peer.snapshot()
        peer.outputs()
        peer.expect("reconnect presence", lambda m: m["type"] == "presence")
    tick(peer, monitor, seconds=0.90)
    stop_by = correction_fence + STOP_BUDGET_MS // BLOCK_MS
    check(len(monitor.blocks) >= stop_by + 400 // BLOCK_MS, f"{case}: too little post-correction PCM captured")
    monitor.quiet(f"{case} correction did not stop playback within {STOP_BUDGET_MS} ms", stop_by)
    print(f"PASS {case}: correction stopped PCM within {STOP_BUDGET_MS} ms, no attached grant or facts", flush=True)


def tcp_port(text):
    port = int(text)
    if not 1024 <= port <= 65535:
        raise argparse.ArgumentTypeError("expected an unprivileged TCP port (1024–65535)")
    return port


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--desktop", required=True, type=Path, help="trusted Slint MCP debug desktop")
    parser.add_argument("--scratch-dir", type=Path, default=Path("/dev/shm"), help="short existing directory for private sockets")
    parser.add_argument("--case", choices=CASES, action="append", help="repeat to select cases; default: all")
    parser.add_argument("--peer-port", type=tcp_port, default=0,
                        help="fixed loopback HTTP + account WebSocket port (default: ephemeral)")
    parser.add_argument("--mcp-port", type=tcp_port, default=0,
                        help="fixed loopback Slint MCP port (default: ephemeral)")
    args = parser.parse_args()
    if args.peer_port and args.peer_port == args.mcp_port:
        parser.error("peer and MCP need distinct TCP ports")
    failures = []
    for case in args.case or CASES:
        try:
            with TemporaryDirectory(prefix="late-go-", dir=args.scratch_dir.resolve(strict=True)) as temp:
                root = Path(temp)
                env = private_env(root)
                connections = deque(maxlen=64)
                with (audio_fixture(wav=timecoded_wav(), peer_class=DelayedPeer, port=args.peer_port,
                                    accepted_connections=connections) as (ready, admitted, _media, requests),
                      output(root, env) as pcm,
                      desktop(args.desktop.resolve(strict=True), root, env, mcp_port=args.mcp_port) as win):
                    exercise(case, win, ready, admitted, requests, connections, pcm)
        except (Failure, OSError, ValueError, subprocess.SubprocessError) as error:
            failures.append(case)
            print(f"FAIL {case}: {error}", file=sys.stderr, flush=True)
    return int(bool(failures))


if __name__ == "__main__":
    sys.exit(main())
