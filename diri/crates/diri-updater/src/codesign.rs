//! The trust anchor: Developer ID + notarization, pinned to the running app.
//!
//! Sparkle's model (a project-owned EdDSA keypair signing the appcast) is not
//! reused here, because of the failure mode it carries: lose the private key
//! and every install is stranded until people redownload by hand. diri leans
//! on the signature it has to produce anyway to ship at
//! all, and checks three things about a downloaded bundle:
//!
//! 1. `codesign --verify --deep --strict` — the bundle is intact and sealed.
//! 2. Its Team ID and bundle identifier equal the *running* app's. Pinning to
//!    ourselves rather than to a hardcoded constant means a rotated team or a
//!    renamed bundle can never silently start accepting someone else's code,
//!    and an ad-hoc-signed dev build (no Team ID) simply refuses to update.
//! 3. `spctl --assess --type execute` — Gatekeeper accepts it, which for a
//!    stapled bundle proves notarization without a network round trip.
//!
//! Downgrade attacks are handled a layer up, in `Feed::newest_eligible`, which
//! only ever offers a strictly-newer version.

use std::path::Path;
#[cfg(unix)]
use std::process::Command;

use crate::error::{Result, UpdateError};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SignatureInfo {
    pub identifier: Option<String>,
    pub team_identifier: Option<String>,
    pub authorities: Vec<String>,
}

impl SignatureInfo {
    /// True when the bundle carries a real Developer ID signature rather than
    /// an ad-hoc one. `codesign` prints `TeamIdentifier=not set` for ad-hoc.
    pub fn is_developer_id(&self) -> bool {
        self.team_identifier.is_some()
            && self
                .authorities
                .iter()
                .any(|authority| authority.starts_with("Developer ID Application:"))
    }

    /// Parses the `key=value` lines `codesign -dv --verbose=4` writes to stderr.
    pub fn parse(output: &str) -> Self {
        let mut info = Self::default();
        for line in output.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "Identifier" => info.identifier = Some(value.to_owned()),
                // Ad-hoc signatures report the literal string "not set".
                "TeamIdentifier" if value != "not set" && !value.is_empty() => {
                    info.team_identifier = Some(value.to_owned());
                }
                "Authority" => info.authorities.push(value.to_owned()),
                _ => {}
            }
        }
        info
    }
}

#[cfg(not(windows))]
pub fn signature_of(bundle: &Path) -> Result<SignatureInfo> {
    let output = Command::new("/usr/bin/codesign")
        .arg("-dv")
        .arg("--verbose=4")
        .arg(bundle)
        .output()?;
    if !output.status.success() {
        return Err(UpdateError::Signature(format!(
            "{} is not signed: {}",
            bundle.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    // codesign writes its report to stderr, not stdout.
    Ok(SignatureInfo::parse(&String::from_utf8_lossy(
        &output.stderr,
    )))
}

/// Confirms `candidate` is a notarized build signed by the same team as
/// `installed`, and refuses it otherwise.
#[cfg(not(windows))]
pub fn verify_matches_installed(candidate: &Path, installed: &SignatureInfo) -> Result<()> {
    let Some(expected_team) = installed.team_identifier.as_deref() else {
        return Err(UpdateError::NotUpdatable(
            "the running build is not signed with a Developer ID".to_owned(),
        ));
    };

    let verify = Command::new("/usr/bin/codesign")
        .arg("--verify")
        .arg("--deep")
        .arg("--strict")
        .arg(candidate)
        .output()?;
    if !verify.status.success() {
        return Err(UpdateError::Signature(
            String::from_utf8_lossy(&verify.stderr).trim().to_owned(),
        ));
    }

    let found = signature_of(candidate)?;
    check_identity(&found, expected_team, installed.identifier.as_deref())?;
    assess_with_gatekeeper(candidate)
}

/// The pure half of the check, split out so the pinning rules are testable
/// without a signed bundle on disk.
pub fn check_identity(
    found: &SignatureInfo,
    expected_team: &str,
    expected_identifier: Option<&str>,
) -> Result<()> {
    match found.team_identifier.as_deref() {
        Some(team) if team == expected_team => {}
        Some(team) => {
            return Err(UpdateError::Signature(format!(
                "signed by team {team}, expected {expected_team}"
            )));
        }
        None => {
            return Err(UpdateError::Signature(
                "the download is ad-hoc signed, not Developer ID".to_owned(),
            ));
        }
    }
    if let Some(expected) = expected_identifier
        && found.identifier.as_deref() != Some(expected)
    {
        return Err(UpdateError::Signature(format!(
            "bundle identifier {:?}, expected {expected:?}",
            found.identifier.as_deref().unwrap_or("<none>")
        )));
    }
    if !found.is_developer_id() {
        return Err(UpdateError::Signature(
            "no Developer ID Application authority in the signature".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn assess_with_gatekeeper(candidate: &Path) -> Result<()> {
    let output = Command::new("/usr/sbin/spctl")
        .arg("--assess")
        .arg("--type")
        .arg("execute")
        .arg("-vv")
        .arg(candidate)
        .output()?;
    if !output.status.success() {
        return Err(UpdateError::Signature(format!(
            "Gatekeeper rejected the download: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(windows)]
pub fn signature_of(bundle: &Path) -> Result<SignatureInfo> {
    crate::windows::signature_of(bundle)
}
#[cfg(windows)]
pub fn verify_matches_installed(candidate: &Path, installed: &SignatureInfo) -> Result<()> {
    crate::windows::verify(candidate, installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim shape of `codesign -dv --verbose=4` stderr for a notarized app.
    const DEVELOPER_ID: &str = r#"
Executable=/Applications/diri.app/Contents/MacOS/diri
Identifier=com.dirijor.diri
Format=app bundle with Mach-O universal (x86_64 arm64)
CodeDirectory v=20500 size=1234 flags=0x10000(runtime) hashes=30+7
Signature size=9012
Authority=Developer ID Application: Cristian Cretu (AH8WARWU6L)
Authority=Developer ID Certification Authority
Authority=Apple Root CA
Timestamp=26 Jul 2026 at 11:04:22
TeamIdentifier=AH8WARWU6L
Runtime Version=15.0.0
"#;

    const AD_HOC: &str = r#"
Executable=/Users/giga/fun/dirijor/diri/dist/diri.app/Contents/MacOS/diri
Identifier=com.dirijor.diri
Format=app bundle with Mach-O universal (x86_64 arm64)
Signature=adhoc
TeamIdentifier=not set
"#;

    #[test]
    fn parses_a_developer_id_signature() {
        let info = SignatureInfo::parse(DEVELOPER_ID);
        assert_eq!(info.identifier.as_deref(), Some("com.dirijor.diri"));
        assert_eq!(info.team_identifier.as_deref(), Some("AH8WARWU6L"));
        assert_eq!(info.authorities.len(), 3);
        assert!(info.is_developer_id());
    }

    #[test]
    fn treats_an_ad_hoc_signature_as_having_no_team() {
        let info = SignatureInfo::parse(AD_HOC);
        assert!(info.team_identifier.is_none());
        assert!(!info.is_developer_id());
    }

    #[test]
    fn accepts_a_download_from_the_same_team_and_identifier() {
        let found = SignatureInfo::parse(DEVELOPER_ID);
        assert!(check_identity(&found, "AH8WARWU6L", Some("com.dirijor.diri")).is_ok());
    }

    #[test]
    fn rejects_a_notarized_app_from_a_different_developer() {
        let other = SignatureInfo {
            identifier: Some("com.dirijor.diri".to_owned()),
            team_identifier: Some("ZZ9PLURAL".to_owned()),
            authorities: vec!["Developer ID Application: Someone Else (ZZ9PLURAL)".to_owned()],
        };
        let error = check_identity(&other, "AH8WARWU6L", Some("com.dirijor.diri"))
            .expect_err("a foreign team must be refused");
        assert!(matches!(error, UpdateError::Signature(_)));
    }

    #[test]
    fn rejects_a_different_app_from_the_same_developer() {
        let sibling = SignatureInfo {
            identifier: Some("com.dirijor.Dirijor".to_owned()),
            ..SignatureInfo::parse(DEVELOPER_ID)
        };
        assert!(check_identity(&sibling, "AH8WARWU6L", Some("com.dirijor.diri")).is_err());
    }

    #[test]
    fn rejects_an_ad_hoc_download_even_with_the_right_identifier() {
        let found = SignatureInfo::parse(AD_HOC);
        assert!(check_identity(&found, "AH8WARWU6L", Some("com.dirijor.diri")).is_err());
    }

    #[test]
    fn rejects_a_signature_with_no_developer_id_authority() {
        let apple_development = SignatureInfo {
            identifier: Some("com.dirijor.diri".to_owned()),
            team_identifier: Some("AH8WARWU6L".to_owned()),
            authorities: vec!["Apple Development: Cristian Cretu (ABCDE12345)".to_owned()],
        };
        assert!(
            check_identity(&apple_development, "AH8WARWU6L", Some("com.dirijor.diri")).is_err()
        );
    }
}
