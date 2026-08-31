#!/usr/bin/env python3
"""Validate dependency license metadata and optionally write an inventory."""

from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys


ROOT = pathlib.Path(__file__).resolve().parents[1]
POLICY_PATH = ROOT / "license-policy.json"
RUST_WORKSPACE = ROOT / "diri"


def cargo_metadata() -> dict:
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=RUST_WORKSPACE,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        print(result.stderr, file=sys.stderr, end="")
        raise SystemExit(result.returncode)
    return json.loads(result.stdout)


def is_unacceptable_expression(expression: str) -> bool:
    upper = expression.upper()
    if any(token in upper for token in ("AGPL", "SSPL", "BUSL", "COMMONS-CLAUSE")):
        return True
    if "GPL" not in upper:
        return False
    # SPDX OR expressions let the distributor choose a permissive branch.
    permissive = ("APACHE-2.0", "MIT", "BSD-", "ISC", "ZLIB", "MPL-2.0")
    return " OR " not in upper or not any(token in upper for token in permissive)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=pathlib.Path, help="write the reviewed inventory as JSON")
    args = parser.parse_args()

    policy = json.loads(POLICY_PATH.read_text())
    metadata = cargo_metadata()
    missing_policy = {
        (entry["name"], entry["version"]): entry
        for entry in policy["rust_missing_metadata"]
    }
    seen_exceptions: set[tuple[str, str]] = set()
    failures: list[str] = []
    inventory: list[dict[str, str]] = []

    for package in sorted(metadata["packages"], key=lambda item: (item["name"], item["version"])):
        if package["source"] is None:
            continue
        license_expression = package.get("license") or ""
        key = (package["name"], package["version"])
        if not license_expression:
            exception = missing_policy.get(key)
            source = package["source"] or ""
            if exception is None or exception["source_contains"] not in source:
                failures.append(
                    f"{package['name']} {package['version']} has no license metadata ({source})"
                )
                effective_license = "UNKNOWN"
            else:
                seen_exceptions.add(key)
                effective_license = exception["conservative_license"]
        else:
            effective_license = license_expression
            if is_unacceptable_expression(license_expression):
                failures.append(
                    f"{package['name']} {package['version']} uses {license_expression}"
                )

        inventory.append(
            {
                "name": package["name"],
                "version": package["version"],
                "license": effective_license,
                "source": package["source"],
            }
        )

    stale = set(missing_policy) - seen_exceptions
    for name, version in sorted(stale):
        failures.append(
            f"stale missing-license exception for {name} {version}; review and remove it"
        )

    rust_names = {package["name"] for package in metadata["packages"]}
    if {"zlog"} & rust_names:
        failures.append("GPL profiling package zlog re-entered the Rust dependency graph")

    reviewed = {
        (entry["ecosystem"].lower(), entry["name"].lower()): entry
        for entry in policy["manually_reviewed_non_rust"]
    }
    found_non_rust: set[tuple[str, str]] = set()

    npm_lock = json.loads((ROOT / "sidecar" / "package-lock.json").read_text())
    for package_path, package in npm_lock["packages"].items():
        if "node_modules/" not in package_path:
            continue
        name = package_path.rsplit("node_modules/", 1)[1]
        key = ("npm", name.lower())
        found_non_rust.add(key)
        entry = reviewed.get(key)
        version = package.get("version", "UNKNOWN")
        if entry is None:
            failures.append(f"unreviewed npm dependency {name} {version}")
        elif entry["version"] != version:
            failures.append(
                f"npm dependency {name} changed {entry['version']} -> {version}; review its license"
            )
        elif package.get("license") and entry["license"] != package["license"]:
            failures.append(
                f"npm dependency {name} declares {package['license']}, policy says {entry['license']}"
            )

    for ecosystem, name in sorted(set(reviewed) - found_non_rust):
        failures.append(f"stale {ecosystem} license review entry for {name}")

    if failures:
        print("dependency license policy failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        report = {
            "schema": 1,
            "project_license": policy["project_license"],
            "rust_dependencies": inventory,
            "non_rust_dependencies": policy["manually_reviewed_non_rust"],
            "notes": policy["notes"],
        }
        args.output.write_text(json.dumps(report, indent=2) + "\n")

    print(
        f"dependency license policy passed: {len(inventory)} Rust packages, "
        f"{len(seen_exceptions)} conservative metadata exceptions"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
