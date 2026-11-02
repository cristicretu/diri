//! Tailnet admission: who is calling, according to this host's `tailscaled`.
//!
//! With admission on, the gateway trusts no secret a browser carries. Each
//! TCP connection's peer address is looked up with Tailscale's WhoIs through
//! the host's own LocalAPI, and the caller is admitted only when that node
//! belongs to the owner of this host, carries no tags, and was paired here
//! (see [`crate::devices`]). Nothing is bundled: no `tsnet`, no second
//! WireGuard stack. If the LocalAPI cannot be reached the gateway refuses
//! every request with `tailnet_identity_unavailable`; it never falls back to
//! the bearer token.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::devices::Devices;
use crate::http::{Request, Response};

/// The `Host` the LocalAPI accepts from any transport.
const LOCALAPI_HOST: &str = "local-tailscaled.sock";
/// A WhoIs answer is a few KiB; `status?peers=false` is about the same.
const MAX_LOCALAPI_RESPONSE: usize = 1024 * 1024;
const LOCALAPI_TIMEOUT: Duration = Duration::from_secs(3);
/// How long one peer address keeps its WhoIs answer. Pairing and revocation
/// are checked on every request regardless; this only bounds how long a node
/// that left the tailnet (or changed owner) keeps its identity here.
const IDENTITY_TTL: Duration = Duration::from_secs(30);
const MAX_CACHED_PEERS: usize = 256;

/// Unix sockets the open-source `tailscaled` listens on, by platform.
#[cfg(unix)]
const SOCKETS: [&str; 3] = [
    "/var/run/tailscale/tailscaled.sock",
    "/run/tailscale/tailscaled.sock",
    "/var/run/tailscaled.socket",
];
/// The standalone macOS app publishes its LocalAPI port as this symlink's
/// target and the password beside it.
const MACSYS_DIR: &str = "/Library/Tailscale";
/// CLIs that can answer for the App Store app, whose LocalAPI is sandboxed.
const CLIS: [&str; 4] = [
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/opt/homebrew/bin/tailscale",
    "/usr/local/bin/tailscale",
    "/usr/bin/tailscale",
];

/// How this process reaches the host's `tailscaled`.
#[derive(Clone, Debug)]
pub enum LocalApi {
    /// `tailscaled`'s Unix socket (Linux, open-source macOS daemon).
    #[cfg(unix)]
    Socket(PathBuf),
    /// Loopback TCP with the per-boot password (standalone macOS app).
    Tcp { port: u16, password: String },
    /// `tailscale … --json` (App Store app). One process per lookup; the
    /// identity cache keeps that to one per peer per [`IDENTITY_TTL`].
    Cli(PathBuf),
}

impl LocalApi {
    /// The first LocalAPI this host offers, or `None` without Tailscale.
    #[must_use]
    pub fn discover() -> Option<Self> {
        #[cfg(unix)]
        for socket in SOCKETS {
            if is_socket(Path::new(socket)) {
                return Some(Self::Socket(socket.into()));
            }
        }
        if let Some(tcp) = macsys() {
            return Some(tcp);
        }
        CLIS.iter()
            .map(Path::new)
            .find(|path| path.is_file())
            .map(|path| Self::Cli(path.to_path_buf()))
    }

    async fn status(&self) -> Result<Status, Refusal> {
        let bytes = match self {
            Self::Cli(cli) => run_cli(cli, &["status", "--json", "--peers=false"]).await,
            _ => self
                .get("/localapi/v0/status?peers=false")
                .await
                .map_err(drop),
        }
        .map_err(|_| Refusal::Unavailable)?;
        serde_json::from_slice(&bytes).map_err(|_| Refusal::Unavailable)
    }

    async fn whois(&self, peer: IpAddr) -> Result<WhoIs, Refusal> {
        let bytes = match self {
            Self::Cli(cli) => run_cli(cli, &["whois", "--json", &peer.to_string()])
                .await
                // The CLI exits nonzero for an address it does not know, and
                // the status call just proved the daemon answers.
                .map_err(|_| Refusal::NotTailnetPeer)?,
            _ => self
                .get(&format!("/localapi/v0/whois?addr={peer}"))
                .await
                .map_err(|error| match error {
                    LocalApiError::Status(404) => Refusal::NotTailnetPeer,
                    _ => Refusal::Unavailable,
                })?,
        };
        serde_json::from_slice(&bytes).map_err(|_| Refusal::Unavailable)
    }

    async fn get(&self, path: &str) -> Result<Vec<u8>, LocalApiError> {
        let exchange = async {
            match self {
                #[cfg(unix)]
                Self::Socket(socket) => {
                    let stream = tokio::net::UnixStream::connect(socket).await?;
                    http_get(stream, path, None).await
                }
                Self::Tcp { port, password } => {
                    let stream = tokio::net::TcpStream::connect(("127.0.0.1", *port)).await?;
                    http_get(stream, path, Some(password)).await
                }
                Self::Cli(_) => Err(LocalApiError::Io),
            }
        };
        tokio::time::timeout(LOCALAPI_TIMEOUT, exchange)
            .await
            .map_err(|_| LocalApiError::Io)?
    }
}

#[cfg(unix)]
fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    std::fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

fn macsys() -> Option<LocalApi> {
    let dir = Path::new(MACSYS_DIR);
    let port = std::fs::read_link(dir.join("ipnport")).ok()?;
    let port = port.to_str()?;
    let password = std::fs::read_to_string(dir.join(format!("sameuserproof-{port}"))).ok()?;
    Some(LocalApi::Tcp {
        port: port.parse().ok()?,
        password: password.trim().to_owned(),
    })
}

async fn run_cli(cli: &Path, arguments: &[&str]) -> Result<Vec<u8>, ()> {
    let output = tokio::time::timeout(
        LOCALAPI_TIMEOUT,
        tokio::process::Command::new(cli)
            .args(arguments)
            // The macOS bundle otherwise guesses GUI mode when its parent is
            // a GUI app.
            .env("TAILSCALE_BE_CLI", "1")
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(drop)?
    .map_err(drop)?;
    if !output.status.success() || output.stdout.len() > MAX_LOCALAPI_RESPONSE {
        return Err(());
    }
    Ok(output.stdout)
}

#[derive(Debug)]
enum LocalApiError {
    Io,
    Status(u16),
    Malformed,
}

impl From<std::io::Error> for LocalApiError {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}

/// One `Connection: close` GET. The LocalAPI is the only server this talks
/// to, so the client handles exactly what it sends: a status line, headers,
/// and a body that is either read to EOF or chunked.
async fn http_get<S>(
    mut stream: S,
    path: &str,
    password: Option<&str>,
) -> Result<Vec<u8>, LocalApiError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use base64::Engine as _;
    let mut head = format!("GET {path} HTTP/1.1\r\nHost: {LOCALAPI_HOST}\r\nConnection: close\r\n");
    if let Some(password) = password {
        let basic = base64::engine::general_purpose::STANDARD.encode(format!(":{password}"));
        head.push_str(&format!("Authorization: Basic {basic}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;

    let mut raw = Vec::new();
    (&mut stream)
        .take(MAX_LOCALAPI_RESPONSE as u64 + 1)
        .read_to_end(&mut raw)
        .await?;
    if raw.len() > MAX_LOCALAPI_RESPONSE {
        return Err(LocalApiError::Malformed);
    }
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(LocalApiError::Malformed)?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| LocalApiError::Malformed)?;
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .ok_or(LocalApiError::Malformed)?;
    if status != 200 {
        return Err(LocalApiError::Status(status));
    }
    let chunked = lines.any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("transfer-encoding")
                && value.trim().eq_ignore_ascii_case("chunked")
        })
    });
    if chunked {
        dechunk(body).ok_or(LocalApiError::Malformed)
    } else {
        Ok(body.to_vec())
    }
}

fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|window| window == b"\r\n")?;
        let size = std::str::from_utf8(&body[..line_end]).ok()?;
        let size = usize::from_str_radix(size.split(';').next()?.trim(), 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(..size)?);
        body = body.get(size + 2..)?;
    }
}

#[derive(Debug, Deserialize)]
struct Status {
    #[serde(rename = "Self")]
    self_node: Option<SelfNode>,
}

#[derive(Debug, Deserialize)]
struct SelfNode {
    #[serde(rename = "UserID")]
    user_id: u64,
    #[serde(rename = "Tags", default)]
    tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct WhoIs {
    #[serde(rename = "Node")]
    node: WhoIsNode,
    #[serde(rename = "UserProfile")]
    user: UserProfile,
}

#[derive(Debug, Deserialize)]
struct WhoIsNode {
    #[serde(rename = "StableID")]
    stable_id: String,
    #[serde(rename = "ComputedName", default)]
    computed_name: Option<String>,
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "Tags", default)]
    tags: Option<Vec<String>>,
    #[serde(rename = "Hostinfo", default)]
    hostinfo: Option<Hostinfo>,
}

#[derive(Debug, Default, Deserialize)]
struct Hostinfo {
    #[serde(rename = "OS", default)]
    os: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserProfile {
    #[serde(rename = "ID")]
    id: u64,
    #[serde(rename = "LoginName", default)]
    login_name: String,
}

/// Whose devices may connect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// The user this host's node is logged in as. A tagged host has no such
    /// user, so it admits nobody until an explicit login is configured.
    ThisNode,
    /// A Tailscale login name, for a tagged server such as a shared VPS.
    Login(String),
}

/// A caller WhoIs identified as one of the owner's untagged devices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// Tailscale's stable node id: what a pairing records and revokes.
    pub node: String,
    /// The device's short name, for Settings.
    pub name: String,
    pub os: String,
    pub login: String,
}

/// Why a caller was not admitted. Each maps to one structured error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The LocalAPI did not answer: fail closed, never fall back.
    Unavailable,
    /// The address is not a tailnet node (loopback, LAN, unknown).
    NotTailnetPeer,
    /// Tagged nodes act for the tailnet, not for a person.
    Tagged,
    /// This host is tagged and no owner login was configured.
    OwnerUnknown,
    /// Another tailnet user's device.
    NotOwner,
    /// The owner's device, but it was never paired here or was revoked.
    NotPaired,
    /// A page from another site, or a name rebound to this address, is
    /// driving the paired device's browser.
    ForeignOrigin,
}

impl Refusal {
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "tailnet_identity_unavailable",
            Self::NotTailnetPeer => "not_a_tailnet_peer",
            Self::Tagged => "tagged_node_refused",
            Self::OwnerUnknown => "tailnet_owner_unknown",
            Self::NotOwner => "not_the_owner",
            Self::NotPaired => "device_not_paired",
            Self::ForeignOrigin => "foreign_origin",
        }
    }

    #[must_use]
    pub fn response(self) -> Response {
        let (status, message) = match self {
            Self::Unavailable => (503, "This computer cannot ask Tailscale who is connecting"),
            Self::NotPaired => (
                401,
                "This device is not paired. Scan the pairing code in Diri Settings",
            ),
            Self::NotTailnetPeer => (403, "Only devices on your tailnet can connect"),
            Self::Tagged => (403, "Tagged Tailscale devices cannot connect"),
            Self::OwnerUnknown => (
                403,
                "This host is tagged; configure the Tailscale login allowed to connect",
            ),
            Self::NotOwner => (403, "This device belongs to another Tailscale user"),
            Self::ForeignOrigin => (403, "Requests from other sites are refused"),
        };
        Response::coded_error(status, self.code(), message)
    }
}

/// The admission policy, apart from any I/O.
fn decide(owner: &Owner, host: &SelfNode, caller: &WhoIs) -> Result<Identity, Refusal> {
    if caller
        .node
        .tags
        .as_ref()
        .is_some_and(|tags| !tags.is_empty())
    {
        return Err(Refusal::Tagged);
    }
    let owned = match owner {
        Owner::Login(login) => caller.user.login_name.eq_ignore_ascii_case(login),
        Owner::ThisNode => {
            if host.tags.as_ref().is_some_and(|tags| !tags.is_empty()) {
                return Err(Refusal::OwnerUnknown);
            }
            caller.user.id == host.user_id
        }
    };
    if !owned {
        return Err(Refusal::NotOwner);
    }
    let name = caller
        .node
        .computed_name
        .clone()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            caller
                .node
                .name
                .split('.')
                .next()
                .unwrap_or_default()
                .to_owned()
        });
    Ok(Identity {
        node: caller.node.stable_id.clone(),
        name,
        os: caller
            .node
            .hostinfo
            .as_ref()
            .and_then(|info| info.os.clone())
            .unwrap_or_default(),
        login: caller.user.login_name.clone(),
    })
}

/// Network identity is ambient, like a cookie: any page the paired device's
/// browser loads could aim requests here. Refuse what a browser marks as
/// cross-site, and any `Host` that is not an address or a tailnet name, which
/// is what a DNS-rebound name would carry.
#[must_use]
pub fn request_is_first_party(request: &Request) -> bool {
    if request
        .header("sec-fetch-site")
        .is_some_and(|site| site.eq_ignore_ascii_case("cross-site"))
    {
        return false;
    }
    let Some(host) = request.header("host") else {
        // HTTP/1.0 or a non-browser client: no name to have been rebound.
        return true;
    };
    let name = match host.strip_prefix('[') {
        Some(bracketed) => {
            return bracketed
                .split(']')
                .next()
                .is_some_and(|ip| ip.parse::<IpAddr>().is_ok());
        }
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name.parse::<IpAddr>().is_ok()
        // A MagicDNS short name, or the tailnet's own domain.
        || (!name.is_empty() && !name.contains('.'))
        || name.ends_with(".ts.net")
}

/// A Tailscale CGNAT IPv4 or ULA IPv6 address.
#[must_use]
pub fn is_tailnet_ip(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            a == 100 && (64..=127).contains(&b)
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            segments[0] == 0xfd7a && segments[1] == 0x115c && segments[2] == 0xa1e0
        }
    }
}

type Cached = (Instant, Result<Identity, Refusal>);

/// The gate one gateway consults: WhoIs, the owner rule, then pairing.
pub struct Admission {
    api: LocalApi,
    owner: Owner,
    pub devices: Arc<Devices>,
    cache: Mutex<HashMap<IpAddr, Cached>>,
    /// Tests connect over loopback; this stands in for the tailnet address
    /// tailscaled would have reported.
    #[cfg(test)]
    pub test_peer: Mutex<Option<IpAddr>>,
}

impl Admission {
    #[must_use]
    pub fn new(api: LocalApi, owner: Owner, devices: Arc<Devices>) -> Self {
        Self {
            api,
            owner,
            devices,
            cache: Mutex::new(HashMap::new()),
            #[cfg(test)]
            test_peer: Mutex::new(None),
        }
    }

    /// Who `peer` is, if it is one of the owner's untagged devices. Pairing
    /// is not checked here; see [`Self::admits`].
    pub async fn identify(&self, peer: IpAddr) -> Result<Identity, Refusal> {
        #[cfg(test)]
        let peer = self
            .test_peer
            .lock()
            .ok()
            .and_then(|test_peer| *test_peer)
            .unwrap_or(peer);
        let peer = peer.to_canonical();
        if !is_tailnet_ip(peer) {
            return Err(Refusal::NotTailnetPeer);
        }
        if let Some(answer) = self.cache.lock().ok().and_then(|cache| {
            cache
                .get(&peer)
                .filter(|(at, _)| at.elapsed() < IDENTITY_TTL)
                .map(|(_, answer)| answer.clone())
        }) {
            return answer;
        }
        let answer = self.ask(peer).await;
        // An outage is not remembered: the next connection asks again.
        if answer != Err(Refusal::Unavailable)
            && let Ok(mut cache) = self.cache.lock()
        {
            if cache.len() >= MAX_CACHED_PEERS {
                cache.retain(|_, (at, _)| at.elapsed() < IDENTITY_TTL);
                if cache.len() >= MAX_CACHED_PEERS {
                    cache.clear();
                }
            }
            cache.insert(peer, (Instant::now(), answer.clone()));
        }
        answer
    }

    async fn ask(&self, peer: IpAddr) -> Result<Identity, Refusal> {
        let status = self.api.status().await?;
        let host = status.self_node.ok_or(Refusal::Unavailable)?;
        let caller = self.api.whois(peer).await?;
        decide(&self.owner, &host, &caller)
    }

    /// Whether this request from `identity` may proceed: the device is
    /// paired, or the request carries a live pairing code that pairs it now.
    pub fn admits(&self, identity: &Identity, request: &Request) -> Result<(), Refusal> {
        if !request_is_first_party(request) {
            return Err(Refusal::ForeignOrigin);
        }
        if self.devices.is_paired(&identity.node)
            || crate::auth::presented(request)
                .is_some_and(|code| self.devices.redeem(&code, identity).unwrap_or(false))
        {
            return Ok(());
        }
        Err(Refusal::NotPaired)
    }
}

/// Ends when `node` is no longer paired: at once for a revocation made in
/// this process, within one recheck for one made by another (`diri-web
/// revoke`). Streams select on it so revoking a device also cuts its live
/// views.
pub async fn revoked(devices: &Devices, node: &str) {
    const RECHECK: Duration = Duration::from_secs(15);
    let mut generation = devices.watch();
    loop {
        tokio::select! {
            changed = generation.changed() => if changed.is_err() {
                std::future::pending::<()>().await;
            },
            () = tokio::time::sleep(RECHECK) => {}
        }
        if !devices.is_paired(node) {
            return;
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) mod fake {
    //! A fake LocalAPI on a Unix socket: answers `status` and `whois` from
    //! tables the test controls, and counts requests.

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    pub struct FakeLocalApi {
        pub socket: std::path::PathBuf,
        pub whois: Arc<Mutex<HashMap<String, serde_json::Value>>>,
        pub status: Arc<Mutex<serde_json::Value>>,
        pub requests: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
        _dir: tempfile::TempDir,
    }

    impl Drop for FakeLocalApi {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl FakeLocalApi {
        pub fn api(&self) -> super::LocalApi {
            super::LocalApi::Socket(self.socket.clone())
        }

        pub fn requests(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    pub const OWNER: u64 = 7;

    pub fn device(stable: &str, name: &str, user: u64, tags: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "Node": {
                "ID": 1, "StableID": stable, "Name": format!("{name}.tail0.ts.net."),
                "ComputedName": name, "Tags": tags, "User": user,
                "Hostinfo": {"OS": "iOS", "Hostname": name}
            },
            "UserProfile": {"ID": user, "LoginName": format!("user{user}@example.com"), "DisplayName": "U"},
            "CapMap": {}
        })
    }

    pub async fn start() -> FakeLocalApi {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let socket = dir.path().join("tailscaled.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let whois: Arc<Mutex<HashMap<String, serde_json::Value>>> = Arc::default();
        let status = Arc::new(Mutex::new(serde_json::json!({
            "BackendState": "Running",
            "Self": {"ID": "nSelf", "UserID": OWNER, "Tags": null},
            "User": {}
        })));
        let requests = Arc::new(AtomicUsize::new(0));
        let task = {
            let (whois, status, requests) = (
                Arc::clone(&whois),
                Arc::clone(&status),
                Arc::clone(&requests),
            );
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    requests.fetch_add(1, Ordering::SeqCst);
                    let (whois, status) = (Arc::clone(&whois), Arc::clone(&status));
                    tokio::spawn(async move {
                        let (read, mut write) = stream.into_split();
                        let mut lines = BufReader::new(read).lines();
                        let Ok(Some(first)) = lines.next_line().await else {
                            return;
                        };
                        let mut host_ok = false;
                        while let Ok(Some(line)) = lines.next_line().await {
                            if line.is_empty() {
                                break;
                            }
                            host_ok |= line == "Host: local-tailscaled.sock";
                        }
                        let target = first.split(' ').nth(1).unwrap_or_default().to_owned();
                        let body = if !host_ok {
                            None
                        } else if target == "/localapi/v0/status?peers=false" {
                            Some(status.lock().unwrap().to_string())
                        } else if let Some(addr) = target.strip_prefix("/localapi/v0/whois?addr=") {
                            whois.lock().unwrap().get(addr).map(ToString::to_string)
                        } else {
                            None
                        };
                        // Chunked, as Go's server sends a body it streamed.
                        let response = match body {
                            Some(body) => {
                                let (a, b) = body.split_at(body.len() / 2);
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                     Transfer-Encoding: chunked\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
                                    a.len(),
                                    b.len()
                                )
                            }
                            None => "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nno match\n"
                                .to_owned(),
                        };
                        let _ = write.write_all(response.as_bytes()).await;
                        let _ = write.shutdown().await;
                    });
                }
            })
        };
        FakeLocalApi {
            socket,
            whois,
            status,
            requests,
            task,
            _dir: dir,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::fake::{self, OWNER};
    use super::*;

    fn admission(api: LocalApi, owner: Owner) -> (Admission, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let devices = Arc::new(Devices::open(dir.path().join("devices.json")));
        (Admission::new(api, owner, devices), dir)
    }

    #[tokio::test]
    async fn whois_admits_only_the_owners_untagged_devices() {
        let local = fake::start().await;
        {
            let mut whois = local.whois.lock().unwrap();
            whois.insert(
                "100.64.0.2".into(),
                fake::device("nPhone", "phone", OWNER, &[]),
            );
            whois.insert(
                "100.64.0.3".into(),
                fake::device("nOther", "theirs", 99, &[]),
            );
            whois.insert(
                "100.64.0.4".into(),
                fake::device("nCi", "ci", OWNER, &["tag:ci"]),
            );
        }
        let (gate, _dir) = admission(local.api(), Owner::ThisNode);
        let phone = gate.identify("100.64.0.2".parse().unwrap()).await.unwrap();
        assert_eq!(
            phone,
            Identity {
                node: "nPhone".into(),
                name: "phone".into(),
                os: "iOS".into(),
                login: format!("user{OWNER}@example.com"),
            }
        );
        assert_eq!(
            gate.identify("100.64.0.3".parse().unwrap()).await,
            Err(Refusal::NotOwner)
        );
        assert_eq!(
            gate.identify("100.64.0.4".parse().unwrap()).await,
            Err(Refusal::Tagged)
        );
        // Unknown to tailscaled (404), and never asked about at all.
        assert_eq!(
            gate.identify("100.64.0.9".parse().unwrap()).await,
            Err(Refusal::NotTailnetPeer)
        );
        let asked = local.requests();
        for outsider in ["127.0.0.1", "192.168.1.2", "::1"] {
            assert_eq!(
                gate.identify(outsider.parse().unwrap()).await,
                Err(Refusal::NotTailnetPeer)
            );
        }
        assert_eq!(local.requests(), asked);
    }

    #[tokio::test]
    async fn a_tagged_host_admits_nobody_unless_an_owner_login_is_named() {
        let local = fake::start().await;
        *local.status.lock().unwrap() =
            serde_json::json!({"Self": {"UserID": 1, "Tags": ["tag:server"]}});
        local.whois.lock().unwrap().insert(
            "100.64.0.2".into(),
            fake::device("nPhone", "phone", OWNER, &[]),
        );
        let (gate, _dir) = admission(local.api(), Owner::ThisNode);
        assert_eq!(
            gate.identify("100.64.0.2".parse().unwrap()).await,
            Err(Refusal::OwnerUnknown)
        );
        let (gate, _dir) = admission(
            local.api(),
            Owner::Login(format!("USER{OWNER}@example.com")),
        );
        assert!(gate.identify("100.64.0.2".parse().unwrap()).await.is_ok());
        let (gate, _dir) = admission(local.api(), Owner::Login("someone@else".into()));
        assert_eq!(
            gate.identify("100.64.0.2".parse().unwrap()).await,
            Err(Refusal::NotOwner)
        );
    }

    #[tokio::test]
    async fn a_missing_localapi_fails_closed_and_is_not_cached() {
        let local = fake::start().await;
        let (gate, _dir) = admission(
            LocalApi::Socket("/nonexistent/tailscaled.sock".into()),
            Owner::ThisNode,
        );
        assert_eq!(
            gate.identify("100.64.0.2".parse().unwrap()).await,
            Err(Refusal::Unavailable)
        );
        assert_eq!(Refusal::Unavailable.response().status, 503);
        drop(local);
    }

    #[tokio::test]
    async fn answers_are_cached_per_peer_address() {
        let local = fake::start().await;
        local.whois.lock().unwrap().insert(
            "100.64.0.2".into(),
            fake::device("nPhone", "phone", OWNER, &[]),
        );
        let (gate, _dir) = admission(local.api(), Owner::ThisNode);
        gate.identify("100.64.0.2".parse().unwrap()).await.unwrap();
        let after_first = local.requests();
        // An IPv4-mapped form of the same peer hits the same entry.
        gate.identify("::ffff:100.64.0.2".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(local.requests(), after_first);
    }

    #[tokio::test]
    async fn rebound_names_and_cross_site_requests_are_not_first_party() {
        let request = |head: &str| {
            let raw = format!("GET /api/sessions HTTP/1.1\r\n{head}\r\n");
            async move {
                let mut reader = tokio::io::BufReader::new(std::io::Cursor::new(raw.into_bytes()));
                crate::http::read_request(&mut reader)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        for host in [
            "100.64.0.1:7380",
            "[fd7a:115c:a1e0::1]:7380",
            "mac:7380",
            "mac.tail0.ts.net:7380",
            "MAC.TAIL0.TS.NET.",
        ] {
            assert!(
                request_is_first_party(&request(&format!("Host: {host}\r\n")).await),
                "{host}"
            );
        }
        for host in ["evil.example:7380", "100.64.0.1.nip.io", "[evil]:80"] {
            assert!(
                !request_is_first_party(&request(&format!("Host: {host}\r\n")).await),
                "{host}"
            );
        }
        assert!(!request_is_first_party(
            &request("Host: mac:7380\r\nSec-Fetch-Site: cross-site\r\n").await
        ));
        assert!(request_is_first_party(
            &request("Host: mac:7380\r\nSec-Fetch-Site: same-origin\r\n").await
        ));
    }

    #[test]
    fn chunked_bodies_are_reassembled() {
        assert_eq!(
            dechunk(b"3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\n\r\n").as_deref(),
            Some(&b"abcde"[..])
        );
        assert_eq!(dechunk(b"5\r\nab"), None);
    }
}
