# Run a command in a Diri terminal

`dirijor session run` launches a program as a new Engine-owned terminal session.
The desktop discovers it through the ordinary session catalog. The command's
output and exit status remain available after it finishes, until explicit removal.

```sh
dirijor session run --title 'Build' --cwd /absolute/project --json -- /usr/bin/make test
# Use the returned id in subsequent commands:
dirijor session wait SESSION_ID --until exited --json
dirijor session read SESSION_ID
dirijor session get SESSION_ID --json
dirijor session release SESSION_ID --remove
```

All CLI options precede `--`. Every element after it is passed literally as an
argument, including spaces, empty strings, Unicode, and values starting with `-`.
Diri does not join these arguments into a shell command. To request shell syntax,
explicitly run a shell, for example `-- /bin/sh -c 'make && make test'`.

The local working directory defaults to the CLI's current directory. `--cwd`
must be absolute. For remote work, supply both a configured host ID and a path
on that host; Diri never substitutes the local working directory for a missing
remote path:

```sh
dirijor session run --host development --cwd /srv/project --title 'Remote build' -- /usr/bin/make test
```

Remote commands use the Engine's existing verified SSH/Helper launch path and
the remote account's environment. The CLI does not open a new controller or
implement SSH itself. Remote bootstrap failures remain structured Engine errors.
This CLI change does not extend host support or persistence guarantees.

`session wait --until exited` waits for process exit, including nonzero exits.
It uses the existing Engine event subscription, with a current-status check
that handles commands which already finished. `--json` includes the structured
`session.status.exited._0` exit facts; inspect its `code`/`signal` fields to decide
whether the command succeeded. Successful waiting returns CLI status 0, even
when the child exited nonzero. A timeout returns CLI status 2. The default wait
condition `done` means Agent idle, so finite commands should specify `exited`.
No screen scraping is used to determine completion.

Limits: argv contains 1–512 NUL-free strings, with a nonempty executable and at
most 1 MiB of argument bytes. The remote launch budget also includes environment
and cwd; the operating system can impose lower execution limits. Malformed
explicit argv fails with `bad_request` before any launch/worktree/bootstrap work.
Missing argv retains the pre-existing Agent-manifest launch behavior.

## Verification

`cargo test -p dirijor-mcp --test session_run` uses a private Engine/socket and
real local PTY to check literal arguments, finite exit code 7, retained final
output, wait before/after completion, explicit removal, invalid argv and a remote
launch rejection. Unit tests cover option boundaries and argument limits.
A successful remote run remains covered by the existing remote Engine/Helper
fixtures; this test does not claim a new real-host end-to-end measurement.
