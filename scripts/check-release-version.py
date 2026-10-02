#!/usr/bin/env python3
"""Check desktop version consistency and the release tag, when present."""
import json
import os
from pathlib import Path
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def read_json(path):
    return json.loads((ROOT / path).read_text())


app = read_json("apps/desktop/src-tauri/tauri.conf.json")["version"]
package = read_json("apps/desktop/package.json")["version"]
npm_lock = read_json("apps/desktop/package-lock.json")
native = tomllib.loads((ROOT / "apps/desktop/src-tauri/Cargo.toml").read_text())
native_lock = tomllib.loads((ROOT / "apps/desktop/src-tauri/Cargo.lock").read_text())
locked = [p["version"] for p in native_lock["package"] if p["name"] == "gather-desktop"]
versions = {
    "tauri.conf.json": app,
    "package.json": package,
    "package-lock.json": npm_lock["version"],
    "package-lock.json root": npm_lock["packages"][""]["version"],
    "Cargo.toml": native["package"]["version"],
    "Cargo.lock": locked[0] if len(locked) == 1 else None,
}
for source, value in versions.items():
    if value != app:
        raise SystemExit(f"{source} version {value!r} differs from app version {app!r}")
ref = os.environ.get("GITHUB_REF", "")
if ref.startswith("refs/tags/v") and ref != f"refs/tags/v{app}":
    raise SystemExit(f"Release tag {ref!r} differs from app version {app!r}")
print(f"Desktop version entries agree: {app}")
