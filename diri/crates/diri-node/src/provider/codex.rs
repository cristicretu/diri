use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use diri_proto::LoginMode;
use diri_proto::control::MAX_CONTROL_LINE_BYTES;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::error::{NodeError, NodeResult};

const CALL_TIMEOUT: Duration = Duration::from_secs(45);

pub struct CodexAppServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl CodexAppServer {
    pub async fn spawn(config_home: &Path) -> NodeResult<Self> {
        let mut child = Command::new("codex")
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", config_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| NodeError::Provider(format!("could not start Codex: {error}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| NodeError::Provider("Codex stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| NodeError::Provider("Codex stdout unavailable".into()))?;
        let mut server = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
        };
        server
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "dirijor-node",
                        "title": "Dirijor Node",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": { "experimentalApi": false }
                }),
            )
            .await?;
        server.notify("initialized", Value::Null).await?;
        Ok(server)
    }

    pub async fn account(&mut self) -> NodeResult<Value> {
        self.request("account/read", json!({"refreshToken": false}))
            .await
    }

    pub async fn begin_login(&mut self, mode: LoginMode) -> NodeResult<Value> {
        let params = match mode {
            LoginMode::DeviceCode => json!({"type": "chatgptDeviceCode"}),
            LoginMode::Browser => json!({
                "type": "chatgpt",
                "appBrand": "codex",
                "codexStreamlinedLogin": true,
                "useHostedLoginSuccessPage": true
            }),
            LoginMode::Interactive => {
                return Err(NodeError::BadRequest(
                    "Codex login is not an interactive terminal flow".into(),
                ));
            }
        };
        self.request("account/login/start", params).await
    }

    pub async fn call(&mut self, method: &str, params: Value) -> NodeResult<Value> {
        self.request(method, params).await
    }

    async fn notify(&mut self, method: &str, params: Value) -> NodeResult<()> {
        let message = if params.is_null() {
            json!({"method": method})
        } else {
            json!({"method": method, "params": params})
        };
        self.write(&message).await
    }

    async fn request(&mut self, method: &str, params: Value) -> NodeResult<Value> {
        if let Some(status) = self.child.try_wait()? {
            return Err(NodeError::Provider(format!(
                "Codex app-server exited with {status}"
            )));
        }
        self.next_id += 1;
        let id = self.next_id;
        self.write(&json!({"id": id, "method": method, "params": params}))
            .await?;

        let response = async {
            loop {
                let Some(line) = read_bounded_provider_line(&mut self.stdout).await? else {
                    return Err(NodeError::Provider(
                        "Codex app-server closed its output".into(),
                    ));
                };
                let value: Value = serde_json::from_slice(&line)?;
                if value.get("id").and_then(Value::as_u64) != Some(id) {
                    // Notifications are deliberately consumed here. The node
                    // records authoritative usage separately and clients ask
                    // for snapshots, so provider event shape stays private.
                    continue;
                }
                if let Some(error) = value.get("error") {
                    return Err(NodeError::Provider(format_codex_error(error)));
                }
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| NodeError::Provider("Codex response omitted result".into()));
            }
        };
        tokio::time::timeout(CALL_TIMEOUT, response)
            .await
            .map_err(|_| NodeError::Provider(format!("Codex `{method}` timed out")))?
    }

    async fn write(&mut self, value: &Value) -> NodeResult<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        Ok(())
    }
}

async fn read_bounded_provider_line<R>(reader: &mut R) -> NodeResult<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
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
            return Err(NodeError::Provider(format!(
                "Codex app-server line exceeds {MAX_CONTROL_LINE_BYTES} bytes"
            )));
        }
        line.extend_from_slice(&available[..payload]);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

fn format_codex_error(error: &Value) -> String {
    let code = error.get("code").and_then(Value::as_i64);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown Codex app-server error");
    code.map_or_else(|| message.to_owned(), |code| format!("{code}: {message}"))
}
