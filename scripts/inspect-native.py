#!/usr/bin/env python3
"""Capture a running opt-in native Slint window; no app-specific test backend.

Use disposable accounts only. Refuses a capture while login inputs have values.
This helper does not launch, authenticate or shut down an application.
"""
import argparse
import base64
import json
from pathlib import Path
import urllib.request


class Mcp:
    def __init__(self, port):
        self.url = f"http://127.0.0.1:{port}/mcp"
        self.sequence = 0

    def rpc(self, method, params):
        self.sequence += 1
        request = urllib.request.Request(
            self.url,
            json.dumps({"jsonrpc": "2.0", "id": self.sequence,
                        "method": method, "params": params}).encode(),
            {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"},
        )
        # Never use ambient proxy settings for the localhost development endpoint.
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(request, timeout=10) as response:
            body = response.read(12 * 1024 * 1024 + 1)
        if len(body) > 12 * 1024 * 1024:
            raise RuntimeError("MCP response exceeds capture bound")
        text = body.decode()
        if text.startswith(("event:", "data:")):
            text = next(line[6:] for line in text.splitlines() if line.startswith("data: "))
        reply = json.loads(text)
        if "error" in reply:
            raise RuntimeError("MCP request failed")
        return reply["result"]

    def call(self, name, arguments):
        result = self.rpc("tools/call", {"name": name, "arguments": arguments})
        if result.get("isError"):
            raise RuntimeError(f"MCP tool failed: {name}")
        return result

    def value(self, name, arguments):
        return json.loads(self.call(name, arguments)["content"][0]["text"])


def capture(port, output):
    mcp = Mcp(port)
    windows = mcp.value("list_windows", {})["windowHandles"]
    if len(windows) != 1:
        raise RuntimeError("Capture requires exactly one native window")
    window = windows[0]
    properties = mcp.value("get_window_properties", {"windowHandle": window})
    tree = mcp.value("get_element_tree", {
        "elementHandle": properties["rootElementHandle"], "maxElements": 1000})
    if tree.get("debugInfoMissing") or tree.get("truncated"):
        raise RuntimeError("Native inspection metadata is missing or incomplete")
    # Do not retain credentials in trees, including values exposed by inherited widgets.
    def scrub(value):
        if isinstance(value, dict):
            label = value.get("accessibleLabel", "")
            if label in ("Username", "Password") and value.get("accessibleValue"):
                raise RuntimeError("Clear login inputs before capturing")
            return {key: scrub(item) for key, item in value.items() if key != "accessibleValue"}
        if isinstance(value, list):
            return [scrub(item) for item in value]
        return value
    safe_tree = scrub(tree)
    image = next(part for part in mcp.call("take_screenshot", {
        "windowHandle": window})["content"] if part["type"] == "image")
    output.mkdir(parents=True, exist_ok=False)
    (output / "native.png").write_bytes(base64.b64decode(image["data"], validate=True))
    (output / "tree.json").write_text(json.dumps(safe_tree, indent=2) + "\n")
    (output / "window.json").write_text(json.dumps(properties, indent=2) + "\n")
    print(output.resolve() / "native.png")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=18471)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not 1024 <= args.port <= 65535:
        parser.error("use an unprivileged localhost port")
    capture(args.port, args.output)
