use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use diri_client::{NodeClient, NodeClientConfig};
use diri_node::{NodeConfig, NodePaths, NodeServer, NodeService};
use diri_proto::{
    AccountLoginStartParams, AccountSetDefaultParams, AccountUpsertParams, CheckpointPrepareParams,
    LoginMode, PortableConfigApplyResult, ProviderKind, TransferMode,
};

#[derive(Clone, Debug, Default)]
struct RemoteOptions {
    endpoint: Option<String>,
    token_file: Option<String>,
    node_id: Option<String>,
}

impl RemoteOptions {
    fn take(arguments: &mut Vec<String>, prefix: &str) -> Self {
        Self {
            endpoint: take_option(arguments, &format!("--{prefix}endpoint")),
            token_file: take_option(arguments, &format!("--{prefix}token-file")),
            node_id: take_option(arguments, &format!("--{prefix}node-id")),
        }
    }

    fn configured(&self) -> bool {
        self.endpoint.is_some() || self.token_file.is_some() || self.node_id.is_some()
    }

    fn client(&self, home: &std::path::Path) -> Result<NodeClient, Box<dyn std::error::Error>> {
        let endpoint = self
            .endpoint
            .as_deref()
            .ok_or("remote node requires --endpoint")?;
        let token_file = self
            .token_file
            .as_deref()
            .ok_or("remote node requires --token-file")?;
        Ok(NodeClient::new(NodeClientConfig::from_token_file(
            endpoint,
            token_file,
            home,
            self.node_id.clone(),
        )?))
    }
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("diri-node: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    {
        print_help();
        return Ok(());
    }
    let root = take_option(&mut arguments, "--home").map(PathBuf::from);
    let listen =
        take_option(&mut arguments, "--listen").or_else(|| env::var("DIRIJOR_NODE_LISTEN").ok());
    let remote = RemoteOptions::take(&mut arguments, "");
    let target = RemoteOptions::take(&mut arguments, "target-");
    let command = arguments.first().map_or("serve", String::as_str);
    let paths = root.map_or_else(NodePaths::discover, NodePaths::for_root);
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths.root.clone());
    match command {
        "init" => {
            reject_remote_options(&remote, &target)?;
            let config = NodeConfig::load_or_initialize(&paths)?;
            print_init(&paths, &config);
        }
        "status" if remote.configured() => {
            reject_target_options(&target)?;
            print_remote_status(&remote.client(&home)?).await?;
        }
        "status" => {
            reject_target_options(&target)?;
            let config = NodeConfig::load_or_initialize(&paths)?;
            print_status(&paths, &config);
        }
        "serve" => {
            reject_remote_options(&remote, &target)?;
            let config = NodeConfig::load_or_initialize(&paths)?;
            serve(paths, config, listen).await?;
        }
        "account" => {
            let source = management_client(&paths, listen, &remote, &home)?;
            if arguments
                .get(1)
                .is_some_and(|command| command == "sync-config")
            {
                let profile_id = required_flag(&arguments[2..], "--id")?;
                let destination = target.client(&home)?;
                let result = source
                    .sync_portable_config(&destination, profile_id)
                    .await?;
                print_portable_config(&result);
            } else {
                reject_target_options(&target)?;
                account_command(&arguments[1..], &source).await?;
            }
        }
        "handoff" => {
            let source = management_client(&paths, listen, &remote, &home)?;
            let target = target.client(&home)?;
            handoff_command(&arguments[1..], &source, &target).await?;
        }
        "help" => print_help(),
        unknown => return Err(format!("unknown command `{unknown}`").into()),
    }
    Ok(())
}

fn print_init(paths: &NodePaths, config: &NodeConfig) {
    println!("Node: {} ({})", config.display_name, config.node_id);
    println!("Listen: {}", config.listen);
    println!("Enrollment token: {}", config.auth_token);
    println!("Config: {}", paths.config.display());
    println!("Keep the token private; copy it only to an enrolled Dirijor client.");
}

fn print_status(paths: &NodePaths, config: &NodeConfig) {
    println!("Node: {} ({})", config.display_name, config.node_id);
    println!("Listen: {}", config.listen);
    println!("Data: {}", paths.root.display());
    println!("Enrollment token: configured (redacted)");
}

async fn print_remote_status(client: &NodeClient) -> Result<(), Box<dyn std::error::Error>> {
    let status = client.status().await?;
    println!(
        "Node: {} ({})",
        status.node.display_name, status.node.node_id
    );
    println!("Build: {}", status.node.build);
    println!("Accounts: {}", status.accounts);
    println!("Active logins: {}", status.active_logins);
    println!("Pending moves: {}", status.pending_moves);
    Ok(())
}

async fn serve(
    paths: NodePaths,
    config: NodeConfig,
    listen: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let address: SocketAddr = listen.as_deref().unwrap_or(&config.listen).parse()?;
    let service = NodeService::open(paths, config)?;
    println!(
        "{} listening on {} as {}",
        diri_node::NODE_BUILD,
        address,
        service.config().node_id
    );
    println!(
        "Bind this to a Tailscale address for remote access; the node also requires its app token."
    );
    let maintenance = service.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = maintenance.refresh_usage().await {
                eprintln!("diri-node usage refresh failed: {error}");
            }
        }
    });
    NodeServer::new(service).run(address).await?;
    Ok(())
}

async fn account_command(
    arguments: &[String],
    client: &NodeClient,
) -> Result<(), Box<dyn std::error::Error>> {
    match arguments.first().map(String::as_str) {
        Some("list") => {
            let catalog = client.accounts().await?;
            if catalog.profiles.is_empty() {
                println!("No account profiles. Add one with `diri-node account add`.");
            }
            for profile in catalog.profiles {
                let default = catalog
                    .defaults
                    .get(&profile.provider)
                    .is_some_and(|id| id == &profile.id);
                println!(
                    "{}\t{}\t{}{}",
                    profile.provider.as_str(),
                    profile.id,
                    profile.label,
                    if default { "\t(default)" } else { "" }
                );
            }
        }
        Some("add") => {
            let provider = parse_provider(required_flag(arguments, "--provider")?)?;
            let id = required_flag(arguments, "--id")?.to_owned();
            let label = flag(arguments, "--label").unwrap_or(&id).to_owned();
            let profile = client
                .upsert_account(AccountUpsertParams {
                    id,
                    provider,
                    label,
                    email: flag(arguments, "--email").map(str::to_owned),
                    organization: flag(arguments, "--organization").map(str::to_owned),
                    tags: Vec::new(),
                })
                .await?;
            println!(
                "Added {} profile `{}`.",
                profile.provider.as_str(),
                profile.id
            );
        }
        Some("default") => {
            let provider = parse_provider(required_flag(arguments, "--provider")?)?;
            let profile_id = required_flag(arguments, "--id")?.to_owned();
            client
                .set_default_account(AccountSetDefaultParams {
                    provider,
                    profile_id: profile_id.clone(),
                })
                .await?;
            println!(
                "Default {} profile is now `{profile_id}`.",
                provider.as_str()
            );
        }
        Some("status") => {
            let profile_id = required_flag(arguments, "--id")?;
            let status = client.account_status(profile_id).await?;
            println!(
                "{} on {}: {:?}{}{}",
                status.profile_id,
                status.node_id,
                status.status,
                status
                    .identity
                    .as_deref()
                    .map_or(String::new(), |identity| format!(" · {identity}")),
                status
                    .plan
                    .as_deref()
                    .map_or(String::new(), |plan| format!(" · {plan}")),
            );
        }
        Some("login") => {
            let profile_id = required_flag(arguments, "--id")?.to_owned();
            let catalog = client.accounts().await?;
            let profile = catalog
                .profiles
                .iter()
                .find(|profile| profile.id == profile_id)
                .ok_or_else(|| format!("unknown profile `{profile_id}`"))?;
            let mode = match flag(arguments, "--mode") {
                Some("browser") => LoginMode::Browser,
                Some("interactive") => LoginMode::Interactive,
                Some("device") | None if profile.provider == ProviderKind::Codex => {
                    LoginMode::DeviceCode
                }
                None => LoginMode::Interactive,
                Some(value) => return Err(format!("unknown login mode `{value}`").into()),
            };
            let mut challenge = client
                .begin_login(AccountLoginStartParams { profile_id, mode })
                .await?;
            let mut printed = 0;
            loop {
                if let Some(url) = &challenge.verification_url {
                    println!("Open: {url}");
                }
                if let Some(code) = &challenge.user_code {
                    println!("Code: {code}");
                }
                if challenge.output.len() > printed {
                    print!("{}", &challenge.output[printed..]);
                    if !challenge.output.ends_with('\n') {
                        println!();
                    }
                    printed = challenge.output.len();
                }
                if challenge.complete {
                    if challenge.success {
                        println!("Login complete.");
                        break;
                    }
                    return Err(challenge
                        .error
                        .unwrap_or_else(|| "login failed".into())
                        .into());
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                challenge = client.poll_login(&challenge.login_id).await?;
            }
        }
        _ => return Err("account command must be list, add, default, status, or login".into()),
    }
    Ok(())
}

async fn handoff_command(
    arguments: &[String],
    source: &NodeClient,
    target: &NodeClient,
) -> Result<(), Box<dyn std::error::Error>> {
    let provider = parse_provider(required_flag(arguments, "--provider")?)?;
    let profile_id = required_flag(arguments, "--profile")?.to_owned();
    let session_id = required_flag(arguments, "--session")?.to_owned();
    let provider_session_id = required_flag(arguments, "--provider-session")?.to_owned();
    let workspace_root = required_flag(arguments, "--workspace")?.to_owned();
    let mode = match flag(arguments, "--mode") {
        Some("move") | None => TransferMode::Move,
        Some("fork") => TransferMode::Fork,
        Some(value) => return Err(format!("unknown handoff mode `{value}`").into()),
    };
    let result = source
        .handoff(
            target,
            CheckpointPrepareParams {
                session_id,
                provider,
                profile_id,
                workspace_root,
                provider_session_id: Some(provider_session_id),
                mode,
            },
        )
        .await?;
    println!(
        "{} complete: {} -> {}",
        match mode {
            TransferMode::Move => "Move",
            TransferMode::Fork => "Fork",
        },
        result.checkpoint.source_node_id,
        result
            .target_commit
            .target_node_id
            .as_deref()
            .unwrap_or("target")
    );
    println!("Checkpoint: {}", result.checkpoint.checkpoint_id);
    println!("Workspace: {}", result.staged.quarantine_path);
    if let Some(portable) = &result.portable_config {
        print_portable_config(portable);
    } else {
        println!("Portable config: skipped (one node predates portable-config.v1)");
    }
    println!(
        "Provider: {}",
        serde_json::to_string(&result.provider_result)?
    );
    Ok(())
}

fn print_portable_config(result: &PortableConfigApplyResult) {
    println!(
        "Portable config: {} installed, {} unchanged, {} conflicts, {} omitted",
        result.installed.len(),
        result.unchanged.len(),
        result.conflicts.len(),
        result.omitted.len()
    );
    for path in &result.installed {
        println!("  installed: {}", terminal_text(path));
    }
    for path in &result.conflicts {
        println!("  kept target conflict: {}", terminal_text(path));
    }
    for omission in &result.omitted {
        println!(
            "  omitted: {} ({})",
            terminal_text(&omission.path),
            terminal_text(&omission.reason)
        );
    }
}

fn terminal_text(value: &str) -> String {
    value.chars().fold(String::new(), |mut output, character| {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
        output
    })
}

fn management_client(
    paths: &NodePaths,
    listen: Option<String>,
    remote: &RemoteOptions,
    home: &std::path::Path,
) -> Result<NodeClient, Box<dyn std::error::Error>> {
    if remote.configured() {
        remote.client(home)
    } else {
        let config = NodeConfig::load_or_initialize(paths)?;
        Ok(local_client(&config, listen))
    }
}

fn reject_target_options(target: &RemoteOptions) -> Result<(), Box<dyn std::error::Error>> {
    if target.configured() {
        Err("--target-* options are only valid with `handoff`".into())
    } else {
        Ok(())
    }
}

fn reject_remote_options(
    remote: &RemoteOptions,
    target: &RemoteOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    if remote.configured() || target.configured() {
        Err("remote connection options are not valid for this command".into())
    } else {
        Ok(())
    }
}

fn local_client(config: &NodeConfig, listen: Option<String>) -> NodeClient {
    NodeClient::new(NodeClientConfig {
        endpoint: format!("tcp://{}", listen.as_deref().unwrap_or(&config.listen)),
        token: config.auth_token.clone(),
        expected_node_id: Some(config.node_id.clone()),
        build: diri_node::NODE_BUILD.into(),
    })
}

fn parse_provider(value: &str) -> Result<ProviderKind, Box<dyn std::error::Error>> {
    match value {
        "claude" => Ok(ProviderKind::Claude),
        "codex" => Ok(ProviderKind::Codex),
        _ => Err(format!("unknown provider `{value}`").into()),
    }
}

fn take_option(arguments: &mut Vec<String>, name: &str) -> Option<String> {
    let index = arguments.iter().position(|argument| argument == name)?;
    arguments.remove(index);
    (index < arguments.len()).then(|| arguments.remove(index))
}

fn flag<'a>(arguments: &'a [String], name: &str) -> Option<&'a str> {
    let index = arguments.iter().position(|argument| argument == name)?;
    arguments.get(index + 1).map(String::as_str)
}

fn required_flag<'a>(
    arguments: &'a [String],
    name: &str,
) -> Result<&'a str, Box<dyn std::error::Error>> {
    flag(arguments, name).ok_or_else(|| format!("missing {name}").into())
}

fn print_help() {
    println!(
        r#"diri-node — first-party Dirijor execution node

Usage:
  diri-node init [--home PATH]
  diri-node status [--home PATH]
  diri-node serve [--home PATH] [--listen IP:PORT]
  diri-node account list
  diri-node account add --provider claude|codex --id ID [--label LABEL]
  diri-node account default --provider claude|codex --id ID
  diri-node account status --id ID
  diri-node account login --id ID [--mode device|browser|interactive]
  diri-node account sync-config --id ID \
    --target-endpoint tcp://HOST:PORT --target-token-file PATH [--target-node-id ID]
  diri-node handoff --provider claude|codex --profile ID --session ID \
    --provider-session ID --workspace PATH [--mode move|fork] \
    --target-endpoint tcp://HOST:PORT --target-token-file PATH [--target-node-id ID]

status, account, and handoff can target a remote source with:
  --endpoint tcp://HOST:PORT --token-file PATH [--node-id ID]

Account commands talk to the running node. The default listener is loopback-only.
For a VPS, bind its Tailscale IP and keep the enrollment token owner-only."#
    );
}
