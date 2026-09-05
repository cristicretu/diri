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
mod http;
mod ui;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use diri_client::DaemonClient;
use diri_proto::net::is_private_bind_address;
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

/// SSE comment sent when nothing has happened, so that a phone NAT does not
/// silently reap a stream the browser still believes is open.
const HEARTBEAT: Duration = Duration::from_secs(20);

#[derive(Debug)]
struct Config {
    listen: SocketAddr,
    socket_path: PathBuf,
    token_path: PathBuf,
    host_label: String,
}

fn usage() -> &'static str {
    "\
diri-web — serve a Dirijor daemon to a phone browser over a private network

USAGE:
    diri-web [--listen ADDR:PORT] [--socket PATH] [--token-file PATH] [--label NAME]
    diri-web url                       print the enrolment URL and exit

OPTIONS:
    --listen ADDR:PORT   default 127.0.0.1:7380; must be loopback, LAN, or Tailscale
    --socket PATH        daemon control socket (default: $DIRIJOR_SOCKET, else the
                         standard Dirijor application-support path)
    --token-file PATH    default ~/.config/dirijor/web.token, created if absent
    --label NAME         name shown in the frontend header (default: the hostname)

The token is minted on first run. Open the printed URL once on the phone; it is
exchanged for a cookie and the secret leaves the address bar.
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

    if arguments.first().is_some_and(|argument| argument == "url") {
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

    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].as_str();
        // `url` is a bare subcommand, handled by the caller.
        if argument == "url" {
            index += 1;
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
            other => return Err(format!("unrecognised argument {other:?}")),
        }
    }

    let listen = resolve_listen(&listen)?;
    if !is_private_bind_address(listen) {
        return Err(format!(
            "refusing to bind {listen}: this frontend can kill sessions and start \
             agents, so it must sit on loopback, a private LAN, or Tailscale — \
             never a public interface"
        ));
    }

    Ok(Config {
        listen,
        socket_path: socket_path.unwrap_or_else(default_socket_path),
        token_path: token_path.unwrap_or_else(auth::default_token_path),
        host_label: host_label.unwrap_or_else(default_host_label),
    })
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
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
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

    eprintln!("diri-web: listening on http://{}", config.listen);
    let _ = minted;
    eprintln!("diri-web: use `diri-web url` to show the private pairing link");

    let api = Arc::new(Api {
        client: Arc::clone(&client),
        auth,
        host_label: config.host_label,
    });

    serve_connections(listener, api).await
}

/// App-owned gateway with a fresh in-memory credential on each enable.
pub async fn start_phone_access(
    address: std::net::Ipv4Addr,
    client: Arc<DaemonClient>,
    label: String,
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
    let auth = Auth::temporary().map_err(|error| error.to_string())?;
    let url = enrolment_url(&SocketAddr::from((address, DEFAULT_PORT)), auth.token());
    let api = Arc::new(Api {
        client,
        auth,
        host_label: label,
    });
    Ok((url, tokio::spawn(serve_connections(listener, api))))
}

async fn serve_connections(listener: TcpListener, api: Arc<Api>) -> Result<(), String> {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        // Dropping this owner revokes keep-alive and SSE connections too.
        let accepted = tokio::select! {
            result = listener.accept(), if connections.len() < 64 => result,
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
            if let Err(error) = handle(stream, api).await {
                // One phone falling off a cell tower is normal; log at a
                // volume that does not drown the journal.
                let _ = (peer, error);
            }
        });
    }
}

async fn handle(stream: TcpStream, api: Arc<Api>) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        let request = match tokio::time::timeout(IDLE_TIMEOUT, http::read_request(&mut reader))
            .await
        {
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

        // The event stream owns the connection for its lifetime.
        if request.method == "GET" && request.path == "/api/events" {
            if !api.auth.authorizes(&request) {
                let _ = http::write_response(&mut write_half, None, auth::unauthorized()).await;
                return Ok(());
            }
            return stream_events(&mut write_half, &api).await;
        }

        let response = respond(&request, &api).await;
        if !http::write_response(&mut write_half, Some(&request), response).await? {
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
                }
                // Lagged: the phone was backgrounded and missed events. Tell
                // it to refetch wholesale rather than pretending it is current.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    http::write_event(writer, "resync", &serde_json::json!({})).await?;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            },
            changed = connection.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let state = format!("{:?}", *connection.borrow());
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

    #[tokio::test]
    async fn phone_entry_point_refuses_lan_and_public_addresses() {
        for address in ["127.0.0.1", "192.168.1.1", "0.0.0.0", "8.8.8.8"] {
            let client = Arc::new(DaemonClient::with_socket_path(
                "/nonexistent/diri-phone-test.sock",
            ));
            assert!(
                start_phone_access(address.parse().unwrap(), client, "test".into())
                    .await
                    .is_err()
            );
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
    fn a_resolved_hostname_still_has_to_be_private() {
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
}
