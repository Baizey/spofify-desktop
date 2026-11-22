#!/usr/bin/env python3
"""Synthetic account-state executor smoke; real Slint desktop + ALSA, not Go integration.

Requires a prebuilt Slint MCP debug binary, cage, wlr-randr, PipeWire and its ALSA plugin,
pipewire-pulse, WirePlumber 0.5 (policy profile), pactl and parec. No real account,
compositor, sound device or ambient audio server is used. All output is temporary.
"""
import argparse
from contextlib import contextmanager, ExitStack
from math import cos, pi
from pathlib import Path
from queue import Empty
import os
import shutil
import signal
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory
from time import monotonic, sleep

# No checkout artifacts even if the command is run without Python's -B flag.
sys.dont_write_bytecode = True
from audio_smoke_wire import audio_fixture, SECONDS, TITLE, TRACK_ID
from fixture import Failure, check, private_env
from native import COMPACT, ESCAPE, WIDE, assert_dock_layout, bounds, desktop, element_type, verify, wait


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
    for name in ("pipewire", "pipewire-pulse", "wireplumber", "pactl", "parec"):
        check(shutil.which(name), f"private PCM monitoring needs {name}")
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


def pcm_tail(pcm, length=24000):
    with pcm.open("rb") as file:
        size = file.seek(0, os.SEEK_END)
        file.seek(max(0, (size - length) & ~1))
        raw = file.read(length)
    return [s for (s,) in struct.iter_unpack("<h", raw[:len(raw) & ~1])]


def heard_tone(pcm, hz=480, start=None):
    if start is None:
        samples = pcm_tail(pcm)
    else:
        # Short Go fixture audio may already have finished by the time the
        # observer sees Play. Inspect only PCM added after the click fence.
        with pcm.open("rb") as file:
            file.seek(start & ~1)
            raw = file.read(6 * 48000 * 2)
        samples = [s for (s,) in struct.iter_unpack("<h", raw[:len(raw) & ~1])]
    # Narrowband power over windowed 250 ms slices. A monitor discontinuity
    # (e.g. a quantum filled with zeros) is not a different pitch. Compare
    # neighboring bins too: Go's next fixture track is only 7 Hz higher.
    rate, size = 8000, 2000
    decimated = samples[::6]
    window = tuple(0.5 - 0.5 * cos(2 * pi * i / size) for i in range(size))

    def power(weighted, frequency):
        coeff = 2 * cos(2 * pi * frequency / rate)
        prev = previous = 0.0
        for sample in weighted:
            prev, previous = sample + coeff * prev - previous, prev
        return prev * prev + previous * previous - coeff * prev * previous

    for start in range(0, len(decimated) - size + 1, size // 2):
        chunk = decimated[start:start + size]
        if max(map(abs, chunk)) < 500:
            continue
        weighted = [sample * weight for sample, weight in zip(chunk, window)]
        energy = sum(sample * value for sample, value in zip(chunk, weighted)) * size / 2
        target = power(weighted, hz)
        if 2 * target >= 0.45 * energy and target > 1.6 * max(power(weighted, hz - 7), power(weighted, hz + 7)):
            return True
    return False


def silence(pcm, label):
    # Let a bounded amount of already queued hardware-buffer-equivalent audio drain.
    baseline = pcm.stat().st_size
    wait(label + " drain", lambda: pcm.stat().st_size - baseline >= 72000, 3)
    check(max(map(abs, pcm_tail(pcm)), default=0) <= 8, label + ": monitor was not silent")


def admission(admitted, requests, label):
    try:
        peer = admitted.get(timeout=12)
    except Empty as error:
        raise Failure(f"no authenticated account hello ({label}); loopback routes/status: {list(requests)}") from error
    if isinstance(peer, Exception):
        raise Failure(f"synthetic account admission ({label}): {peer}") from peer
    return peer


def check_mini_player(win, peer, cursor, pcm):
    def control(label, description=None, role="Button"):
        def find():
            dock = element_type(win.elements(), "PlayerBar")
            return dock and next((e for e in win.subtree(dock) if win.matches(e, label) and
                                  e.get("accessibleRole") == role and
                                  (description is None or e.get("accessibleDescription") == description)), None)
        return wait("dock " + label, find, 3)

    win.click_element(control("Shuffle", "Off"))
    peer.mode("shuffle", True, cursor)
    win.click_element(control("Shuffle", "On"))
    peer.mode("shuffle", False, cursor)
    for before, value in (("Off", "all"), ("Repeat queue", "one"), ("Repeat track", "off")):
        win.click_element(control("Repeat", before))
        peer.mode("repeat", value, cursor)
    control("Repeat", "Off")
    control("Playback devices", peer.hello["name"])

    def peak():
        return max(map(abs, pcm_tail(pcm)), default=0)

    full = wait("unscaled PCM on private monitor", lambda: peak() if peak() > 2500 else 0, 2)
    slider = control("Desktop volume", role="Slider")
    x, y, right, bottom = bounds(slider)
    win.mcp.call("drag_element", {"elementHandle": slider["handle"],
        "target": {"x": x + (right - x) / 4, "y": (y + bottom) / 2}})
    wait("desktop volume reduces local PCM", lambda: full * 0.13 < peak() < full * 0.42, 2)
    win.click_element(control("Mute desktop playback"))
    silence(pcm, "volume mute during playback")
    win.click_element(control("Unmute desktop playback"))
    wait("unmute restores the reduced gain", lambda: full * 0.13 < peak() < full * 0.42, 2)
    win.click_element(control("Desktop volume", role="Slider"))
    win.key("\uf72b")  # Slint Key.End
    wait("keyboard volume End restores full gain", lambda: peak() >= full * 0.9, 2)
    peer.fact(cursor, lambda m: m["playing"], "same playback run after local volume changes")
    print("PASS repeat modes and shuffle through the selected account; app-only PCM attenuation, mute, and keyboard restore", flush=True)

    win.click_element(control("Playback devices"))
    win.find("Close playback devices", roles=("Button",))
    win.absent("Devices", roles=("Text",))
    win.key(ESCAPE)
    win.absent("Close playback devices", roles=("Button",))
    win.click_element(control("Lyrics"))
    win.find(r"Fixture verse[\s\S]*", roles=("Text",))
    win.click("Back")
    control("Lyrics")
    print("PASS dock opens the anchored device selector and server-backed Lyrics in Now Playing", flush=True)


def run(win, ready, admitted, media, requests, pcm):
    win.fill("Server( origin| URL| address)?", ready["origin"])
    win.fill("Username", ready["username"])
    win.fill("Password", ready["password"])
    win.click("Sign in")
    peer = admission(admitted, requests, "sign-in")
    peer.open()
    peer.expect("presence to the selected connection", lambda m: m["type"] == "presence")
    win.section("History")
    print("PASS Slint sign-in; account v1 upgrade, one-use ticket, selected output and presence/grant", flush=True)
    win.fill("Search tracks, albums or artists", TITLE)
    win.find("Play " + TITLE)

    def play():
        win.click("Play " + TITLE)
        return peer.play(peer.command("play"))

    def audible(m):
        return m["playing"] and not m["buffering"] and not m["ended"]

    silence(pcm, "before play")
    baseline = pcm.stat().st_size
    active = play()
    initial = peer.fact(active, audible, "bound playing fact")
    wait("480 Hz fixture tone at the private ALSA sink monitor", lambda: heard_tone(pcm, start=baseline), 3)
    advanced = peer.fact(active, lambda m: audible(m) and m["elapsed"] >= initial["elapsed"] + 0.5,
                         "advancing native account fact")
    check(media and media[0] == 44 + 2 * 48000 * SECONDS, "missing complete WAV delivery")
    print(f"PASS canonical Play; native elapsed {initial['elapsed']:.2f} → {advanced['elapsed']:.2f}s; 480 Hz PCM",
          flush=True)

    # Go's Pause increments Run and marks the cursor stopped. No native
    # decoder remains to report on that paused run; verify sink silence and
    # that no advancing fact follows, rather than inventing a receipt.
    win.click("Pause")
    paused = peer.control(peer.command("pause", before=lambda msg: peer.observe(active, msg)))
    elapsed = paused["elapsed"]
    check(advanced["elapsed"] - 0.2 <= elapsed < SECONDS - 0.5 and not paused["desired"] and
          not paused["playing"] and paused["run"] == active["run"] + 1, "incorrect pause cursor")
    win.find("Play|Resume", roles=("Button",))
    silence(pcm, "after pause")
    peer.frame(9, b"paused-connection")
    peer.no_replay(0.6, pong=b"paused-connection", label="paused run")

    baseline = pcm.stat().st_size
    win.click("Play|Resume")
    active = peer.control(peer.command("resume"))
    resumed = peer.fact(active, audible, "resumed native playing fact")
    check(elapsed - 0.15 <= resumed["elapsed"] < elapsed + 2.5, "resume did not restore the position")
    marker = 480 if elapsed < 2 else 360 if elapsed < 6 else 600
    wait("PCM from resumed position", lambda: heard_tone(pcm, hz=marker, start=baseline), 3)
    print("PASS canonical Pause, silent monitor, no advancing fact, and Resume from observed position", flush=True)

    check_mini_player(win, peer, active, pcm)
    failures = []
    for label, mode, forward in (("wide", WIDE, 0.60), ("compact", COMPACT, 0.65)):
        if mode == COMPACT:  # Cage already starts in WIDE.
            win.resize(mode)
        assert_dock_layout(win, "Open album for " + TITLE)
        win.pixels("player-controls-" + label, mode[0])
        for direction, fraction, marker in (("forward", forward, 600), ("backward", 0.25, 360)):
            peer.ping(f"before-{label}-{direction}-seek")
            with verify(failures, f"{label} dock {direction} pointer seek"):
                baseline = pcm.stat().st_size
                win.drag_slider(fraction)
                cmd = peer.command("seek")
                requested = cmd["value"]
                started = monotonic()
                active = peer.control(cmd)
                report = peer.fact(active, audible, "playing fact for new seek run")
                check(abs(requested - fraction * SECONDS) < 0.6, f"unexpected pointer seek target: {requested:.2f}s")
                check(requested - 0.15 <= report["elapsed"] <= requested + monotonic() - started + 0.25,
                      f"seek sent {requested:.2f}s but executor reported {report['elapsed']:.2f}s")
                wait(f"{marker} Hz PCM from the {direction} seek target", lambda: heard_tone(pcm, hz=marker, start=baseline), 3)
                print(f"PASS {label} {direction} seek to {requested:.2f}s: native fact {report['elapsed']:.2f}s and {marker} Hz PCM",
                      flush=True)
    win.resize(WIDE)
    streams = len(media)
    ended = peer.fact(active, lambda m: m["ended"], "bound native EOF fact", timeout=SECONDS + 2)
    check(not ended["playing"] and not ended["buffering"] and ended["landed"] == 0 and
          abs(ended["elapsed"] - SECONDS) < 0.05, "incorrect repeat-off EOF fact")
    silence(pcm, "after natural end")
    check(len(media) == streams, "terminal EOF requested another stream without a Play command")
    baseline = pcm.stat().st_size
    win.click("Play|Resume")
    cmd = peer.command("resume")
    started = monotonic()
    replay = peer.control(cmd)
    restarted = peer.fact(replay, audible, "playing fact after natural EOF")
    check(0 <= restarted["elapsed"] < 2 and restarted["elapsed"] <= monotonic() - started + 0.25,
          "replay after natural EOF did not start near zero")
    wait("480 Hz opening PCM after natural EOF", lambda: heard_tone(pcm, start=baseline), 3)
    print("PASS natural EOF (landed=0), silent monitor, and dock Resume from zero with 480 Hz PCM", flush=True)

    # Stop the replay and drain PCM so it cannot satisfy the short-grant check.
    win.click("Pause")
    peer.control(peer.command("pause", before=lambda msg: peer.observe(replay, msg)))
    silence(pcm, "after pausing the EOF replay")

    # Withhold renewals without sending a pause, a new cursor, or an owner change.
    peer.grant_ms = 3500
    peer.expect("fresh short grant basis", lambda m: m["type"] == "presence")
    deadline = peer.last_grant + peer.grant_ms / 1000
    peer.grant_ms = None
    baseline = pcm.stat().st_size
    started = monotonic()
    short = play()
    peer.fact(short, audible, "playing under the short grant", timeout=3)
    wait("short-grant 480 Hz tone", lambda: heard_tone(pcm, start=baseline), 2)
    check(monotonic() < deadline, "did not verify audible playback within the short grant")
    sleep(max(0, deadline - monotonic()))
    silence(pcm, "after unrenewed grant expires")
    check(monotonic() - started < SECONDS - 0.5, "expiry silence was not verified before natural end")
    peer.ping("same-connection-after-expiry")
    print("PASS local grant expiry stopped tone before EOF without dropping the account socket", flush=True)

    peer.drop_output()
    peer.expect("release after loss of ownership", lambda m: m["type"] == "release")
    # After release, a new user Play claims an unowned output, as on Go.
    peer.grant_ms = 4500
    baseline = pcm.stat().st_size
    started = monotonic()
    win.click("Play " + TITLE)
    cmd = peer.command("play")
    peer.claim_output()
    active = peer.play(cmd)
    fact = peer.fact(active, audible, "playing before disconnect", timeout=4)
    wait("480 Hz tone before disconnect", lambda: heard_tone(pcm, start=baseline), 2)
    deadline = peer.last_grant + peer.grant_ms / 1000
    check(monotonic() < deadline - 0.4, "transport was not cut during a live grant")
    peer.disconnect()
    # Hold the replacement snapshot until we have PCM that could only be
    # rendered under the already-accepted grant on the disconnected run. Fence
    # after the new hello so prior monitor/backend buffers already had time to drain.
    rejoined = admission(admitted, requests, "reconnect")
    check(peer.hello["deviceId"] == rejoined.hello["deviceId"] and peer.id != rejoined.id,
          "reconnect did not retain logical device identity with a new connection")
    baseline = pcm.stat().st_size
    check(0 <= fact["elapsed"] < 5, "disconnect did not exercise the early/middle PCM markers")
    wait("360 Hz PCM under the detached run's existing grant",
         lambda: heard_tone(pcm, hz=360, start=baseline), min(2, max(0, deadline - monotonic())))
    check(monotonic() < deadline, "detached tone was not observed within the old grant")
    # Drop grace by publishing still-desired state with no owner and no new
    # grant. No new connection may replay choices, facts, or ungranted audio.
    rejoined.open(selected=False, prior=peer)
    rejoined.no_replay(4)
    silence(pcm, "after account reconnect without ownership")
    check(monotonic() - started < SECONDS - 0.5, "disconnect silence was not verified before natural EOF")
    rejoined.frame(9, b"account-no-replay")
    rejoined.no_replay(2, pong=b"account-no-replay")
    print("PASS disconnect/rejoin did not replay account choices, stale facts, or ungranted audio", flush=True)
    check(not failures, "; ".join(failures))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--desktop", type=Path, default=Path.home() / "repos/spofify-desktop/target/debug/spofify-desktop")
    parser.add_argument("--scratch-dir", type=Path, default=Path("/dev/shm"),
                        help="existing short path for disposable profile and private sockets (default: /dev/shm)")
    args = parser.parse_args()
    try:
        binary = args.desktop.resolve(strict=True)
        print("SYNTHETIC ACCOUNT-STATE NATIVE AUDIO ONLY — not real Go integration", flush=True)
        # Short, absolute path so the private Unix sockets fit sun_path.
        with TemporaryDirectory(prefix="aud-smoke-", dir=args.scratch_dir.resolve(strict=True)) as temp:
            root = Path(temp)
            env = private_env(root)
            with (audio_fixture() as (ready, admitted, media, requests),
                  output(root, env) as pcm, desktop(binary, root, env, multisize=True) as win):
                run(win, ready, admitted, media, requests, pcm)
    except (Failure, OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"FAIL synthetic native audio smoke: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
