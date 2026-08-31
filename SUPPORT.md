# Support

Use [GitHub Discussions](https://github.com/cristicretu/diri/discussions) for
setup questions, workflows, and ideas that are not yet concrete bug reports.
Use [Issues](https://github.com/cristicretu/diri/issues) for reproducible bugs
and scoped feature requests.

Before reporting a bug, check the latest release. In the app, open **Settings →
General → Support → Copy diagnostics**, preview the exact report, and then copy
it into the issue. The report is deliberately limited to app/platform/daemon
metadata, agent availability, remote-host ids and reachability state, and
storage reachability. Review it before posting publicly anyway.

If the app will not open or the action is unavailable, run `dirijor doctor`
when the CLI is on `PATH`. Include the Diri version, macOS version and chip, the
agent involved, reproduction steps, and the smallest useful log excerpt.

Logs under `~/Library/Application Support/Dirijor` may contain prompt text,
command output, repository paths, or secrets printed by a process. Redact them
before posting publicly.

## Status debug info

If an agent is shown as working, waiting, done, or unknown at the wrong time,
open **Session Inspector → Info → Why Diri thinks this**. The disclosure shows
the structured authority, manifest/rule id, timing guards, and fallback reason;
it never includes a screen capture or prompt. Use **Copy status debug info** for
a bounded snippet suitable for an issue.

User-provided manifest identifiers are validated at the copy boundary. Invalid
or path-like ids are omitted instead of being redacted after the fact.

Report security issues privately as described in [SECURITY.md](SECURITY.md),
not through Discussions or a public issue.
