#[derive(Clone, Debug)]
pub enum NativeNotificationEvent {
    Open {
        session_id: String,
        notification_id: String,
    },
    /// Text typed into a needs-input banner, for the banner's own session.
    Reply {
        session_id: String,
        notification_id: String,
        text: ReplyText,
    },
    Read(String),
    Health(String),
}

/// A reply is a prompt: keep it out of any `{:?}` that reaches a log.
#[derive(Clone)]
pub struct ReplyText(pub String);
impl std::fmt::Debug for ReplyText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ReplyText({} bytes)", self.0.len())
    }
}
