#!/usr/bin/env python3
"""Exercise CI simulator setup without requiring Xcode or creating devices."""

import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location(
    "prepare_simulator", Path(__file__).with_name("prepare-simulator.py")
)
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


def runtime(version, available=True, platform="iOS"):
    return {
        "identifier": f"com.apple.CoreSimulator.SimRuntime.{platform}-{version.replace('.', '-')}",
        "version": version,
        "isAvailable": available,
    }


class PrepareSimulatorTests(unittest.TestCase):
    @patch.object(prepare, "simctl")
    def test_existing_available_destination_is_reused(self, simctl):
        simctl.return_value = json.dumps({"devices": {"ios": [
            {"name": "iPhone 17 Pro", "isAvailable": True},
        ]}})
        prepare.prepare_simulator()
        simctl.assert_called_once_with("list", "devices", "available", "--json")

    @patch.object(prepare, "simctl")
    def test_missing_destination_uses_latest_available_ios_runtime(self, simctl):
        simctl.side_effect = [
            json.dumps({"devices": {"ios": [
                {"name": "iPhone 17 Pro Max", "isAvailable": True},
                {"name": "iPhone 17 Pro", "isAvailable": False},
            ]}}),
            json.dumps({"runtimes": [
                runtime("26.3"), runtime("26.10"), runtime("27.0", available=False),
                runtime("30.0", platform="watchOS"),
            ]}),
            "new-device-uuid",
        ]
        prepare.prepare_simulator()
        simctl.assert_called_with(
            "create", "iPhone 17 Pro", "iPhone 17 Pro",
            "com.apple.CoreSimulator.SimRuntime.iOS-26-10",
        )

    @patch.object(prepare, "simctl")
    def test_no_available_ios_runtime_fails_without_creating_a_device(self, simctl):
        simctl.side_effect = [
            json.dumps({"devices": {}}),
            json.dumps({"runtimes": [runtime("26.3", available=False)]}),
        ]
        with self.assertRaisesRegex(RuntimeError, "no available iOS"):
            prepare.prepare_simulator()
        self.assertEqual(simctl.call_count, 2)


if __name__ == "__main__":
    unittest.main()
