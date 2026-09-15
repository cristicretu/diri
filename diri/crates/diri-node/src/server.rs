use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use diri_proto::control::{
    ControlError, ControlMessage, MAX_CONTROL_LINE_BYTES, decode_line, encode_line,
};
use diri_proto::{NODE_PROTOCOL_VERSION, NodeHelloParams, NodeMethod};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::error::{NodeError, NodeResult};
use crate::service::NodeService;

const MAX_CONNECTIONS: usize = 64;
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

pub struct NodeServer {
    service: Arc<NodeService>,
}

impl NodeServer {
    pub fn new(service: Arc<NodeService>) -> Self {
        Self { service }
    }

    pub async fn run(&self, address: SocketAddr) -> NodeResult<()> {
        if !is_safe_plaintext_address(address) {
            return Err(NodeError::BadRequest(format!(
                "refusing unencrypted node listener {address}; bind loopback or Tailscale"
            )));
        }
        let listener = TcpListener::bind(address).await?;
        self.serve(listener).await
    }

    pub async fn serve(&self, listener: TcpListener) -> NodeResult<()> {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let accepted = tokio::select! {
                result = listener.accept(), if connections.len() < MAX_CONNECTIONS => result,
                _ = connections.join_next(), if !connections.is_empty() => continue,
            };
            let (stream, _) = accepted?;
            let service = Arc::clone(&self.service);
            connections.spawn(async move {
                if let Err(error) = serve_connection(stream, service).await {
                    eprintln!("diri-node connection closed: {error}");
                }
            });
        }
    }
}

use diri_proto::net::is_safe_plaintext_address;

async fn serve_connection(stream: TcpStream, service: Arc<NodeService>) -> NodeResult<()> {
    stream.set_nodelay(true)?;
    serve_stream(stream, service).await
}

async fn serve_stream<S>(stream: S, service: Arc<NodeService>) -> NodeResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_stream_with_auth_timeout(stream, service, AUTH_TIMEOUT).await
}

async fn serve_stream_with_auth_timeout<S>(
    stream: S,
    service: Arc<NodeService>,
    auth_timeout: Duration,
) -> NodeResult<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut authenticated = false;
    let auth_deadline = tokio::time::Instant::now() + auth_timeout;
    loop {
        let line = if authenticated {
            read_bounded_line(&mut reader).await?
        } else {
            tokio::time::timeout_at(auth_deadline, read_bounded_line(&mut reader))
                .await
                .map_err(|_| NodeError::Unauthorized)??
        };
        let Some(line) = line else {
            return Ok(());
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let message = decode_line(&line)?;
        let ControlMessage::Request { id, method, params } = message else {
            return Err(NodeError::Protocol(
                "clients may only send control requests".into(),
            ));
        };
        let result = if method == NodeMethod::HELLO {
            authenticate(&service, params).inspect(|_| {
                authenticated = true;
            })
        } else if !authenticated {
            Err(NodeError::Unauthorized)
        } else {
            service.dispatch(&method, params).await
        };
        let response = ControlMessage::Response {
            id,
            result: result.map_err(ControlError::from),
        };
        write_half.write_all(&encode_line(&response)?).await?;
        write_half.flush().await?;
    }
}

fn authenticate(
    service: &NodeService,
    params: Option<serde_json::Value>,
) -> NodeResult<serde_json::Value> {
    let params: NodeHelloParams = serde_json::from_value(
        params.ok_or_else(|| NodeError::BadRequest("missing node hello params".into()))?,
    )?;
    if params.proto != NODE_PROTOCOL_VERSION {
        return Err(NodeError::Protocol(format!(
            "node protocol {} is not supported (expected {NODE_PROTOCOL_VERSION})",
            params.proto
        )));
    }
    if !service.config().token_matches(&params.token) {
        return Err(NodeError::Unauthorized);
    }
    serde_json::to_value(service.hello()).map_err(Into::into)
}

async fn read_bounded_line<R>(reader: &mut R) -> NodeResult<Option<Vec<u8>>>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        let payload = newline.unwrap_or(available.len());
        if line.len().saturating_add(payload) > MAX_CONTROL_LINE_BYTES {
            return Err(NodeError::Protocol(format!(
                "control line exceeds {MAX_CONTROL_LINE_BYTES} bytes"
            )));
        }
        line.extend_from_slice(&available[..payload]);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diri_proto::{NodeHelloResult, NodeStatusResult};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use crate::{NodeConfig, NodePaths};

    #[test]
    fn listeners_require_loopback_or_tailscale() {
        assert!(is_safe_plaintext_address("127.0.0.1:7337".parse().unwrap()));
        assert!(is_safe_plaintext_address(
            "100.64.12.2:7337".parse().unwrap()
        ));
        assert!(!is_safe_plaintext_address(
            "192.168.1.2:7337".parse().unwrap()
        ));
        assert!(!is_safe_plaintext_address("0.0.0.0:7337".parse().unwrap()));
        assert!(!is_safe_plaintext_address("8.8.8.8:7337".parse().unwrap()));
    }

    #[tokio::test]
    async fn tcp_requires_node_hello_before_management_calls() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = NodePaths::for_root(directory.path().join("node"));
        let config = NodeConfig::load_or_initialize(&paths).expect("config");
        let token = config.auth_token.clone();
        let service = NodeService::open(paths, config).expect("service");
        let (client, server) = tokio::io::duplex(16 * 1024);
        let task = tokio::spawn(async move { serve_stream(server, service).await });
        let (read, mut write) = tokio::io::split(client);
        let mut read = BufReader::new(read);
        write
            .write_all(
                &encode_line(&ControlMessage::Request {
                    id: 1,
                    method: NodeMethod::STATUS.into(),
                    params: None,
                })
                .expect("encode"),
            )
            .await
            .expect("write");
        let mut line = Vec::new();
        read.read_until(b'\n', &mut line).await.expect("read");
        let denied = decode_line(&line).expect("decode");
        assert!(matches!(
            denied,
            ControlMessage::Response { result: Err(_), .. }
        ));

        write
            .write_all(
                &encode_line(&ControlMessage::Request {
                    id: 2,
                    method: NodeMethod::HELLO.into(),
                    params: Some(
                        serde_json::to_value(NodeHelloParams::new("test", token))
                            .expect("hello params"),
                    ),
                })
                .expect("encode"),
            )
            .await
            .expect("write hello");
        line.clear();
        read.read_until(b'\n', &mut line).await.expect("read hello");
        let ControlMessage::Response {
            result: Ok(value), ..
        } = decode_line(&line).expect("decode hello")
        else {
            panic!("hello failed")
        };
        let hello: NodeHelloResult = serde_json::from_value(value).expect("typed hello");
        assert_eq!(hello.proto, NODE_PROTOCOL_VERSION);

        write
            .write_all(
                &encode_line(&ControlMessage::Request {
                    id: 3,
                    method: NodeMethod::STATUS.into(),
                    params: None,
                })
                .expect("encode"),
            )
            .await
            .expect("write status");
        line.clear();
        read.read_until(b'\n', &mut line)
            .await
            .expect("read status");
        let ControlMessage::Response {
            result: Ok(value), ..
        } = decode_line(&line).expect("decode status")
        else {
            panic!("status failed")
        };
        let _: NodeStatusResult = serde_json::from_value(value).expect("typed status");
        task.abort();
    }

    #[tokio::test]
    async fn idle_connection_must_authenticate_before_the_deadline() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = NodePaths::for_root(directory.path().join("node"));
        let config = NodeConfig::load_or_initialize(&paths).expect("config");
        let service = NodeService::open(paths, config).expect("service");
        let (_client, server) = tokio::io::duplex(1024);
        let error = serve_stream_with_auth_timeout(server, service, Duration::from_millis(10))
            .await
            .expect_err("idle peer must time out");
        assert!(matches!(error, NodeError::Unauthorized));
    }
}
