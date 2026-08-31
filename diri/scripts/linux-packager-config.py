#!/usr/bin/env python3
"""Emit cargo-packager's Linux config from the release workspace layout."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


BINARIES = (
    "diri",
    "dirijor",
    "dirijor-mcp",
    "dirijord-rs",
    "diri-holder",
    "diri-ssh-askpass",
    "diri-remote",
)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--workspace", required=True, type=Path)
    parser.add_argument("--binaries", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--license-inventory", required=True, type=Path)
    args = parser.parse_args()

    workspace = args.workspace.resolve()
    repository = workspace.parent
    config = {
        "name": "diri-linux",
        "productName": "diri",
        "version": args.version,
        "identifier": "com.dirijor.diri",
        "description": "A focused desktop orchestrator for coding agents",
        "homepage": "https://github.com/cristicretu/diri",
        "authors": ["Cristi Cretu"],
        "licenseFile": str(repository / "LICENSE"),
        "category": "DeveloperTool",
        "formats": ["appimage", "deb"],
        "outDir": str(args.output.resolve()),
        "binariesDir": str(args.binaries.resolve()),
        "binaries": [
            {"path": name, "main": name == "diri"} for name in BINARIES
        ],
        "icons": [str(workspace / "assets" / "icon.png")],
        "resources": [
            {
                "src": str(workspace / "crates" / "diri-engine" / "manifests"),
                "target": "manifests",
            },
            {"src": str(repository / "sidecar"), "target": "sidecar"},
            {
                "src": str(args.license_inventory.resolve()),
                "target": "licenses/THIRD-PARTY-LICENSES.json",
            },
            {
                "src": str(repository / "LICENSE"),
                "target": "licenses/Apache-2.0.txt",
            },
            {
                "src": str(repository / "NOTICE"),
                "target": "licenses/NOTICE.txt",
            },
            {
                "src": str(repository / "license-policy.json"),
                "target": "licenses/license-policy.json",
            },
            {"src": str(repository / "LICENSES"), "target": "licenses"},
        ],
        "linux": {"generateDesktopEntry": True},
        "deb": {
            "packageName": "diri",
            "section": "devel",
            "priority": "optional",
            "desktopTemplate": str(
                workspace / "packaging" / "linux" / "diri.desktop"
            ),
            "depends": [
                "libc6 (>= 2.35)",
                "libasound2",
                "libfontconfig1",
                "libglib2.0-0",
                "libvulkan1",
                "libwayland-client0",
                "libx11-xcb1",
                "libxkbcommon0",
                "libxkbcommon-x11-0",
            ],
        },
    }
    print(json.dumps(config, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
