//! `diri-web` — the phone frontend for a Dirijor daemon.
//!
//! It is a *frontend*, in exactly the sense `dirijor-mcp` is: it owns no
//! session state, it holds no registry, and killing it loses nothing. It
//! connects to whichever `dirijord` it is pointed at and re-publishes that
//! daemon's control surface over HTTP so a browser — specifically a phone
//! browser, on a tailnet — can drive it.
//!
//! That is what makes the local and remote stories one story: run it beside
//! the Mac daemon and you get your Mac's sessions on your phone; run it on the
//! VPS beside a Linux daemon and you get the VPS's sessions, with the same
//! binary, the same protocol, and the same page.
//!
//! ```text
//!   phone ──http──▶ diri-web ──unix socket──▶ dirijord ──pty──▶ claude/codex
//!         (tailnet)
//! ```

mod api;
mod auth;
#[cfg(test)]
mod bench;
mod devices;
mod http;
mod input_ledger;
mod tailnet;
mod terminal;
#[cfg(test)]
mod testing;
mod turns;
mod ui;

pub use devices::{CODE_LIFETIME as PAIRING_CODE_LIFETIME, Device, Devices};
pub use tailnet::{LocalApi, Owner};
pub use terminal::{TerminalPalette, set_terminal_palette};

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use diri_client::DaemonClient;
use diri_proto::net::is_safe_plaintext_address;
use diri_proto::paths::{DirijorEnv, DirijorPaths};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::api::Api;
use crate::auth::Auth;
use crate::http::{Request, Response};

/// Not one of the ports a dev server grabs, and adjacent to the node's 7337
/// so the two Dirijor surfaces sit together in a firewall rule.
const DEFAULT_PORT: u16 = 7380;

/// How long a browser may hold an idle keep-alive connection. Phones suspend
/// aggressively; reclaiming their sockets promptly keeps the table small.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: usize = 64;

/// SSE comment sent when nothing has happened, so that a phone NAT does not
/// silently reap a stream the browser still believes is open.
const HEARTBEAT: Duration = Duration::from_secs(20);

#[derive(Debug)]
struct Config {
    listen: SocketAddr,
    socket_path: PathBuf,
    token_path: PathBuf,
    host_label: String,
    /// `Some` admits callers by Tailscale identity and pairing.
    tailnet_owner: Option<Owner>,
    devices_path: PathBuf,
}

/// What a standalone invocation does besides serving.
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Serve,
    Url,
    Pair,
    Devices,
    Revoke(String),
}

fn usage() -> &'static str {
    "\
diri-web — serve a Dirijor daemon to a phone browser over a private network

USAGE:
    diri-web [--listen ADDR:PORT] [--socket PATH] [--token-file PATH] [--label NAME]
             [--admit token|tailnet] [--tailnet-owner LOGIN] [--devices-file PATH]
    diri-web url                       print the enrolment URL and exit
    diri-web pair                      mint a one-time pairing link (tailnet admission)
    diri-web devices                   list paired devices
    diri-web revoke NODE_ID            unpair one device; its connections close

OPTIONS:
    --listen ADDR:PORT   default 127.0.0.1:7380; must be loopback or Tailscale
    --socket PATH        daemon control socket (default: $DIRIJOR_SOCKET, else the
                         standard Dirijor application-support path)
    --token-file PATH    default ~/.config/dirijor/web.token, created if absent
    --label NAME         name shown in the frontend header (default: the hostname)
    --admit MODE         `token` (default): a shared bearer token. `tailnet`: each
                         connection is identified by this host's tailscaled (WhoIs);
                         only the owner's untagged, paired devices get in
    --tailnet-owner LOGIN
                         the Tailscale login admitted on a tagged host (implies
                         --admit tailnet; default: the user this node is logged in as)
    --devices-file PATH  default ~/.config/dirijor/web-devices.json

The token is minted on first run. Open the printed URL once on the phone; it is
exchanged for a cookie and the secret leaves the address bar. Under tailnet
admission there is no standing secret: `diri-web pair` prints a link holding a
single-use code valid for ten minutes, and the device is remembered by its
Tailscale node id.
"
}

pub fn run() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments
        .iter()
        .any(|argument| argument == "-h" || argument == "--help")
    {
        print!("{}", usage());
        return ExitCode::SUCCESS;
    }

    let config = match parse(&arguments) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("diri-web: {message}\n\n{}", usage());
            return ExitCode::from(2);
        }
    };

    let command = match command(&arguments) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("diri-web: {message}\n\n{}", usage());
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Serve | Command::Url => {}
        Command::Pair => {
            let devices = Devices::open(config.devices_path.clone());
            return match devices.mint_code() {
                Ok(code) => {
                    println!("{}", enrolment_url(&config.listen, &code));
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("diri-web: cannot write a pairing code: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        Command::Devices => {
            for device in Devices::open(config.devices_path.clone()).list() {
                println!(
                    "{}\t{}\t{}\t{}",
                    device.id, device.name, device.os, device.login
                );
            }
            return ExitCode::SUCCESS;
        }
        Command::Revoke(id) => {
            return match Devices::open(config.devices_path.clone()).revoke(&id) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => {
                    eprintln!("diri-web: no paired device {id:?}");
                    ExitCode::FAILURE
                }
                Err(error) => {
                    eprintln!("diri-web: cannot revoke {id:?}: {error}");
                    ExitCode::FAILURE
                }
            };
        }
    }

    let (auth, minted) = match Auth::load_or_create(&config.token_path) {
        Ok(pair) => pair,
        Err(error) => {
            eprintln!(
                "diri-web: cannot read or create {}: {error}",
                config.token_path.display()
            );
            return ExitCode::FAILURE;
        }
    };

    if command == Command::Url {
        println!("{}", enrolment_url(&config.listen, auth.token()));
        return ExitCode::SUCCESS;
    }

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("diri-web: cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(serve(config, auth, minted)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("diri-web: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse(arguments: &[String]) -> Result<Config, String> {
    let mut listen = format!("127.0.0.1:{DEFAULT_PORT}");
    let mut socket_path = None;
    let mut token_path = None;
    let mut host_label = None;
    let mut admit = None;
    let mut tailnet_owner = None;
    let mut devices_path = None;

    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        // Subcommands are handled by the caller.
        if matches!(argument, "url" | "pair" | "devices") {
            index += 1;
            continue;
        }
        if argument == "revoke" {
            index += 2;
            continue;
        }
        let value = || {
            arguments
                .get(index + 1)
                .cloned()
                .ok_or_else(|| format!("{argument} needs a value"))
        };
        match argument {
            "--listen" => {
                listen = value()?;
                index += 2;
            }
            "--socket" => {
                socket_path = Some(PathBuf::from(value()?));
                index += 2;
            }
            "--token-file" => {
                token_path = Some(PathBuf::from(value()?));
                index += 2;
            }
            "--label" => {
                host_label = Some(value()?);
                index += 2;
            }
            "--admit" => {
                admit = Some(value()?);
                index += 2;
            }
            "--tailnet-owner" => {
                tailnet_owner = Some(Owner::Login(value()?));
                index += 2;
            }
            "--devices-file" => {
                devices_path = Some(PathBuf::from(value()?));
                index += 2;
            }
            other => return Err(format!("unrecognised argument {other:?}")),
        }
    }

    let listen = resolve_listen(&listen)?;
    if !is_safe_plaintext_address(listen) {
        return Err(format!(
            "refusing to bind {listen}: this frontend carries a bearer over \
             plaintext HTTP, so it must sit on loopback or Tailscale"
        ));
    }

    let tailnet_owner = match (admit.as_deref(), tailnet_owner) {
        (None | Some("tailnet"), Some(owner)) => Some(owner),
        (Some("tailnet"), None) => Some(Owner::ThisNode),
        (None | Some("token"), None) => None,
        (Some("token"), Some(_)) => {
            return Err("--tailnet-owner needs --admit tailnet".into());
        }
        (Some(other), _) => return Err(format!("unknown --admit mode {other:?}")),
    };

    Ok(Config {
        listen,
        socket_path: socket_path.unwrap_or_else(default_socket_path),
        token_path: token_path.unwrap_or_else(auth::default_token_path),
        host_label: host_label.unwrap_or_else(default_host_label),
        tailnet_owner,
        devices_path: devices_path.unwrap_or_else(default_devices_path),
    })
}

fn command(arguments: &[String]) -> Result<Command, String> {
    let mut found = Command::Serve;
    let mut index = 0;
    while index < arguments.len() {
        let next = match arguments[index].as_str() {
            "url" => Command::Url,
            "pair" => Command::Pair,
            "devices" => Command::Devices,
            "revoke" => Command::Revoke(
                arguments
                    .get(index + 1)
                    .cloned()
                    .ok_or("revoke needs a device id")?,
            ),
            argument if argument.starts_with("--") => {
                index += 2;
                continue;
            }
            _ => {
                index += 1;
                continue;
            }
        };
        if found != Command::Serve {
            return Err("give at most one subcommand".into());
        }
        index += if matches!(next, Command::Revoke(_)) {
            2
        } else {
            1
        };
        found = next;
    }
    Ok(found)
}

fn default_devices_path() -> PathBuf {
    auth::default_token_path().with_file_name("web-devices.json")
}

/// Accepts either a literal `ADDR:PORT` or a `HOST:PORT` to resolve.
///
/// Hostnames matter for the tailnet case: a unit file pinned to a literal
/// Tailscale IP crash-loops forever if that address ever changes, whereas
/// MagicDNS (`forge:7380`) follows the device. IPv4 is preferred because
/// that is what the tailnet hands out first and what people type.
fn resolve_listen(raw: &str) -> Result<SocketAddr, String> {
    use std::net::ToSocketAddrs;

    if let Ok(address) = raw.parse::<SocketAddr>() {
        return Ok(address);
    }
    let mut resolved: Vec<SocketAddr> = raw
        .to_socket_addrs()
        .map_err(|error| format!("{raw:?} is not an ADDR:PORT and did not resolve: {error}"))?
        .collect();
    resolved.sort_by_key(|address| u8::from(address.is_ipv6()));
    resolved
        .into_iter()
        .next()
        .ok_or_else(|| format!("{raw:?} resolved to no addresses"))
}

fn default_socket_path() -> PathBuf {
    if let Some(from_environment) = std::env::var_os(DirijorEnv::SOCKET) {
        return PathBuf::from(from_environment);
    }
    let home = diri_platform::home_dir()
        .map(|p| p.into_os_string())
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    DirijorPaths::socket(home)
}

fn default_host_label() -> String {
    std::env::var("DIRIJOR_HOST_LABEL")
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "local".to_string())
}

fn enrolment_url(listen: &SocketAddr, token: &str) -> String {
    format!("http://{listen}/?token={token}")
}

async fn serve(config: Config, auth: Auth, minted: bool) -> Result<(), String> {
    let client = Arc::new(DaemonClient::with_socket_path(config.socket_path.clone()));
    client.connect();

    // Not fatal: the daemon may still be starting, and the client reconnects
    // on its own. Say so rather than exiting, so a systemd unit ordering
    // problem does not look like a crash.
    match client.wait_until_connected(Duration::from_secs(5)).await {
        Ok(hello) => eprintln!(
            "diri-web: connected to dirijord (wire {}) at {}",
            hello.proto,
            config.socket_path.display()
        ),
        Err(error) => eprintln!(
            "diri-web: daemon not up yet at {} ({error}); serving anyway and reconnecting",
            config.socket_path.display()
        ),
    }

    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|error| format!("cannot bind {}: {error}", config.listen))?;

    let tailnet = match config.tailnet_owner {
        None => None,
        Some(owner) => {
            let local = LocalApi::discover().ok_or(
                "--admit tailnet needs Tailscale on this host, and found no tailscaled \
                 LocalAPI or tailscale CLI",
            )?;
            let devices = Arc::new(Devices::open(config.devices_path));
            Some(Arc::new(tailnet::Admission::new(local, owner, devices)))
        }
    };

    eprintln!("diri-web: listening on http://{}", config.listen);
    let _ = minted;
    if tailnet.is_some() {
        eprintln!("diri-web: admitting paired tailnet devices; `diri-web pair` pairs one");
    } else {
        eprintln!("diri-web: use `diri-web url` to show the private pairing link");
    }

    let api = Arc::new(Api {
        client: Arc::clone(&client),
        auth,
        host_label: config.host_label,
        inputs: Default::default(),
        tailnet,
    });

    serve_connections(listener, api).await
}

/// Where the Mac app keeps its phone pairing token: owner-only, in the app's
/// support directory, so re-enabling access keeps phones paired.
#[must_use]
pub fn phone_token_path() -> PathBuf {
    let home = diri_platform::home_dir()
        .map(|p| p.into_os_string())
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    DirijorPaths::app_support(home).join(PHONE_TOKEN_FILE)
}

const PHONE_TOKEN_FILE: &str = "phone-access.token";

/// Mints a new pairing code at `token_path`. Every phone paired with the old
/// one must pair again; a running gateway keeps the old code until restarted.
pub fn reset_phone_pairing(token_path: &std::path::Path) -> Result<(), String> {
    Auth::rotate(token_path)
        .map(drop)
        .map_err(|error| format!("Cannot create a new pairing code: {error}"))
}

/// Where the Mac app keeps the devices paired under tailnet admission.
#[must_use]
pub fn phone_devices_path() -> PathBuf {
    phone_token_path().with_file_name(PHONE_DEVICES_FILE)
}

const PHONE_DEVICES_FILE: &str = "phone-devices.json";

/// The link a phone opens (or scans) to present `secret` to the gateway on
/// `address`: the bearer token, or a one-time pairing code.
#[must_use]
pub fn phone_link(address: std::net::Ipv4Addr, secret: &str) -> String {
    enrolment_url(&SocketAddr::from((address, DEFAULT_PORT)), secret)
}

/// How the app's gateway decides who may connect.
pub enum PhoneAdmission {
    /// The persistent bearer token at this path.
    Token(PathBuf),
    /// Tailscale WhoIs through this LocalAPI, then pairing against `devices`.
    Tailnet {
        local: LocalApi,
        devices: Arc<Devices>,
    },
}

/// App-owned gateway. Under token admission its credential persists at the
/// token path (created owner-only on first enable), so turning access off
/// and on again, or restarting Diri, keeps existing phone pairings. Under
/// tailnet admission the returned link holds a fresh one-time pairing code.
pub async fn start_phone_access(
    address: std::net::Ipv4Addr,
    client: Arc<DaemonClient>,
    label: String,
    admission: PhoneAdmission,
) -> Result<(String, tokio::task::JoinHandle<Result<(), String>>), String> {
    let [a, b, _, _] = address.octets();
    if a != 100 || !(64..=127).contains(&b) {
        return Err("Phone access requires a connected Tailscale IPv4 address".into());
    }
    client
        .wait_until_connected(Duration::from_secs(10))
        .await
        .map_err(|error| format!("Diri is still connecting to its Engine: {error}"))?;
    let listener = TcpListener::bind((address, DEFAULT_PORT))
        .await
        .map_err(|error| format!("Cannot enable phone access: {error}"))?;
    let (auth, url, tailnet) = match admission {
        PhoneAdmission::Token(token_path) => {
            let (auth, _) = Auth::load_or_create(&token_path)
                .map_err(|error| format!("Cannot read the phone pairing code: {error}"))?;
            let url = phone_link(address, auth.token());
            (auth, url, None)
        }
        PhoneAdmission::Tailnet { local, devices } => {
            let code = devices
                .mint_code()
                .map_err(|error| format!("Cannot create a pairing code: {error}"))?;
            // Never presented: tailnet admission ignores the bearer token.
            let auth =
                Auth::ephemeral().map_err(|error| format!("Cannot start phone access: {error}"))?;
            let gate = tailnet::Admission::new(local, Owner::ThisNode, devices);
            (auth, phone_link(address, &code), Some(Arc::new(gate)))
        }
    };
    let api = Arc::new(Api {
        client,
        auth,
        host_label: label,
        inputs: Default::default(),
        tailnet,
    });
    Ok((url, tokio::spawn(serve_connections(listener, api))))
}

async fn serve_connections(listener: TcpListener, api: Arc<Api>) -> Result<(), String> {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        // Dropping this owner revokes keep-alive and SSE connections too.
        let accepted = tokio::select! {
            result = listener.accept(), if connections.len() < MAX_CONNECTIONS => result,
            _ = connections.join_next(), if !connections.is_empty() => continue,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                eprintln!("diri-web: accept failed: {error}");
                continue;
            }
        };
        let api = Arc::clone(&api);
        connections.spawn(async move {
            if let Err(error) = handle(stream, peer, api).await {
                // One phone falling off a cell tower is normal; log at a
                // volume that does not drown the journal.
                let _ = (peer, error);
            }
        });
    }
}

async fn handle(stream: TcpStream, peer: SocketAddr, api: Arc<Api>) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut authenticated = false;
    // A connection's peer cannot change, so it is identified once.
    let identity = match &api.tailnet {
        Some(gate) => Some(gate.identify(peer.ip()).await),
        None => None,
    };

    loop {
        let timeout = if authenticated {
            IDLE_TIMEOUT
        } else {
            AUTH_TIMEOUT
        };
        let request = match tokio::time::timeout(timeout, http::read_request(&mut reader)).await {
            Err(_) => return Ok(()),
            Ok(Ok(Some(request))) => request,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(http::ReadError::Io(error))) => return Err(error),
            Ok(Err(http::ReadError::Malformed(reason))) => {
                let _ =
                    http::write_response(&mut write_half, None, Response::error(400, reason)).await;
                return Ok(());
            }
        };
        let mut request = request;
        let mut guard = None;
        match (&api.tailnet, &identity) {
            (Some(gate), Some(Ok(identity))) => match gate.admits(identity, &request) {
                Ok(()) => {
                    request.admitted = true;
                    guard = Some((Arc::clone(&gate.devices), identity.node.clone()));
                }
                // Unpaired falls through to the page and the 401 below, so
                // the browser can show where to enter a pairing code.
                Err(tailnet::Refusal::NotPaired) => {}
                Err(refusal) => {
                    let _ = http::write_response(&mut write_half, None, refusal.response()).await;
                    return Ok(());
                }
            },
            // Refused by identity: say why, once, and hang up.
            (Some(_), Some(Err(refusal))) => {
                let _ = http::write_response(&mut write_half, None, refusal.response()).await;
                return Ok(());
            }
            _ => {}
        }
        authenticated |= api.authorizes(&request);
        let refusal = || match api.tailnet {
            Some(_) => tailnet::Refusal::NotPaired.response(),
            None => auth::unauthorized(),
        };

        // The event stream owns the connection for its lifetime.
        if request.method == "GET" && request.path == "/api/events" {
            if !api.authorizes(&request) {
                let _ = http::write_response(&mut write_half, None, refusal()).await;
                return Ok(());
            }
            return until_revoked(guard, stream_events(&mut write_half, &api)).await;
        }
        if request.method == "GET"
            && let ["api", "session", id, "stream"] = request.segments().as_slice()
        {
            if !api.authorizes(&request) {
                let _ = http::write_response(&mut write_half, None, refusal()).await;
                return Ok(());
            }
            let id = diri_proto::SessionId((*id).to_owned());
            return until_revoked(
                guard,
                stream_terminal(&mut reader, &mut write_half, &api, id),
            )
            .await;
        }

        let response = respond(&request, &api).await;
        if !http::write_response(&mut write_half, Some(&request), response).await? || !authenticated
        {
            return Ok(());
        }
    }
}

async fn respond(request: &Request, api: &Api) -> Response {
    match (request.method.as_str(), request.path.as_str()) {
        // The page itself is public HTML; it renders nothing until an
        // authenticated API call succeeds, and shipping it unauthenticated is
        // what lets the login screen exist at all.
        ("GET" | "HEAD", "/") => {
            if api.tailnet.is_some() && request.query.contains_key("token") {
                // The pairing code was spent (or refused) on arrival; take it
                // out of the address bar either way. No cookie: identity
                // comes from the tailnet.
                return Response::redirect("/");
            }
            if api.auth.arrived_in_url(request) {
                // Trade the URL token for a cookie so the secret is not left
                // in history, or in the referrer of anything the page loads.
                return Response::redirect("/").with_header("Set-Cookie", api.auth.cookie_header());
            }
            Response::text(200, "text/html; charset=utf-8", ui::INDEX_HTML)
        }
        ("GET" | "HEAD", "/manifest.webmanifest") => Response::text(
            200,
            "application/manifest+json; charset=utf-8",
            ui::MANIFEST,
        ),
        ("GET" | "HEAD", "/icon.svg") => {
            Response::text(200, "image/svg+xml; charset=utf-8", ui::ICON_SVG)
        }
        _ => api.route(request).await,
    }
}

/// One phone terminal view: a read-only Engine preview republished as
/// `snapshot`/`diff`/`exit` SSE. The stream task owns the terminal state;
/// this loop only writes finished frames, so a slow socket never stalls the
/// preview. Dropping either end closes the preview, which removes its Engine
/// sink.
async fn stream_terminal<R, W>(
    reader: &mut R,
    writer: &mut W,
    api: &Api,
    session: diri_proto::SessionId,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    use tokio::io::AsyncReadExt as _;
    // Subscribed before the seed so an exit cannot fall in between.
    let events = match api.client.subscribe_events().await {
        Ok(receiver) => receiver,
        Err(_) => api.client.events(),
    };
    let (preview, mirror) = match terminal::open(&api.client, &session).await {
        Ok(opened) => opened,
        Err(error) => {
            let _ = http::write_response(writer, None, api::open_error(&error)).await;
            return Ok(());
        }
    };
    http::begin_event_stream(writer).await?;
    let (out, mut frames) = tokio::sync::mpsc::channel(terminal::OUT_QUEUE);
    let task = tokio::spawn(terminal::pump(
        preview,
        mirror,
        Arc::clone(&api.client),
        session,
        events,
        out,
    ));
    let _abort = AbortOnDrop(task);
    let mut scratch = [0_u8; 512];
    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else { return Ok(()) };
                writer.write_all(frame.as_bytes()).await?;
                writer.flush().await?;
            }
            // The phone sends nothing on a stream; EOF is its disconnect.
            read = reader.read(&mut scratch) => {
                if read.map_or(true, |read| read == 0) {
                    return Ok(());
                }
            }
            () = tokio::time::sleep(HEARTBEAT) => {
                writer.write_all(b": ping\n\n").await?;
                writer.flush().await?;
            }
        }
    }
}

/// Runs a stream until it ends or, under tailnet admission, its device is
/// revoked; dropping the stream closes its Engine preview.
async fn until_revoked(
    guard: Option<(Arc<Devices>, String)>,
    stream: impl std::future::Future<Output = std::io::Result<()>>,
) -> std::io::Result<()> {
    let Some((devices, node)) = guard else {
        return stream.await;
    };
    tokio::select! {
        result = stream => result,
        () = tailnet::revoked(&devices, &node) => Ok(()),
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Republishes the daemon's event feed as SSE.
///
/// The frontend uses this only as a *hint to refetch*: the payloads are the
/// daemon's own event params, and the phone re-reads the session list rather
/// than trying to apply deltas. That keeps a dropped stream from ever leaving
/// the UI subtly wrong — the failure mode is a stale list, which the next
/// event or the next poll corrects.
async fn stream_events<W>(writer: &mut W, api: &Api) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    http::begin_event_stream(writer).await?;

    let mut events = match api.client.subscribe_events().await {
        Ok(receiver) => receiver,
        Err(_) => api.client.events(),
    };
    let mut connection = api.client.connection_state();
    // Subscribed first, then seeded, so no completion falls in between.
    let mut turns = turns::TurnWatcher::default();
    api.seed_turns(&mut turns).await;

    http::write_event(
        writer,
        "ready",
        &serde_json::json!({ "host": api.host_label }),
    )
    .await?;

    loop {
        tokio::select! {
            received = events.recv() => match received {
                Ok(event) => {
                    http::write_event(
                        writer,
                        &event.name,
                        &serde_json::json!({ "seq": event.seq, "params": event.params }),
                    )
                    .await?;
                    if let Some((session, turn)) = turns.observe(&event.name, &event.params) {
                        let finished = api.turn_finished(session, turn).await;
                        http::write_event(writer, turns::TURN_FINISHED, &finished).await?;
                    }
                }
                // Lagged: the phone was backgrounded and missed events. Tell
                // it to refetch wholesale rather than pretending it is current.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    api.seed_turns(&mut turns).await;
                    http::write_event(writer, "resync", &serde_json::json!({})).await?;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            },
            changed = connection.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let connected = matches!(*connection.borrow(), diri_client::ConnectionState::Connected(_));
                let state = format!("{:?}", *connection.borrow());
                if connected {
                    api.seed_turns(&mut turns).await;
                }
                http::write_event(writer, "daemon", &serde_json::json!({ "state": state })).await?;
            }
            () = tokio::time::sleep(HEARTBEAT) => {
                // A bare SSE comment: keeps the socket warm without waking the
                // page's event handlers.
                writer.write_all(b": ping\n\n").await?;
                writer.flush().await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn phone_entry_point_refuses_lan_and_public_addresses() {
        for address in ["127.0.0.1", "192.168.1.1", "0.0.0.0", "8.8.8.8"] {
            let client = Arc::new(DaemonClient::with_socket_path(
                "/nonexistent/diri-phone-test.sock",
            ));
            let token = tempfile::tempdir().unwrap();
            assert!(
                start_phone_access(
                    address.parse().unwrap(),
                    client,
                    "test".into(),
                    PhoneAdmission::Token(token.path().join("phone.token")),
                )
                .await
                .is_err()
            );
            // Refused before anything is written.
            assert!(!token.path().join("phone.token").exists());
        }
    }

    #[tokio::test]
    async fn disabling_gateway_closes_existing_authenticated_connections() {
        use tokio::io::AsyncReadExt as _;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let auth = Auth::temporary().unwrap();
        let token = auth.token().to_owned();
        assert_ne!(token, Auth::temporary().unwrap().token());
        let api = Arc::new(Api {
            client: Arc::new(DaemonClient::with_socket_path(
                "/nonexistent/diri-phone-test.sock",
            )),
            auth,
            host_label: "test".into(),
            inputs: Default::default(),
            tailnet: None,
        });
        let server = tokio::spawn(serve_connections(listener, api));
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                format!(
                    "GET /api/health HTTP/1.1\r\nHost: {address}\r\nX-Diri-Token: {token}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = [0; 4096];
        let read = stream.read(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response[..read]).starts_with("HTTP/1.1 200"));
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        // Drain any already-written bytes; every child must reach EOF.
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut Vec::new()))
            .await
            .unwrap()
            .unwrap();
        assert!(TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn the_event_stream_announces_a_finished_turn() {
        use diri_proto::Method;
        use tokio::io::{AsyncBufReadExt as _, BufReader as TokioBufReader};
        let engine = crate::testing::fake_engine(|method, _| match method {
            Method::EVENTS_SUBSCRIBE => json!({"subscribed": true}),
            Method::VOICE_LATEST_SPOKEN => json!({"turn": 2, "text": "Tests pass."}),
            _ => json!({}),
        })
        .await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let auth = Auth::temporary().unwrap();
        let token = auth.token().to_owned();
        let api = Arc::new(Api {
            client: Arc::clone(&engine.client),
            auth,
            host_label: "test".into(),
            inputs: Default::default(),
            tailnet: None,
        });
        let server = tokio::spawn(serve_connections(listener, api));
        let stream = TcpStream::connect(address).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        write_half
            .write_all(
                format!(
                    "GET /api/events HTTP/1.1\r\nHost: {address}\r\nX-Diri-Token: {token}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut lines = TokioBufReader::new(read_half).lines();
        let mut next_data = async |event: &str| loop {
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .expect("event in time")
                .unwrap()
                .expect("stream open");
            if line == format!("event: {event}") {
                let data = lines.next_line().await.unwrap().unwrap();
                let data = data.strip_prefix("data: ").expect("data line");
                break serde_json::from_str::<serde_json::Value>(data).unwrap();
            }
        };
        next_data("ready").await;

        let attention = |sequence: u64, working: bool, completed: bool| {
            json!({
                "id": "s_a",
                "attentionState": {
                    "version": 1, "epoch": "e", "sequence": sequence, "turn": 2,
                    "working": working,
                    "events": if completed { json!([{
                        "sequence": sequence, "turn": 2, "kind": "completion",
                        "occurredAt": 0.0, "resolved": false, "blocking": false,
                        "detail": null,
                    }]) } else { json!([]) },
                },
            })
        };
        for (sequence, working, completed) in [(1, true, false), (2, false, true)] {
            engine
                .events
                .send((
                    diri_proto::EventName::SESSION_UPDATED.into(),
                    attention(sequence, working, completed),
                ))
                .await
                .unwrap();
        }
        assert_eq!(
            next_data("turn_finished").await,
            json!({"sessionID": "s_a", "turn": 2, "spoken": "Tests pass."})
        );
        server.abort();
    }

    fn grid_frame(update: &diri_proto::grid::GridUpdate) -> Vec<u8> {
        use diri_proto::frames::{Frame, FrameCodec};
        FrameCodec::encode(&Frame::grid(update).unwrap()).unwrap()
    }

    fn grid_update(full: bool, rows: &[(u16, &str)]) -> diri_proto::grid::GridUpdate {
        use diri_proto::grid::{ChangedRow, GridCell, TermColor, TermStyle};
        diri_proto::grid::GridUpdate {
            cols: 10,
            rows: 4,
            cursor_col: 2,
            cursor_row: 1,
            cursor_visible: true,
            is_full_snapshot: full,
            changed_rows: rows
                .iter()
                .map(|(y, text)| {
                    let mut cells: Vec<GridCell> = text
                        .chars()
                        .map(|ch| {
                            GridCell::new(
                                ch as u32,
                                TermColor::Ansi(1),
                                TermColor::DefaultInverted,
                                TermStyle::BOLD,
                            )
                        })
                        .collect();
                    cells.resize(10, GridCell::BLANK);
                    ChangedRow::new(*y, cells)
                })
                .collect(),
            cursor_style: Default::default(),
            history: None,
        }
    }

    async fn serve_engine(
        engine: &crate::testing::FakeEngine,
    ) -> (
        SocketAddr,
        String,
        tokio::task::JoinHandle<Result<(), String>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let auth = Auth::temporary().unwrap();
        let token = auth.token().to_owned();
        let api = Arc::new(Api {
            client: Arc::clone(&engine.client),
            auth,
            host_label: "test".into(),
            inputs: Default::default(),
            tailnet: None,
        });
        (
            address,
            token,
            tokio::spawn(serve_connections(listener, api)),
        )
    }

    /// The phone's terminal stream end to end: an Engine preview's seed
    /// becomes `snapshot`, its next update a `diff` of only the changed row,
    /// the session's exit an `exit`, and hanging up closes the preview.
    #[tokio::test]
    async fn the_terminal_stream_turns_a_preview_into_snapshot_diff_and_exit() {
        use diri_proto::Method;
        use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader as TokioBufReader};
        let mut engine = crate::testing::fake_engine(|method, _| match method {
            Method::EVENTS_SUBSCRIBE => json!({"subscribed": true}),
            Method::SESSION_READ_SCROLLBACK_CELLS => json!({
                "payload": "", "firstRow": 0, "rowCount": 0, "totalRows": 46,
                "liveStartRow": 42, "cols": 10, "contentSeq": 1,
            }),
            _ => json!({}),
        })
        .await;
        let (address, token, server) = serve_engine(&engine).await;

        // No credential, no preview.
        let mut anonymous = TcpStream::connect(address).await.unwrap();
        anonymous
            .write_all(b"GET /api/session/s_a/stream HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut refused = String::new();
        anonymous.read_to_string(&mut refused).await.unwrap();
        assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");

        let stream = TcpStream::connect(address).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        write_half
            .write_all(
                format!(
                    "GET /api/session/s_a/stream HTTP/1.1\r\nHost: {address}\r\nX-Diri-Token: {token}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let (id, mut preview) =
            tokio::time::timeout(Duration::from_secs(5), engine.previews.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(id, "s_a");
        assert!(
            engine.previews.try_recv().is_err(),
            "one preview per stream"
        );
        preview
            .write_all(&grid_frame(&grid_update(true, &[(0, "hello")])))
            .await
            .unwrap();
        let modes = diri_proto::frames::Frame::modes_with_bracketed_paste(
            true,
            true,
            diri_proto::terminal::MouseModes::OFF,
        );
        preview
            .write_all(&diri_proto::frames::FrameCodec::encode(&modes).unwrap())
            .await
            .unwrap();

        let mut lines = TokioBufReader::new(read_half).lines();
        let mut next = async |event: &str| loop {
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .expect("event in time")
                .unwrap()
                .expect("stream open");
            if line == format!("event: {event}") {
                let data = lines.next_line().await.unwrap().unwrap();
                break serde_json::from_str::<serde_json::Value>(
                    data.strip_prefix("data: ").unwrap(),
                )
                .unwrap();
            }
            assert!(!line.starts_with("event:"), "expected {event}, got {line}");
        };
        let snapshot = next("snapshot").await;
        assert_eq!(snapshot["cols"], 10);
        assert_eq!(snapshot["rows"], 4);
        assert_eq!(snapshot["historySize"], 42);
        assert_eq!(
            snapshot["cursor"],
            json!({"x": 2, "y": 1, "visible": true, "shape": "block"})
        );
        assert_eq!(
            snapshot["modes"],
            json!({"altScreen": true, "appCursor": false, "bracketedPaste": true, "mouse": false})
        );
        assert_eq!(snapshot["palette"]["ansi"].as_array().unwrap().len(), 16);
        assert_eq!(
            snapshot["lines"][0],
            json!({"runs": [{"t": "hello", "fg": 1, "bg": null, "a": 1}], "wrapped": false})
        );
        assert_eq!(snapshot["lines"][3], json!({"runs": [], "wrapped": false}));

        preview
            .write_all(&grid_frame(&grid_update(false, &[(2, "next")])))
            .await
            .unwrap();
        let diff = next("diff").await;
        assert_eq!(
            diff["lines"],
            json!({"2": {"runs": [{"t": "next", "fg": 1, "bg": null, "a": 1}], "wrapped": false}})
        );
        assert!(diff["seq"].as_u64() > snapshot["seq"].as_u64());
        assert_eq!(diff["historySize"], 42);

        let exited: diri_proto::ExitInfo =
            serde_json::from_value(json!({"reason": "exited", "code": 3})).unwrap();
        engine
            .events
            .send((
                diri_proto::EventName::SESSION_UPDATED.into(),
                json!({
                    "id": "s_a",
                    "status": diri_proto::SessionStatus::Exited(exited),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(next("exit").await, json!({"code": 3}));

        // The stream is over; its preview must not outlive it.
        drop(write_half);
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), preview.read_to_end(&mut rest))
            .await
            .expect("preview closed")
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn closing_the_phone_closes_the_preview() {
        use diri_proto::Method;
        use tokio::io::AsyncReadExt as _;
        let mut engine = crate::testing::fake_engine(|method, _| match method {
            Method::EVENTS_SUBSCRIBE => json!({"subscribed": true}),
            _ => json!({}),
        })
        .await;
        let (address, token, server) = serve_engine(&engine).await;
        let mut phone = TcpStream::connect(address).await.unwrap();
        phone
            .write_all(
                format!(
                    "GET /api/session/s_b/stream HTTP/1.1\r\nHost: {address}\r\nX-Diri-Token: {token}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let (_, mut preview) = engine.previews.recv().await.unwrap();
        preview
            .write_all(&grid_frame(&grid_update(true, &[])))
            .await
            .unwrap();
        preview
            .write_all(
                &diri_proto::frames::FrameCodec::encode(&diri_proto::frames::Frame::modes(
                    false,
                    diri_proto::terminal::MouseModes::OFF,
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        let mut head = [0; 64];
        let read = phone.read(&mut head).await.unwrap();
        assert!(head[..read].starts_with(b"HTTP/1.1 200"));
        drop(phone);
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), preview.read_to_end(&mut rest))
            .await
            .expect("preview closed when the phone hung up")
            .unwrap();
        server.abort();
    }

    fn arguments(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn the_default_listener_is_loopback() {
        let config = parse(&[]).expect("defaults parse");
        assert_eq!(
            config.listen.to_string(),
            format!("127.0.0.1:{DEFAULT_PORT}")
        );
    }

    #[test]
    fn a_tailscale_listener_is_accepted() {
        let config =
            parse(&arguments(&["--listen", "100.66.149.100:7380"])).expect("tailnet address");
        assert_eq!(config.listen.to_string(), "100.66.149.100:7380");
    }

    /// The single most consequential mistake this binary could make is
    /// exposing session control to the internet, so it is a parse-time error
    /// rather than a runtime warning.
    #[test]
    fn a_public_or_wildcard_listener_is_refused() {
        for address in ["0.0.0.0:7380", "8.8.8.8:7380", "[::]:7380"] {
            let error = parse(&arguments(&["--listen", address])).expect_err("must refuse");
            assert!(error.contains("refusing to bind"), "{address}: {error}");
        }
    }

    #[test]
    fn an_unencrypted_lan_listener_is_refused() {
        for address in ["192.168.1.20:7380", "10.0.0.20:7380", "[fd12::1]:7380"] {
            let error = parse(&arguments(&["--listen", address])).expect_err("must refuse");
            assert!(error.contains("plaintext HTTP"), "{address}: {error}");
        }
    }

    #[test]
    fn unknown_arguments_are_refused_rather_than_ignored() {
        let error = parse(&arguments(&["--public"])).expect_err("must refuse");
        assert!(error.contains("unrecognised"));
    }

    #[test]
    fn a_flag_without_a_value_is_an_error() {
        let error = parse(&arguments(&["--listen"])).expect_err("must refuse");
        assert!(error.contains("needs a value"));
    }

    #[test]
    fn a_hostname_listener_resolves_and_prefers_ipv4() {
        // `localhost` resolves to both families on most hosts; the v4 entry
        // must win so a unit file written as `host:port` binds where people
        // expect.
        let address = resolve_listen("localhost:7380").expect("resolves");
        assert!(address.is_ipv4(), "expected IPv4, got {address}");
        assert_eq!(address.port(), 7380);
    }

    #[test]
    fn a_resolved_hostname_still_has_to_be_secure() {
        // Resolution must not become a way around the public-bind refusal.
        let error = parse(&arguments(&["--listen", "one.one.one.one:7380"]))
            .expect_err("public hostname must be refused");
        assert!(error.contains("refusing to bind"), "{error}");
    }

    #[test]
    fn an_unresolvable_hostname_says_so() {
        let error =
            parse(&arguments(&["--listen", "no-such-host.invalid:7380"])).expect_err("must refuse");
        assert!(error.contains("did not resolve"), "{error}");
    }

    #[test]
    fn the_enrolment_url_carries_the_token_once() {
        let url = enrolment_url(&"100.66.149.100:7380".parse().unwrap(), "deadbeef");
        assert_eq!(url, "http://100.66.149.100:7380/?token=deadbeef");
    }

    #[cfg(unix)]
    mod tailnet_admission {
        use super::*;
        use crate::tailnet::fake::{self, OWNER};
        use tokio::io::AsyncReadExt as _;

        const PHONE: &str = "100.64.0.2";

        struct Gateway {
            address: SocketAddr,
            devices: Arc<Devices>,
            gate: Arc<tailnet::Admission>,
            token: String,
            local: fake::FakeLocalApi,
            server: tokio::task::JoinHandle<Result<(), String>>,
            _dir: tempfile::TempDir,
        }

        impl Drop for Gateway {
            fn drop(&mut self) {
                self.server.abort();
            }
        }

        async fn gateway(client: Arc<DaemonClient>) -> Gateway {
            let local = fake::start().await;
            {
                let mut whois = local.whois.lock().unwrap();
                whois.insert(PHONE.into(), fake::device("nPhone", "phone", OWNER, &[]));
                whois.insert("100.64.0.3".into(), fake::device("nThem", "them", 99, &[]));
            }
            let dir = tempfile::tempdir().unwrap();
            let devices = Arc::new(Devices::open(dir.path().join("devices.json")));
            let gate = Arc::new(tailnet::Admission::new(
                local.api(),
                Owner::ThisNode,
                Arc::clone(&devices),
            ));
            *gate.test_peer.lock().unwrap() = Some(PHONE.parse().unwrap());
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let auth = Auth::temporary().unwrap();
            let token = auth.token().to_owned();
            let api = Arc::new(Api {
                client,
                auth,
                host_label: "test".into(),
                inputs: Default::default(),
                tailnet: Some(Arc::clone(&gate)),
            });
            Gateway {
                address,
                devices,
                gate,
                token,
                local,
                server: tokio::spawn(serve_connections(listener, api)),
                _dir: dir,
            }
        }

        fn offline_client() -> Arc<DaemonClient> {
            Arc::new(DaemonClient::with_socket_path(
                "/nonexistent/diri-tailnet-test.sock",
            ))
        }

        async fn get(address: SocketAddr, target: &str, headers: &str) -> String {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream
                .write_all(
                    format!(
                        "GET {target} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n{headers}\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = String::new();
            tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response))
                .await
                .unwrap()
                .unwrap();
            response
        }

        #[tokio::test]
        async fn an_owner_device_gets_in_only_after_pairing_and_needs_no_secret_after() {
            let gateway = gateway(offline_client()).await;
            let refused = get(gateway.address, "/api/health", "").await;
            assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");
            assert!(refused.contains("\"code\":\"device_not_paired\""));

            let code = gateway.devices.mint_code().unwrap();
            let paired = get(
                gateway.address,
                "/api/health",
                &format!("X-Diri-Token: {code}\r\n"),
            )
            .await;
            assert!(paired.starts_with("HTTP/1.1 200"), "{paired}");
            assert_eq!(gateway.devices.list()[0].id, "nPhone");

            // Identity, not a secret, admits it from now on.
            let later = get(gateway.address, "/api/health", "").await;
            assert!(later.starts_with("HTTP/1.1 200"), "{later}");
        }

        #[tokio::test]
        async fn the_bearer_token_is_not_a_way_in() {
            let gateway = gateway(offline_client()).await;
            for header in [
                "Authorization: Bearer",
                "X-Diri-Token:",
                "Cookie: diri_web_token=",
            ] {
                let response = get(
                    gateway.address,
                    "/api/health",
                    &format!("{header} {}\r\n", gateway.token),
                )
                .await;
                assert!(response.starts_with("HTTP/1.1 401"), "{response}");
            }
        }

        #[tokio::test]
        async fn a_pairing_link_leaves_the_address_bar_without_a_cookie() {
            let gateway = gateway(offline_client()).await;
            let code = gateway.devices.mint_code().unwrap();
            let landing = get(gateway.address, &format!("/?token={code}"), "").await;
            assert!(landing.starts_with("HTTP/1.1 303"), "{landing}");
            assert!(!landing.to_ascii_lowercase().contains("set-cookie"));
            assert!(gateway.devices.is_paired("nPhone"));
        }

        #[tokio::test]
        async fn other_users_tagged_nodes_and_outages_are_refused_with_reasons() {
            let gateway = gateway(offline_client()).await;
            *gateway.gate.test_peer.lock().unwrap() = Some("100.64.0.3".parse().unwrap());
            let code = gateway.devices.mint_code().unwrap();
            let them = get(
                gateway.address,
                "/api/health",
                &format!("X-Diri-Token: {code}\r\n"),
            )
            .await;
            assert!(them.starts_with("HTTP/1.1 403"), "{them}");
            assert!(them.contains("\"code\":\"not_the_owner\""));
            // A refused caller cannot spend the code.
            assert!(gateway.devices.list().is_empty());

            *gateway.gate.test_peer.lock().unwrap() = Some("100.64.0.9".parse().unwrap());
            let stranger = get(gateway.address, "/", "").await;
            assert!(
                stranger.contains("\"code\":\"not_a_tailnet_peer\""),
                "{stranger}"
            );

            *gateway.local.status.lock().unwrap() = serde_json::json!({"nope": true});
            *gateway.gate.test_peer.lock().unwrap() = Some("100.64.0.8".parse().unwrap());
            let outage = get(gateway.address, "/api/health", "").await;
            assert!(outage.starts_with("HTTP/1.1 503"), "{outage}");
            assert!(outage.contains("\"code\":\"tailnet_identity_unavailable\""));
        }

        #[tokio::test]
        async fn revoking_a_device_closes_its_live_event_stream() {
            use diri_proto::Method;
            use tokio::io::{AsyncBufReadExt as _, BufReader as TokioBufReader};
            let engine = crate::testing::fake_engine(|method, _| match method {
                Method::EVENTS_SUBSCRIBE => json!({"subscribed": true}),
                _ => json!({}),
            })
            .await;
            let gateway = gateway(Arc::clone(&engine.client)).await;
            let code = gateway.devices.mint_code().unwrap();
            let stream = TcpStream::connect(gateway.address).await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            write_half
                .write_all(
                    format!(
                        "GET /api/events HTTP/1.1\r\nHost: {}\r\nX-Diri-Token: {code}\r\n\r\n",
                        gateway.address
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut lines = TokioBufReader::new(read_half).lines();
            loop {
                let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                    .await
                    .unwrap()
                    .unwrap()
                    .expect("stream open");
                if line == "event: ready" {
                    break;
                }
            }
            assert!(gateway.devices.revoke("nPhone").unwrap());
            // The server hangs up: EOF well before any heartbeat would fire.
            let closed = tokio::time::timeout(Duration::from_secs(2), async {
                while let Ok(Some(_)) = lines.next_line().await {}
            })
            .await;
            assert!(closed.is_ok(), "revoked stream stayed open");
            let after = get(gateway.address, "/api/health", "").await;
            assert!(after.contains("\"code\":\"device_not_paired\""), "{after}");
        }

        #[test]
        fn standalone_flags_choose_the_admission_mode() {
            let args = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
            let token = parse(&args(&[])).unwrap();
            assert_eq!(token.tailnet_owner, None);
            let tailnet = parse(&args(&["--admit", "tailnet"])).unwrap();
            assert_eq!(tailnet.tailnet_owner, Some(Owner::ThisNode));
            let tagged = parse(&args(&["--tailnet-owner", "me@example.com"])).unwrap();
            assert_eq!(
                tagged.tailnet_owner,
                Some(Owner::Login("me@example.com".into()))
            );
            assert!(parse(&args(&["--admit", "token", "--tailnet-owner", "x"])).is_err());
            assert!(parse(&args(&["--admit", "open"])).is_err());
            assert_eq!(
                command(&args(&["--admit", "tailnet", "revoke", "nPhone"])).unwrap(),
                Command::Revoke("nPhone".into())
            );
            assert_eq!(command(&args(&["pair"])).unwrap(), Command::Pair);
            assert!(command(&args(&["pair", "devices"])).is_err());
            let revoke = parse(&args(&["revoke", "nPhone", "--label", "x"])).unwrap();
            assert_eq!(revoke.host_label, "x");
        }
    }
}
