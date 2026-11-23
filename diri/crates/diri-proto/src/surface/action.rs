//! What a surface button does, and the one gate every click passes.
//!
//! An action is a request, never code. [`authorize`] turns it into an
//! [`Effect`] the app performs, or refuses it. The rules, which the Lean model
//! in `diri/formal/SurfaceModel.lean` mirrors:
//!
//! 1. Nothing runs without the person's gesture: a surface update can never
//!    click its own buttons.
//! 2. Links open only for allowed schemes ([`super::link::is_allowed`]).
//! 3. A prompt, app skill or event reaches only the surface's owner or an
//!    agent the surface lists in `agents`; with neither, it is refused.
//! 4. Every string an effect carries is bounded.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::limits;
use super::link::{self, LinkKind};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Open a link: the browser, Slack, Linear, or a `diri://` target.
    OpenUrl { url: String },
    /// Show a Diri session.
    OpenSession { session: String },
    /// Show a Diri note.
    OpenNote { note: String },
    /// Type `text` into an agent and submit it. `session` defaults to the
    /// surface's owner.
    SendPrompt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        text: String,
    },
    /// Ask an agent to run one of the person's saved App Skills.
    RunAppSkill {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
    },
    /// A named callback: recorded in the surface's event log and delivered
    /// to the owning agent.
    Event {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
    },
    /// Ask the owning agent to bring the surface up to date.
    Refresh,
}

impl Action {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::OpenUrl { .. } => "open_url",
            Self::OpenSession { .. } => "open_session",
            Self::OpenNote { .. } => "open_note",
            Self::SendPrompt { .. } => "send_prompt",
            Self::RunAppSkill { .. } => "run_app_skill",
            Self::Event { .. } => "event",
            Self::Refresh => "refresh",
        }
    }
}

/// Who started an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    /// The person clicked or pressed a key on the surface.
    User,
    /// Anything else: a patch, a binding, a timer.
    Programmatic,
}

/// Who may receive what a surface sends.
#[derive(Clone, Copy, Debug)]
pub struct Authority<'a> {
    pub owner: Option<&'a str>,
    pub agents: &'a [String],
}

impl Authority<'_> {
    /// Whether `session` may receive this surface's prompts and events.
    pub fn may_reach(&self, session: &str) -> bool {
        self.owner == Some(session) || self.agents.iter().any(|agent| agent == session)
    }

    fn target(&self, requested: Option<&str>) -> Result<String, Denied> {
        let target = requested.or(self.owner).ok_or(Denied::NoAgent)?;
        if self.may_reach(target) {
            Ok(target.to_owned())
        } else {
            Err(Denied::AgentNotAllowed(target.to_owned()))
        }
    }
}

/// What the app does for an authorized action.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    OpenExternal(String),
    OpenSession(String),
    OpenNote(String),
    OpenSurface(String),
    Prompt {
        session: String,
        text: String,
    },
    /// Record `name` in the event log and tell `session` about it.
    Deliver {
        session: String,
        name: String,
        payload: Option<Value>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Denied {
    NoGesture,
    UrlNotAllowed,
    NoAgent,
    AgentNotAllowed(String),
    TooLong,
    InvalidName,
}

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoGesture => f.write_str("surface actions run only when the person clicks"),
            Self::UrlNotAllowed => f.write_str("that link's scheme is not allowed"),
            Self::NoAgent => f.write_str("this surface has no agent to send to"),
            Self::AgentNotAllowed(session) => {
                write!(f, "session {session} is not one of this surface's agents")
            }
            Self::TooLong => f.write_str("the action's text is too long"),
            Self::InvalidName => f.write_str("the action names something invalid"),
        }
    }
}

/// The gate: what `action` may do, given who started it and who the
/// surface may reach.
pub fn authorize(
    action: &Action,
    gesture: Gesture,
    authority: Authority<'_>,
) -> Result<Effect, Denied> {
    if gesture != Gesture::User {
        return Err(Denied::NoGesture);
    }
    check_static(action)?;
    match action {
        Action::OpenUrl { url } => Ok(match link::classify(url) {
            LinkKind::DiriSession { session } => Effect::OpenSession(session),
            LinkKind::DiriNote { note } => Effect::OpenNote(note),
            LinkKind::DiriSurface { surface } => Effect::OpenSurface(surface),
            _ => Effect::OpenExternal(url.clone()),
        }),
        Action::OpenSession { session } => Ok(Effect::OpenSession(session.clone())),
        Action::OpenNote { note } => Ok(Effect::OpenNote(note.clone())),
        Action::SendPrompt { session, text } => Ok(Effect::Prompt {
            session: authority.target(session.as_deref())?,
            text: text.clone(),
        }),
        Action::RunAppSkill { name, session } => Ok(Effect::Prompt {
            session: authority.target(session.as_deref())?,
            text: format!(
                "Run my Diri App Skill \"{name}\" (app_skills tool), then update any Live Surface it affects."
            ),
        }),
        Action::Event { name, payload } => Ok(Effect::Deliver {
            session: authority.target(None)?,
            name: name.clone(),
            payload: payload.clone(),
        }),
        Action::Refresh => Ok(Effect::Deliver {
            session: authority.target(None)?,
            name: "refresh".into(),
            payload: None,
        }),
    }
}

/// The checks that need no context: run when a surface is written, so a bad
/// button is refused up front, and again at click time.
pub fn check_static(action: &Action) -> Result<(), Denied> {
    match action {
        Action::OpenUrl { url } => {
            if !link::is_allowed(url) {
                return Err(Denied::UrlNotAllowed);
            }
        }
        Action::OpenSession { session } => ref_ok(session)?,
        Action::OpenNote { note } => ref_ok(note)?,
        Action::SendPrompt { session, text } => {
            if let Some(session) = session {
                ref_ok(session)?;
            }
            if text.trim().is_empty() {
                return Err(Denied::InvalidName);
            }
            if text.len() > limits::MAX_PROMPT_BYTES {
                return Err(Denied::TooLong);
            }
        }
        Action::RunAppSkill { name, session } => {
            if let Some(session) = session {
                ref_ok(session)?;
            }
            if name.trim().is_empty() || name.len() > limits::MAX_LABEL_BYTES {
                return Err(Denied::InvalidName);
            }
            if name.contains(['"', '\n', '\r']) {
                return Err(Denied::InvalidName);
            }
        }
        Action::Event { name, payload } => {
            if !limits::is_valid_id(name) {
                return Err(Denied::InvalidName);
            }
            if let Some(payload) = payload
                && serde_json::to_string(payload)
                    .map_or(true, |s| s.len() > limits::MAX_PAYLOAD_BYTES)
            {
                return Err(Denied::TooLong);
            }
        }
        Action::Refresh => {}
    }
    Ok(())
}

fn ref_ok(id: &str) -> Result<(), Denied> {
    if limits::is_valid_ref(id) {
        Ok(())
    } else {
        Err(Denied::InvalidName)
    }
}
