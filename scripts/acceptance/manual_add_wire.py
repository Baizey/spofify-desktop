"""Manual-add extension of the private account replica in wire_ui (no Go)."""
from contextlib import contextmanager
import http.client
import http.server
import json
from threading import Event, Lock
from time import monotonic
from types import SimpleNamespace
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

from fixture import check
import wire_ui as base

QUERY = "Night signals"
HIDDEN_QUERY = "Signal studies"
LONG = "Night Train Studies — Field Recordings and After-Hours Arrangements for the Long Way Home"
SHORT, EMPTY, READ_ONLY, RULES = "Road mix", "Sunday at the Harbour", "Read-only manual", "Rules only"
TARGET_ID = "wire-night-train-studies"
DESCRIPTION = "Studio demos"
EXPIRY = "2099-01-01T00:00:00Z"


def track(key, name, *, available=True):
    return dict(base.TRACK, id=f"wire-manual-{key}", name=name, hasAudio=available,
                coverage=dict(tracks=1, inLibrary=int(available), albums=0))


NORMAL = (
    track("opening", "When the Streetlights Come On Across the Other Side of the River"),
    dict(base.ALBUM, name="Night Signals: Fourteen Pieces for Passing Trains"),
    track("unavailable", "A Beacon with No Keeper", available=False),
    dict(base.ARTIST, name="Night Signal Ensemble"),
    track("member", "The Night We Walked All the Way Back"),
    track("ferry", "Between the Last Ferry and the First Morning Train"),
    track("blue", "Blue Hour at Low Tide"),
) + tuple(track(i, f"Night Signal at Mile {i:02}") for i in range(8, 34))
HIDDEN = tuple(track(f"study-{i}", f"Signal Study {i:02}") for i in range(1, 27))


def playlist(ident, name, tracks, *, kind="manual", edit=True, description=""):
    summary, _, definition = base.playlist_case(ident.removeprefix("wire-"), name, len(tracks), kind=kind,
                                                source=TARGET_ID)
    summary["description"] = description
    if not edit:
        for key in ("editMetadata", "editEntries", "editDefinition", "addTracks", "delete"):
            summary["capabilities"][key] = False
    rows = [entry(ident, i, item, kind=kind) for i, item in enumerate(tracks)]
    return summary, rows, definition


def entry(ident, index, item, *, kind="manual"):
    return dict(id=f"{'o1_' if kind == 'derived' else 'entry-'}{ident}-{index}", position=index,
                addedAt=base.DATE, track=item)


class Wire:
    def __init__(self, output, port):
        self.output, self.port = output, port
        self.lock = Lock()
        self.events = []
        self.expected_add = None
        self.confirm = Event()
        self.continue_hidden = Event()
        self.playlists = dict((case[0]["id"], case) for case in (
            playlist("wire-road-mix", SHORT, (NORMAL[5],)),
            playlist(TARGET_ID, LONG, (NORMAL[4], *HIDDEN[:-1]), description=DESCRIPTION),
            playlist("wire-sunday", EMPTY, ()),
            playlist("wire-read-only", READ_ONLY, (NORMAL[0],), edit=False),
            playlist("wire-rules", RULES, (NORMAL[0],), kind="derived", edit=False),
        ))
        (output / "wire.jsonl").touch(exist_ok=False)

    def record(self, event, **data):
        item = dict(event=event, time=monotonic(), **data)
        with self.lock:
            self.events.append(item)
            with (self.output / "wire.jsonl").open("a") as log:
                print(json.dumps(item, sort_keys=True), file=log)

    def seen(self, event, **match):
        with self.lock:
            return [item for item in self.events if item["event"] == event
                    and all(item.get(key) == value for key, value in match.items())]

    def expect_add(self, track):
        with self.lock:
            writes = sum(event["event"] == "add" for event in self.events)
            echoes = sum(event["event"] == "add-confirmed" for event in self.events)
            check(writes == echoes, "cannot arm a new add before the prior write has settled")
            self.expected_add = (writes + 1, track)
            self.confirm.clear()
            return writes + 1

    def exact_add(self, ordinal, track):
        entries = self.seen("add")
        if len(entries) < ordinal:
            return None
        expected = dict(type="addEntry", playlistId=TARGET_ID, trackId=track["id"])
        check(len(entries) == ordinal and entries[-1]["command"] == expected,
              f"write {ordinal} did not use the exact visible track and destination: {entries}")
        return entries[-1]

    def preflight(self):
        # Prove the same worker-local destination can be reached without any
        # session secret; the existing fixture returns 401 before sign-in.
        client = http.client.HTTPConnection("127.0.0.1", self.port, timeout=6)
        try:
            client.request("GET", "/api/auth/session")
            status = client.getresponse().status
            check(status == 401, f"unexpected synthetic fixture preflight status: {status}")
            self.record("preflight", status=status)
        finally:
            client.close()

    @contextmanager
    def serve(self):
        wire = self

        class Account(base.AccountWire):
            def initial(self):
                super().initial()
                wire.record("snapshot")

            def playlist(self, command):
                pid = command.get("playlistId")
                check(pid in wire.playlists, "manual-add fixture asked for an unknown playlist")
                summary, rows, definition = wire.playlists[pid]
                token = f"{pid}-rows-v{summary['revision']}"
                offset, known = command.get("offset", 0), command.get("known")
                check(type(offset) is int and (offset == 0 or 0 < offset < len(rows)
                      and offset % base.PLAYLIST_PART_ROWS == 0 and known == token),
                      "invalid manual-add fixture playlist offset or token")
                has_part = offset > 0 or known != token
                if has_part:
                    end = offset + base.PLAYLIST_PART_ROWS
                    part = dict(summary=summary, token=token, offset=offset, rows=rows[offset:end], more=end < len(rows))
                    if definition is not None:
                        part["definition"] = definition
                    self.personal(part=part)
                wire.record("membership-sync", playlist=pid, offset=offset, part=has_part,
                            revision=summary["revision"])

            def message(self):
                while True:
                    notice = super().message()
                    command = notice.get("command", {})
                    if notice.get("type") != "personal" or command.get("type") != "addEntry":
                        return notice
                    wire.record("add", command=command)
                    expected = wire.expected_add
                    check(expected is not None and set(notice) == {"type", "command"}
                          and wire.exact_add(*expected), "unexpected native addEntry")
                    # This deliberately holds real account echo until the UI has
                    # actually painted pending and the test attempts a second click.
                    check(wire.confirm.wait(6), "native pending check did not release the fixture write")
                    summary, rows, _ = wire.playlists[TARGET_ID]
                    added = entry(TARGET_ID, len(rows), expected[1])
                    rows.append(added)
                    summary.update(trackCount=len(rows), availableCount=len(rows), revision=summary["revision"] + 1)
                    self.personal(change=dict(playlist=summary, entries=dict(added=added)))
                    wire.record("add-confirmed", ordinal=expected[0], track=command["trackId"],
                                occurrence=added["id"], revision=summary["revision"])

        class Server(http.server.ThreadingHTTPServer):
            daemon_threads = False

            def __init__(self, address, base_handler):
                class Handler(base_handler):
                    def setup(self):
                        super().setup()
                        self.connection.settimeout(25)

                    def reply(self, value=None, status=200, headers=None):
                        if self.path in ("/api/auth/login", base.TICKET_PATH):
                            wire.record("auth-response", endpoint=self.path, status=status)
                        return super().reply(value, status, headers)

                    def do_GET(self):
                        route = urlsplit(self.path)
                        if route.path.endswith("/audio"):
                            wire.record("unexpected-audio", path=route.path)
                            return self.error(404)
                        if route.path != "/search":
                            return super().do_GET()
                        if not self.trusted() or not self.owned():
                            return
                        args = parse_qs(route.query, keep_blank_values=True)
                        query = args.get("query", [""])[0]
                        cursor = args.get("cursor", [""])[0]
                        if not cursor:
                            # This also accepts typed focus probes whose intentional
                            # suffix doesn't match any fixture rows.
                            if set(args) != {"query", "limit"} or args["limit"] != ["20"] or not query:
                                wire.record("unexpected-search-shape", keys=sorted(args))
                                return self.error(400)
                            rows = {QUERY: NORMAL, HIDDEN_QUERY: HIDDEN}.get(query, ())
                            wire.record("search", query=query, page=1)
                            value = dict(items=rows[:20], resultCount=len(rows), expiresAt=EXPIRY)
                            if len(rows) > 20:
                                value["nextCursor"] = "hidden-second" if rows is HIDDEN else "normal-second"
                            return self.reply(value)
                        if args != {"cursor": [cursor]} or cursor not in ("hidden-second", "normal-second"):
                            wire.record("unexpected-search-shape", keys=sorted(args))
                            return self.error(400)
                        rows = HIDDEN if cursor == "hidden-second" else NORMAL
                        wire.record("search", query=HIDDEN_QUERY if rows is HIDDEN else QUERY, page=2)
                        if rows is HIDDEN and not wire.continue_hidden.wait(6):
                            wire.record("unexpected-continuation-timeout")
                            return self.error(503)
                        return self.reply(dict(items=rows[20:], resultCount=len(rows), expiresAt=EXPIRY))

                check(address == ("127.0.0.1", 0), "fixture is not worker-local")
                super().__init__((address[0], wire.port), Handler)

        facade = SimpleNamespace(server=SimpleNamespace(BaseHTTPRequestHandler=http.server.BaseHTTPRequestHandler,
                                                         ThreadingHTTPServer=Server))
        with (patch.object(base, "http", facade), patch.object(base, "AccountWire", Account),
              patch.object(base, "PLAYLISTS", self.playlists), base.wire_fixture() as ready):
            try:
                yield ready
            finally:
                self.confirm.set()
                self.continue_hidden.set()
        check(not any(e["event"].startswith("unexpected-") for e in self.events),
              "unexpected wire activity; see wire.jsonl")
