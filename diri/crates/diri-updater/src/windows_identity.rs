//! Signer identity after Windows has validated the Authenticode chain.
//! Artifact Signing renews the leaf daily; its Public Trust profile EKU is
//! durable. Ordinary certificates retain the conservative thumbprint pin.
use crate::error::{Result, UpdateError};

const PUBLIC_TRUST: &str = "1.3.6.1.4.1.311.97.1.0";
const PROFILE_PREFIX: &str = "1.3.6.1.4.1.311.97.";

pub(super) fn signer_identity(thumbprint: &str, subject: &str, ekus: &[String]) -> Result<String> {
    let invalid = || UpdateError::Signature("invalid Authenticode signer identity".into());
    if thumbprint.len() != 40 || !thumbprint.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    if !ekus.iter().any(|eku| eku == PUBLIC_TRUST) {
        return Ok(format!("certificate:{}", thumbprint.to_ascii_uppercase()));
    }
    // Never pin the common Public Trust marker: it identifies all publishers.
    // Private Trust/CI policy EKUs under .97.1.* are not a Public Trust profile.
    let profiles: Vec<_> = ekus
        .iter()
        .filter(|eku| {
            eku.strip_prefix(PROFILE_PREFIX).is_some_and(|suffix| {
                !suffix.starts_with("1.")
                    && suffix.split('.').count() >= 4
                    && suffix
                        .split('.')
                        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            })
        })
        .collect();
    if profiles.len() != 1 || subject.trim().is_empty() || subject.len() > 2048 {
        return Err(invalid());
    }
    // The subject also must remain unchanged. Recreating a profile or changing
    // the publisher requires a manually installed trusted release.
    Ok(format!("artifact-signing:{}:{subject}", profiles[0]))
}

#[cfg(test)]
mod tests {
    use super::*;
    const A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const B: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    const PROFILE: &str = "1.3.6.1.4.1.311.97.990309390.766961637.194916062.941502583";
    fn ekus() -> Vec<String> {
        vec![
            "1.3.6.1.5.5.7.3.3".into(),
            PUBLIC_TRUST.into(),
            PROFILE.into(),
        ]
    }

    #[test]
    fn leaf_renewal_preserves_only_the_same_profile_and_publisher() {
        let installed = signer_identity(A, "CN=Diri publisher", &ekus()).unwrap();
        assert_eq!(
            installed,
            signer_identity(B, "CN=Diri publisher", &ekus()).unwrap()
        );
        assert_ne!(
            installed,
            signer_identity(B, "CN=Other publisher", &ekus()).unwrap()
        );
        let mut other = ekus();
        other[2].push_str(".1");
        assert_ne!(
            installed,
            signer_identity(B, "CN=Diri publisher", &other).unwrap()
        );
    }
    #[test]
    fn shared_or_ambiguous_azure_identity_fails_closed() {
        assert!(signer_identity(A, "CN=Diri", &[PUBLIC_TRUST.into()]).is_err());
        let mut ambiguous = ekus();
        ambiguous.push(format!("{PROFILE}.2"));
        assert!(signer_identity(A, "CN=Diri", &ambiguous).is_err());
        assert!(signer_identity(A, "", &ekus()).is_err());
        assert!(signer_identity("", "CN=Diri", &ekus()).is_err());
    }
    #[test]
    fn regular_certificate_rotation_still_requires_manual_install() {
        assert_ne!(
            signer_identity(A, "CN=Diri", &[]).unwrap(),
            signer_identity(B, "CN=Diri", &[]).unwrap()
        );
    }
}
