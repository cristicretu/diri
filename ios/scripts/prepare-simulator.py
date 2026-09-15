#!/usr/bin/env python3
"""Ensure the unsigned iPhone CI destination exists on the current runner."""

import json
import subprocess


DEVICE_NAME = "iPhone 17 Pro"


def simctl(*args):
    return subprocess.run(
        ["xcrun", "simctl", *args], check=True, text=True, capture_output=True
    ).stdout


def prepare_simulator():
    devices = json.loads(simctl("list", "devices", "available", "--json"))
    if any(
        device["name"] == DEVICE_NAME and device.get("isAvailable", False)
        for runtime_devices in devices["devices"].values()
        for device in runtime_devices
    ):
        return

    runtimes = json.loads(simctl("list", "runtimes", "available", "--json"))
    eligible = [
        runtime
        for runtime in runtimes["runtimes"]
        if runtime.get("isAvailable", False)
        and runtime["identifier"].startswith("com.apple.CoreSimulator.SimRuntime.iOS-")
    ]
    if not eligible:
        raise RuntimeError("this runner has no available iOS simulator runtime")
    runtime = max(eligible, key=lambda item: tuple(map(int, item["version"].split("."))))
    print(f"Creating {DEVICE_NAME} with {runtime['identifier']}")
    simctl("create", DEVICE_NAME, DEVICE_NAME, runtime["identifier"])


if __name__ == "__main__":
    try:
        prepare_simulator()
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.stderr) from error
    except RuntimeError as error:
        raise SystemExit(str(error)) from error
