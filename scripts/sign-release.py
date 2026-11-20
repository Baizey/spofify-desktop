#!/usr/bin/env python3
"""Assemble the signed x86-64 updater envelope from a built release binary."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

ASSET = "spofify-desktop-linux-x86_64"
MANIFEST = "update-v1-linux-x86_64.json"
LIMIT = 64 * 1024 * 1024
ED25519_SPKI_PREFIX = bytes.fromhex("302a300506032b6570032100")


def version_tuple(version):
    if not re.fullmatch(r"(?:0|[1-9][0-9]{0,9})(?:\.(?:0|[1-9][0-9]{0,9})){2}", version):
        raise ValueError("Expected a stable Cargo version, e.g. 0.1.0")
    parts = tuple(map(int, version.split(".")))
    if any(x > 2**32 - 1 for x in parts):
        raise ValueError("Version component outside supported range")
    return parts


def sign(directory, repo, version, public_key, private_key):
    version_tuple(version)
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,99}/[A-Za-z0-9][A-Za-z0-9_.-]{0,99}", repo):
        raise ValueError("Invalid GitHub release repository")
    if not re.fullmatch(r"[a-f0-9]{64}", public_key):
        raise ValueError("Invalid Ed25519 public key")
    with (directory / ASSET).open("rb") as binary:
        size = os.fstat(binary.fileno()).st_size
        if not 0 < size <= LIMIT:
            raise ValueError("Invalid release binary size")
        sha256 = hashlib.file_digest(binary, "sha256").hexdigest()
    body = json.dumps(dict(schema=1, repository=repo, version=version, asset=ASSET,
                           bytes=size, sha256=sha256), separators=(",", ":"))
    with tempfile.TemporaryDirectory(prefix="spofify-sign-") as work:
        key, payload = Path(work) / "key.pem", Path(work) / "manifest"
        fd = os.open(key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as handle:
            handle.write(private_key)
        payload.write_text(body, encoding="utf-8")
        public = subprocess.check_output(["openssl", "pkey", "-in", key, "-pubout", "-outform", "DER"])
        if public != ED25519_SPKI_PREFIX + bytes.fromhex(public_key):
            raise ValueError("Signing key is not the Ed25519 key embedded in the application")
        signature = subprocess.check_output(["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", key, "-in", payload])
    (directory / MANIFEST).write_text(json.dumps(dict(manifest=body, signature=signature.hex())) + "\n", encoding="utf-8")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("version")
    args = parser.parse_args()
    private = os.environ.pop("SPOFIFY_RELEASE_SIGNING_KEY", "")
    try:
        if not private:
            raise ValueError("Missing SPOFIFY_RELEASE_SIGNING_KEY")
        sign(args.directory, os.environ["SPOFIFY_UPDATE_REPO"], args.version,
             os.environ["SPOFIFY_UPDATE_PUBLIC_KEY"], private)
    except (KeyError, OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Release signing failed: {error}\n")


if __name__ == "__main__":
    main()
