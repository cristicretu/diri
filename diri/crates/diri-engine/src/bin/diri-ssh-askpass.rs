//! Native OpenSSH askpass broker shipped with Diri.
//!
//! OpenSSH owns authentication; this process only presents one prompt and
//! returns one response on stdout. It deliberately has no logging and no
//! connection to the Engine control protocol, so credentials cannot enter
//! session state or diagnostic payloads.

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn main() {
    eprintln!("diri-ssh-askpass is unavailable on this platform");
    std::process::exit(1);
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
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

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PromptKind {
    ConfirmHost,
    Secret,
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
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

#[cfg(target_os = "linux")]
fn present(kind: PromptKind, prompt: &str) -> Option<String> {
    if program_available("zenity") {
        present_with_zenity(kind, prompt)
    } else if program_available("kdialog") {
        present_with_kdialog(kind, prompt)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn program_available(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|directory| directory.join(name).is_file()))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn present_with_zenity(kind: PromptKind, prompt: &str) -> Option<String> {
    use std::process::Command;

    let mut command = Command::new("zenity");
    match kind {
        PromptKind::ConfirmHost => {
            command.args([
                "--question",
                "--title=Verify SSH host",
                "--ok-label=Allow",
                "--cancel-label=Cancel",
                "--text",
                prompt,
            ]);
        }
        PromptKind::Secret => {
            command.args(["--password", "--title=SSH authentication", "--text", prompt]);
        }
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    response_from_dialog(kind, output.stdout)
}

#[cfg(target_os = "linux")]
fn present_with_kdialog(kind: PromptKind, prompt: &str) -> Option<String> {
    use std::process::Command;

    let output = match kind {
        PromptKind::ConfirmHost => Command::new("kdialog")
            .args(["--title", "Verify SSH host", "--yesno", prompt])
            .output(),
        PromptKind::Secret => Command::new("kdialog")
            .args(["--title", "SSH authentication", "--password", prompt])
            .output(),
    }
    .ok()?;
    if !output.status.success() {
        return None;
    }
    response_from_dialog(kind, output.stdout)
}

#[cfg(target_os = "linux")]
fn response_from_dialog(kind: PromptKind, output: Vec<u8>) -> Option<String> {
    match kind {
        PromptKind::ConfirmHost => Some(String::from("yes")),
        PromptKind::Secret => String::from_utf8(output)
            .ok()
            .map(|secret| secret.trim_end_matches(['\r', '\n']).to_owned())
            .filter(|secret| !secret.is_empty()),
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos", windows)))]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn trims_dialog_newlines_without_trimming_the_secret_itself() {
        assert_eq!(
            response_from_dialog(PromptKind::Secret, b" pass phrase \n".to_vec()),
            Some(" pass phrase ".to_owned())
        );
        assert_eq!(
            response_from_dialog(PromptKind::ConfirmHost, Vec::new()),
            Some("yes".to_owned())
        );
    }
}

#[cfg(windows)]
fn present(kind: PromptKind, prompt: &str) -> Option<String> {
    use diri_platform::windows_sys::Win32::{Security::Credentials::*, UI::WindowsAndMessaging::*};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<_>>();
    if prompt.contains('\0') || prompt.len() > 16 * 1024 {
        return None;
    }
    let message = wide(prompt);
    let title = wide(if kind == PromptKind::ConfirmHost {
        "Diri · Verify SSH host"
    } else {
        "Diri · SSH authentication"
    });
    if kind == PromptKind::ConfirmHost {
        let selected = unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2 | MB_SETFOREGROUND,
            )
        };
        return (selected == IDYES).then(|| "yes".into());
    }
    let info = CREDUI_INFOW {
        cbSize: size_of::<CREDUI_INFOW>() as u32,
        hwndParent: std::ptr::null_mut(),
        pszMessageText: message.as_ptr(),
        pszCaptionText: title.as_ptr(),
        hbmBanner: std::ptr::null_mut(),
    };
    let target = wide("Diri OpenSSH");
    let mut username = [0u16; 514];
    username[..3].copy_from_slice(&[b'S' as u16, b'S' as u16, b'H' as u16]);
    let mut password = [0u16; 1024];
    let mut save = 0;
    let result = unsafe {
        CredUIPromptForCredentialsW(
            &info,
            target.as_ptr(),
            std::ptr::null(),
            0,
            username.as_mut_ptr(),
            username.len() as u32,
            password.as_mut_ptr(),
            password.len() as u32,
            &mut save,
            CREDUI_FLAGS_GENERIC_CREDENTIALS
                | CREDUI_FLAGS_ALWAYS_SHOW_UI
                | CREDUI_FLAGS_DO_NOT_PERSIST
                | CREDUI_FLAGS_KEEP_USERNAME,
        )
    };
    let response = if result == 0 {
        let length = password
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(password.len());
        String::from_utf16(&password[..length]).ok()
    } else {
        None
    };
    // The dialog never stores credentials. Clear its scratch buffer before
    // returning the one response directly to OpenSSH's dedicated stdout pipe.
    for unit in &mut password {
        unsafe {
            std::ptr::write_volatile(unit, 0);
        }
    }
    response
}
