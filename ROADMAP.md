# Roadmap

This is direction, not a promise or release calendar. Concrete work is tracked
in [Issues](https://github.com/cristicretu/diri/issues).

## Now

- Keep `main` green and protected by required CI checks.
- Make session persistence and daemon upgrades boring and recoverable.
- Expand first-class agent manifests and status-detection coverage.
- Improve contributor docs, security reporting, privacy disclosure, and release
  supply-chain checks.

## Next

- Harden the Rust Engine upgrade path and expand cross-platform coverage.
- Improve remote-node setup, diagnostics, and least-privilege guidance.
- Add deeper end-to-end tests for app updates and session recovery.
- Move more release provenance into reproducible, attestable CI steps while
  preserving Apple signing and notarization requirements.

## Distribution

- Continue publishing signed, notarized releases and the maintained Homebrew
  tap.
- Submit Diri to the official Homebrew cask repository once it is eligible.
  The owner-submission notability threshold is met; the remaining normal policy
  gate opens on September 3, 2026, at 10:08:23 UTC, when the repository is 30
  days old. The [submission runbook](docs/HOMEBREW_CASK_SUBMISSION.md) keeps the
  candidate, checksum verification, and validation commands ready. Until
  acceptance, the supported command is
  `brew install --cask cristicretu/diri/diri`.

## Not planned

- A hosted Diri account or telemetry service.
- Treating agent processes as a security sandbox. Diri orchestrates trusted
  developer tools; it does not contain them.
