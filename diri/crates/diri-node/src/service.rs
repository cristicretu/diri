use std::sync::Arc;

use diri_proto::control::JsonValue;
use diri_proto::{
    AccountLoginStartParams, AccountProfileParams, AccountSetDefaultParams, BlobHasParams,
    BlobPutParams, BlobReadParams, CheckpointIdParams, CheckpointManifestParams,
    CheckpointPrepareParams, EmptyParams, LoginInputParams, LoginSessionParams, MoveAbortParams,
    MoveCommitParams, NodeCapability, NodeHelloResult, NodeMethod, NodeStatusResult,
    PortableConfigApplyParams, ProviderCallParams, UsageQueryParams, UsageRecordParams,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::sync::Mutex;

use crate::NODE_BUILD;
use crate::accounts::{AccountStore, now_seconds};
use crate::checkpoint::CheckpointStore;
use crate::config::{NodeConfig, NodePaths};
use crate::error::{NodeError, NodeResult};
use crate::provider::ProviderManager;
use crate::usage::UsageLedger;

pub struct NodeService {
    config: NodeConfig,
    started_at: i64,
    accounts: Mutex<AccountStore>,
    providers: Mutex<ProviderManager>,
    usage: Mutex<UsageLedger>,
    checkpoints: CheckpointStore,
}

impl NodeService {
    pub fn open(paths: NodePaths, config: NodeConfig) -> NodeResult<Arc<Self>> {
        paths.create_layout()?;
        let accounts = AccountStore::load(paths.clone(), config.node_id.clone())?;
        let usage = UsageLedger::open(&paths.usage_db, config.node_id.clone())?;
        let checkpoints = CheckpointStore::new(paths, config.node_id.clone());
        Ok(Arc::new(Self {
            config,
            started_at: now_seconds(),
            accounts: Mutex::new(accounts),
            providers: Mutex::new(ProviderManager::new()),
            usage: Mutex::new(usage),
            checkpoints,
        }))
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    pub fn hello(&self) -> NodeHelloResult {
        NodeHelloResult {
            proto: diri_proto::NODE_PROTOCOL_VERSION,
            control_proto: NodeHelloResult::control_wire_version(),
            build: NODE_BUILD.to_owned(),
            node_id: self.config.node_id.clone(),
            display_name: self.config.display_name.clone(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            capabilities: vec![
                NodeCapability::ACCOUNTS.into(),
                NodeCapability::CODEX_APP_SERVER.into(),
                NodeCapability::CLAUDE_SUPERVISOR.into(),
                NodeCapability::FLEET_USAGE.into(),
                NodeCapability::CHECKPOINTS.into(),
                NodeCapability::MOVE_LEASES.into(),
                NodeCapability::PORTABLE_CONFIG.into(),
            ],
        }
    }

    pub async fn dispatch(&self, method: &str, params: Option<JsonValue>) -> NodeResult<JsonValue> {
        match method {
            NodeMethod::STATUS => {
                decode_optional_empty(params)?;
                let accounts = self.accounts.lock().await.profiles().len();
                let active_logins = self.providers.lock().await.active_logins();
                encode(NodeStatusResult {
                    node: self.hello(),
                    started_at: self.started_at,
                    accounts,
                    active_logins,
                    pending_moves: self.checkpoints.pending_moves(),
                })
            }
            NodeMethod::ACCOUNT_LIST => {
                decode_optional_empty(params)?;
                encode(self.accounts.lock().await.catalog())
            }
            NodeMethod::ACCOUNT_UPSERT => {
                let params = decode(params)?;
                encode(self.accounts.lock().await.upsert(params)?)
            }
            NodeMethod::ACCOUNT_SET_DEFAULT => {
                let params: AccountSetDefaultParams = decode(params)?;
                self.accounts.lock().await.set_default(params)?;
                encode(json!({"ok": true}))
            }
            NodeMethod::ACCOUNT_STATUS => {
                let params: AccountProfileParams = decode(params)?;
                let accounts = self.accounts.lock().await;
                let result = self
                    .providers
                    .lock()
                    .await
                    .status(&accounts, &params.profile_id)
                    .await?;
                encode(result)
            }
            NodeMethod::ACCOUNT_LOGIN_START => {
                let params: AccountLoginStartParams = decode(params)?;
                let accounts = self.accounts.lock().await;
                let result = self
                    .providers
                    .lock()
                    .await
                    .begin_login(&accounts, params)
                    .await?;
                encode(result)
            }
            NodeMethod::ACCOUNT_LOGIN_POLL => {
                let params: LoginSessionParams = decode(params)?;
                let accounts = self.accounts.lock().await;
                let result = self
                    .providers
                    .lock()
                    .await
                    .poll_login(&accounts, params)
                    .await?;
                encode(result)
            }
            NodeMethod::ACCOUNT_LOGIN_INPUT => {
                let params: LoginInputParams = decode(params)?;
                self.providers.lock().await.login_input(params).await?;
                encode(json!({"ok": true}))
            }
            NodeMethod::ACCOUNT_LOGIN_CANCEL => {
                let params: LoginSessionParams = decode(params)?;
                self.providers.lock().await.cancel_login(params).await?;
                encode(json!({"ok": true}))
            }
            NodeMethod::ACCOUNT_PORTABLE_CONFIG_EXPORT => {
                let params: AccountProfileParams = decode(params)?;
                let accounts = self.accounts.lock().await;
                let profile = accounts.profile(&params.profile_id)?;
                encode(crate::portable::capture(
                    profile,
                    &accounts.config_home(profile),
                )?)
            }
            NodeMethod::ACCOUNT_PORTABLE_CONFIG_APPLY => {
                let params: PortableConfigApplyParams = decode(params)?;
                let profile_id = params.bundle.profile_id.clone();
                let result = {
                    let accounts = self.accounts.lock().await;
                    let profile = accounts.profile(&profile_id)?;
                    crate::portable::apply(profile, &accounts.config_home(profile), params.bundle)?
                };
                if !result.installed.is_empty() {
                    self.providers.lock().await.reset_profile(&profile_id);
                }
                encode(result)
            }
            NodeMethod::PROVIDER_CALL => {
                let params: ProviderCallParams = decode(params)?;
                let accounts = self.accounts.lock().await;
                let result = self.providers.lock().await.call(&accounts, params).await?;
                encode(result)
            }
            NodeMethod::USAGE_RECORD => {
                let params: UsageRecordParams = decode(params)?;
                let inserted = self.usage.lock().await.record(&params.event)?;
                encode(json!({"inserted": inserted}))
            }
            NodeMethod::USAGE_QUERY => {
                let params: UsageQueryParams = decode_or_default(params)?;
                encode(self.usage.lock().await.query(&params)?)
            }
            NodeMethod::USAGE_REFRESH => {
                decode_optional_empty(params)?;
                let accounts = self.accounts.lock().await;
                let imported = self.usage.lock().await.import_transcripts(&accounts)?;
                encode(json!({"imported": imported, "updatedAt": now_seconds()}))
            }
            NodeMethod::CHECKPOINT_PREPARE => {
                let params: CheckpointPrepareParams = decode(params)?;
                let accounts = self.accounts.lock().await;
                let profile = accounts.profile(&params.profile_id)?;
                if profile.provider != params.provider {
                    return Err(NodeError::BadRequest(
                        "checkpoint provider does not match its account profile".into(),
                    ));
                }
                let config_home = accounts.config_home(profile);
                drop(accounts);
                encode(self.checkpoints.prepare(params, &config_home)?)
            }
            NodeMethod::CHECKPOINT_MANIFEST_PUT => {
                let params: CheckpointManifestParams = decode(params)?;
                self.checkpoints.put_manifest(&params.manifest)?;
                encode(json!({"ok": true}))
            }
            NodeMethod::CHECKPOINT_BLOB_HAS => {
                let params: BlobHasParams = decode(params)?;
                encode(self.checkpoints.missing_blobs(params)?)
            }
            NodeMethod::CHECKPOINT_BLOB_READ => {
                let params: BlobReadParams = decode(params)?;
                encode(self.checkpoints.read_blob(params)?)
            }
            NodeMethod::CHECKPOINT_BLOB_PUT => {
                let params: BlobPutParams = decode(params)?;
                let complete = self.checkpoints.put_blob(params)?;
                encode(json!({"complete": complete}))
            }
            NodeMethod::CHECKPOINT_STAGE => {
                let params: CheckpointIdParams = decode(params)?;
                let manifest = self.checkpoints.manifest(&params.checkpoint_id)?;
                let accounts = self.accounts.lock().await;
                let profile = accounts.profile(&manifest.profile_id)?;
                if profile.provider != manifest.provider {
                    return Err(NodeError::Conflict(
                        "target account profile has the wrong provider".into(),
                    ));
                }
                let config_home = accounts.config_home(profile);
                drop(accounts);
                encode(
                    self.checkpoints
                        .stage(&params.checkpoint_id, &config_home)?,
                )
            }
            NodeMethod::MOVE_COMMIT => {
                let params: MoveCommitParams = decode(params)?;
                encode(self.checkpoints.commit(params)?)
            }
            NodeMethod::MOVE_ABORT => {
                let params: MoveAbortParams = decode(params)?;
                let manifest = self.checkpoints.manifest(&params.checkpoint_id)?;
                let movement = self.checkpoints.abort(params)?;
                if manifest.source_node_id != self.config.node_id {
                    let accounts = self.accounts.lock().await;
                    if let Ok(profile) = accounts.profile(&manifest.profile_id) {
                        let config_home = accounts.config_home(profile);
                        self.checkpoints
                            .rollback_provider_state(&manifest, &config_home)?;
                    }
                }
                encode(movement)
            }
            _ => Err(NodeError::NotFound(format!("method `{method}`"))),
        }
    }

    pub async fn refresh_usage(&self) -> NodeResult<usize> {
        let accounts = self.accounts.lock().await;
        self.usage.lock().await.import_transcripts(&accounts)
    }
}

fn decode<T: DeserializeOwned>(params: Option<JsonValue>) -> NodeResult<T> {
    serde_json::from_value(
        params.ok_or_else(|| NodeError::BadRequest("missing method params".into()))?,
    )
    .map_err(Into::into)
}

fn decode_or_default<T: DeserializeOwned + Default>(params: Option<JsonValue>) -> NodeResult<T> {
    params.map_or_else(
        || Ok(T::default()),
        |params| serde_json::from_value(params).map_err(Into::into),
    )
}

fn decode_optional_empty(params: Option<JsonValue>) -> NodeResult<EmptyParams> {
    decode_or_default(params)
}

fn encode<T: Serialize>(value: T) -> NodeResult<JsonValue> {
    serde_json::to_value(value).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use diri_proto::{
        PortableConfigApplyParams, PortableConfigBundle, ProviderKind, UsageEvent, UsageSource,
        UsageValueKind,
    };

    async fn add_codex_profile(service: &NodeService) {
        service
            .dispatch(
                NodeMethod::ACCOUNT_UPSERT,
                Some(json!({
                    "id": "work",
                    "provider": "codex",
                    "label": "Work"
                })),
            )
            .await
            .expect("account");
    }

    #[tokio::test]
    async fn service_exposes_accounts_and_fleet_usage_without_secrets() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let paths = NodePaths::for_root(directory.path().join("node"));
        let config = NodeConfig::load_or_initialize(&paths).expect("config");
        let service = NodeService::open(paths, config).expect("service");
        add_codex_profile(&service).await;
        service
            .dispatch(
                NodeMethod::USAGE_RECORD,
                Some(
                    serde_json::to_value(UsageRecordParams {
                        event: UsageEvent {
                            id: "usage-1".into(),
                            occurred_at: 1_800_000_000,
                            provider: ProviderKind::Codex,
                            profile_id: Some("work".into()),
                            session_id: None,
                            model: None,
                            input_tokens: 5,
                            output_tokens: 2,
                            cache_read_tokens: 0,
                            cache_write_tokens: 0,
                            estimated_usd: None,
                            billed_usd: None,
                            value_kind: UsageValueKind::SubscriptionQuota,
                            source: UsageSource::AppServer,
                        },
                    })
                    .expect("params"),
                ),
            )
            .await
            .expect("usage");
        let catalog = service
            .dispatch(NodeMethod::ACCOUNT_LIST, None)
            .await
            .expect("catalog");
        assert_eq!(catalog["profiles"][0]["id"], "work");
        assert!(!catalog.to_string().contains("authToken"));
        let usage = service
            .dispatch(NodeMethod::USAGE_QUERY, None)
            .await
            .expect("usage query");
        assert_eq!(usage["totals"]["inputTokens"], 5);
    }

    #[tokio::test]
    async fn portable_config_rebases_from_a_mac_home_to_the_remote_node_home() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source_paths = NodePaths::for_root(
            directory
                .path()
                .join("Users/alice/Library/Application Support/Dirijor/node"),
        );
        let target_paths = NodePaths::for_root(
            directory
                .path()
                .join("home/alice/.local/share/dirijor/node"),
        );
        let source = NodeService::open(
            source_paths.clone(),
            NodeConfig::load_or_initialize(&source_paths).expect("source config"),
        )
        .expect("source service");
        let target = NodeService::open(
            target_paths.clone(),
            NodeConfig::load_or_initialize(&target_paths).expect("target config"),
        )
        .expect("target service");
        add_codex_profile(&source).await;
        add_codex_profile(&target).await;

        let source_home = source_paths.accounts_root.join("codex/work");
        let target_home = target_paths.accounts_root.join("codex/work");
        assert_ne!(source_home, target_home);
        fs::write(source_home.join("AGENTS.md"), "portable instructions\n")
            .expect("source settings");

        let exported = source
            .dispatch(
                NodeMethod::ACCOUNT_PORTABLE_CONFIG_EXPORT,
                Some(json!({"profileId": "work"})),
            )
            .await
            .expect("export");
        let bundle: PortableConfigBundle =
            serde_json::from_value(exported).expect("portable bundle");
        assert!(
            bundle
                .files
                .iter()
                .all(|file| Path::new(&file.path).is_relative())
        );
        assert!(
            !serde_json::to_string(&bundle)
                .expect("bundle json")
                .contains(&source_home.to_string_lossy().to_string())
        );

        target
            .dispatch(
                NodeMethod::ACCOUNT_PORTABLE_CONFIG_APPLY,
                Some(
                    serde_json::to_value(PortableConfigApplyParams { bundle })
                        .expect("apply params"),
                ),
            )
            .await
            .expect("apply");
        assert_eq!(
            fs::read_to_string(target_home.join("AGENTS.md")).expect("target settings"),
            "portable instructions\n"
        );
    }
}
