"""In-memory, noninteractive Secret Service double for the private test bus only.

Executed with a Python that provides dbus-python and PyGObject (GLib).
The receipt contains operation names only; credentials never appear on disk.
"""
from pathlib import Path
import json
import os
import re
import sys

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib


BASE = "/org/freedesktop/secrets"
COLLECTION = BASE + "/collection/default"
SERVICE = "org.freedesktop.Secret.Service"
ITEM = "org.freedesktop.Secret.Item"
SECRET = "(oayays)"
NONE = dbus.ObjectPath("/")


def require(condition):
    if not condition:
        raise dbus.DBusException("Unexpected operation for the private Secret Service double")


class Service(dbus.service.Object):
    def __init__(self, name, receipt):
        super().__init__(name, BASE)
        self.name = name
        self.receipt = receipt
        self.items = {}
        self.sessions = {}
        self.sequence = 0
        self.collection = Collection(self)

    def note(self, event):
        with self.receipt.open("a", encoding="ascii") as log:
            print(event, file=log)

    def path(self, group):
        self.sequence += 1
        return dbus.ObjectPath(f"{BASE}/{group}/test{self.sequence}")

    @dbus.service.method(SERVICE, in_signature="s", out_signature="o")
    def ReadAlias(self, alias):
        require(alias == "default")
        return dbus.ObjectPath(COLLECTION)

    @dbus.service.method(SERVICE, in_signature="sv", out_signature="vo", sender_keyword="sender")
    def OpenSession(self, algorithm, input_value, sender=None):
        require(algorithm == "plain" and input_value == "")
        path = self.path("session")
        self.sessions[path] = sender
        return dbus.String("", variant_level=1), path

    @dbus.service.method(SERVICE, in_signature="a{ss}", out_signature="aoao")
    def SearchItems(self, attrs):
        return [path for path, item in self.items.items() if item.attrs == attrs], []


class Collection(dbus.service.Object):
    IFACE = "org.freedesktop.Secret.Collection"

    def __init__(self, service):
        super().__init__(service.name, COLLECTION)
        self.service = service

    @dbus.service.method("org.freedesktop.DBus.Properties", in_signature="ss", out_signature="v")
    def Get(self, interface, name):
        require(interface == self.IFACE and name == "Locked")
        return dbus.Boolean(False, variant_level=1)

    @dbus.service.method(IFACE, in_signature="a{sv}" + SECRET + "b", out_signature="oo",
                         byte_arrays=True, sender_keyword="sender")
    def CreateItem(self, properties, secret, replace, sender=None):
        attrs = properties.get(ITEM + ".Attributes", {})
        session, params, blob, content_type = secret
        require(self.service.sessions.get(session) == sender and not params and not replace
                and content_type in ("text/plain; charset=utf8", "application/json") and 0 < len(blob) <= 16384)
        require(properties.get(ITEM + ".Label") and set(attrs) == {"application", "slot"}
                and attrs["application"] == "spofify-desktop-remembered-login-v1"
                and re.fullmatch(r"[0-9a-f]{32}", attrs["slot"]) and not self.service.items)
        try:
            credential = json.loads(bytes(blob))
        except (ValueError, UnicodeError):
            require(False)
        require(isinstance(credential, dict) and set(credential) == {"origin", "accountId", "cookie"}
                and credential["origin"].startswith("http://127.0.0.1:") and credential["accountId"]
                and credential["cookie"].startswith("spofify_session="))
        path = self.service.path("item")
        self.service.items[path] = Item(self.service, path, attrs, blob)
        self.service.note("store")
        return path, NONE


class Item(dbus.service.Object):
    def __init__(self, service, path, attrs, blob):
        super().__init__(service.name, path)
        self.service, self.path, self.attrs, self.blob = service, path, attrs, blob

    @dbus.service.method(ITEM, in_signature="o", out_signature=SECRET, sender_keyword="sender")
    def GetSecret(self, session, sender=None):
        require(self.service.sessions.get(session) == sender)
        self.service.note("lookup")
        return session, dbus.ByteArray(b""), self.blob, "application/json"

    @dbus.service.method(ITEM, in_signature="", out_signature="o")
    def Delete(self):
        del self.service.items[self.path]
        self.remove_from_connection()
        self.service.note("clear")
        return NONE


if __name__ == "__main__":
    DBusGMainLoop(set_as_default=True)
    address = os.environ["DBUS_SESSION_BUS_ADDRESS"]
    require(address.startswith("unix:path=" + os.environ["XDG_RUNTIME_DIR"] + "/"))
    bus = dbus.bus.BusConnection(address)
    name = dbus.service.BusName("org.freedesktop.secrets", bus=bus, do_not_queue=True)
    service = Service(name, Path(sys.argv[1]))
    service.note("ready")
    GLib.MainLoop().run()
