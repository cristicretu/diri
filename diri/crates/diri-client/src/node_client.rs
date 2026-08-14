use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use diri_proto::control::{
    ControlMessage, JsonValue, MAX_CONTROL_LINE_BYTES, decode_line, encode_line,
};
use diri_proto::{
    AccountCatalogResult, AccountInstallation, AccountLoginStartParams, AccountProfile,
    AccountProfileParams, AccountSetDefaultParams, AccountUpsertParams, BlobChunk, BlobHasParams,
    BlobHasResult, BlobPutParams, BlobReadParams, CheckpointIdParams, CheckpointManifest,
    CheckpointManifestParams, CheckpointPrepareParams, CheckpointStageResult, HostEntry,
    InstallationStatus, LoginChallenge, LoginInputParams, LoginSessionParams, MoveAbortParams,
    MoveCommitParams, MoveRecord, NodeHelloParams, NodeHelloResult, NodeMethod, NodeStatusResult,
    PortableConfigApplyParams, PortableConfigApplyResult, PortableConfigBundle, ProviderCallParams,
    ProviderCallResult, ProviderKind, SessionHandoffResult, TransferMode, UsageEvent,
    UsageQueryParams, UsageQueryResult, UsageRecordParams,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::client::{CLIENT_BUILD, ClientError};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeClientConfig {
    pub endpoint: String,
    pub token: String,
    pub expected_node_id: Option<String>,
    pub build: String,
}

impl NodeClientConfig {
    pub fn from_token_file(
        endpoint: impl Into<String>,
        token_file: &str,
        home: &Path,
        expected_node_id: Option<String>,
    ) -> Result<Self, ClientError> {
        let token_path = expand_home(token_file, home);
        let raw = fs::read_to_string(&token_path).map_err(|error| {
            ClientError::io(format!(
                "could not read node token {}: {error}",
                token_path.display()
            ))
        })?;
        Ok(Self {
            endpoint: endpoint.into(),
            token: parse_token_file(&raw)?,
            expected_node_id,
            build: CLIENT_BUILD.into(),
        })
    }

    pub fn from_host(host: &HostEntry, home: &Path) -> Result<Self, ClientError> {
        let node = host.node.as_ref().ok_or_else(|| {
            ClientError::protocol(format!("host `{}` has no first-party node", host.id))
        })?;
        Self::from_token_file(
            node.endpoint.clone(),
            &node.token_file,
            home,
            node.node_id.clone(),
        )
    }
}

#[derive(Clone, Debug)]
pub struct NodeClient {
    config: NodeClientConfig,
}

impl NodeClient {
    pub fn new(config: NodeClientConfig) -> Self {
        Self { config }
    }

    pub fn from_host(host: &HostEntry, home: &Path) -> Result<Self, ClientError> {
        NodeClientConfig::from_host(host, home).map(Self::new)
    }

    pub async fn hello(&self) -> Result<NodeHelloResult, ClientError> {
        let (_, hello) = self.connect().await?;
        Ok(hello)
    }

    pub async fn status(&self) -> Result<NodeStatusResult, ClientError> {
        self.no_params(NodeMethod::STATUS).await
    }

    pub async fn accounts(&self) -> Result<AccountCatalogResult, ClientError> {
        self.no_params(NodeMethod::ACCOUNT_LIST).await
    }

    pub async fn upsert_account(
        &self,
        params: AccountUpsertParams,
    ) -> Result<AccountProfile, ClientError> {
        self.typed(NodeMethod::ACCOUNT_UPSERT, &params).await
    }

    pub async fn set_default_account(
        &self,
        params: AccountSetDefaultParams,
    ) -> Result<(), ClientError> {
        let _: Value = self.typed(NodeMethod::ACCOUNT_SET_DEFAULT, &params).await?;
        Ok(())
    }

    pub async fn account_status(
        &self,
        profile_id: impl Into<String>,
    ) -> Result<AccountInstallation, ClientError> {
        self.typed(
            NodeMethod::ACCOUNT_STATUS,
            &AccountProfileParams {
                profile_id: profile_id.into(),
            },
        )
        .await
    }

    pub async fn begin_login(
        &self,
        params: AccountLoginStartParams,
    ) -> Result<LoginChallenge, ClientError> {
        self.typed(NodeMethod::ACCOUNT_LOGIN_START, &params).await
    }

    pub async fn poll_login(&self, login_id: &str) -> Result<LoginChallenge, ClientError> {
        self.typed(
            NodeMethod::ACCOUNT_LOGIN_POLL,
            &LoginSessionParams {
                login_id: login_id.into(),
            },
        )
        .await
    }

    pub async fn login_input(&self, login_id: &str, text: String) -> Result<(), ClientError> {
        let _: Value = self
            .typed(
                NodeMethod::ACCOUNT_LOGIN_INPUT,
                &LoginInputParams {
                    login_id: login_id.into(),
                    text,
                },
            )
            .await?;
        Ok(())
    }

    pub async fn cancel_login(&self, login_id: &str) -> Result<(), ClientError> {
        let _: Value = self
            .typed(
                NodeMethod::ACCOUNT_LOGIN_CANCEL,
                &LoginSessionParams {
                    login_id: login_id.into(),
                },
            )
            .await?;
        Ok(())
    }

    pub async fn export_portable_config(
        &self,
        profile_id: impl Into<String>,
    ) -> Result<PortableConfigBundle, ClientError> {
        self.typed(
            NodeMethod::ACCOUNT_PORTABLE_CONFIG_EXPORT,
            &AccountProfileParams {
                profile_id: profile_id.into(),
            },
        )
        .await
    }

    pub async fn apply_portable_config(
        &self,
        bundle: PortableConfigBundle,
    ) -> Result<PortableConfigApplyResult, ClientError> {
        self.typed(
            NodeMethod::ACCOUNT_PORTABLE_CONFIG_APPLY,
            &PortableConfigApplyParams { bundle },
        )
        .await
    }

    /// Sync only portable provider settings. The source and target account
    /// profiles must already exist; login remains a per-node action.
    pub async fn sync_portable_config(
        &self,
        target: &NodeClient,
        profile_id: &str,
    ) -> Result<PortableConfigApplyResult, ClientError> {
        let (source, destination) = tokio::try_join!(self.hello(), target.hello())?;
        if !supports_portable_config(&source) || !supports_portable_config(&destination) {
            return Err(ClientError::protocol(
                "both nodes must support portable-config.v1",
            ));
        }
        let bundle = self.export_portable_config(profile_id).await?;
        target.apply_portable_config(bundle).await
    }

    pub async fn provider_call(
        &self,
        params: ProviderCallParams,
    ) -> Result<ProviderCallResult, ClientError> {
        self.typed(NodeMethod::PROVIDER_CALL, &params).await
    }

    pub async fn record_usage(&self, event: UsageEvent) -> Result<bool, ClientError> {
        let value: Value = self
            .typed(NodeMethod::USAGE_RECORD, &UsageRecordParams { event })
            .await?;
        Ok(value
            .get("inserted")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    pub async fn usage(&self, query: UsageQueryParams) -> Result<UsageQueryResult, ClientError> {
        self.typed(NodeMethod::USAGE_QUERY, &query).await
    }

    pub async fn refresh_usage(&self) -> Result<JsonValue, ClientError> {
        self.no_params(NodeMethod::USAGE_REFRESH).await
    }

    pub async fn prepare_checkpoint(
        &self,
        params: CheckpointPrepareParams,
    ) -> Result<CheckpointManifest, ClientError> {
        self.typed_with_timeout(NodeMethod::CHECKPOINT_PREPARE, &params, HANDOFF_TIMEOUT)
            .await
    }

    /// Transactional cross-node handoff. Provider startup happens while the
    /// restore is quarantined; both location leases commit only after the
    /// target runtime confirms it can resume/fork the provider session.
    pub async fn handoff(
        &self,
        target: &NodeClient,
        params: CheckpointPrepareParams,
    ) -> Result<SessionHandoffResult, ClientError> {
        let provider = params.provider;
        let profile_id = params.profile_id.clone();
        let (source_hello, target_hello) = tokio::try_join!(self.hello(), target.hello())?;
        let (source_installation, target_installation) = tokio::try_join!(
            self.account_status(&profile_id),
            target.account_status(&profile_id)
        )?;
        validate_account_continuity(
            provider,
            &source_installation,
            &target_installation,
            &source_hello.display_name,
            &target_hello.display_name,
        )?;
        if target_installation.status != InstallationStatus::Ready {
            return Err(ClientError::protocol(format!(
                "account profile `{profile_id}` is not signed in on {}",
                target_hello.display_name
            )));
        }
        let portable_config =
            if supports_portable_config(&source_hello) && supports_portable_config(&target_hello) {
                let bundle = self.export_portable_config(&profile_id).await?;
                Some(target.apply_portable_config(bundle).await?)
            } else {
                None
            };
        let checkpoint = self.prepare_checkpoint(params).await?;
        if checkpoint.provider_session_id.is_none() {
            return Err(ClientError::protocol(
                "provider session id is required for an instant handoff",
            ));
        }

        let operation = async {
            target.put_manifest(checkpoint.clone()).await?;
            let mut digests = checkpoint
                .files
                .iter()
                .map(|file| file.digest.clone())
                .collect::<Vec<_>>();
            if let Some(provider_state) = &checkpoint.provider_state {
                digests.push(provider_state.digest.clone());
            }
            let missing = target.missing_blobs(digests).await?;
            for digest in missing {
                self.transfer_blob(target, &digest).await?;
            }
            let staged = target.stage_checkpoint(&checkpoint.checkpoint_id).await?;
            let provider_session_id = checkpoint
                .provider_session_id
                .as_deref()
                .expect("checked above");
            let (method, call_params) = provider_resume_call(
                provider,
                checkpoint.mode,
                provider_session_id,
                &staged.quarantine_path,
            );
            let provider_result = target
                .provider_call(ProviderCallParams {
                    profile_id: profile_id.clone(),
                    method: method.into(),
                    params: call_params,
                })
                .await?
                .result;
            let lease_id = random_lease_id()?;
            let target_commit = target
                .commit_move(MoveCommitParams {
                    checkpoint_id: checkpoint.checkpoint_id.clone(),
                    target_node_id: target_hello.node_id.clone(),
                    lease_id: lease_id.clone(),
                })
                .await?;
            let source_commit = self
                .commit_move(MoveCommitParams {
                    checkpoint_id: checkpoint.checkpoint_id.clone(),
                    target_node_id: target_hello.node_id,
                    lease_id,
                })
                .await?;
            Ok::<SessionHandoffResult, ClientError>(SessionHandoffResult {
                checkpoint: checkpoint.clone(),
                staged,
                portable_config,
                provider_result,
                target_commit,
                source_commit,
            })
        };
        match tokio::time::timeout(HANDOFF_TIMEOUT, operation).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => {
                self.abort_best_effort(target, &checkpoint.checkpoint_id, &error.to_string())
                    .await;
                Err(error)
            }
            Err(_) => {
                let error = ClientError::Timeout("node handoff timed out".into());
                self.abort_best_effort(target, &checkpoint.checkpoint_id, &error.to_string())
                    .await;
                Err(error)
            }
        }
    }

    async fn put_manifest(&self, manifest: CheckpointManifest) -> Result<(), ClientError> {
        let _: Value = self
            .typed(
                NodeMethod::CHECKPOINT_MANIFEST_PUT,
                &CheckpointManifestParams { manifest },
            )
            .await?;
        Ok(())
    }

    async fn missing_blobs(&self, digests: Vec<String>) -> Result<Vec<String>, ClientError> {
        let result: BlobHasResult = self
            .typed(NodeMethod::CHECKPOINT_BLOB_HAS, &BlobHasParams { digests })
            .await?;
        Ok(result.missing)
    }

    async fn transfer_blob(&self, target: &NodeClient, digest: &str) -> Result<(), ClientError> {
        let mut offset = 0_u64;
        loop {
            let chunk: BlobChunk = self
                .typed_with_timeout(
                    NodeMethod::CHECKPOINT_BLOB_READ,
                    &BlobReadParams {
                        digest: digest.into(),
                        offset,
                        max_bytes: 512 * 1024,
                    },
                    HANDOFF_TIMEOUT,
                )
                .await?;
            let bytes = u64::try_from(chunk.hex.len() / 2).unwrap_or(u64::MAX);
            let value: Value = target
                .typed_with_timeout(
                    NodeMethod::CHECKPOINT_BLOB_PUT,
                    &BlobPutParams {
                        digest: digest.into(),
                        offset,
                        hex: chunk.hex,
                        eof: chunk.eof,
                    },
                    HANDOFF_TIMEOUT,
                )
                .await?;
            offset = offset.saturating_add(bytes);
            if chunk.eof {
                if !value
                    .get("complete")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    return Err(ClientError::protocol("target did not finalize blob"));
                }
                return Ok(());
            }
        }
    }

    async fn stage_checkpoint(
        &self,
        checkpoint_id: &str,
    ) -> Result<CheckpointStageResult, ClientError> {
        self.typed_with_timeout(
            NodeMethod::CHECKPOINT_STAGE,
            &CheckpointIdParams {
                checkpoint_id: checkpoint_id.into(),
            },
            HANDOFF_TIMEOUT,
        )
        .await
    }

    async fn commit_move(&self, params: MoveCommitParams) -> Result<MoveRecord, ClientError> {
        self.typed(NodeMethod::MOVE_COMMIT, &params).await
    }

    async fn abort_best_effort(&self, target: &NodeClient, checkpoint_id: &str, reason: &str) {
        let params = MoveAbortParams {
            checkpoint_id: checkpoint_id.into(),
            reason: reason.chars().take(500).collect(),
        };
        let _: Result<Value, _> = target.typed(NodeMethod::MOVE_ABORT, &params).await;
        let _: Result<Value, _> = self.typed(NodeMethod::MOVE_ABORT, &params).await;
    }

    async fn no_params<R: DeserializeOwned>(&self, method: &str) -> Result<R, ClientError> {
        self.request_typed::<Value, R>(method, None, REQUEST_TIMEOUT)
            .await
    }

    async fn typed<P: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, ClientError> {
        self.request_typed(method, Some(params), REQUEST_TIMEOUT)
            .await
    }

    async fn typed_with_timeout<P: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
        timeout: Duration,
    ) -> Result<R, ClientError> {
        self.request_typed(method, Some(params), timeout).await
    }

    async fn request_typed<P: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        method: &str,
        params: Option<&P>,
        timeout: Duration,
    ) -> Result<R, ClientError> {
        let request = async {
            let (stream, _) = self.connect().await?;
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let params = params
                .map(serde_json::to_value)
                .transpose()
                .map_err(ClientError::json)?;
            write
                .write_all(
                    &encode_line(&ControlMessage::Request {
                        id: 2,
                        method: method.into(),
                        params,
                    })
                    .map_err(ClientError::json)?,
                )
                .await
                .map_err(ClientError::io)?;
            let value = read_response(&mut reader, 2).await?;
            serde_json::from_value(value).map_err(ClientError::json)
        };
        tokio::time::timeout(timeout, request)
            .await
            .map_err(|_| ClientError::Timeout(format!("node request `{method}` timed out")))?
    }

    async fn connect(&self) -> Result<(TcpStream, NodeHelloResult), ClientError> {
        let address = endpoint_address(&self.config.endpoint)?;
        let stream = TcpStream::connect(address).await.map_err(ClientError::io)?;
        stream.set_nodelay(true).map_err(ClientError::io)?;
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let hello = NodeHelloParams {
            proto: diri_proto::NODE_PROTOCOL_VERSION,
            build: self.config.build.clone(),
            token: self.config.token.clone(),
            client_node_id: None,
        };
        write
            .write_all(
                &encode_line(&ControlMessage::Request {
                    id: 1,
                    method: NodeMethod::HELLO.into(),
                    params: Some(serde_json::to_value(hello).map_err(ClientError::json)?),
                })
                .map_err(ClientError::json)?,
            )
            .await
            .map_err(ClientError::io)?;
        let value = read_response(&mut reader, 1).await?;
        let hello: NodeHelloResult = serde_json::from_value(value).map_err(ClientError::json)?;
        if self
            .config
            .expected_node_id
            .as_ref()
            .is_some_and(|expected| expected != &hello.node_id)
        {
            return Err(ClientError::protocol(format!(
                "node identity mismatch: expected {}, received {}",
                self.config.expected_node_id.as_deref().unwrap_or_default(),
                hello.node_id
            )));
        }
        let read = reader.into_inner();
        Ok((read.reunite(write).map_err(ClientError::io)?, hello))
    }
}

fn supports_portable_config(hello: &NodeHelloResult) -> bool {
    hello
        .capabilities
        .iter()
        .any(|capability| capability == diri_proto::NodeCapability::PORTABLE_CONFIG)
}

fn validate_account_continuity(
    provider: ProviderKind,
    source: &AccountInstallation,
    target: &AccountInstallation,
    source_name: &str,
    target_name: &str,
) -> Result<(), ClientError> {
    if source.provider != provider || target.provider != provider {
        return Err(ClientError::protocol(
            "source and target profiles must use the handoff provider",
        ));
    }
    if source.status != InstallationStatus::Ready {
        return Err(ClientError::protocol(format!(
            "account profile `{}` is not signed in on {source_name}",
            source.profile_id
        )));
    }
    if let (Some(source_identity), Some(target_identity)) =
        (source.identity.as_deref(), target.identity.as_deref())
        && target.status == InstallationStatus::Ready
        && !source_identity.trim().is_empty()
        && !target_identity.trim().is_empty()
        && !source_identity.eq_ignore_ascii_case(target_identity)
    {
        return Err(ClientError::protocol(format!(
            "account profile `{}` resolves to different identities on {source_name} and {target_name}",
            source.profile_id
        )));
    }
    Ok(())
}

async fn read_response<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    expected_id: u64,
) -> Result<JsonValue, ClientError> {
    let mut line = Vec::new();
    let bytes = reader
        .read_until(b'\n', &mut line)
        .await
        .map_err(ClientError::io)?;
    if bytes == 0 {
        return Err(ClientError::disconnected("node closed the connection"));
    }
    if line.len() > MAX_CONTROL_LINE_BYTES {
        return Err(ClientError::protocol(
            "node response exceeded the wire limit",
        ));
    }
    match decode_line(&line).map_err(ClientError::json)? {
        ControlMessage::Response { id, result } if id == expected_id => result.map_err(Into::into),
        other => Err(ClientError::protocol(format!(
            "unexpected node response: {other:?}"
        ))),
    }
}

fn endpoint_address(endpoint: &str) -> Result<&str, ClientError> {
    let endpoint = endpoint.strip_prefix("tcp://").unwrap_or(endpoint);
    if endpoint.is_empty() || endpoint.contains('/') || !endpoint.contains(':') {
        return Err(ClientError::protocol(
            "node endpoint must be tcp://HOST:PORT or HOST:PORT",
        ));
    }
    Ok(endpoint)
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_owned()
    } else if let Some(relative) = path.strip_prefix("~/") {
        home.join(relative)
    } else {
        PathBuf::from(path)
    }
}

fn parse_token_file(raw: &str) -> Result<String, ClientError> {
    let trimmed = raw.trim();
    let token = if trimmed.starts_with('{') {
        let value: Value = serde_json::from_str(trimmed).map_err(ClientError::json)?;
        value
            .get("token")
            .or_else(|| value.get("authToken"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    } else {
        trimmed.to_owned()
    };
    if token.len() < 32 || token.chars().any(char::is_whitespace) {
        return Err(ClientError::protocol("node token file is invalid"));
    }
    Ok(token)
}

fn provider_resume_call(
    provider: ProviderKind,
    mode: TransferMode,
    session_id: &str,
    cwd: &str,
) -> (&'static str, Value) {
    let method = match (provider, mode) {
        (ProviderKind::Codex, TransferMode::Move) => "thread/resume",
        (ProviderKind::Codex, TransferMode::Fork) => "thread/fork",
        (ProviderKind::Claude, TransferMode::Move) => "session/resume",
        (ProviderKind::Claude, TransferMode::Fork) => "session/fork",
    };
    let params = match provider {
        ProviderKind::Codex => json!({"threadId": session_id, "cwd": cwd}),
        ProviderKind::Claude => json!({"sessionId": session_id, "cwd": cwd}),
    };
    (method, params)
}

fn random_lease_id() -> Result<String, ClientError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(ClientError::io)?;
    let mut result = String::with_capacity(38);
    result.push_str("lease-");
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut result, "{byte:02x}").expect("writing to string cannot fail");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installation(identity: &str) -> AccountInstallation {
        AccountInstallation {
            profile_id: "work".into(),
            provider: ProviderKind::Codex,
            node_id: "node".into(),
            status: InstallationStatus::Ready,
            config_home: "/tmp/codex-work".into(),
            identity: Some(identity.into()),
            plan: None,
            last_error: None,
            checked_at: None,
        }
    }

    #[test]
    fn token_files_accept_plain_or_explicit_json_but_not_short_values() {
        let token = "a".repeat(64);
        assert_eq!(parse_token_file(&token).expect("plain"), token);
        assert_eq!(
            parse_token_file(&format!(r#"{{"token":"{token}"}}"#)).expect("json"),
            token
        );
        assert!(parse_token_file("secret").is_err());
    }

    #[test]
    fn explicit_remote_config_expands_the_local_token_path_and_keeps_the_pin() {
        let home = tempfile::tempdir().expect("home");
        let config_dir = home.path().join(".config/dirijor");
        fs::create_dir_all(&config_dir).expect("config directory");
        let token = "b".repeat(64);
        fs::write(config_dir.join("forge.token"), &token).expect("token");
        let config = NodeClientConfig::from_token_file(
            "tcp://100.64.0.2:7337",
            "~/.config/dirijor/forge.token",
            home.path(),
            Some("node-forge".into()),
        )
        .expect("remote config");
        assert_eq!(config.token, token);
        assert_eq!(config.expected_node_id.as_deref(), Some("node-forge"));
    }

    #[test]
    fn provider_handoff_uses_first_party_resume_shapes() {
        let (codex, params) = provider_resume_call(
            ProviderKind::Codex,
            TransferMode::Move,
            "thread-1",
            "/tmp/staged",
        );
        assert_eq!(codex, "thread/resume");
        assert_eq!(params["threadId"], "thread-1");
        let (claude, params) = provider_resume_call(
            ProviderKind::Claude,
            TransferMode::Fork,
            "session-1",
            "/tmp/staged",
        );
        assert_eq!(claude, "session/fork");
        assert_eq!(params["sessionId"], "session-1");
    }

    #[test]
    fn endpoint_requires_an_explicit_tcp_address() {
        assert_eq!(
            endpoint_address("tcp://100.64.0.2:7337").expect("endpoint"),
            "100.64.0.2:7337"
        );
        assert!(endpoint_address("https://example.com").is_err());
    }

    #[test]
    fn handoff_rejects_a_profile_that_resolves_to_another_identity() {
        let source = installation("person@example.com");
        let target = installation("someone-else@example.com");
        let error =
            validate_account_continuity(ProviderKind::Codex, &source, &target, "Local", "Forge")
                .expect_err("identity mismatch");
        assert!(error.to_string().contains("different identities"));
    }

    #[test]
    fn handoff_identity_comparison_is_case_insensitive() {
        validate_account_continuity(
            ProviderKind::Codex,
            &installation("Person@Example.com"),
            &installation("person@example.com"),
            "Local",
            "Forge",
        )
        .expect("same identity");
    }
}
