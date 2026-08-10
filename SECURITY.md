# Security policy

## Supported versions

Security fixes are made for the latest Diri release. Because the app can launch
processes with your user account's privileges, keeping Diri and the coding-agent
CLIs it launches current is important.

## Report a vulnerability

Please use [GitHub's private vulnerability reporting](https://github.com/cristicretu/diri/security/advisories/new).
Do not include an exploit, private terminal output, tokens, or personal paths in
a public issue.

Include the affected version and macOS version, a minimal reproduction, the
impact you believe is possible, and any suggested mitigation. You should receive
an acknowledgement within seven days. Timing for a fix or disclosure depends on
severity and complexity; the maintainer will coordinate that with the reporter.

For ordinary bugs, use the [bug report form](https://github.com/cristicretu/diri/issues/new?template=bug_report.yml).

## Scope and trust model

Diri intentionally runs local shells, coding agents, MCP tools, and optional
remote-node commands. A tool doing something the user explicitly authorized is
not itself a Diri vulnerability. Permission-boundary bypasses, unsafe update or
IPC behavior, credential disclosure, session isolation failures, and unintended
remote execution are in scope.

See the [security model](docs/SECURITY-MODEL.md) for the boundaries Diri does
and does not provide.
