# Support

[Discussions](https://github.com/cristicretu/diri/discussions) is for setup
questions and workflows. Use a
[bug report](https://github.com/cristicretu/diri/issues/new?template=bug_report.yml)
for something broken or a
[feature request](https://github.com/cristicretu/diri/issues/new?template=feature_request.yml)
for a concrete improvement. Report vulnerabilities [privately](SECURITY.md).

## Report a bug

Check for an existing issue and try the latest release when possible. Include:

- Steps to reproduce, expected behavior, and what actually happens.
- Diri version, OS version, and installation method.
- Agent CLI version and local or SSH host, if relevant.

On macOS, include your chip. For Linux rendering problems, include the display
server, desktop environment, GPU, and driver; see the
[Linux troubleshooting guide](diri/LINUX.md#troubleshooting-graphics-and-display-startup).

## Collect diagnostics

Open **Settings → General → Support → Copy diagnostics**, review the report,
and paste it into the issue. It includes app, platform, and Engine metadata;
agent availability; remote-host identifiers and reachability; and storage
reachability. It does not include raw logs.

If the app will not open, run `dirijor doctor` when the CLI is on `PATH`.
For incorrect session status, use **Session Inspector → Info → Why Diri thinks
this → Copy status debug info**. This gives the detection rule and timing
context without a screen capture or prompt.

Raw logs are an optional fallback:

| Platform | Default Engine log |
| :--- | :--- |
| macOS | `~/Library/Application Support/Dirijor/logs/dirijord.log` |
| Linux | `~/.local/state/diri/logs/dirijord.log` |

Linux paths follow `XDG_STATE_HOME` when set. Logs and screenshots can contain
prompts, command output, personal paths, and credentials. Share only relevant
excerpts and redact private content before posting.
