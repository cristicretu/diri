# Roadmap

Diri's priorities are session reliability, useful agent integrations, and a
compact workspace. This is direction, not a release calendar. Specific work
is tracked in [Issues](https://github.com/cristicretu/diri/issues).

## Focus

- **Session continuity.** Reliable persistence, Engine upgrades, and recovery;
  deeper tests for updates and reconnects.
- **Agent integration.** More launch and resume support, with accurate status
  detection and real terminal fixtures.
- **Platform coverage.** Harden the Rust Engine across supported platforms and
  improve remote setup and diagnostics.
- **Release quality.** Keep CI green, strengthen provenance and supply-chain
  checks, and maintain signed, notarized macOS builds and the Homebrew tap.

## Boundaries

A hosted Diri account or telemetry service is not planned. Agent processes run
with your user permissions; Diri does not sandbox them.

For proposals, describe the workflow and the smallest useful improvement in
[Discussions](https://github.com/cristicretu/diri/discussions) or a
[feature request](https://github.com/cristicretu/diri/issues/new?template=feature_request.yml).
