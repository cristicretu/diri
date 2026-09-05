# Reusable recipes and scheduled tasks

## Implemented in this branch

An empty new-session composer exposes the first three saved recipes as Run
buttons (also Command-1/2/3). The library's existing ordering controls determine
these shortcuts; Recipes contains the complete library and editing controls.
With a prompt entered, Save recipe / Command-S stores it directly. An edited
saved task offers Update recipe. Saving works while agent discovery or a host
is unavailable; launching still checks the saved destination and agent.

Each run creates a new session with the saved prompt, agent kind, project/host,
session title, and worktree policy. Fresh-worktree recipes allocate a new branch
on each run. They do not resume or depend on the session used for a previous run.
Project identities follow the tracked project's current root; unavailable or
moved destinations require repair rather than silently launching elsewhere.

Prompts preserve whitespace and final instructions. Over-limit prompts are
rejected with an actionable error rather than silently truncated. Recipes store
launch settings, not credentials, an agent's conversation, or a frozen copy of
the agent's own model/configuration files. Live agent configuration still applies.

## Scheduling proposal — not implemented

Scheduling belongs to the Rust Engine. A recipe describes what to run; a
schedule describes when to create an independent run. An agent should never
have to remain open to act as its own scheduler.

Start with local schedules managed through the existing Engine. This proposal
does not change `REMOTE_PORT.md`, install cron entries or LaunchAgents, or put a
scheduler in a Remote Helper. Independent execution while the Mac is asleep
requires a separate, explicitly designed always-on Engine deployment; SSH to a
Holder alone cannot provide that.

### User experience

Recipe details gain an optional Schedule action. Offer daily, weekdays, weekly,
and a five-field cron expression as an advanced option. Require an explicit
IANA time zone and show the next three occurrences before enabling. The default
missed-run policy is “Run once when available”; offer “Skip missed runs.”

Show next run, execution host, last outcome, and an enabled/paused control.
Run history links to the actual session and retained result. Useful outcomes
include Starting, Running, Needs attention, Succeeded, Failed, Timed out,
Skipped (overlap), Missed, and Outcome unknown. Saving or editing a recipe must
never implicitly enable a schedule or broaden its permissions.

### Ownership and persistence

Move the reusable recipe model and validation into `diri-proto`/`diri-engine`
before adding scheduling; today they live in `diri-app` preferences. Add Engine
CRUD and run APIs used by both the UI and scheduler. Import existing recipes
idempotently, preserve stable IDs and unknown versions, and keep a migration
receipt so retrying an import cannot overwrite a later Engine edit.

Persist versioned schedules and run records under owner-only Engine state.
A schedule contains its ID, recipe ID/revision, time expression, time zone,
enabled flag, next occurrence, missed-run policy, overlap policy and deadline.
A run contains schedule ID, scheduled UTC instant, immutable recipe revision,
session ID/incarnation, timestamps and outcome. Do not put prompts or auth data
in logs; the protected recipe snapshot may contain the user's prompt.

Use the existing single Engine owner to claim occurrences and a uniqueness
constraint on `(schedule_id, scheduled_at_utc)`. Commit the run intent and a
reserved session identity durably before spawning. Add idempotent Engine spawn
semantics for that identity; never implement a second launch path in the UI.
Reconcile run records against registry/Holder facts after an Engine restart.
If spawn or initial-prompt delivery may have happened, show Outcome unknown
until reconciled; never blindly retry a task with possibly completed effects.
This is duplicate prevention, not an exactly-once guarantee for external actions.

Use one deadline-driven scheduler, asleep when no schedules are enabled. Wake
for the nearest occurrence, schedule changes, system wake, and clock changes.
Persist occurrence progress, recompute against wall time after wake/restart,
and never replay an already claimed UTC occurrence after a clock rollback.
An enabled schedule must count as work for Engine idle shutdown. Explicit
Engine shutdown remains explicit: catch up only when the Engine next starts.

### Edge cases and proposed defaults

| Situation | Behavior |
| --- | --- |
| Previous Claude/Codex session closed or deleted | Start a new session from the recipe at the next occurrence. |
| Diri window closed, Engine still running | Scheduler continues. UI attachment is unnecessary. |
| Engine stopped, user logged out, or machine rebooted | No runs until the Engine starts; reconcile persisted occurrences then. No promise of automatic startup. |
| Lid closed / Mac asleep at the due time | Nothing executes on the sleeping Mac. On wake, coalesce missed occurrences into at most one run per schedule, or mark them missed under the skip policy. |
| Many schedules become due on wake | Bound simultaneous launches and process the backlog in due-time order. Do not spawn a burst for every missed interval. |
| A previous run is still running or awaiting approval | Skip that occurrence as overlap; never launch a second copy by default. Do not count a disconnected session as finished. |
| Agent login expired, SSH requires interaction, or tool approval is needed | Record Needs attention and stop unattended retries. Retain normal permissions; never auto-answer or enable bypass flags. |
| Network/host unavailable before any launch effect | Bounded backoff within the run deadline, retaining the same run identity. After expiry, record failure. |
| Network drops after a remote launch | Reconcile the same session incarnation. Never spawn a replacement merely because attachment failed. |
| Remote machine reboots or Holder dies | Mark the run interrupted/failed from verified facts; session survival is not promised across reboot. |
| Folder/host/agent missing or moved | Fail with a specific repair reason. Never fall back to another folder, host, agent, or bare shell. |
| Schedule edited/paused while a run is active | Existing run keeps its immutable snapshot. Changes affect future occurrences; pause prevents new claims. |
| Recipe deleted | Reject deletion until dependent schedules are removed or disabled, with an explicit UI choice. |
| User stops an active run | Record Cancelled, release its overlap slot after confirmed process cleanup, and retain the schedule. |
| DST spring-forward or fall-back | Skip nonexistent wall-clock times; choose the first occurrence of an ambiguous time. Preview these choices and test with pinned zone data. |
| Time zone or clock changes | A schedule stays in its saved zone. Recompute next due instant and apply the same missed-run/uniqueness rules. |

### Finite agent runs

The scheduler must distinguish “waiting for another prompt” from “process
finished.” Terminal silence, lack of an attachment, and generic idle status do
not prove task success. Add an explicit unattended capability to the canonical
agent manifests, using structured argv for an agent's supported finite execution
mode and explicit result/exit semantics. Unsupported agents remain manual-only.
Verify Claude/Codex execution modes and authentication behavior against their
current CLIs when implementing this; do not infer completion from screen text.

Every run needs a configurable duration limit and bounded output/result
retention. A stuck run or approval prompt must not hold an overlap slot forever.
On timeout, terminate through the Engine's ordinary lifecycle path and confirm
cleanup. Preserve a result summary and session reference for review. Separate
process success from task-level success when the agent exposes structured results.

### Running while the Mac is asleep

Apple documents that `launchd` calendar jobs missed during sleep execute on
wake, while cron skips sleeping-time invocations. Neither makes the local agent
execute while the machine is asleep. See Apple's
[Scheduling Timed Jobs](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/ScheduledJobs.html)
and [launchd calendar semantics](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5).

The current repository disallows persistent user services/LaunchAgents. Keep
the first scheduler inside the existing Engine and state its availability
honestly. For guaranteed availability independent of this Mac, propose an
explicitly configured always-on Rust Engine with durable schedule ownership,
remote project and credential provisioning, and single-owner transfer fencing.
That requires a separate architecture decision; it must not expand the minimal
Remote Helper or reintroduce a remote supervisor into the current bootstrap path.

### Acceptance gates for scheduler implementation

Use a fake clock, fixture state and fake/spawned agents, without real SSH hosts.
Cover restart between every claim/spawn/prompt-ack/result persistence boundary;
repeated wake events; days of downtime; clock rollback and DST; edit/delete/pause
races; concurrent run requests; slow agents; missing credentials; hung approval;
overlap; limits; cleanup; remote disconnect with unchanged incarnation; and
failure to persist state. Verify that an absent UI and a closed previous session
do not prevent a new run, and that unknown outcomes never trigger a duplicate.
Measure idle wakeups and wake-backlog latency before shipping the scheduler.
