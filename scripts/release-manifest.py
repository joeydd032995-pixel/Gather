#!/usr/bin/env python3
"""Write the updater manifest (latest.json) and SHA256SUMS for a release.

Usage: release-manifest.py <artifact dir> <tag> <repo> <out dir>

SHA256SUMS always covers every published file, so unsigned installers can be
verified by hand (docs/INSTALL.md). latest.json is written only when the
build produced updater signatures (.sig files), i.e. when the updater key
secret is configured; without it, installed apps report that they cannot
update themselves.
"""

import hashlib
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

# Updater platform key -> suffix of the signed bundle for that platform.
PLATFORMS = {
    "linux-x86_64": ".AppImage",
    "windows-x86_64": "-setup.exe",
    "darwin-aarch64": ".app.tar.gz",
}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    artifacts, tag, repo, out = Path(sys.argv[1]), sys.argv[2], sys.argv[3], Path(sys.argv[4])
    out.mkdir(parents=True, exist_ok=True)
    files = sorted(p for p in artifacts.rglob("*") if p.is_file())

    names = [p.name for p in files]
    duplicates = {n for n in names if names.count(n) > 1}
    if duplicates:
        print(f"duplicate release asset names: {sorted(duplicates)}", file=sys.stderr)
        return 1

    sums = "".join(f"{sha256(p)}  {p.name}\n" for p in files if not p.name.endswith(".sig"))
    (out / "SHA256SUMS").write_text(sums)

    platforms = {}
    for key, suffix in PLATFORMS.items():
        bundle = next((p for p in files if p.name.endswith(suffix)), None)
        sig = bundle and bundle.with_name(bundle.name + ".sig")
        if bundle and sig.exists():
            platforms[key] = {
                "signature": sig.read_text().strip(),
                "url": f"https://github.com/{repo}/releases/download/{tag}/{bundle.name}",
            }
    if not platforms:
        print("no updater signatures: skipping latest.json (unsigned build)")
        return 0

    manifest = {
        "version": tag.removeprefix("v"),
        "notes": f"Gather {tag}: https://github.com/{repo}/releases/tag/{tag}",
        "pub_date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": platforms,
    }
    (out / "latest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"latest.json covers {sorted(platforms)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
