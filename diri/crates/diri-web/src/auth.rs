//! Who is allowed to drive the sessions.
//!
//! The transport is a tailnet, so this is not the only thing standing between
//! a stranger and the agents — but it is the layer that survives someone else
//! being *on* the tailnet, which for a shared VPS is the realistic case. Same
//! posture as `diri-node`: private bind, app-layer token.
//!
//! A phone browser cannot set an `Authorization` header by typing a URL, so
//! the token also arrives as `?token=…` once and is exchanged for a cookie.

use std::io;
use std::path::{Path, PathBuf};

use crate::http::{Request, Response};

/// The cookie the browser holds after the first authenticated load.
pub const COOKIE: &str = "diri_web_token";

/// 256 bits of `getrandom` entropy, hex-encoded.
const TOKEN_BYTES: usize = 32;

#[derive(Clone)]
pub struct Auth {
    token: String,
}

impl Auth {
    /// A token that exists only in memory and is shown to nobody: what a
    /// tailnet-admitted gateway holds, since it never accepts one.
    pub fn ephemeral() -> io::Result<Self> {
        Ok(Self {
            token: mint_token()?,
        })
    }

    #[cfg(test)]
    pub fn temporary() -> io::Result<Self> {
        Ok(Self {
            token: mint_token()?,
        })
    }
    /// Loads the token at `path`, creating one if the file does not exist.
    ///
    /// Returns the token and whether it was freshly minted, so `main` can
    /// print the enrolment URL exactly once rather than on every restart.
    pub fn load_or_create(path: &Path) -> io::Result<(Self, bool)> {
        if let Ok(existing) = std::fs::read_to_string(path) {
            let token = existing.trim().to_string();
            if !token.is_empty() {
                return Ok((Self { token }, false));
            }
        }
        Ok((Self::rotate(path)?, true))
    }

    /// Replaces the token at `path` with a fresh one, revoking every client
    /// paired with the old one. The file is owner-only from creation and
    /// swapped in atomically, so a crash never leaves a half-written token.
    pub fn rotate(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            restrict(parent, 0o700)?;
        }
        let token = mint_token()?;
        let mut staging = path.as_os_str().to_owned();
        staging.push(format!(".{}.tmp", std::process::id()));
        let staging = PathBuf::from(staging);
        let written = write_owner_only(&staging, format!("{token}\n").as_bytes())
            .and_then(|()| std::fs::rename(&staging, path));
        if written.is_err() {
            let _ = std::fs::remove_file(&staging);
        }
        written?;
        Ok(Self { token })
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    #[must_use]
    pub fn authorizes(&self, request: &Request) -> bool {
        presented(request).is_some_and(|presented| constant_time_eq(&presented, &self.token))
    }

    /// True when the token arrived in the URL, meaning the browser should be
    /// handed a cookie and redirected so the secret leaves the address bar.
    #[must_use]
    pub fn arrived_in_url(&self, request: &Request) -> bool {
        request
            .query
            .get("token")
            .is_some_and(|token| constant_time_eq(token, &self.token))
    }

    #[must_use]
    pub fn cookie_header(&self) -> String {
        // `SameSite=Strict` is what stops another page on the tailnet from
        // driving these endpoints with the browser's ambient cookie. No
        // `Secure`: this is plain HTTP over WireGuard, and `Secure` would
        // make the cookie unsettable.
        format!(
            "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=31536000",
            self.token
        )
    }
}

/// The secret this request presents, by any of the accepted routes: the
/// bearer token, or under tailnet admission a one-time pairing code.
pub(crate) fn presented(request: &Request) -> Option<String> {
    if let Some(header) = request.header("authorization")
        && let Some(rest) = header.strip_prefix("Bearer ")
    {
        return Some(rest.trim().to_string());
    }
    if let Some(header) = request.header("x-diri-token") {
        return Some(header.trim().to_string());
    }
    if let Some(query) = request.query.get("token") {
        return Some(query.clone());
    }
    request.cookie(COOKIE)
}

/// A state-changing request must not be driven by another origin's page.
///
/// `SameSite=Strict` already covers browsers that honour it; this is the
/// belt to that pair of braces, and it is what catches a `fetch` from a page
/// served by some other service on the same tailnet.
#[must_use]
pub fn origin_is_acceptable(request: &Request) -> bool {
    let Some(origin) = request.header("origin") else {
        // Absent `Origin` means a same-origin navigation or a non-browser
        // client (curl, a script) — neither is a cross-site forgery.
        return true;
    };
    let Some(host) = request.header("host") else {
        return false;
    };
    origin
        .rsplit_once("//")
        .is_some_and(|(_, authority)| authority == host)
}

#[must_use]
pub fn unauthorized() -> Response {
    Response::error(401, "unauthorized")
}

/// The default token location, alongside the node's enrolment tokens.
#[must_use]
pub fn default_token_path() -> PathBuf {
    let home = diri_platform::home_dir()
        .map(|p| p.into_os_string())
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".config").join("dirijor").join("web.token")
}

fn mint_token() -> io::Result<String> {
    mint_secret(TOKEN_BYTES)
}

/// `len` bytes of system entropy, hex-encoded.
pub(crate) fn mint_secret(len: usize) -> io::Result<String> {
    let mut bytes = vec![0_u8; len];
    getrandom::fill(&mut bytes)
        .map_err(|error| io::Error::other(format!("no system entropy: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(unix)]
pub(crate) fn write_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` is filtered by the umask and ignored for an existing file.
    restrict(path, 0o600)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn write_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    std::fs::write(path, bytes)
}

#[cfg(unix)]
pub(crate) fn restrict(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub(crate) fn restrict(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Compares without leaking the position of the first difference through
/// timing. Length is allowed to leak; the token length is not a secret.
pub(crate) fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |accumulator, (a, b)| accumulator | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    async fn request(raw: &str) -> Request {
        let mut reader = BufReader::new(raw.as_bytes());
        crate::http::read_request(&mut reader)
            .await
            .expect("read")
            .expect("request")
    }

    fn auth() -> Auth {
        Auth {
            token: "0123456789abcdef".into(),
        }
    }

    #[tokio::test]
    async fn every_accepted_route_carries_the_token() {
        let auth = auth();
        assert!(auth.authorizes(
            &request("GET /x HTTP/1.1\r\nAuthorization: Bearer 0123456789abcdef\r\n\r\n").await
        ));
        assert!(auth.authorizes(
            &request("GET /x HTTP/1.1\r\nX-Diri-Token: 0123456789abcdef\r\n\r\n").await
        ));
        assert!(auth.authorizes(&request("GET /x?token=0123456789abcdef HTTP/1.1\r\n\r\n").await));
        assert!(auth.authorizes(
            &request("GET /x HTTP/1.1\r\nCookie: diri_web_token=0123456789abcdef\r\n\r\n").await
        ));
    }

    #[tokio::test]
    async fn a_wrong_or_missing_token_is_refused() {
        let auth = auth();
        assert!(!auth.authorizes(&request("GET /x HTTP/1.1\r\n\r\n").await));
        assert!(!auth.authorizes(&request("GET /x?token=wrong HTTP/1.1\r\n\r\n").await));
        // Same length, one byte different — the case constant-time comparison
        // exists for.
        assert!(!auth.authorizes(&request("GET /x?token=0123456789abcdee HTTP/1.1\r\n\r\n").await));
    }

    #[tokio::test]
    async fn a_url_token_is_recognised_so_it_can_be_traded_for_a_cookie() {
        let auth = auth();
        assert!(
            auth.arrived_in_url(&request("GET /?token=0123456789abcdef HTTP/1.1\r\n\r\n").await)
        );
        assert!(!auth.arrived_in_url(
            &request("GET / HTTP/1.1\r\nCookie: diri_web_token=0123456789abcdef\r\n\r\n").await
        ));
    }

    #[tokio::test]
    async fn a_cross_origin_post_is_rejected_even_with_a_valid_cookie() {
        let same =
            request("POST /x HTTP/1.1\r\nHost: forge:7380\r\nOrigin: http://forge:7380\r\n\r\n")
                .await;
        assert!(origin_is_acceptable(&same));

        let cross =
            request("POST /x HTTP/1.1\r\nHost: forge:7380\r\nOrigin: http://evil.local\r\n\r\n")
                .await;
        assert!(!origin_is_acceptable(&cross));

        let headerless = request("POST /x HTTP/1.1\r\nHost: forge:7380\r\n\r\n").await;
        assert!(origin_is_acceptable(&headerless));
    }

    #[test]
    fn a_created_token_is_owner_only_and_stable_across_reloads() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("nested").join("web.token");

        let (first, minted) = Auth::load_or_create(&path).expect("create");
        assert!(minted);
        assert_eq!(first.token().len(), TOKEN_BYTES * 2);

        let (second, minted_again) = Auth::load_or_create(&path).expect("reload");
        assert!(!minted_again);
        assert_eq!(first.token(), second.token());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn rotating_revokes_the_old_token_and_stays_owner_only() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("phone.token");
        let (first, _) = Auth::load_or_create(&path).expect("create");
        let rotated = Auth::rotate(&path).expect("rotate");
        assert_ne!(first.token(), rotated.token());
        let (reloaded, minted) = Auth::load_or_create(&path).expect("reload");
        assert!(!minted);
        assert_eq!(reloaded.token(), rotated.token());
        // Only the token file is left behind: no staging file.
        let entries: Vec<_> = std::fs::read_dir(directory.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("phone.token")]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
