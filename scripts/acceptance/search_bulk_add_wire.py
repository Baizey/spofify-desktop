"""Independent synthetic account peer for native Search collection adds (no Go).

The HTTP manifest expires on each account mutation. A dedicated, non-playing
writer must finish the manifest before writing, and a player snapshot is not
its FIFO personal checkpoint. All fixture IDs, writes and reads are synthetic.
"""
from contextlib import contextmanager
from dataclasses import dataclass, field
from hashlib import sha256
import http.client
import http.server
import json
from socket import SHUT_RDWR
from threading import Event, RLock
from time import monotonic
from types import SimpleNamespace
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

from fixture import check
from manual_add_wire import EMPTY, EXPIRY, LONG, READ_ONLY, RULES, TARGET_ID, entry, playlist
import wire_ui as base

QUERY = "Counterpoint"
LIMIT = 20
WINDOW = 8
TARGET_LENGTH = 1161

ARTIST = base.catalog_item("artist", "bulk-artist", "Counterpoint Ensemble")
ALBUM = base.catalog_item("album", "bulk-album", "Counterpoint: The Collected Recordings for Night Trains",
                          artists=[{k: ARTIST[k] for k in ("id", "name")}], albumType="album",
                          releaseDate="2023-08-16")


def track(key, name, index, *, anthology=True):
    return base.catalog_item("track", f"bulk-{key}", name, artists=ALBUM["artists"],
                             albumId=ALBUM["id"] if anthology else "bulk-other-release",
                             albumName=ALBUM["name"] if anthology else "Counterpoint: Other Sessions",
                             durationMs=84000, discNumber=1, trackNumber=index)


ALBUM_TRACKS = tuple(track(f"album-{index:03}", f"Counterpoint Sequence {index:03}", index)
                     for index in range(1, 174))
ARTIST_ONLY = tuple(track(f"artist-{index:03}", "Counterpoint – The Same Title" if index < 3
                         else f"Counterpoint Study {index:03}", index, anthology=False)
                    for index in range(1, 252))
ARTIST_TRACKS = ALBUM_TRACKS + ARTIST_ONLY
DIRECT = ARTIST_ONLY[0]
FOREIGN = ARTIST_ONLY[25]
SEED = ALBUM_TRACKS[-1]
FILLER = track("target-filler", "Counterpoint Destination Seed", 1, anthology=False)
ALBUM["coverage"] = dict(tracks=len(ALBUM_TRACKS), inLibrary=len(ALBUM_TRACKS), albums=0)
ARTIST["coverage"] = dict(tracks=len(ARTIST_TRACKS), inLibrary=len(ARTIST_TRACKS), albums=2)
RESULTS = (ALBUM, ARTIST, DIRECT, SEED, FOREIGN, *ALBUM_TRACKS[:8])
SOURCES = {entity["id"]: (entity, source) for entity, source in ((ALBUM, ALBUM_TRACKS), (ARTIST, ARTIST_TRACKS))}
TRACKS = {row["id"]: row for row in (*ARTIST_TRACKS, FILLER)}


def digest(value):
    return sha256(value.encode()).hexdigest()


def add_frame(track, target=TARGET_ID):
    return dict(type="personal", command=dict(type="addEntry", playlistId=target, trackId=track["id"]))


@dataclass
class Action:
    serial: int
    entity: dict
    source: tuple
    expected: tuple
    target: str
    gate_terminal: bool
    hold_checkpoints: frozenset
    hold_primary: frozenset
    terminal: Event = field(default_factory=Event)
    fence_revision: int | None = None
    offset: int = 0
    settled_manifest: bool = False
    position: int = 0
    outstanding: int = 0
    checkpoints: int = 0
    private_initials: int = 0
    waiting: object = None
    deferred_primary: dict | None = None
    peer: object = None
    closed: bool = False


class BulkWire:
    def __init__(self, output, port):
        self.port, self.log = port, output / "wire.jsonl"
        self.log.touch(exist_ok=False)
        self.lock = RLock()
        self.events = []
        self.serial = 0
        self.action = None
        self.main = self.writer = None
        self.generation = 0
        self.direct = []
        self.playlists = {p[0]["id"]: p for p in (
            playlist(TARGET_ID, LONG, (*([FILLER] * (TARGET_LENGTH - 1)), SEED)),
            playlist("wire-bulk-other", EMPTY, ()),
            playlist("wire-bulk-read-only", READ_ONLY, (FILLER,), edit=False),
            playlist("wire-bulk-rules", RULES, (FILLER,), kind="derived", edit=False),
        )}

    def record(self, event, **data):
        with self.lock:
            row = dict(event=event, time=monotonic(), **data)
            self.events.append(row)
            with self.log.open("a") as log:
                print(json.dumps(row, sort_keys=True), file=log)

    def seen(self, event, **match):
        with self.lock:
            return [e for e in self.events if e["event"] == event
                    and all(e.get(k) == v for k, v in match.items())]

    def reject(self, reason, **data):
        self.record("violation", reason=reason, **data)
        check(False, reason)

    def preflight(self):
        # Deliberately send no session secrets; a 401 proves this local route.
        client = http.client.HTTPConnection("127.0.0.1", self.port, timeout=8)
        try:
            client.request("GET", "/api/auth/session")
            check(client.getresponse().status == 401, "bulk fixture preflight was not isolated")
        finally:
            client.close()

    def arm(self, entity, *, skip, exclude=(), target=TARGET_ID, gate_terminal=False, hold=(),
            hold_primary=(), outcome="complete"):
        """Compute the independent ordered write oracle before exercising the UI.

        exclude explicitly names fixture insertions that will become visible to
        the main socket before a later skip-on window is allowed to start.
        A repeated track ID in a canonical collection manifest is malformed;
        distinct occurrence IDs do not make a second append valid.
        """
        with self.lock:
            check(self.action is None and self.main and outcome in ("complete", "duplicate", "cancel", "ambiguous"),
                  "invalid synthetic action setup")
            entity, source = SOURCES[entity["id"]]
            known = {e["track"]["id"] for e in self.playlists[target][1]} | {e["id"] for e in exclude}
            expected = tuple(t for t in source if not skip or t["id"] not in known)
            if outcome == "duplicate":
                # Unique occurrence identity, repeated stable recording ID on a later page.
                source = (*source[:LIMIT + 2], source[0], *source[LIMIT + 2:])
            if outcome in ("duplicate", "cancel"):
                expected = ()
            self.serial += 1
            action = self.action = Action(self.serial, entity, source, expected, target,
                                          gate_terminal, frozenset(hold), frozenset(hold_primary))
            self.record("arm", serial=action.serial, source=entity["id"], target=target, skip=skip,
                        count=len(source), expected=[e["id"] for e in expected], outcome=outcome)
            return action

    def finish(self, action, *, complete=True):
        with self.lock:
            check(action is self.action and not self.direct and not self.seen("violation"), "unfinished or invalid wire case")
            if complete:
                check(action.position == len(action.expected) and action.outstanding == 0
                      and action.checkpoints == action.private_initials and action.waiting is None
                      and action.deferred_primary is None and (not action.expected or action.settled_manifest),
                      f"incomplete bulk trace: {action.position}/{len(action.expected)} exact writes")
            action.terminal.set()
            self.record("finish", serial=action.serial, writes=action.position,
                        checkpoints=action.checkpoints, private_initials=action.private_initials)
            self.action = None

    def expect_direct(self, row, target=TARGET_ID):
        with self.lock:
            self.direct.append(add_frame(row, target))

    def manifest(self, handler, path, args):
        with self.lock:
            action = self.action
            if not action:
                return self.reject("unarmed collection manifest", path=path)
            entity, offset = action.entity, action.offset
            expected_path = f"/api/me/offline/v1/{entity['kind']}s/{entity['id']}/manifest"
            expected_args = {"limit": [str(LIMIT)]}
            if offset:
                expected_args["cursor"] = [f"ofc1_{entity['id']}-{offset}.synthetic"]
            if path != expected_path or args != expected_args or action.settled_manifest:
                return self.reject("wrong collection manifest route or continuation", path=path, offset=offset)
            if offset == 0:
                action.fence_revision = self.generation
            if action.fence_revision != self.generation:
                self.record("stale-manifest", serial=action.serial, offset=offset)
                return handler.reply({"code": "STALE_EVALUATION"}, status=409)
            end = min(offset + LIMIT, len(action.source))
            terminal = end == len(action.source)
            body = dict(version=1, collection={k: entity[k] for k in ("id", "name", "kind")},
                        fence="f1_" + digest(f"{entity['id']}:{self.generation}"), total=len(action.source),
                        recordedAudioTotal=len(action.source), limit=LIMIT,
                        nextCursor=None if terminal else f"ofc1_{entity['id']}-{end}.synthetic",
                        items=[dict(occurrenceId="oc1_" + digest(f"{entity['id']}:{i}:{row['id']}"),
                                    position=i, track=row) for i, row in enumerate(action.source[offset:end], offset)])
            action.offset = end
            self.record("manifest-read", serial=action.serial, offset=offset, count=end - offset,
                        generation=self.generation, terminal=terminal)
        if terminal and action.gate_terminal and not action.terminal.wait(15):
            return self.reject("terminal manifest gate timed out", serial=action.serial)
        with self.lock:
            check(action.fence_revision == self.generation or action is not self.action,
                  "account mutated before the terminal manifest was served")
            action.settled_manifest = terminal
            self.record("manifest-reply", serial=action.serial, offset=offset, terminal=terminal)
            handler.reply(body)

    def append(self, row, target, origin, *, defer_primary=False):
        summary, rows, _ = self.playlists[target]
        added = entry(target, len(rows), row)
        rows.append(added)
        summary.update(trackCount=len(rows), availableCount=len(rows), revision=summary["revision"] + 1)
        self.generation += 1
        change = dict(playlist=dict(summary), entries=dict(added=added))
        if self.writer is not None:
            self.writer.personal(change=change)
        if defer_primary:
            self.action.deferred_primary = change
            self.record("primary-change-held", serial=self.action.serial, window=self.action.checkpoints + 1,
                        revision=summary["revision"])
        elif self.main is not None:
            self.main.personal(change=change)
        self.record("change", origin=origin, track=row["id"], occurrence=added["id"],
                    playlist=target, revision=summary["revision"], generation=self.generation)

    def release_primary(self, action):
        with self.lock:
            check(self.main is not None and action is self.action and action.deferred_primary is not None,
                  "no held primary revision")
            self.main.personal(change=action.deferred_primary)
            self.record("primary-change-released", serial=action.serial,
                        revision=action.deferred_primary["playlist"]["revision"])
            action.deferred_primary = None

    def foreign(self, row, target=TARGET_ID):
        with self.lock:
            check(row["id"] in TRACKS and target in self.playlists, "invalid foreign fixture occurrence")
            self.append(row, target, "foreign")

    def mutate(self, peer, notice):
        with self.lock:
            if peer is self.main:
                if not self.direct or notice != self.direct[0]:
                    return self.reject("unexpected mutation on the main socket")
                command = self.direct.pop(0)["command"]
                self.record("direct-add", command=command)
                return self.append(TRACKS[command["trackId"]], command["playlistId"], "direct")
            action = self.action
            if (peer is not self.writer or not action or not action.settled_manifest or action.closed
                    or action.waiting or action.deferred_primary or action.outstanding >= WINDOW
                    or action.position >= len(action.expected)):
                return self.reject("unexpected writer mutation or write crossed a manifest/checkpoint boundary")
            track = action.expected[action.position]
            if notice != add_frame(track, action.target):
                return self.reject("bulk write did not match exact planned ID/target/order", serial=action.serial,
                                   expected=track["id"], position=action.position)
            action.peer = peer
            action.position += 1
            action.outstanding += 1
            self.record("add", serial=action.serial, window=action.checkpoints + 1, ordinal=action.position,
                        command=notice["command"])
            self.append(track, action.target, "writer", defer_primary=(action.outstanding == WINDOW
                        and action.checkpoints + 1 in action.hold_primary))

    def checkpoint(self, peer):
        with self.lock:
            action = self.action
            if not action or action.peer is not peer or action.waiting or not 1 <= action.outstanding <= WINDOW:
                return self.reject("writer sync was not a boundary after a started window")
            action.checkpoints += 1
            action.waiting = peer
            self.record("sync", serial=action.serial, window=action.checkpoints, writes=action.outstanding)
            # Go publishes this immediate player snapshot first; it is not an
            # acknowledgement of the queued personal writes.
            peer.send(peer.snapshot)
            self.record("player-snapshot-only", serial=action.serial, window=action.checkpoints)
            if action.checkpoints not in action.hold_checkpoints:
                self.release(action)

    def release(self, action):
        with self.lock:
            check(action is self.action and action.waiting is self.writer and self.writer is not None,
                  "no private writer checkpoint to release")
            action.waiting.personal(snapshot=self.personal_snapshot())
            action.private_initials += 1
            action.outstanding = 0
            action.waiting = None
            self.record("private-initial", serial=action.serial, window=action.checkpoints)

    def disconnect_writer(self, action):
        with self.lock:
            check(action is self.action and action.waiting is self.writer and self.writer is not None,
                  "cannot simulate loss without a started and uncheckpointed window")
            action.closed = True
            self.record("writer-disconnect-before-private-initial", serial=action.serial, window=action.checkpoints)
            self.writer.handler.connection.shutdown(SHUT_RDWR)

    def disconnect_primary(self, action):
        with self.lock:
            check(action is self.action and action.waiting is self.writer and self.main is not None,
                  "cannot simulate primary loss without a connected primary and a started writer")
            self.record("primary-disconnect", serial=action.serial, window=action.checkpoints)
            self.main.handler.connection.shutdown(SHUT_RDWR)

    def roster(self, *, except_peer=None):
        # Simulate the full connection roster from Go in append order, even
        # when both entries share a device identity. Desktop owns presentation
        # deduplication; the fixture must not pre-hide the auxiliary peer.
        peers = [p for p in (self.main, self.writer) if p is not None]
        devices = [p.device for p in peers]
        owner = self.main.connection if self.main else ""
        for p in peers:
            p.snapshot.update(owner=owner, devices=devices)
            if p is not except_peer:
                p.send(dict(type="outputs", owner=owner, devices=devices))
        self.record("roster", can_play=[p.playing for p in peers], owner="main" if owner else None)

    def personal_snapshot(self):
        return dict(playlists=[p[0] for p in self.playlists.values()], recent=[])

    @contextmanager
    def serve(self):
        wire = self

        class Account(base.AccountWire):
            playing = None

            def frame(self, opcode, payload):
                with wire.lock:
                    super().frame(opcode, payload)

            def initial(self):
                with wire.lock:
                    if self.playing is None:
                        self.device = self.snapshot["devices"][0]
                        self.playing = self.device["canPlay"]
                        role = "main" if self.playing else "writer"
                        check(getattr(wire, role) is None, f"second {role} socket connected concurrently")
                        check(self.playing or wire.main is not None, "private writer attached before the main account")
                        if not self.playing and any(wire.main.device[k] != self.device[k] for k in ("deviceId", "name")):
                            wire.reject("non-playing writer did not reuse the main device identity")
                        setattr(wire, role, self)
                        wire.record("socket-open", role=role, can_play=self.playing)
                        wire.roster(except_peer=self)
                    self.send(self.snapshot)
                    self.personal(snapshot=wire.personal_snapshot())

            def serve(self):
                try:
                    super().serve()
                finally:
                    with wire.lock:
                        for role in ("writer", "main"):
                            if getattr(wire, role) is self:
                                setattr(wire, role, None)
                                wire.record("socket-close", role=role)
                                wire.roster()

            def message(self):
                while True:
                    notice = super().message()
                    if self.playing is None:
                        return notice  # ordinary ticket-authenticated Hello
                    command = notice.get("command", {}).get("type")
                    if notice.get("type") == "personal" and command == "addEntry":
                        wire.mutate(self, notice)
                    elif not self.playing and notice == {"type": "sync"}:
                        wire.checkpoint(self)
                    elif not self.playing and not (notice.get("type") == "personal" and command == "syncPlaylist"):
                        wire.reject("unexpected command on the non-playing writer", command=notice.get("type"))
                    else:
                        return notice

            def playlist(self, command):
                with wire.lock:
                    pid, offset = command.get("playlistId"), command.get("offset", 0)
                    check(pid in wire.playlists, "sync requested an unknown synthetic playlist")
                    summary, rows, definition = wire.playlists[pid]
                    action = wire.action
                    if (self.playing and action is not None and action.deferred_primary is not None
                            and pid == action.target):
                        # A token probe must not accidentally bypass the gate
                        # withholding the primary's final Change this window.
                        rows = rows[:-1]
                        summary = dict(summary, trackCount=len(rows), availableCount=len(rows),
                                       revision=summary["revision"] - 1)
                    token = f"{pid}-bulk-v{summary['revision']}"
                    known = command.get("known")
                    check(type(offset) is int and (offset == 0 or (0 < offset < len(rows)
                          and offset % base.PLAYLIST_PART_ROWS == 0 and known == token)), "invalid playlist continuation")
                    if offset or known != token:
                        end = offset + base.PLAYLIST_PART_ROWS
                        part = dict(summary=summary, token=token, offset=offset, rows=rows[offset:end], more=end < len(rows))
                        if definition is not None:
                            part["definition"] = definition
                        self.personal(part=part)
                    wire.record("target-read", playlist=pid, offset=offset, revision=summary["revision"],
                                role="main" if self.playing else "writer")

            def play(self, _message):
                wire.reject("Search add issued playback")

        class Server(http.server.ThreadingHTTPServer):
            daemon_threads = False

            def __init__(self, address, base_handler):
                class Handler(base_handler):
                    def setup(self):
                        super().setup()
                        self.connection.settimeout(70)

                    def do_GET(self):
                        url = urlsplit(self.path)
                        args = parse_qs(url.query, keep_blank_values=True)
                        if url.path.startswith("/api/me/offline/v1/"):
                            if self.trusted() and self.owned():
                                if url.path.endswith("/manifest"):
                                    return wire.manifest(self, url.path, args)
                                wire.reject("Search bulk downloaded media", path=url.path)
                            return
                        if url.path == "/search":
                            if not (self.trusted() and self.owned()):
                                return
                            if set(args) != {"query", "limit"} or args["limit"] != [str(LIMIT)]:
                                return wire.reject("wrong Search request shape")
                            rows = RESULTS if args["query"] == [QUERY] else ()
                            wire.record("search", rows=len(rows))
                            return self.reply(dict(items=rows, resultCount=len(rows), expiresAt=EXPIRY))
                        if url.path.endswith("/audio"):
                            return wire.reject("Search bulk requested playback audio", path=url.path)
                        return super().do_GET()

                check(address == ("127.0.0.1", 0), "bulk fixture must be worker-local")
                super().__init__((address[0], wire.port), Handler)

        facade = SimpleNamespace(server=SimpleNamespace(BaseHTTPRequestHandler=http.server.BaseHTTPRequestHandler,
                                                         ThreadingHTTPServer=Server))
        with (patch.object(base, "http", facade), patch.object(base, "AccountWire", Account),
              patch.object(base, "PLAYLISTS", self.playlists), base.wire_fixture() as ready):
            try:
                yield ready
            finally:
                if self.action:
                    self.action.terminal.set()
        check(not self.seen("violation"), "unexpected bulk wire activity; see wire.jsonl")
