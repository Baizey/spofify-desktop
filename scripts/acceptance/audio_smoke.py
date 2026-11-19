#!/usr/bin/env python3
"""Synthetic WS v2 executor smoke; real Slint desktop + ALSA, not Go integration.

Requires a prebuilt Slint MCP debug binary, cage, PipeWire and its ALSA plugin,
pipewire-pulse, WirePlumber 0.5 (policy profile), pactl and parec. No real account,
compositor, sound device or ambient audio server is used. All output is temporary.
"""
import argparse
from contextlib import contextmanager, ExitStack
from pathlib import Path
from queue import Empty
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory
from time import monotonic, sleep
from urllib.error import URLError

# No checkout artifacts even if the command is run without Python's -B flag.
sys.dont_write_bytecode = True
from audio_smoke_wire import audio_fixture, SECONDS, TITLE, TRACK_ID
from fixture import Failure, check, private_env
from native import mcp_class, wait, Window


@contextmanager
def child(command, root, env, stdout=None):
    with (root / (Path(command[0]).name + ".log")).open("w+b") as log:
        process = subprocess.Popen(command, cwd=root, env=env, stdout=stdout if stdout is not None else log,
                                   stderr=log, start_new_session=True)
        try:
            yield process
        finally:
            try:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=2)
            except (ProcessLookupError, subprocess.TimeoutExpired):
                pass
            # Cage may exit before its desktop child. Target only this owned group.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=3)


def require_running(process, root, name):
    if process.poll() is not None:
        tail = (root / (name + ".log")).read_bytes()[-2048:].decode(errors="replace")
        raise Failure(f"owned {name} exited ({process.returncode}): {tail}")


@contextmanager
def output(root, env):
    runtime = Path(env["XDG_RUNTIME_DIR"])
    alsa = root / "asound.conf"
    alsa.write_text('pcm.!default { type pipewire playback_node "synthetic_smoke" '
                    'hint { show on description "Disposable synthetic smoke sink" } }\n')
    env.update(ALSA_CONFIG_PATH=str(alsa), PIPEWIRE_RUNTIME_DIR=str(runtime),
               PULSE_SERVER=f"unix:{runtime}/pulse/native",
               DBUS_SESSION_BUS_ADDRESS=f"unix:path={runtime}/no-bus",
               DBUS_SYSTEM_BUS_ADDRESS=f"unix:path={runtime}/no-bus")
    pcm = root / "monitor.s16le"
    with ExitStack() as stack, pcm.open("wb") as recording:
        pw = stack.enter_context(child(["pipewire"], root, env))
        pulse = stack.enter_context(child(["pipewire-pulse"], root, env))
        policy = stack.enter_context(child(["wireplumber", "--profile=policy"], root, env))

        def ready():
            for process, name in ((pw, "pipewire"), (pulse, "pipewire-pulse"), (policy, "wireplumber")):
                require_running(process, root, name)
            if not (runtime / "pulse/native").is_socket():
                return False
            info = subprocess.run(["pactl", "info"], env=env, capture_output=True, timeout=2)
            return info.returncode == 0

        wait("private Pulse control socket", ready, 5)
        subprocess.run(["pactl", "load-module", "module-null-sink", "sink_name=synthetic_smoke"],
                       env=env, capture_output=True, check=True, timeout=3)
        sinks = subprocess.check_output(["pactl", "list", "short", "sinks"], env=env, timeout=3, text=True)
        check([line.split()[1] for line in sinks.splitlines()] == ["synthetic_smoke"], "unexpected sink in private graph")
        recorder = stack.enter_context(child(["parec", "--raw", "--format=s16le", "--rate=48000", "--channels=1",
                                             "--latency-msec=25", "--device=synthetic_smoke.monitor"], root, env,
                                            stdout=recording))
        wait("private monitor PCM", lambda: pcm.stat().st_size >= 4800 or require_running(recorder, root, "parec"), 6)
        yield pcm


@contextmanager
def desktop(binary, root, env):
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    env = dict(env, WLR_BACKENDS="headless", WLR_RENDERER="pixman", WLR_HEADLESS_OUTPUTS="1",
               SLINT_BACKEND="winit-software", SLINT_MCP_PORT=str(port), SPOFIFY_DEV_SIZE="1100x760")
    mcp = mcp_class()(port)
    with child(["cage", "--", str(binary)], root, env) as process:
        def discover():
            require_running(process, root, "cage")
            try:
                handles = mcp.value("list_windows", {})["windowHandles"]
            except URLError:
                return None
            check(len(handles) <= 1, "expected one owned Slint window")
            return handles[0] if handles else None

        yield Window(mcp, wait("Slint MCP", discover, 15), None)


def pcm_tail(pcm, length=24000):
    with pcm.open("rb") as file:
        size = file.seek(0, os.SEEK_END)
        file.seek(max(0, (size - length) & ~1))
        raw = file.read(length)
    return [s for (s,) in struct.iter_unpack("<h", raw[:len(raw) & ~1])]


def heard_tone(pcm):
    samples = pcm_tail(pcm)
    crossings = [i for i in range(1, len(samples)) if samples[i - 1] <= 0 < samples[i]]
    periods = sum(98 <= b - a <= 102 for a, b in zip(crossings, crossings[1:]))
    return periods >= 40 and max(map(abs, samples), default=0) > 500


def silence(pcm, label):
    # Let a bounded amount of already queued hardware-buffer-equivalent audio drain.
    baseline = pcm.stat().st_size
    wait(label + " drain", lambda: pcm.stat().st_size - baseline >= 72000, 3)
    check(max(map(abs, pcm_tail(pcm)), default=0) <= 8, label + ": monitor was not silent")


def run(win, ready, admitted, media, requests, pcm):
    win.fill("Server( origin| URL| address)?", ready["origin"])
    win.fill("Username", ready["username"])
    win.fill("Password", ready["password"])
    win.click(r"Allow trusted.LAN HTTP")
    win.click("Sign in")
    win.find("Home", 15)
    try:
        peer = admitted.get(timeout=12)
    except Empty as error:
        raise Failure(f"no authenticated v2 WebSocket hello; loopback routes/status: {list(requests)}") from error
    if isinstance(peer, Exception):
        raise Failure("synthetic socket admission: " + str(peer)) from peer
    peer.state(opening=True, selected=False)
    print("PASS Slint sign-in; authenticated v2 upgrade and one-use ticket hello (canPlay=true)", flush=True)
    win.click("Search")
    win.section("Search")
    win.fill("Search music", TITLE)
    win.find("Open " + TITLE)

    def play(click, kind="queueIntent", *, elapsed=0, lease=8000, source=False):
        win.click(click)
        command = peer.command(kind)
        if kind == "queueIntent":
            cmd = command["command"]
            check(cmd["intent"] == "now" and cmd["selection"] == {"kind": "track", "track": {"id": TRACK_ID}},
                  "clicked search result sent unexpected selection")
            peer.select_track()
        peer.state(playing=True, elapsed=elapsed, selected=source, phase="preparing", source=source)
        peer.result(command)
        peer.instruct("prepare", elapsed=elapsed, command=command)
        if source:
            peer.instruct("release", command=command, after_send=lambda: peer.state(
                playing=True, elapsed=elapsed, phase="releasing", source=True))
        peer.version += 1
        i = peer.instruct("activate", playing=True, elapsed=elapsed, duration_ms=lease, command=command,
                          after_send=lambda: peer.state(playing=True, elapsed=elapsed, phase="activating", source=source))
        peer.state(playing=True, elapsed=elapsed)
        peer.result(command, final=True)
        return i

    silence(pcm, "before play")
    active = play("Play " + TITLE)
    initial = peer.report(active, lambda m: m["isPlaying"], "playing report")
    wait("480 Hz fixture tone at the ALSA sink monitor", lambda: heard_tone(pcm), 3)
    advanced = peer.report(active, lambda m: m["isPlaying"] and m["elapsed"] >= initial["elapsed"] + 0.5,
                           "advancing native elapsed")
    check(media and advanced["duration"] == SECONDS, "missing WAV delivery or incorrect decoded duration")
    print(f"PASS queueIntent(now) → prepare → activate; WAV {media[0]} bytes; native elapsed "
          f"{initial['elapsed']:.2f} → {advanced['elapsed']:.2f}s; captured 480 Hz", flush=True)

    win.click("Pause")
    cmd = peer.command("pause")
    elapsed = advanced["elapsed"]
    peer.state(playing=False, elapsed=elapsed)
    peer.result(cmd)
    paused = peer.instruct("execute", elapsed=elapsed, duration_ms=8000, command=cmd)
    peer.result(cmd, final=True)
    peer.report(paused, lambda m: not m["isPlaying"] and not m["ended"], "stopped report after pause")
    silence(pcm, "after pause ACK")
    active = play("Play|Resume", "resume", elapsed=elapsed, source=True)
    resumed = peer.report(active, lambda m: m["isPlaying"], "resumed native playback")
    check(elapsed - 0.1 <= resumed["elapsed"] < elapsed + 3.5, "resume did not restore the position")
    wait("resumed fixture tone", lambda: heard_tone(pcm), 3)
    ended = peer.report(active, lambda m: m["ended"], "native ended report")
    check(not ended["isPlaying"] and ended["elapsed"] == ended["duration"] == SECONDS, "incorrect natural end facts")
    # Repeat off: Go parks the final occurrence by issuing one stopped execute.
    parked = peer.instruct("execute", elapsed=SECONDS, duration_ms=8000,
                           after_send=lambda: peer.state(playing=False, elapsed=SECONDS))
    peer.report(parked, lambda m: not m["isPlaying"] and not m["ended"], "stopped report after natural end")
    print("PASS Pause command/ACK + silent monitor; Resume command + tone; bound natural ended report", flush=True)
    silence(pcm, "after natural end")

    before_replay = monotonic()
    short = play("Play " + TITLE, source=True, lease=3500)
    peer.report(short, lambda m: m["isPlaying"] and not m["ended"], "short-lease playback", renew=False)
    wait("tone with short lease", lambda: heard_tone(pcm), 2)
    # Withhold renewals while keeping this exact socket alive and desiredPlaying=true.
    sleep(3)
    silence(pcm, "after lease expiration")
    check(monotonic() - before_replay < SECONDS - 0.5, "silence was not verified before natural end")
    peer.frame(9, b"synthetic-after-expiry")
    peer.expect("pong on the same socket after lease expires", lambda m: m == {"type": "pong", "data": b"synthetic-after-expiry"})
    print("PASS short lease with no renewal: tone stopped before WAV end; same WebSocket answered ping", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--desktop", type=Path, default=Path.home() / "repos/spofify-desktop/target/debug/spofify-desktop")
    args = parser.parse_args()
    try:
        binary = args.desktop.resolve(strict=True)
        for name in ("cage", "pipewire", "pipewire-pulse", "wireplumber", "pactl", "parec"):
            check(shutil.which(name), f"synthetic audio smoke needs {name}")
        print("SYNTHETIC WS v2 NATIVE AUDIO ONLY — not real Go integration", flush=True)
        # Short, absolute path so the private Unix sockets fit sun_path.
        with TemporaryDirectory(prefix="aud-smoke-", dir="/tmp") as temp:
            root = Path(temp)
            env = private_env(root)
            with audio_fixture() as (ready, admitted, media, requests), output(root, env) as pcm, desktop(binary, root, env) as win:
                run(win, ready, admitted, media, requests, pcm)
    except (Failure, OSError, subprocess.SubprocessError) as error:
        print(f"FAIL synthetic native audio smoke: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
