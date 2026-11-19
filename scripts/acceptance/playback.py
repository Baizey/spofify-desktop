"""Bounded loopback observer and test client for Go's account-state socket."""
from base64 import b64encode
import hashlib
import json
import os
import socket
import struct
from time import monotonic
from uuid import uuid4

from fixture import Failure, check

PROTOCOL = "spofify.account-state.v1"
CONNECT = "/api/me/player/connect"
SOCKET = "/api/me/player/socket"
MAX_FRAME = (2 << 20) + (64 << 10)
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


class Peer:
    """Does not claim audio. `wait` predicates receive the merged account view."""

    def __init__(self, api, timeout=8, *, name="Acceptance observer"):
        ticket = api.json(CONNECT, method="POST", body={})["ticket"]
        self.deadline = monotonic() + timeout
        self.device_id = "acceptance-" + uuid4().hex
        self.snapshot = self.personal_snapshot = self.last_frame = None
        self.sock = socket.create_connection(("127.0.0.1", api.port), timeout=timeout)
        self.buffer = bytearray()
        try:
            key = b64encode(os.urandom(16)).decode()
            self.sock.sendall((f"GET {SOCKET} HTTP/1.1\r\nHost: 127.0.0.1:{api.port}\r\n"
                               f"Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
                               f"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: {PROTOCOL}\r\n"
                               f"Origin: {api.origin}\r\nCookie: {api.cookie}\r\n\r\n").encode("ascii"))
            while b"\r\n\r\n" not in self.buffer:
                check(len(self.buffer) <= 16 * 1024, "account upgrade exceeded bound")
                self.receive()
            split = self.buffer.index(b"\r\n\r\n")
            check(split <= 16 * 1024, "account upgrade exceeded bound")
            response = bytes(self.buffer[:split]).split(b"\r\n")
            del self.buffer[:split + 4]
            check(response[0].split()[1:2] == [b"101"], "production account socket did not upgrade")
            headers = dict((k.lower(), v.strip()) for k, v in (line.split(b":", 1) for line in response[1:]))
            accept = b64encode(hashlib.sha1((key + WS_GUID).encode()).digest())
            check(headers.get(b"sec-websocket-accept") == accept and
                  headers.get(b"sec-websocket-protocol") == PROTOCOL.encode(), "account upgrade mismatch")
            self.send(dict(type="hello", ticket=ticket, name=name, deviceId=self.device_id, canPlay=False))
            first = self.message()
            self.apply(first)
            check(first["type"] == "snapshot" and first.get("connectionId") and any(
                device["id"] == first["connectionId"] and device["deviceId"] == self.device_id
                and device["name"] == name and device["canPlay"] is False for device in first["devices"]
            ), "account observer admission")
            self.connection_id = first["connectionId"]
            try:
                while self.personal_snapshot is None:
                    self.apply(self.message())
            except TimeoutError as error:
                raise Failure("account socket admitted, but no personal snapshot arrived; "
                              "verify fixture PlaybackCatalog composition") from error
        except BaseException:
            self.sock.close()
            raise

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        try:
            self.sock.settimeout(1)
            self.frame(8, b"\x03\xe8")
        except OSError:
            pass
        finally:
            self.sock.close()

    def receive(self):
        remaining = self.deadline - monotonic()
        if remaining <= 0:
            raise TimeoutError("account observation deadline exceeded")
        self.sock.settimeout(remaining)
        recv = self.sock.recv(65536)
        check(recv, "account observer socket closed")
        self.buffer.extend(recv)

    def read(self, count):
        while len(self.buffer) < count:
            self.receive()
        result = bytes(self.buffer[:count])
        del self.buffer[:count]
        return result

    def frame(self, opcode, payload):
        check(len(payload) <= (125 if opcode >= 8 else 65535), "observer outgoing frame exceeded bound")
        size = bytes([len(payload) | 128]) if len(payload) < 126 else b"\xfe" + struct.pack("!H", len(payload))
        key = os.urandom(4)
        self.sock.sendall(bytes([128 | opcode]) + size + key +
                          bytes(value ^ key[i % 4] for i, value in enumerate(payload)))

    def send(self, message):
        self.frame(1, json.dumps(message).encode())

    def message(self):
        data = bytearray()
        fragmented = False
        while True:
            if monotonic() >= self.deadline:
                raise TimeoutError("account observation deadline exceeded")
            flags, size = self.read(2)
            opcode = flags & 15
            check(not flags & 112 and not size & 128, "unexpected account WebSocket frame")
            if size == 126:
                size = struct.unpack("!H", self.read(2))[0]
            elif size == 127:
                size = struct.unpack("!Q", self.read(8))[0]
            end = bool(flags & 128)
            check(size + len(data) <= MAX_FRAME and (opcode < 8 or end and size <= 125),
                  "account frame exceeded bound")
            payload = self.read(size)
            if opcode == 8:
                raise Failure("account server closed observer socket")
            if opcode == 9:
                self.frame(10, payload)
                continue
            if opcode == 10:
                continue
            check(opcode == (0 if fragmented else 1), "expected text account message")
            data.extend(payload)
            if end:
                return json.loads(data)
            fragmented = True

    def apply(self, frame):
        self.last_frame = frame
        kind = frame["type"]
        if kind == "snapshot":
            self.snapshot = frame
        elif kind == "outputs":
            self.snapshot = dict(self.snapshot, owner=frame["owner"], devices=frame["devices"])
        elif kind == "player":
            self.snapshot = dict(self.snapshot, player=apply_player(self.snapshot["player"], frame["player"]))
        elif kind == "personal":
            update = frame["update"]
            if "snapshot" in update:
                self.personal_snapshot = update["snapshot"]
            elif "change" in update:
                change = update["change"]
                snapshot = self.personal_snapshot
                playlists = {p["id"]: p for p in snapshot["playlists"]}
                if "playlist" in change:
                    playlist = change["playlist"]
                    playlists[playlist["id"]] = playlist
                playlists.pop(change.get("deleted"), None)
                removed = {(p["id"], p["origin"]) for p in change.get("recent", {}).get("removed", [])}
                recent = [p for p in snapshot["recent"] if (p["id"], p["origin"]) not in removed]
                for item in change.get("recent", {}).get("inserted", []):
                    recent.insert(item["index"], item["item"])
                self.personal_snapshot = dict(playlists=list(playlists.values()), recent=recent)
            else:
                check("part" in update, "unknown personal update")
        else:
            raise Failure(f"unexpected observer frame type: {kind}")

    def wait_frame(self, predicate, timeout=12):
        self.deadline = monotonic() + timeout
        while True:
            frame = self.message()
            self.apply(frame)
            if predicate(frame):
                return frame

    def wait(self, predicate, timeout=12):
        self.wait_frame(lambda _: predicate(self.snapshot), timeout)
        return self.snapshot

    def personal(self, kind, **fields):
        self.send(dict(type="personal", command=dict(type=kind, **fields)))

    def personal_change(self, predicate, timeout=12):
        def matches(frame):
            change = frame.get("update", {}).get("change")
            return change is not None and predicate(change)

        return self.wait_frame(matches, timeout)["update"]["change"]

    def playlist(self, playlist_id, timeout=12):
        deadline = monotonic() + timeout
        rows, fields = [], {}
        for _ in range(128):
            self.personal("syncPlaylist", playlistId=playlist_id, **fields)
            frame = self.wait_frame(lambda f: "part" in f.get("update", {}), max(0, deadline - monotonic()))
            part = frame["update"]["part"]
            check(part["summary"]["id"] == playlist_id and part["token"], "wrong playlist part identity")
            check(part["offset"] == len(rows) and part["token"] == fields.get("known", part["token"]),
                  "playlist changed or skipped rows during fenced readback")
            rows.extend(part["rows"])
            if not part["more"]:
                check([row["position"] for row in rows] == list(range(len(rows))) and
                      len({row["id"] for row in rows}) == len(rows), "playlist row positions or identities")
                return {**part, "offset": 0, "rows": rows}
            check(part["rows"], "empty nonfinal playlist part")
            fields = dict(known=part["token"], offset=len(rows))
        raise Failure("playlist exceeded part bound")


def apply_player(player, delta):
    """Mirror the canonical edits; check the patch before using it as evidence."""
    state = {**player, **{k: delta[k] for k in ("cursor", "repeat", "shuffle") if k in delta}}
    edit = delta.get("queue")
    if edit is None:
        return state
    rows = list(player["queue"])
    kind = edit["kind"]
    if kind == "replace":
        rows = edit.get("rows", [])
        state.pop("collectionFirst", None)
        if edit.get("collectionFirst"):
            state["collectionFirst"] = edit["collectionFirst"]
    elif kind == "insert":
        state.pop("collectionFirst", None)
        if not rows and edit.get("collectionFirst"):
            state["collectionFirst"] = edit["collectionFirst"]
        before = edit.get("before", 0)
        index = next((i for i, r in enumerate(rows) if r["key"] == before), len(rows))
        check(not before or index < len(rows), "missing queue insert anchor")
        rows[index:index] = edit["rows"]
    elif kind == "remove":
        state.pop("collectionFirst", None)
        keys = set(edit["keys"])
        check(keys <= {r["key"] for r in rows}, "unknown removed queue key")
        rows = [r for r in rows if r["key"] not in keys]
    elif kind == "order":
        by_key = {r["key"]: r for r in rows}
        keys = edit["keys"]
        check(len(keys) == len(rows) and set(keys) == set(by_key), "invalid queue order patch")
        rows = [by_key[k] for k in keys]
        if edit.get("rebaseAfter"):
            state.pop("collectionFirst", None)
            after = keys.index(edit["rebaseAfter"])
            rows = [dict(row, order=i + 1, intent="high" if i > after else row["intent"])
                    for i, row in enumerate(rows)]
    else:
        raise Failure(f"unknown queue edit: {kind}")
    check(len({r["key"] for r in rows}) == len(rows), "duplicate queue keys")
    state["queue"] = rows
    return state
