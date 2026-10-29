//! "fix-auth: Zoom pane". When an Agent changes what the window shows
//! through Diri's tools, a small capsule at the bottom of the work surface
//! names the Agent and the action. A burst of actions updates the one capsule
//! in place. The person's own input never carries an actor, so it is never
//! announced back to them.

use diri_proto::SessionRecord;
use diri_proto::agent_action::AgentAction;

use crate::i18n::{t, tf};
use crate::toast::Toast;

/// The notice for `actor` doing `action`.
pub fn notice(actor: &SessionRecord, action: AgentAction) -> Toast {
    message(actor, label(action))
}

/// The notice for `actor` running one of the app's exported actions: the
/// action's own title, as the menu and palette show it ("fix-auth: Toggle
/// Sidebar").
pub fn app_action_notice(actor: &SessionRecord, title: &str) -> Toast {
    message(actor, title)
}

fn message(actor: &SessionRecord, action: &str) -> Toast {
    let agent = crate::switcher::display_title_str(actor);
    Toast::ambient(tf(
        "agent_notice.message",
        &[("agent", &agent), ("action", &action)],
    ))
}

fn label(action: AgentAction) -> &'static str {
    t(match action {
        AgentAction::NewTab => "agent_notice.new_tab",
        AgentAction::NewNote => "agent_notice.new_note",
        AgentAction::CloseTab => "agent_notice.close_tab",
        AgentAction::ShowTab => "agent_notice.show_tab",
        AgentAction::RenameTab => "agent_notice.rename_tab",
        AgentAction::MoveTab => "agent_notice.move_tab",
        AgentAction::SplitPane => "agent_notice.split_pane",
        AgentAction::MovePane => "agent_notice.move_pane",
        AgentAction::ClosePane => "agent_notice.close_pane",
        AgentAction::FocusPane => "agent_notice.focus_pane",
        AgentAction::ZoomPane => "agent_notice.zoom_pane",
        AgentAction::UnzoomPane => "agent_notice.unzoom_pane",
        AgentAction::EqualizePanes => "agent_notice.equalize_panes",
        AgentAction::SwapPanes => "agent_notice.swap_panes",
        AgentAction::ResizePane => "agent_notice.resize_pane",
        AgentAction::ChangeWorkspaces => "agent_notice.change_workspaces",
        AgentAction::OpenApiRequest => "agent_notice.open_api_request",
        AgentAction::StopAgent => "agent_notice.stop_agent",
        AgentAction::SleepAgent => "agent_notice.sleep_agent",
        AgentAction::WakeAgent => "agent_notice.wake_agent",
        AgentAction::ResumeAgent => "agent_notice.resume_agent",
        AgentAction::AppAction => "agent_notice.app_action",
    })
}
