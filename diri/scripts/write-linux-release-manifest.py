#!/usr/bin/env python3
"""Write checksums and a machine-readable manifest for Linux artifacts."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--architecture", default="x86_64")
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()

    artifacts = sorted(
        path
        for path in args.directory.iterdir()
        if path.is_file() and path.suffix in {".deb", ".AppImage"}
    )
    if len(artifacts) != 2:
        raise SystemExit(f"expected one AppImage and one DEB, found {len(artifacts)}")

    records = [
        {
            "file": path.name,
            "format": "appimage" if path.suffix == ".AppImage" else "deb",
            "sha256": sha256(path),
            "size": path.stat().st_size,
        }
        for path in artifacts
    ]
    manifest = {
        "schema": 1,
        "product": "diri",
        "version": args.version,
        "platform": "linux",
        "architecture": args.architecture,
        "commit": args.commit,
        "artifacts": records,
        "update": {
            "mode": "package-or-download",
            "automaticInAppUpdates": False,
        },
    }
    (args.directory / "linux-release.json").write_text(
        json.dumps(manifest, indent=2) + "\n"
    )
    (args.directory / "SHA256SUMS").write_text(
        "".join(f"{record['sha256']}  {record['file']}\n" for record in records)
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
