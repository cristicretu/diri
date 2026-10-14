#!/usr/bin/env python3
"""Regenerate diri-usage/prices.json from LiteLLM's published price table.

Run at release time: promote-nightly.sh folds it into its version-bump
commits, and release.sh refuses to build when the bundled table is stale.

Usage: sync-model-prices.py [--source PATH_OR_URL] [--check]

Keeps first-party Anthropic and OpenAI chat models (no provider prefix), converts
per-token USD to per-million, and writes a sorted, stable JSON file so a price
change shows up as a one-line diff. --check exits 3 when the file would
change and 1 on any other failure, so release.sh can tell stale prices from
an unreachable source.
"""

import argparse
import json
import pathlib
import re
import sys
import urllib.request

SOURCE = (
    "https://raw.githubusercontent.com/BerriAI/litellm/main/"
    "model_prices_and_context_window.json"
)
CRATE = pathlib.Path(__file__).resolve().parent.parent / "crates" / "diri-usage"
OUTPUT = CRATE / "prices.json"
OVERRIDES = CRATE / "price-overrides.json"

PROVIDERS = {"anthropic", "openai"}
MODES = {"chat", "responses"}
MODEL = re.compile(r"^(claude-|gpt-|codex-)[a-z0-9.-]+$")
FIELDS = {
    "input": "input_cost_per_token",
    "output": "output_cost_per_token",
    "cache_read": "cache_read_input_token_cost",
    "cache_write_5m": "cache_creation_input_token_cost",
    "cache_write_1h": "cache_creation_input_token_cost_above_1hr",
}
# Any parse that leaves fewer rows than this is a broken upstream, not a price change.
MIN_MODELS = 40


def load(source):
    if re.match(r"^https?://", source):
        with urllib.request.urlopen(source, timeout=60) as response:
            return json.load(response)
    return json.loads(pathlib.Path(source).read_text())


def per_million(value):
    # Per-token floats like 1.75e-07 carry binary noise; 6 decimals per
    # million tokens is a ten-thousandth of a cent.
    return round(value * 1_000_000, 6)


def extract(upstream):
    models = {}
    for name, entry in upstream.items():
        if not isinstance(entry, dict) or not MODEL.match(name):
            continue
        if entry.get("litellm_provider") not in PROVIDERS:
            continue
        if entry.get("mode") not in MODES:
            continue
        if entry.get(FIELDS["input"]) is None or entry.get(FIELDS["output"]) is None:
            continue
        row = {}
        for field, key in FIELDS.items():
            value = entry.get(key)
            if isinstance(value, (int, float)):
                row[field] = per_million(value)
        models[name] = row
    return models


def render(models):
    lines = ["{"]
    for index, name in enumerate(sorted(models)):
        comma = "," if index + 1 < len(models) else ""
        lines.append(f"  {json.dumps(name)}: {json.dumps(models[name], separators=(', ', ': '))}{comma}")
    lines.append("}")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", default=SOURCE)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    models = extract(load(args.source))
    if len(models) < MIN_MODELS:
        sys.exit(f"only {len(models)} models parsed from {args.source}; refusing to write")

    overrides = json.loads(OVERRIDES.read_text())["models"]
    for name in sorted(set(overrides) & set(models)):
        print(f"note: override {name} is now published upstream; consider removing it")

    text = render(models)
    current = OUTPUT.read_text() if OUTPUT.exists() else ""
    if text == current:
        print(f"{OUTPUT.name}: up to date ({len(models)} models)")
        return
    if args.check:
        print(f"{OUTPUT.name} is stale; run diri/scripts/sync-model-prices.py", file=sys.stderr)
        sys.exit(3)
    OUTPUT.write_text(text)
    print(f"{OUTPUT.name}: wrote {len(models)} models")


if __name__ == "__main__":
    main()
