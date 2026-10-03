#!/usr/bin/env python3
"""Write checksums and a machine-readable manifest for Linux artifacts.

Two modes:

  write-linux-release-manifest.py --directory DIR --version V --commit C
      [--architecture x86_64|aarch64]
      Describe the one AppImage and one Debian package that package-linux.sh
      just built natively in DIR. The architecture defaults to the host's.

  write-linux-release-manifest.py --merge DIR [DIR ...] --directory OUT
      Combine per-architecture build directories (each written by the first
      mode) into one release set in OUT: every package, one SHA256SUMS over all
      of them, and one linux-release.json.

linux-release.json keeps the shape published since the first Linux beta: in a
release set the top-level "architecture" and "artifacts" always describe the
x86_64 build, so anything that already reads them keeps getting x86_64 files
and never an aarch64 one. Every architecture, x86_64 included, is listed under
the additive "builds" array; readers that need another architecture select
their entry from there by "architecture".
"""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import shutil
from pathlib import Path

# Release architecture -> Debian "Architecture" field. cargo-packager names the
# AppImage with the release name (diri_<v>_x86_64.AppImage,
# diri_<v>_aarch64.AppImage) and the Debian package with the Debian name
# (diri_<v>_amd64.deb, diri_<v>_arm64.deb).
DEB_ARCHITECTURES = {"x86_64": "amd64", "aarch64": "arm64"}
ALIASES = {"x86_64": "x86_64", "amd64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}
# The architecture the legacy top-level fields of a release set describe.
PRIMARY = "x86_64"
MANIFEST = "linux-release.json"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def normalize_architecture(name: str) -> str:
    architecture = ALIASES.get(name.lower())
    if architecture is None:
        raise SystemExit(
            f"unsupported Linux architecture {name!r}; expected one of {sorted(DEB_ARCHITECTURES)}"
        )
    return architecture


def describe_build(directory: Path, architecture: str) -> dict:
    """The one AppImage and one DEB of <architecture> in <directory>."""
    suffixes = {
        ".AppImage": f"_{architecture}.AppImage",
        ".deb": f"_{DEB_ARCHITECTURES[architecture]}.deb",
    }
    artifacts = sorted(
        path
        for path in directory.iterdir()
        if path.is_file() and path.name.endswith(tuple(suffixes.values()))
    )
    if sorted(path.suffix for path in artifacts) != sorted(suffixes):
        raise SystemExit(
            f"expected one {architecture} AppImage (*{suffixes['.AppImage']}) and one DEB "
            f"(*{suffixes['.deb']}) in {directory}, found {[path.name for path in artifacts]}"
        )
    return {
        "architecture": architecture,
        "debArchitecture": DEB_ARCHITECTURES[architecture],
        "artifacts": [
            {
                "file": path.name,
                "format": "appimage" if path.suffix == ".AppImage" else "deb",
                "sha256": sha256(path),
                "size": path.stat().st_size,
            }
            for path in artifacts
        ],
    }


def write_manifest(directory: Path, version: str, commit: str, builds: list[dict]) -> None:
    # Every package in the directory must belong to exactly one build, so a
    # stray file is neither checksummed nor signed by accident.
    declared = {record["file"] for build in builds for record in build["artifacts"]}
    present = {
        path.name
        for path in directory.iterdir()
        if path.is_file() and path.suffix in {".deb", ".AppImage"}
    }
    if present != declared:
        raise SystemExit(f"undeclared Linux packages in {directory}: {sorted(present - declared)}")
    builds = sorted(builds, key=lambda build: (build["architecture"] != PRIMARY, build["architecture"]))
    top = builds[0]
    manifest = {
        "schema": 1,
        "product": "diri",
        "version": version,
        "platform": "linux",
        "architecture": top["architecture"],
        "commit": commit,
        "artifacts": top["artifacts"],
        "update": {
            "mode": "package-or-download",
            "automaticInAppUpdates": False,
        },
        "builds": builds,
    }
    (directory / MANIFEST).write_text(json.dumps(manifest, indent=2) + "\n")
    (directory / "SHA256SUMS").write_text(
        "".join(
            f"{record['sha256']}  {record['file']}\n"
            for build in builds
            for record in build["artifacts"]
        )
    )


def merge(sources: list[Path], output: Path) -> None:
    builds: list[tuple[Path, dict]] = []
    version = commit = None
    for source in sources:
        manifest = json.loads((source / MANIFEST).read_text())
        if manifest.get("product") != "diri" or manifest.get("platform") != "linux":
            raise SystemExit(f"{source / MANIFEST} is not a diri Linux manifest")
        if version is None:
            version, commit = manifest.get("version"), manifest.get("commit")
        elif (manifest.get("version"), manifest.get("commit")) != (version, commit):
            raise SystemExit(
                f"{source} was built from {manifest.get('version')} @ {manifest.get('commit')}, "
                f"not {version} @ {commit}"
            )
        for build in manifest.get("builds", []):
            architecture = normalize_architecture(build["architecture"])
            if any(existing["architecture"] == architecture for _, existing in builds):
                raise SystemExit(f"two {architecture} builds in the release set")
            # Re-describe from the bytes on disk: the merged manifest must
            # never vouch for a digest it did not compute.
            fresh = describe_build(source, architecture)
            if fresh["artifacts"] != build["artifacts"]:
                raise SystemExit(f"{source} no longer matches its {MANIFEST}")
            builds.append((source, fresh))
    if not any(build["architecture"] == PRIMARY for _, build in builds):
        # The legacy top-level fields promise x86_64 to existing readers;
        # never fill them with another architecture's files.
        raise SystemExit(f"a Linux release set must include the {PRIMARY} build")
    output.mkdir(parents=True, exist_ok=True)
    for source, build in builds:
        for record in build["artifacts"]:
            target = output / record["file"]
            if target.exists():
                raise SystemExit(f"{output} already contains {record['file']}")
            shutil.copy2(source / record["file"], target)
    write_manifest(output, version, commit, [build for _, build in builds])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--merge", nargs="+", type=Path, metavar="BUILD_DIR")
    parser.add_argument("--version")
    parser.add_argument("--architecture")
    parser.add_argument("--commit")
    args = parser.parse_args()

    if args.merge:
        if args.version or args.commit or args.architecture:
            parser.error("--merge takes version, commit, and architecture from each build")
        merge(args.merge, args.directory)
        return 0
    if not args.version or not args.commit:
        parser.error("--version and --commit are required")
    architecture = normalize_architecture(args.architecture or platform.machine())
    write_manifest(
        args.directory, args.version, args.commit, [describe_build(args.directory, architecture)]
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
