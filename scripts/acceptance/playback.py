"""Bounded loopback WebSocket observer for the production playback v2 protocol."""
from base64 import b64encode
import hashlib
import json
import os
import socket
import struct
from time import monotonic

from fixture import Failure, check


class Peer:
    protocol = "spofify.library-playback.v2"

    def __init__(self, api, timeout=8):
        ticket = api.json("/api/me/player/connect", method="POST", body={})["ticket"]
        self.deadline = monotonic() + timeout
        self.sock = socket.create_connection(("127.0.0.1", api.port), timeout=timeout)
        self.buffer = bytearray()
        try:
            key = b64encode(os.urandom(16)).decode()
            self.sock.sendall((f"GET /api/me/player/socket HTTP/1.1\r\nHost: 127.0.0.1:{api.port}\r\n"
                               f"Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
                               f"Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: {self.protocol}\r\n"
                               f"Origin: {api.origin}\r\nCookie: {api.cookie}\r\n\r\n").encode("ascii"))
            while b"\r\n\r\n" not in self.buffer:
                check(len(self.buffer) <= 16 * 1024, "playback upgrade exceeded bound")
                self.receive()
            split = self.buffer.index(b"\r\n\r\n")
            response = bytes(self.buffer[:split]).split(b"\r\n")
            del self.buffer[:split + 4]
            check(response[0].split()[1] == b"101", "production playback socket did not upgrade")
            headers = dict((k.lower(), v.strip()) for k, v in (line.split(b":", 1) for line in response[1:]))
            accept = b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
            check(headers.get(b"sec-websocket-accept") == accept and
                  headers.get(b"sec-websocket-protocol") == self.protocol.encode(), "playback upgrade mismatch")
            self.send({"type": "hello", "ticket": ticket, "name": "Acceptance observer",
                       "deviceId": "desktop-acceptance-read-only", "canPlay": False})
            self.snapshot = self.message()
            check(self.snapshot["type"] == "snapshot" and self.snapshot.get("connectionId") and
                  self.snapshot.get("sessionId") and self.snapshot["deviceId"] == "desktop-acceptance-read-only",
                  "playback observer admission")
        except BaseException:
            self.sock.close()
            raise

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        try:
            self.frame(8, b"\x03\xe8")
        except OSError:
            pass
        self.sock.close()

    def receive(self):
        remaining = self.deadline - monotonic()
        if remaining <= 0:
            raise TimeoutError("playback observation deadline exceeded")
        self.sock.settimeout(remaining)
        recv = self.sock.recv(65536)
        check(recv, "playback observer socket closed")
        self.buffer.extend(recv)

    def read(self, count):
        while len(self.buffer) < count:
            self.receive()
        result = bytes(self.buffer[:count])
        del self.buffer[:count]
        return result

    def frame(self, opcode, payload):
        check(len(payload) < 126, "observer outgoing control frame exceeded bound") if opcode >= 8 else None
        check(len(payload) <= 65535, "observer outgoing frame exceeded bound")
        size = bytes([len(payload) | 128]) if len(payload) < 126 else b"\xfe" + struct.pack("!H", len(payload))
        key = os.urandom(4)
        self.sock.sendall(bytes([128 | opcode]) + size + key +
                          bytes(value ^ key[i % 4] for i, value in enumerate(payload)))

    def send(self, message):
        self.frame(1, json.dumps(message).encode())

    def message(self):
        data = bytearray()
        while True:
            check(monotonic() < self.deadline, "playback observation deadline exceeded")
            flags, size = self.read(2)
            opcode = flags & 15
            check(not flags & 112 and not size & 128, "unexpected playback WebSocket frame")
            if size == 126:
                size = struct.unpack("!H", self.read(2))[0]
            elif size == 127:
                size = struct.unpack("!Q", self.read(8))[0]
            check(size + len(data) <= 3 * 1024 * 1024, "playback frame exceeded bound")
            payload = self.read(size)
            if opcode == 8:
                raise Failure("playback server closed observer socket")
            if opcode == 9:
                self.frame(10, payload)
                continue
            if opcode == 10:
                continue
            check(opcode == (0 if data else 1), "expected text playback message")
            data.extend(payload)
            if flags & 128:
                return json.loads(data)

    def wait(self, predicate, timeout=12):
        self.deadline = monotonic() + timeout
        while True:
            message = self.message()
            if predicate(message):
                return message
