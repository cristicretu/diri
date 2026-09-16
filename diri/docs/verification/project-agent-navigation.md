# Project and agent navigation

Projects remain the sidebar's first level. Agent rows retain their conversation
names, provider icons, status, children, archive controls and New Agent action.
Selecting a project layout no longer replaces these rows with a second tab list.
Horizontal and vertical presentation share the same saved layout and processes.

Opening an agent asks the Engine to open its project layout automatically. The
Engine uses its authoritative ProjectId, not a display name or guessed path.
Only the requested agent gets a new tab when it has no placement. Existing
splits, user titles and pane identities survive; tabs deliberately closed by the
user are not all reopened on every selection. A current mixed-project layout
keeps its existing pane when that pane already contains the requested agent.
An unbound legacy layout can acquire a project binding only when it is the sole
unambiguous candidate and all its references resolve to that project.

The optional project binding is additive to workspace schema 1. Old layouts
remain readable. The operation validates and durably commits under the existing
workspace revision lock; it does not launch, stop, attach to or mutate an Agent
process. The UI does not replay a failed revision-gated edit. If layout storage
is unavailable, the agent remains accessible in the normal terminal view.

## Verification commands

From `diri/`:

```sh
cargo test -p diri-engine workspace
cargo test -p diri-app project_agents
DIRI_PROJECT_AGENT_SCREENSHOTS=/tmp/diri-project-agents cargo test -p diri-app --bin diri project_agents_remain_visible_and_open_preserved_layouts -- --ignored --nocapture --test-threads=1
```

The opt-in native test uses disposable local shell PTYs and a real Engine
Control socket. It checks painted agent rows while a split layout is active,
clicks the second agent through GPUI pointer dispatch, verifies preserved tab,
pane and process identities, switches orientation, and reopens the project
through agent navigation. Screenshots show native rendering of fixture data.
This is not a physical trackpad or remote-host acceptance test.
