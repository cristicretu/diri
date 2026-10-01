const DEV_BUNDLE_PREFIX: &str = "com.dirijor.diri.dev.";
#[cfg(windows)]
const WINDOWS_BUNDLE_ID_FILE: &str = "diri-dev-bundle-id";

/// Windows executables carry no bundle metadata. `scripts/dev.ps1` writes the
/// id beside its copy of `diri.exe`; an installed package never contains it.
#[cfg(windows)]
pub(crate) fn windows_bundle_id() -> Option<String> {
    let executable = std::env::current_exe().ok()?;
    parse_bundle_id(&std::fs::read_to_string(executable.with_file_name(WINDOWS_BUNDLE_ID_FILE)).ok()?)
}

#[cfg(any(windows, test))]
fn parse_bundle_id(contents: &str) -> Option<String> {
    let id = contents.trim();
    (!id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '-')))
    .then(|| id.to_owned())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DevBuildIdentity {
    bundle_id: String,
    label: String,
}

impl DevBuildIdentity {
    pub(crate) fn from_process_environment(bundle_id: Option<&str>) -> Option<Self> {
        Self::from_parts(
            std::env::var("DIRI_DEV").ok().as_deref(),
            bundle_id,
            std::env::var("DIRI_DEV_BUILD").ok().as_deref(),
        )
    }

    fn from_parts(
        enabled: Option<&str>,
        bundle_id: Option<&str>,
        label: Option<&str>,
    ) -> Option<Self> {
        // The environment opt-in keeps release-profile development useful, and
        // the bundle-id check makes the same variable inert for a shipped app.
        if enabled != Some("1") || !bundle_id.is_some_and(|id| id.starts_with(DEV_BUNDLE_PREFIX)) {
            return None;
        }

        let bundle_id = bundle_id?.to_owned();
        let label = label
            .map(sanitized_label)
            .filter(|label| !label.is_empty())?;
        Some(Self { bundle_id, label })
    }

    pub(crate) fn bundle_id(&self) -> &str {
        &self.bundle_id
    }

    pub(crate) fn window_title(&self) -> String {
        format!("diri dev — {}", self.label)
    }

    pub(crate) fn marker_label(&self) -> &str {
        &self.label
    }
}

fn sanitized_label(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(48)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_dev_bundle_enables_identity() {
        let identity = DevBuildIdentity::from_parts(
            Some("1"),
            Some("com.dirijor.diri.dev.abc1234"),
            Some("worktree@abc1234"),
        )
        .expect("dev bundle should identify itself");

        assert_eq!(identity.bundle_id(), "com.dirijor.diri.dev.abc1234");
        assert_eq!(identity.window_title(), "diri dev — worktree@abc1234");
        assert_eq!(identity.marker_label(), "worktree@abc1234");
    }

    #[test]
    fn release_bundle_cannot_show_dev_identity() {
        assert!(
            DevBuildIdentity::from_parts(
                Some("1"),
                Some("com.dirijor.diri"),
                Some("main@abc1234"),
            )
            .is_none()
        );
    }

    #[test]
    fn opt_in_and_build_label_are_both_required() {
        let bundle = Some("com.dirijor.diri.dev.abc1234");
        assert!(DevBuildIdentity::from_parts(None, bundle, Some("main@abc1234")).is_none());
        assert!(DevBuildIdentity::from_parts(Some("0"), bundle, Some("main@abc1234")).is_none());
        assert!(DevBuildIdentity::from_parts(Some("1"), bundle, None).is_none());
    }

    #[test]
    fn windows_bundle_id_file_accepts_only_an_identifier() {
        assert_eq!(
            parse_bundle_id("com.dirijor.diri.dev.abc1234\r\n").as_deref(),
            Some("com.dirijor.diri.dev.abc1234")
        );
        for rejected in ["", "  ", "com.dirijor.diri.dev.a b", "com\\dirijor", "id\0x"] {
            assert_eq!(parse_bundle_id(rejected), None, "{rejected:?}");
        }
        assert_eq!(parse_bundle_id(&"a".repeat(129)), None);
    }

    #[test]
    fn label_is_safe_for_window_chrome() {
        let identity = DevBuildIdentity::from_parts(
            Some("1"),
            Some("com.dirijor.diri.dev.abc1234"),
            Some("  feature/dev\nspoofed  "),
        )
        .expect("valid dev identity");

        assert_eq!(identity.marker_label(), "feature/devspoofed");
    }
}
