//! Authenticode-pinned, per-user versioned Windows installer updates.
//! The signed installer writes a new directory; live Holders keep their old
//! binaries. No in-place image replacement, services, or elevation is needed.
use crate::{
    Release, UpdaterConfig, Version,
    codesign::SignatureInfo,
    error::{Result, UpdateError},
};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SignedImage {
    thumbprint: String,
    subject: String,
    ekus: Vec<String>,
    product: String,
    version: String,
}

fn image(path: &Path) -> Result<SignedImage> {
    use std::{io::Read, os::windows::process::CommandExt};
    // The script is fixed. The filename is data in one environment value,
    // never interpolated into PowerShell source or a cmd.exe command line.
    const SCRIPT: &str = "$ErrorActionPreference='Stop'; $s=Get-AuthenticodeSignature -LiteralPath $env:DIRI_VERIFY_IMAGE; if ($s.Status -ne 'Valid' -or $null -eq $s.SignerCertificate) { exit 2 }; $v=(Get-Item -LiteralPath $env:DIRI_VERIFY_IMAGE).VersionInfo; @{Thumbprint=$s.SignerCertificate.Thumbprint; Subject=$s.SignerCertificate.Subject; Ekus=@($s.SignerCertificate.EnhancedKeyUsageList | ForEach-Object { $_.ObjectId.Value }); Product=$v.ProductName; Version=$v.ProductVersion} | ConvertTo-Json -Compress";
    let shell = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .ok_or_else(|| UpdateError::Signature("Windows directory is unavailable".into()))?
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut child = Command::new(shell)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .env("DIRI_VERIFY_IMAGE", path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(diri_platform::windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| UpdateError::Signature("signature output unavailable".into()))?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(8193).read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(UpdateError::Signature(
                "Authenticode verification timed out".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let bytes = reader
        .join()
        .map_err(|_| UpdateError::Signature("signature reader failed".into()))??;
    if !status.success() || bytes.len() > 8192 {
        return Err(UpdateError::Signature(
            "Windows did not validate the Authenticode signature".into(),
        ));
    }
    let image: SignedImage = serde_json::from_slice(&bytes)
        .map_err(|_| UpdateError::Signature("invalid signed image metadata".into()))?;
    if image.thumbprint.len() != 40
        || !image.thumbprint.bytes().all(|b| b.is_ascii_hexdigit())
        || image.product != "Diri"
    {
        return Err(UpdateError::Signature(
            "image is not a signed Diri product".into(),
        ));
    }
    Ok(image)
}

pub fn signature_of(path: &Path) -> Result<SignatureInfo> {
    let image = image(path)?;
    let identity =
        crate::windows_identity::signer_identity(&image.thumbprint, &image.subject, &image.ekus)?;
    Ok(SignatureInfo {
        identifier: Some(image.product),
        team_identifier: Some(identity),
        authorities: vec!["Authenticode".into()],
    })
}
pub fn verify(path: &Path, installed: &SignatureInfo) -> Result<()> {
    let found = signature_of(path)?;
    if installed.team_identifier.is_none()
        || found.team_identifier != installed.team_identifier
        || found.identifier != installed.identifier
    {
        return Err(UpdateError::Signature(
            "installer signer does not match the running app".into(),
        ));
    }
    Ok(())
}
pub fn running_config(current: &str) -> Result<UpdaterConfig> {
    let executable = std::env::current_exe()?;
    let root = install_root(&executable)?;
    let signature = signature_of(&executable)?;
    let current_version = Version::parse(current)
        .ok_or_else(|| UpdateError::NotUpdatable("invalid application version".into()))?;
    let architecture = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    Ok(UpdaterConfig {
        feed_url: std::env::var(crate::FEED_URL_ENV).unwrap_or_else(|_| format!("https://github.com/cristicretu/diri/releases/latest/download/appcast-windows-{architecture}.json")),
        current_version, bundle: executable, cache_dir: root.join("updates"), installed_signature: signature,
    })
}
fn install_root(executable: &Path) -> Result<PathBuf> {
    let version = executable
        .parent()
        .ok_or_else(|| UpdateError::NotUpdatable("no installation directory".into()))?;
    let versions = version
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "versions"))
        .ok_or_else(|| {
            UpdateError::NotUpdatable("install the signed Windows package to enable updates".into())
        })?;
    versions
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| UpdateError::NotUpdatable("invalid versioned installation".into()))
}
pub fn unpack(archive: &Path, into: &Path) -> Result<PathBuf> {
    diri_platform::security::private_dir_all(into)?;
    let staged = into.join("diri-setup.exe");
    std::fs::copy(archive, &staged)?;
    Ok(staged)
}
pub fn verify_version(path: &Path, release: &Release) -> Result<()> {
    let found = image(path)?;
    let found = Version::parse(found.version.trim());
    if found.is_none() || found != release.parsed_version() {
        return Err(UpdateError::Integrity(
            "signed installer version differs from the update feed".into(),
        ));
    }
    Ok(())
}
pub fn install(staged: &Path, target: &Path, relaunch: bool) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let root = install_root(target)?;
    // Revalidate immediately before executing; the download hash alone is not
    // an authority to run an installer from a mutable staging directory.
    verify(staged, &signature_of(target)?)?;
    Command::new(staged)
        .args([
            "/VERYSILENT",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            "/NOCLOSEAPPLICATIONS",
        ])
        .arg(format!("/DIR={}", root.display()))
        .arg(format!("/DiriWaitPID={}", std::process::id()))
        .arg(format!("/DiriRelaunch={}", u8::from(relaunch)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(
            diri_platform::windows_sys::Win32::System::Threading::DETACHED_PROCESS
                | diri_platform::windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP,
        )
        .spawn()?;
    Ok(())
}
