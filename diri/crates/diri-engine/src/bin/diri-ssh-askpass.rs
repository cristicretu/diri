//! Native OpenSSH askpass broker shipped with Diri.
//!
//! OpenSSH owns authentication; this process only presents one prompt and
//! returns one response on stdout. It deliberately has no logging and no
//! connection to the Engine control protocol, so credentials cannot enter
//! session state or diagnostic payloads.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("diri-ssh-askpass is only available on macOS");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    use std::io::Write as _;

    let prompt = std::env::args().nth(1).unwrap_or_default();
    let prompt_kind =
        PromptKind::classify(std::env::var("SSH_ASKPASS_PROMPT").ok().as_deref(), &prompt);
    let Some(response) = present(prompt_kind, &prompt) else {
        std::process::exit(1);
    };

    let mut stdout = std::io::stdout().lock();
    if writeln!(stdout, "{response}")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptKind {
    ConfirmHost,
    Secret,
}

#[cfg(target_os = "macos")]
impl PromptKind {
    fn classify(kind: Option<&str>, prompt: &str) -> Self {
        if kind == Some("confirm")
            || prompt
                .to_ascii_lowercase()
                .contains("are you sure you want to continue connecting")
        {
            Self::ConfirmHost
        } else {
            Self::Secret
        }
    }
}

#[cfg(target_os = "macos")]
fn present(kind: PromptKind, prompt: &str) -> Option<String> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
        NSSecureTextField,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    let mtm = MainThreadMarker::new()?;
    let application = NSApplication::sharedApplication(mtm);
    application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    application.activate();

    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(match kind {
        PromptKind::ConfirmHost => "Verify SSH host",
        PromptKind::Secret => "SSH authentication",
    }));
    alert.setInformativeText(&NSString::from_str(prompt));
    alert.addButtonWithTitle(&NSString::from_str(match kind {
        PromptKind::ConfirmHost => "Allow",
        PromptKind::Secret => "Connect",
    }));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    let secret = (kind == PromptKind::Secret).then(|| {
        let field = NSSecureTextField::initWithFrame(
            mtm.alloc(),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(360.0, 24.0)),
        );
        field.setPlaceholderString(Some(&NSString::from_str("Password or key passphrase")));
        alert.setAccessoryView(Some(&field));
        field
    });

    if alert.runModal() != NSAlertFirstButtonReturn {
        return None;
    }
    match secret {
        Some(field) => Some(field.stringValue().to_string()),
        None => Some(String::from("yes")),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn classifies_confirmation_without_localization_sensitive_password_rules() {
        assert_eq!(
            PromptKind::classify(Some("confirm"), "translated prompt"),
            PromptKind::ConfirmHost
        );
        assert_eq!(
            PromptKind::classify(
                None,
                "Are you sure you want to continue connecting (yes/no/[fingerprint])?"
            ),
            PromptKind::ConfirmHost
        );
        assert_eq!(
            PromptKind::classify(None, "Enter passphrase for key '/tmp/id_ed25519':"),
            PromptKind::Secret
        );
    }
}
