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

## Navigation controls

The horizontal header has one anchored Projects picker. Selecting a project
returns to its agent or opens New Agent at that project's exact location.
The picker does not reveal or resize the sidebar. Missing remote hosts retain
their location and display an unavailable state instead of offering local launches.

Horizontal tabs use the complete active-agent list for the selected project,
including agents without saved layout placements. Their icons use the same
provider marks as the sidebar. Navigation, rename, reorder and close shortcuts
operate on these displayed agents; closing retains session-close confirmation.
The compact Projects icon keeps the accessible name and existing dropdown.

The top-left saved terminal pane reserves the native window-button lane when
navigation is hidden. A single pane has no focus stripe; visible split panes
use a subtle outline to identify keyboard focus.

Cmd+B toggles the sidebar in vertical mode and the top tab strip in horizontal
mode. Horizontal visibility is persisted independently and defaults to visible
for existing preferences. The terminal header uses a matching toolbar icon.
The top strip opens and closes on the existing panel easing; reduced motion
skips the transition. Terminal viewport sizing follows settled visibility,
not every animation frame.

The native fixture also exercises Cmd+B with live split terminals, retaining
the selected agent, layout and process identities. Screenshots in
`project-agent-navigation/` contain disposable fixture data. Interactive review
of the actual preview covered the restored agent sidebar, project picker,
toolbar icon and opening/closing the top bar with both Cmd+B and the icon.
