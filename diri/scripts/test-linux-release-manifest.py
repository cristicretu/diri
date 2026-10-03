#!/usr/bin/env python3
"""Tests write-linux-release-manifest.py on dummy packages, no Linux needed.

Covers the per-architecture build manifest, the merged release set Nightly
signs, and the compatibility promise: the merged linux-release.json keeps the
0.9.x top-level shape and its top-level files are always the x86_64 ones.
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "write-linux-release-manifest.py"
COMMIT = "0" * 40
# The keys linux-release.json carried before arm64 packages existed.
LEGACY_KEYS = {"schema", "product", "version", "platform", "architecture", "commit", "artifacts", "update"}


def run(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(SCRIPT), *args], capture_output=True, text=True, check=False
    )


def ok(*args: str) -> None:
    result = run(*args)
    assert result.returncode == 0, result.stderr


def fails(pattern: str, *args: str) -> None:
    result = run(*args)
    assert result.returncode != 0, f"expected failure for {args}"
    assert pattern in result.stderr, f"{pattern!r} not in {result.stderr!r}"


def build(root: Path, arch: str, deb_arch: str, version: str = "1.2.3") -> Path:
    directory = root / f"build-{arch}"
    directory.mkdir()
    (directory / f"diri_{version}_{arch}.AppImage").write_bytes(arch.encode() * 1000)
    (directory / f"diri_{version}_{deb_arch}.deb").write_bytes(deb_arch.encode() * 100)
    ok("--directory", str(directory), "--version", version, "--commit", COMMIT, "--architecture", arch)
    return directory


def manifest(directory: Path) -> dict:
    return json.loads((directory / "linux-release.json").read_text())


def test_single_build(root: Path) -> None:
    for arch, deb_arch in (("x86_64", "amd64"), ("aarch64", "arm64")):
        data = manifest(build(root, arch, deb_arch))
        assert data["architecture"] == arch
        assert sorted(record["file"] for record in data["artifacts"]) == sorted([
            f"diri_1.2.3_{deb_arch}.deb",
            f"diri_1.2.3_{arch}.AppImage",
        ])
        assert data["builds"] == [
            {"architecture": arch, "debArchitecture": deb_arch, "artifacts": data["artifacts"]}
        ]
    print("ok: per-architecture manifests name the matching packages")


def test_aliases_and_wrong_architecture(root: Path) -> None:
    directory = root / "arm64-alias"
    directory.mkdir()
    (directory / "diri_1.2.3_aarch64.AppImage").write_bytes(b"a")
    (directory / "diri_1.2.3_arm64.deb").write_bytes(b"d")
    ok("--directory", str(directory), "--version", "1.2.3", "--commit", COMMIT, "--architecture", "arm64")
    assert manifest(directory)["architecture"] == "aarch64"
    fails("expected one x86_64 AppImage", "--directory", str(directory), "--version", "1.2.3",
          "--commit", COMMIT, "--architecture", "x86_64")
    fails("unsupported Linux architecture", "--directory", str(directory), "--version", "1.2.3",
          "--commit", COMMIT, "--architecture", "riscv64")
    (directory / "diri_1.2.3_amd64.deb").write_bytes(b"x")
    fails("undeclared Linux packages", "--directory", str(directory), "--version", "1.2.3",
          "--commit", COMMIT, "--architecture", "aarch64")
    print("ok: architecture aliases, mismatches, and stray packages")


def test_merge(root: Path) -> None:
    x86 = build(root, "x86_64", "amd64")
    arm = build(root, "aarch64", "arm64")
    out = root / "release"
    # Order of the inputs does not decide which build the legacy fields describe.
    ok("--directory", str(out), "--merge", str(arm), str(x86))
    data = manifest(out)
    assert LEGACY_KEYS <= set(data), data.keys()
    assert data["schema"] == 1 and data["platform"] == "linux"
    assert data["architecture"] == "x86_64"
    assert data["artifacts"] == manifest(x86)["artifacts"]
    assert [b["architecture"] for b in data["builds"]] == ["x86_64", "aarch64"]
    assert [b["debArchitecture"] for b in data["builds"]] == ["amd64", "arm64"]
    sums = (out / "SHA256SUMS").read_text().splitlines()
    assert sorted(line.split("  ")[1] for line in sums) == sorted([
        "diri_1.2.3_amd64.deb",
        "diri_1.2.3_x86_64.AppImage",
        "diri_1.2.3_arm64.deb",
        "diri_1.2.3_aarch64.AppImage",
    ])
    for build_dir in (x86, arm):
        for line in (build_dir / "SHA256SUMS").read_text().splitlines():
            assert line in sums
    fails("already contains", "--directory", str(out), "--merge", str(x86), str(arm))
    print("ok: merged release set keeps the x86_64 legacy fields and lists both builds")


def test_merge_rejections(root: Path) -> None:
    x86 = build(root, "x86_64", "amd64")
    arm = build(root, "aarch64", "arm64")
    fails("must include the x86_64 build", "--directory", str(root / "a"), "--merge", str(arm))
    fails("two x86_64 builds", "--directory", str(root / "b"), "--merge", str(x86), str(x86))
    other = root / "other"
    other.mkdir()
    (other / "diri_1.2.4_aarch64.AppImage").write_bytes(b"a")
    (other / "diri_1.2.4_arm64.deb").write_bytes(b"d")
    ok("--directory", str(other), "--version", "1.2.4", "--commit", COMMIT, "--architecture", "aarch64")
    fails("was built from", "--directory", str(root / "c"), "--merge", str(x86), str(other))
    (arm / "diri_1.2.3_arm64.deb").write_bytes(b"tampered")
    fails("no longer matches", "--directory", str(root / "d"), "--merge", str(x86), str(arm))
    fails("takes version", "--directory", str(root / "e"), "--merge", str(x86), "--version", "1")
    print("ok: merge refuses missing x86_64, duplicates, mixed versions, and tampering")


def main() -> int:
    for test in (test_single_build, test_aliases_and_wrong_architecture, test_merge, test_merge_rejections):
        with tempfile.TemporaryDirectory(prefix="diri-linux-manifest-") as root:
            test(Path(root))
    print("linux-release manifest tests passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
