"""Version policy for published desktop releases."""
import argparse
import json
from pathlib import Path
import re
import sys
import tomllib

MAX_COMPONENT = 2**32 - 1
EMPTY_BASELINE = (0, 0, 0)
BUMPS = ("major", "minor", "patch")


def version_tuple(version):
    if not re.fullmatch(r"(?:0|[1-9][0-9]{0,9})(?:\.(?:0|[1-9][0-9]{0,9})){2}", version):
        raise ValueError("Expected a stable X.Y.Z version, e.g. 0.1.0")
    parts = tuple(map(int, version.split(".")))
    if any(x > MAX_COMPONENT for x in parts):
        raise ValueError("Version component outside supported range")
    return parts


def project_version(root):
    with (root / "Cargo.toml").open("rb") as file:
        package = tomllib.load(file)["package"]
    with (root / "Cargo.lock").open("rb") as file:
        locked = [p["version"] for p in tomllib.load(file)["package"]
                  if p["name"] == package["name"] and "source" not in p]
    version = package["version"]
    version_tuple(version)
    if locked != [version]:
        raise ValueError("Cargo.toml and Cargo.lock must agree on the desktop package version")
    return version


def check_bump(version, bump, pages):
    current = version_tuple(version)
    tag = f"v{version}"
    previous = []
    for page in pages:
        for release in page:
            existing = release["tag_name"]
            if existing == tag:
                raise ValueError(f"Release {tag} already exists")
            if release["draft"] or release["prerelease"] or not existing.startswith("v"):
                continue
            try:
                previous.append(version_tuple(existing[1:]))
            except ValueError:
                continue  # Not a stable desktop release tag.

    if bump not in BUMPS:
        raise ValueError("Choose major, minor, or patch")
    base = max(previous, default=EMPTY_BASELINE)
    i = BUMPS.index(bump)
    expected = base[:i] + (base[i] + 1,) + (0,) * (2 - i)
    if expected[i] > MAX_COMPONENT:
        raise ValueError("Version component outside supported range; choose a larger bump")
    if current != expected:
        dotted = lambda parts: ".".join(map(str, parts))
        raise ValueError(f"A {bump} bump from {dotted(base)} must be {dotted(expected)}, not {version}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bump", choices=BUMPS)
    args = parser.parse_args()
    try:
        version = project_version(Path.cwd())
        check_bump(version, args.bump, json.load(sys.stdin))
        print(version)
    except (KeyError, OSError, ValueError) as error:
        parser.exit(1, f"Release version: {error}\n")


if __name__ == "__main__":
    main()
