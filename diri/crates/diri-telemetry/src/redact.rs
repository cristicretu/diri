//! Scrubbing for the few free-form strings telemetry accepts.

use std::sync::OnceLock;

const TEXT_MAX_CHARS: usize = 400;
const TOKEN_MIN_LEN: usize = 24;

struct Identity {
    home: Option<String>,
    user: Option<String>,
}

fn identity() -> &'static Identity {
    static IDENTITY: OnceLock<Identity> = OnceLock::new();
    IDENTITY.get_or_init(|| Identity {
        home: diri_platform::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .ok_or(std::env::VarError::NotPresent)
            .ok()
            .filter(|home| home.len() > 1),
        user: crate::identity::login_name().filter(|user| user.len() >= 3),
    })
}

/// Replaces the home directory with `~`, the login name with `<user>`,
/// e-mail addresses with `<email>`, and long token-like runs with
/// `<redacted>`; strips control characters; truncates to 400 characters.
#[must_use]
pub fn scrub(input: &str) -> String {
    let identity = identity();
    scrub_with(input, identity.home.as_deref(), identity.user.as_deref())
}

fn scrub_with(input: &str, home: Option<&str>, user: Option<&str>) -> String {
    let mut text: String = input
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if let Some(home) = home {
        text = text.replace(home, "~");
    }
    if let Some(user) = user {
        text = text.replace(user, "<user>");
    }
    let mut out = String::with_capacity(text.len());
    for (index, word) in text.split(' ').enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&scrub_word(word));
    }
    if out.chars().count() > TEXT_MAX_CHARS {
        out = out.chars().take(TEXT_MAX_CHARS).collect();
        out.push('…');
    }
    out
}

fn scrub_word(word: &str) -> std::borrow::Cow<'_, str> {
    let core = word.trim_matches(|c: char| "()[]{},;:\"'".contains(c));
    if let Some(at) = core.find('@') {
        let (local, domain) = core.split_at(at);
        if !local.is_empty() && domain.len() > 3 && domain.contains('.') {
            return word.replace(core, "<email>").into();
        }
    }
    // Secrets are long runs of mixed letters and digits: API keys, bearer
    // tokens, base64 blobs. Hex digests and UUIDs look the same and are
    // redacted too; identifiers that matter belong in `Id` fields instead.
    let run = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let token_like = run.len() >= TOKEN_MIN_LEN
        && run
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-'))
        && run.bytes().any(|b| b.is_ascii_digit())
        && run.bytes().any(|b| b.is_ascii_alphabetic());
    if token_like {
        return word.replace(run, "<redacted>").into();
    }
    word.into()
}

#[cfg(test)]
mod tests {
    use super::scrub_with;

    #[test]
    fn scrubs_home_user_email_and_tokens() {
        let out = scrub_with(
            "open /Users/alex/fun/app failed for alex (alex@example.com) key sk-ant-api03-AbCdEf1234567890XyZ\n",
            Some("/Users/alex"),
            Some("alex"),
        );
        assert_eq!(
            out,
            "open ~/fun/app failed for <user> (<email>) key <redacted> "
        );
    }

    #[test]
    fn keeps_ordinary_error_text() {
        let out = scrub_with("No such file or directory (os error 2)", None, None);
        assert_eq!(out, "No such file or directory (os error 2)");
    }

    #[test]
    fn truncates_long_text() {
        let out = scrub_with(&"ab ".repeat(500), None, None);
        assert_eq!(out.chars().count(), 401);
    }
}
