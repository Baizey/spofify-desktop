"""Bounded window-close and real Go output reattachment checks."""
from contextlib import contextmanager, closing
from pathlib import Path
import socket
import sqlite3
import struct
from threading import Event, Thread
from time import monotonic
from urllib.error import URLError
import wave

from audio_smoke import child, heard_tone, silence
from fixture import Failure, check
from native import Window, mcp_class, player_bar, wait
from playback import Peer
from remember_login_wire import admitted, event, no_writes, settle, state, vacant_output
from workspace_checks import open_manual, open_playlists, play_manual


@contextmanager
def closeable_desktop(binary, root, env):
    """Use a real headless Slint window; cleanup signals aren't the test action."""
    with socket.socket() as reserved:
        reserved.bind(("127.0.0.1", 0))
        port = reserved.getsockname()[1]
    launch = dict(env, WLR_BACKENDS="headless", WLR_RENDERER="pixman", WLR_HEADLESS_OUTPUTS="1",
                  SLINT_BACKEND="winit-software", SLINT_MCP_PORT=str(port))
    check(not {"DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"} & launch.keys(),
          "native acceptance requires a private display environment")
    mcp = mcp_class()(port)
    with child(["cage", "--", str(binary)], root, launch) as process:
        def discover():
            check(process.poll() is None, "owned Cage/desktop exited before readiness")
            try:
                windows = mcp.value("list_windows", {})["windowHandles"]
            except URLError:
                return None
            check(len(windows) <= 1, "expected only the owned Slint desktop window")
            return windows[0] if windows else None

        handle = wait("Slint MCP on owned Wayland display", discover, 15)

        def close():
            check(process.poll() is None, "owned Cage/desktop exited before the close request")
            sockets = [p for p in Path(launch["XDG_RUNTIME_DIR"]).glob("wayland-*") if p.is_socket()]
            check(len(sockets) == 1, "expected one owned Cage display socket")
            close_toplevel(sockets[0])
            check(process.wait(timeout=4) == 0, "owned Cage/desktop did not exit cleanly after CloseRequested")

        yield Window(mcp, handle, None), close


def close_toplevel(path):
    # Minimal Wayland wire client for Cage's wlr-foreign-toplevel-management
    # v1. Cage translates toplevel_handle.close into xdg_toplevel.close, so
    # winit delivers a genuine CloseRequested to the app. No test-only build
    # feature or real display/input is needed.
    interface = b"zwlr_foreign_toplevel_manager_v1\0"
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        conn.settimeout(2)
        conn.connect(str(path))

        def request(object_id, opcode, payload=b""):
            conn.sendall(struct.pack("=II", object_id, ((8 + len(payload)) << 16) | opcode) + payload)

        with conn.makefile("rb") as stream:
            def roundtrip(callback_id):
                request(1, 0, struct.pack("=I", callback_id))  # wl_display.sync
                for _ in range(128):
                    header = stream.read(8)
                    check(len(header) == 8, "owned Wayland socket closed during close request setup")
                    obj, info = struct.unpack("=II", header)
                    size, opcode = info >> 16, info & 0xffff
                    check(8 <= size <= 4096 and size % 4 == 0, "unexpected Wayland message size")
                    body = stream.read(size - 8)
                    check(len(body) == size - 8 and (obj, opcode) != (1, 0), "owned display rejected Wayland request")
                    if (obj, opcode) == (callback_id, 0):
                        return
                    yield obj, opcode, body
                raise Failure("too many events from owned Cage display")

            request(1, 1, struct.pack("=I", 2))  # wl_display.get_registry
            globals_ = [struct.unpack_from("=I", body)[0] for obj, op, body in roundtrip(3)
                        if (obj, op) == (2, 0) and body[8:8 + len(interface)] == interface]
            check(len(globals_) == 1, "--output-relaunch requires Cage with wlr-foreign-toplevel-management")
            request(2, 0, struct.pack("=II", globals_[0], len(interface)) +
                    interface + b"\0" * (-len(interface) % 4) + struct.pack("=II", 1, 4))  # wl_registry.bind
            toplevels = [struct.unpack("=I", body)[0] for obj, op, body in roundtrip(5) if (obj, op) == (4, 0)]
            check(len(toplevels) == 1, "expected one owned native toplevel to close")
            request(toplevels[0], 5)  # zwlr_foreign_toplevel_handle_v1.close


def prepare_tone(root, go):
    # Make the first synthetic tone longer than Desktop's 2s reporting
    # interval. Only edit the generated fixture catalog and WAV, before
    # admission; auth, playback resolution and streaming stay real Go.
    state = root / "fixture" / "offline-fixture"
    check(str(state) == go["stateRoot"], "unexpected disposable playback fixture root")
    tone = state / "audio" / "tone-001.wav"
    duration = 24
    with wave.open(str(tone), "rb") as wav:
        check((wav.getnchannels(), wav.getsampwidth(), wav.getframerate(), wav.getnframes()) == (1, 2, 8000, 8000),
              "stock fixture tone shape changed")
        samples = wav.readframes(8000)
    with wave.open(str(tone), "wb") as wav:
        wav.setparams((1, 2, 8000, 0, "NONE", ""))
        wav.writeframes(samples * duration)
    try:
        with closing(sqlite3.connect((state / "catalog.db").as_uri() + "?mode=rw", uri=True, timeout=3)) as db, db:
            changed = db.execute("UPDATE tracks SET duration_ms = ? WHERE id = ? AND duration_ms = 1000",
                                 (duration * 1000, go["duplicateTrackId"]))
            check(changed.rowcount == 1, "stock fixture tone metadata changed")
    except sqlite3.Error as error:
        raise Failure("could not extend the disposable Go fixture tone metadata") from error


def same_run(left, right):
    return all(left[k] == right[k] for k in ("chain", "visit", "run", "key", "desired"))


def latest_grant(receipt, mark, connection):
    return next((row for row in reversed(receipt.events(mark)) if row[0] == "grant" and row[1] == connection), None)


def continued(receipt, mark, connection, previous):
    grant = wait("same-device output grant", lambda: latest_grant(receipt, mark, connection), timeout=6)
    check(grant[2] == previous[2] and grant[3] < previous[3] + previous[4] / 1000,
          "desktop got a new lease rather than rebinding the live one")


def no_pcm(pcm, mark):
    # Check every sample since the launch fence, not just a silent tail.
    with pcm.open("rb") as recording:
        recording.seek(mark & ~1)
        samples = recording.read(30 * 48000 * 2 + 1)
    check(0 < len(samples) <= 30 * 48000 * 2 and
          max((abs(sample) for sample, in struct.iter_unpack("<h", samples[:len(samples) & ~1])), default=0) <= 8,
          "unexpected PCM (or missing/beyond-bound monitor evidence) during silent relaunch")


class RemoteOutput(Peer):
    """A fake playback-capable device using the existing real Go peer transport."""
    def __init__(self, api):
        super().__init__(api, name="Acceptance remote output", can_play=True)
        self.basis = 0

    def renew(self):
        self.basis += 1
        self.send(dict(type="presence", basis=self.basis))
        self.wait_frame(lambda frame: frame["type"] == "grant" and frame["basis"] == self.basis, timeout=4)

    def take_over(self):
        self.send(dict(type="select", deviceId=self.device_id))
        self.wait(lambda room: room["owner"] == self.connection_id, timeout=8)
        self.renew()
        self.send(dict(type="resume"))
        cursor = self.wait(lambda room: room["player"]["cursor"]["desired"])["player"]["cursor"]
        identity = {key: cursor[key] for key in ("chain", "visit", "run", "key", "elapsed")}
        self.send(dict(type="fact", **identity, playing=True, buffering=False, ended=False))
        return self.wait(lambda room: room["player"]["cursor"]["playing"])["player"]

    @contextmanager
    def keep_live(self):
        end = Event()
        errors = []

        def run():
            try:
                while not end.wait(0.8):
                    self.renew()
                    check(self.snapshot["owner"] == self.connection_id, "remote fake output lost ownership")
            except (Failure, OSError, ValueError):
                errors.append(True)

        thread = Thread(target=run, name="acceptance-remote-output")
        thread.start()
        try:
            yield
        finally:
            end.set()
            thread.join(timeout=5)
            check(not thread.is_alive() and not errors, "remote fake output lease stopped renewing")


@contextmanager
def output_relaunches(binary, root, env, receipt, pcm, mark, *, on_restore, remote_api):
    with closeable_desktop(binary, root, env) as (window, close):
        window.section("History")
        on_restore()
        device, old = vacant_output(receipt, mark)
        wait("Go default output grant", lambda: latest_grant(receipt, mark, old))
        # A passive real Go observer fences canonical state at disconnect.
        with Peer(remote_api) as observer:
            open_playlists(window)
            open_manual(window)
            play_mark, start = len(receipt.events()), pcm.stat().st_size
            play_manual(window)
            wait("canonical playlist selection", lambda: ("owned-playlist-selection",) in receipt.events(play_mark))
            wait("220 Hz on private null sink", lambda: heard_tone(pcm, hz=220, start=start), 5)
            _, _, _, active = wait("nonzero canonical playing cursor", lambda: state(
                receipt, play_mark, connection=old, owner=old,
                cursor=lambda c: c["desired"] and c["playing"] and c["elapsed"] >= 0.5))
            play_events = receipt.events(play_mark)
            check(play_events.count(("client", "play")) == 1 and ("playlist-play", True) in play_events,
                  "native playlist Play did not issue exactly one scoped command")
            close_mark = len(receipt.events())
            old_grant = latest_grant(receipt, mark, old)
            close()
            wait("closed desktop socket", lambda: event(receipt, close_mark, "socket-end", lambda r: r[1] == old))
            detached = observer.wait(lambda room: all(d["id"] != old for d in room["devices"]), timeout=4)
            check(detached["owner"] == old and detached["player"]["queue"] == active["queue"] and
                  same_run(detached["player"]["cursor"], active["cursor"]) and
                  detached["player"]["cursor"]["elapsed"] >= active["cursor"]["elapsed"],
                  "closing the playing window changed canonical playback or released its lease")
            no_writes(receipt, close_mark, playing=True)

    mark, start = len(receipt.events()), pcm.stat().st_size
    with closeable_desktop(binary, root, env) as (window, close):
        _, new, owner, initial = admitted(receipt, mark, device)
        check(new != old and owner == old and initial == detached["player"],
              "playing relaunch did not start with the same canonical queue and cursor on a new connection")
        continued(receipt, mark, new, old_grant)
        wait("same canonical run playing on new connection", lambda: state(
            receipt, mark, connection=new, owner=new, cursor=lambda c: same_run(c, initial["cursor"]) and
            c["playing"] and c["elapsed"] >= initial["cursor"]["elapsed"] - 0.15))
        wait("220 Hz after playing relaunch", lambda: heard_tone(pcm, hz=220, start=start), 5)
        settle(receipt, mark)
        no_writes(receipt, mark, playing=True)
        print("PASS playing window closed gracefully; same device/new connection kept the Go lease and canonical cursor, with PCM and no replay", flush=True)
        pause_mark = len(receipt.events())
        window.click("Pause", within=player_bar(window))
        _, _, _, paused = wait("Go paused cursor", lambda: state(
            receipt, pause_mark, connection=new, owner=new,
            cursor=lambda c: not c["desired"] and not c["playing"] and c["key"] == initial["cursor"]["key"]))
        check(receipt.events(pause_mark).count(("client", "pause")) == 1, "expected one native Pause command")
        silence(pcm, "paused desktop")
        old_grant = latest_grant(receipt, mark, new)
        close_mark = len(receipt.events())
        close()
        wait("paused desktop socket closed", lambda: event(receipt, close_mark, "socket-end", lambda r: r[1] == new))
        no_writes(receipt, close_mark)

    mark, start = len(receipt.events()), pcm.stat().st_size
    with closeable_desktop(binary, root, env) as (window, close):
        _, third, owner, initial = admitted(receipt, mark, device)
        check(third != new and owner == new and initial == paused,
              "paused relaunch did not preserve the canonical queue and paused cursor")
        continued(receipt, mark, third, old_grant)
        settle(receipt, mark)
        wait("paused canonical cursor rebound to Desktop", lambda: state(
            receipt, mark, connection=third, owner=third, cursor=lambda c: c == paused["cursor"]))
        check(all(row[3] == paused for row in receipt.events(mark) if row[0] == "state"),
              "paused relaunch changed the canonical player state")
        no_writes(receipt, mark)
        no_pcm(pcm, start)
        print("PASS paused relaunch kept the same paused cursor without PCM, media or transport writes", flush=True)

        with RemoteOutput(remote_api) as remote:
            # The simulated device requests an ordinary Go handoff from the
            # incumbent Desktop; the desktop user never clicks Deselect.
            remote_player = remote.take_over()
            with remote.keep_live():
                close()
                wait("third desktop socket closed", lambda: event(receipt, mark, "socket-end", lambda r: r[1] == third))
                mark, start = len(receipt.events()), pcm.stat().st_size
                with closeable_desktop(binary, root, env) as (window, _):
                    _, fourth, owner, initial = admitted(receipt, mark, device)
                    check(fourth != third and owner == remote.connection_id and initial == remote_player,
                          "desktop relaunch replaced the live remote owner or changed its cursor")
                    settle(receipt, mark)
                    states = [row for row in receipt.events(mark) if row[0] == "state"]
                    check(all(row[2:] == (remote.connection_id, remote_player) for row in states) and
                          not latest_grant(receipt, mark, fourth), "desktop interfered with live remote playback")
                    no_writes(receipt, mark)
                    no_pcm(pcm, start)
                    print("PASS relaunch left the live remote owner and playing cursor untouched, with no Desktop PCM or writes", flush=True)
                    yield window
