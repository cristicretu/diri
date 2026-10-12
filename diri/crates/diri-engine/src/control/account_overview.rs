//! What the account menu shows, and the one-click ways to keep a login.
//!
//! `account.overview` reads, for every local profile that shares its Agent's
//! home, whose login it holds (email, organization, plan) and which login new
//! tabs use now -- including one that no profile holds yet. Identities come
//! from the logins themselves (Codex's ID token, Claude's account record).
//! Nothing here returns or logs a credential.
//!
//! `account.adopt` saves the login in use now as a profile named after its
//! email. `account.add` creates a profile and opens its sign-in tab; the
//! profile takes its email's name once the login lands.
use super::claude_accounts::{self as claude, Store};
use super::codex_accounts as codex;
use super::*;
use base64::Engine as _;
use diri_proto::{
    AgentAccountAgent, AgentAccountCatalog, AgentAccountIdentity, AgentAccountLogin,
    AgentAccountOverview, AgentAccountProfile, AgentKind, AgentLiveLogin,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Marks a profile `account.add` created: it is named after the login's
/// email once one lands in its slot.
const PENDING_NAME: &str = ".pending-name";
/// What `claude auth status` reported for a Claude slot, kept for display.
const REPORTED_IDENTITY: &str = "identity.json";
/// Longest generated profile name.
const NAME_LIMIT: usize = 40;

/// Claude slots whose identity was already asked for this Engine run, so a
/// login Claude will not describe is not asked about on every menu open.
static ASKED: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Default::default);
/// A store whose question failed (Keychain locked, Claude slow) is asked
/// again after this long, not never.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(5 * 60);

/// A login's identity and the key that tells its account apart.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Known {
    /// `claude:<account>:<organization>` or `codex:<user>:<workspace>`.
    /// `None` when only the email is known.
    pub(super) key: Option<String>,
    pub(super) identity: AgentAccountIdentity,
}

impl Known {
    pub(super) fn same_account(&self, other: &Known) -> bool {
        match (&self.key, &other.key) {
            (Some(a), Some(b)) => a == b,
            _ => {
                self.identity.email.is_some()
                    && self.identity.email.as_deref().map(str::to_lowercase)
                        == other.identity.email.as_deref().map(str::to_lowercase)
                    && (self.identity.organization.is_none()
                        || other.identity.organization.is_none()
                        || self.identity.organization == other.identity.organization)
            }
        }
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        .map(str::to_owned)
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Who a Codex `auth.json` belongs to.
pub(super) fn codex_identity(bytes: &[u8]) -> Option<Known> {
    let auth: Value = serde_json::from_slice(bytes).ok()?;
    if let Some(token) = text(auth.pointer("/tokens/id_token")) {
        let claims = jwt_claims(&token).unwrap_or(Value::Null);
        let openai = claims
            .get("https://api.openai.com/auth")
            .unwrap_or(&Value::Null);
        let user = text(openai.get("chatgpt_user_id"))
            .or_else(|| text(openai.get("user_id")))
            .or_else(|| text(claims.get("sub")))
            .or_else(|| text(claims.get("email")));
        let workspace = text(openai.get("chatgpt_account_id"))
            .or_else(|| text(auth.pointer("/tokens/account_id")));
        return Some(Known {
            key: Some(format!(
                "codex:{}:{}",
                user.as_deref().unwrap_or("?"),
                workspace.as_deref().unwrap_or("-")
            )),
            identity: AgentAccountIdentity {
                email: text(claims.get("email")),
                organization: None,
                plan: text(openai.get("chatgpt_plan_type")),
            },
        });
    }
    let key = text(auth.get("OPENAI_API_KEY"))?;
    let digest = Sha256::digest(key.as_bytes());
    let short: String = digest[..6].iter().map(|b| format!("{b:02x}")).collect();
    Some(Known {
        key: Some(format!("codex:apikey:{short}")),
        identity: AgentAccountIdentity {
            plan: Some("API key".into()),
            ..Default::default()
        },
    })
}

/// Who a Claude `oauthAccount` record names.
pub(super) fn claude_identity(account: &Value) -> Option<Known> {
    let account = account.as_object()?;
    let user = text(account.get("accountUuid"));
    let email = text(account.get("emailAddress"));
    if user.is_none() && email.is_none() {
        return None;
    }
    let organization = text(account.get("organizationUuid"));
    Some(Known {
        key: user.map(|user| format!("claude:{user}:{}", organization.as_deref().unwrap_or("-"))),
        identity: AgentAccountIdentity {
            email,
            organization: text(account.get("organizationName")),
            plan: None,
        },
    })
}

/// The account record a sign-in tab left in the slot's own `.claude.json`.
pub(super) fn signed_in_account(store: &str) -> Option<Value> {
    let bytes = claude::read_private(&Path::new(store).join(".claude.json"))
        .ok()
        .flatten()?;
    serde_json::from_slice::<Value>(&bytes)
        .ok()?
        .get("oauthAccount")
        .filter(|account| account.is_object())
        .cloned()
}

/// The account record Diri saved beside a store when it took the login from
/// Claude's default store (save) or switched away from it.
fn saved_record(store: &str) -> Option<Known> {
    claude::read_private(&claude::snapshot_path(store))
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|account| claude_identity(&account))
}

/// Who a Claude credential store's login belongs to: the record saved beside
/// it, else the one its sign-in left, completed by what `claude auth status`
/// reported.
pub(super) fn claude_store_identity(store: &str) -> Option<Known> {
    let recorded = saved_record(store)
        .or_else(|| signed_in_account(store).and_then(|account| claude_identity(&account)));
    let reported = claude::read_private(&Path::new(store).join(REPORTED_IDENTITY))
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<AgentAccountIdentity>(&bytes).ok())
        .filter(|identity| identity.email.is_some());
    match (recorded, reported) {
        (Some(mut known), Some(reported)) => {
            if known.identity.email == reported.email {
                known.identity.plan = reported.plan;
            }
            Some(known)
        }
        (Some(known), None) => Some(known),
        (None, Some(identity)) => Some(Known {
            key: None,
            identity,
        }),
        (None, None) => None,
    }
}

/// A profile name from an email: its local part.
fn email_name(email: &str) -> Option<String> {
    let local = email.split('@').next()?.trim();
    (!local.is_empty()).then(|| local.to_owned())
}

fn agent_title(agent: &str) -> &'static str {
    if agent == AgentKind::CODEX_ID {
        "Codex"
    } else {
        "Claude Code"
    }
}

/// `base`, or `base 2`, `base 3`, … so local profiles of one Agent never
/// share a name.
fn unique_label(
    catalog: &AgentAccountCatalog,
    agent: &str,
    base: &str,
    except: Option<&str>,
) -> String {
    let mut base: String = base
        .chars()
        .filter(|c| !c.is_control())
        .take(NAME_LIMIT)
        .collect();
    base = base.trim().to_owned();
    if base.is_empty() {
        base = agent_title(agent).to_owned();
    }
    let taken = |name: &str| {
        catalog.profiles.iter().any(|p| {
            p.agent == agent
                && p.host.is_none()
                && Some(p.id.as_str()) != except
                && p.label.eq_ignore_ascii_case(name)
        })
    };
    if !taken(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base} {n}"))
        .find(|name| !taken(name))
        .expect("an unused name")
}

fn new_profile_id(catalog: &AgentAccountCatalog, agent: &str) -> String {
    let prefix = if agent == AgentKind::CODEX_ID {
        "codex"
    } else {
        "claude"
    };
    loop {
        let id = format!("{prefix}-{}", &crate::inject::uuid_v4()[..8]);
        if !catalog.profiles.iter().any(|p| p.id == id) {
            return id;
        }
    }
}

fn shares_home(profile: &AgentAccountProfile) -> bool {
    profile.host.is_none()
        && matches!(
            profile.agent.as_str(),
            AgentKind::CODEX_ID | AgentKind::CLAUDE_CODE_ID
        )
}

fn expand_home(path: &str) -> PathBuf {
    path.strip_prefix("~/").map_or_else(
        || PathBuf::from(path),
        |relative| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(relative),
    )
}

fn user_home() -> Result<PathBuf, ControlError> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| ControlError::internal("HOME is not set"))
}

/// The paths an overview reads; tests point them into a sandbox.
pub(super) struct Homes {
    pub(super) claude: PathBuf,
    pub(super) codex: PathBuf,
}

impl Homes {
    fn detect() -> Result<Self, ControlError> {
        let home = user_home()?;
        Ok(Self {
            claude: home.join(".claude"),
            codex: home.join(".codex"),
        })
    }
}

// MARK: Overview

impl ControlServer {
    /// A profile's private login directory, without creating it.
    fn slot_dir(&self, agent: &str, id: &str) -> Option<PathBuf> {
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return None;
        }
        let root = if agent == AgentKind::CODEX_ID {
            "codex-logins"
        } else {
            "claude-logins"
        };
        Some(self.socket_path.parent()?.join(root).join(id))
    }

    pub(super) fn account_overview(&self, _params: Option<Value>) -> Result<Value, ControlError> {
        encode(&self.overview_at(&Homes::detect()?)?)
    }

    pub(super) fn overview_at(&self, homes: &Homes) -> Result<AgentAccountOverview, ControlError> {
        let mut catalog = self.accounts.lock().map_err(poisoned)?.catalog()?;
        let open: HashSet<String> = self
            .registry
            .lock()
            .map_err(poisoned)?
            .records()
            .into_iter()
            .filter(|r| !matches!(r.status, diri_proto::SessionStatus::Exited(_)))
            .map(|r| r.cwd)
            .collect();

        let mut logins = Vec::new();
        let mut knowns: Vec<(String, Known)> = Vec::new();
        for profile in catalog.profiles.iter().filter(|p| shares_home(p)) {
            let (login, known) = if profile.agent == AgentKind::CODEX_ID {
                self.codex_login(profile, homes, &open)
            } else {
                self.claude_login(profile, homes, &open)
            };
            if let Some(known) = known {
                knowns.push((profile.id.clone(), known));
            }
            logins.push(login);
        }
        let known_of = |id: &str| knowns.iter().find(|(p, _)| p == id).map(|(_, k)| k);

        // Profiles added by signing in take their email's name once known.
        let renamed = self.name_pending_profiles(&mut catalog, &knowns);
        if renamed {
            catalog = self.accounts.lock().map_err(poisoned)?.catalog()?;
        }

        let mut live = Vec::new();
        // Codex: every local tab reads the one shared auth.json.
        if let Some(known) = codex::read(&homes.codex.join("auth.json"))
            .ok()
            .flatten()
            .as_deref()
            .and_then(codex_identity)
        {
            let owner = catalog
                .profiles
                .iter()
                .filter(|p| p.agent == AgentKind::CODEX_ID && shares_home(p))
                .filter(|p| known_of(&p.id).is_some_and(|k| k.same_account(&known)))
                .max_by_key(|p| p.is_default)
                .map(|p| p.id.clone());
            if let Some(owner) = &owner
                && let Some(login) = logins.iter_mut().find(|l| &l.profile_id == owner)
            {
                // The shared file is fresher than the slot: Codex refreshes it.
                login.login_file = Some(homes.codex.join("auth.json").to_string_lossy().into());
                login.signed_in = true;
            }
            live.push(AgentLiveLogin {
                agent: AgentKind::CODEX_ID.into(),
                profile_id: owner,
                identity: known.identity,
            });
        }
        // Claude: the default profile's store, else Claude's own default store.
        let default_claude = catalog.profiles.iter().find(|p| {
            p.agent == AgentKind::CLAUDE_CODE_ID
                && p.host.is_none()
                && p.is_default
                && p.login_store.is_some()
        });
        if let Some(profile) = default_claude {
            live.push(AgentLiveLogin {
                agent: AgentKind::CLAUDE_CODE_ID.into(),
                profile_id: Some(profile.id.clone()),
                identity: known_of(&profile.id)
                    .map(|k| k.identity.clone())
                    .unwrap_or_default(),
            });
        } else if let Some(known) = claude::read_global_config(&homes.claude)
            .and_then(|config| config.get("oauthAccount").cloned())
            .and_then(|account| claude_identity(&account))
            && claude::has_login(Store::Default, &homes.claude).unwrap_or(true)
        {
            let owner = catalog
                .profiles
                .iter()
                .filter(|p| p.agent == AgentKind::CLAUDE_CODE_ID && shares_home(p))
                .find(|p| known_of(&p.id).is_some_and(|k| k.same_account(&known)))
                .map(|p| p.id.clone());
            live.push(AgentLiveLogin {
                agent: AgentKind::CLAUDE_CODE_ID.into(),
                profile_id: owner,
                identity: known.identity,
            });
        }

        Ok(AgentAccountOverview {
            catalog,
            logins,
            live,
        })
    }

    fn codex_login(
        &self,
        profile: &AgentAccountProfile,
        homes: &Homes,
        open: &HashSet<String>,
    ) -> (AgentAccountLogin, Option<Known>) {
        let slot = self.slot_dir(AgentKind::CODEX_ID, &profile.id);
        let mut file = slot.as_ref().map(|slot| slot.join("auth.json"));
        let mut bytes = file
            .as_deref()
            .and_then(|file| codex::read(file).ok().flatten());
        // A profile from the older isolated-home design keeps its own file.
        let home = expand_home(&profile.config_home);
        if bytes.is_none() && home != homes.codex {
            file = Some(home.join("auth.json"));
            bytes = codex::read(&home.join("auth.json")).ok().flatten();
        }
        let known = bytes.as_deref().and_then(codex_identity);
        (
            AgentAccountLogin {
                profile_id: profile.id.clone(),
                identity: known
                    .as_ref()
                    .map(|k| k.identity.clone())
                    .unwrap_or_default(),
                signed_in: bytes.is_some(),
                signing_in: slot
                    .as_ref()
                    .is_some_and(|slot| open.contains(&*slot.to_string_lossy())),
                login_file: bytes
                    .is_some()
                    .then(|| file.map(|f| f.to_string_lossy().into_owned()))
                    .flatten(),
            },
            known,
        )
    }

    fn claude_login(
        &self,
        profile: &AgentAccountProfile,
        homes: &Homes,
        open: &HashSet<String>,
    ) -> (AgentAccountLogin, Option<Known>) {
        let mut login = AgentAccountLogin {
            profile_id: profile.id.clone(),
            ..Default::default()
        };
        let Some(store) = profile.login_store.as_deref() else {
            return (login, None);
        };
        login.signing_in = open.contains(store);
        let mut known = claude_store_identity(store);
        login.signed_in = known.is_some()
            || claude::has_login(Store::Slot(store), &homes.claude).unwrap_or(false);
        if known.is_none() && login.signed_in && !login.signing_in {
            known = self.learn_claude_identity(store);
        }
        if let Some(known) = &known {
            login.identity = known.identity.clone();
        }
        (login, known)
    }

    /// Ask Claude who a store's login belongs to (once per sign-in, or again
    /// after a while when it could not answer), and keep the answer beside
    /// the store for next time.
    fn learn_claude_identity(&self, store: &str) -> Option<Known> {
        {
            let mut asked = ASKED.lock().ok()?;
            if asked
                .get(store)
                .is_some_and(|at| at.elapsed() < ASK_AGAIN_AFTER)
            {
                return None;
            }
            asked.insert(store.to_owned(), Instant::now());
        }
        let reported = self.claude_auth_status(store)?;
        if reported.get("loggedIn") != Some(&Value::Bool(true)) {
            return None;
        }
        let identity = AgentAccountIdentity {
            email: text(reported.get("email")),
            organization: text(reported.get("orgName")),
            plan: text(reported.get("subscriptionType")),
        };
        identity.email.as_ref()?;
        if let Ok(bytes) = serde_json::to_vec_pretty(&identity) {
            let _ = claude::write_private(&Path::new(store).join(REPORTED_IDENTITY), &bytes);
        }
        Some(Known {
            key: None,
            identity,
        })
    }

    /// A new sign-in may land another account: forget what was known.
    pub(super) fn forget_claude_identity(store: &Path) {
        let _ = fs::remove_file(store.join(REPORTED_IDENTITY));
        let _ = fs::remove_file(claude::snapshot_path(&store.to_string_lossy()));
        if let Ok(mut asked) = ASKED.lock() {
            asked.remove(&*store.to_string_lossy());
        }
    }

    /// Before the first switch, local Claude tabs run on Claude's default
    /// store and refresh (rotate) its tokens there. When that account was
    /// saved from the default store, give its profile the latest login before
    /// switching away, so switching back does not land a stale copy. Best
    /// effort: the default store itself is never changed.
    pub(super) fn save_default_store_login(&self, home: &Path, except_store: &str) {
        let Some(live) = claude::read_global_config(home)
            .and_then(|config| config.get("oauthAccount").cloned())
            .and_then(|account| claude_identity(&account))
        else {
            return;
        };
        let Ok(catalog) = self
            .accounts
            .lock()
            .map_err(poisoned)
            .and_then(|a| a.catalog())
        else {
            return;
        };
        // Only a profile whose record came from the default store: one signed
        // in through its own tab may hold a newer login of the same account.
        if let Some(store) = catalog
            .profiles
            .iter()
            .filter(|p| p.agent == AgentKind::CLAUDE_CODE_ID && p.host.is_none())
            .filter_map(|p| p.login_store.as_deref())
            .filter(|store| *store != except_store)
            .find(|store| saved_record(store).is_some_and(|k| k.same_account(&live)))
        {
            let _ = claude::copy_login(Store::Default, Store::Slot(store), home);
        }
    }

    /// A name the user chose is never replaced by the email's.
    pub(super) fn keep_chosen_name(&self, profile: &AgentAccountProfile) {
        if let Some(slot) = self.slot_dir(&profile.agent, &profile.id) {
            let _ = fs::remove_file(slot.join(PENDING_NAME));
        }
    }

    /// Name each profile created by `account.add` after its login's email.
    /// Skipped while an account operation holds the catalog.
    fn name_pending_profiles(
        &self,
        catalog: &mut AgentAccountCatalog,
        knowns: &[(String, Known)],
    ) -> bool {
        let Ok(_shared) = self.account_operations.try_read() else {
            return false;
        };
        // Decide against the catalog as it is now, under its lock: the user
        // may have renamed or removed the profile while Claude was asked.
        // A rename clears the marker before releasing this lock.
        let Ok(accounts) = self.accounts.lock() else {
            return false;
        };
        let Ok(mut current) = accounts.catalog() else {
            return false;
        };
        let mut renamed = false;
        for (id, known) in knowns {
            let Some(profile) = current.profiles.iter().find(|p| &p.id == id).cloned() else {
                continue;
            };
            let Some(marker) = self
                .slot_dir(&profile.agent, &profile.id)
                .map(|slot| slot.join(PENDING_NAME))
                .filter(|marker| marker.exists())
            else {
                continue;
            };
            let Some(name) = known.identity.email.as_deref().and_then(email_name) else {
                continue;
            };
            let mut named = profile.clone();
            named.label = unique_label(&current, &profile.agent, &name, Some(&profile.id));
            let Ok(updated) = accounts.upsert(named) else {
                continue;
            };
            current = updated;
            let _ = fs::remove_file(marker);
            renamed = true;
        }
        if renamed {
            *catalog = current;
        }
        renamed
    }

    // MARK: Adopt and add

    pub(super) fn account_adopt(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: AgentAccountAgent = decode(params)?;
        let homes = Homes::detect()?;
        match p.agent.as_str() {
            AgentKind::CODEX_ID => self.adopt_codex(self.codex_home()?),
            AgentKind::CLAUDE_CODE_ID => self.adopt_claude(&self.claude_home()?),
            _ => Err(ControlError::bad_request(
                "Account switching supports Claude Code and Codex",
            )),
        }?;
        encode(&self.overview_at(&homes)?)
    }

    /// Save the Codex login in use now: into the profile that already holds
    /// that account, or a new one named after its email.
    pub(super) fn adopt_codex(&self, home: PathBuf) -> Result<String, ControlError> {
        let bytes = codex::read(&home.join("auth.json"))?.ok_or_else(|| {
            ControlError::bad_request("No Codex login is active on this Mac. Sign in first.")
        })?;
        let live = codex_identity(&bytes).ok_or_else(|| {
            ControlError::bad_request("The Codex login in use could not be read.")
        })?;
        let catalog = self.accounts.lock().map_err(poisoned)?.catalog()?;
        let existing = catalog
            .profiles
            .iter()
            .filter(|p| p.agent == AgentKind::CODEX_ID && shares_home(p))
            .find(|p| {
                self.slot_dir(AgentKind::CODEX_ID, &p.id)
                    .and_then(|slot| codex::read(&slot.join("auth.json")).ok().flatten())
                    .as_deref()
                    .and_then(codex_identity)
                    .is_some_and(|k| k.same_account(&live))
            })
            .cloned();
        let has_default = catalog
            .profiles
            .iter()
            .any(|p| p.agent == AgentKind::CODEX_ID && p.host.is_none() && p.is_default);
        let mut profile = existing.unwrap_or_else(|| AgentAccountProfile {
            id: new_profile_id(&catalog, AgentKind::CODEX_ID),
            label: unique_label(
                &catalog,
                AgentKind::CODEX_ID,
                live.identity
                    .email
                    .as_deref()
                    .and_then(email_name)
                    .as_deref()
                    .unwrap_or("Codex"),
                None,
            ),
            agent: AgentKind::CODEX_ID.into(),
            host: None,
            config_home: String::new(),
            is_default: false,
            login_store: None,
        });
        let slot = self.codex_slot(&profile.id)?;
        codex::write(&slot.join("auth.json"), &bytes)?;
        profile.config_home = home.to_string_lossy().into_owned();
        // It is the login every local Codex tab uses already.
        profile.is_default = profile.is_default || !has_default;
        let id = profile.id.clone();
        self.accounts.lock().map_err(poisoned)?.upsert(profile)?;
        Ok(id)
    }

    /// Save the login Claude's default store holds into a profile of its
    /// own. Only meaningful before the first switch: afterwards new tabs use
    /// the default profile's store, which is already saved.
    pub(super) fn adopt_claude(&self, home: &Path) -> Result<String, ControlError> {
        let catalog = self.accounts.lock().map_err(poisoned)?.catalog()?;
        if let Some(current) = catalog.profiles.iter().find(|p| {
            p.agent == AgentKind::CLAUDE_CODE_ID
                && p.host.is_none()
                && p.is_default
                && p.login_store.is_some()
        }) {
            return Err(ControlError::bad_request(format!(
                "The Claude login in use is already saved as {}.",
                current.label
            )));
        }
        let live = claude::read_global_config(home)
            .and_then(|config| config.get("oauthAccount").cloned())
            .and_then(|account| claude_identity(&account));
        // Refresh a profile saved from the default store before. One signed in
        // through its own tab keeps its login: the default store's record may
        // be all that matches it.
        let existing = live.as_ref().and_then(|live| {
            catalog
                .profiles
                .iter()
                .filter(|p| p.agent == AgentKind::CLAUDE_CODE_ID && shares_home(p))
                .find(|p| {
                    p.login_store
                        .as_deref()
                        .and_then(saved_record)
                        .is_some_and(|k| k.same_account(live))
                })
                .cloned()
        });
        let profile = existing.unwrap_or_else(|| AgentAccountProfile {
            id: new_profile_id(&catalog, AgentKind::CLAUDE_CODE_ID),
            label: unique_label(
                &catalog,
                AgentKind::CLAUDE_CODE_ID,
                live.as_ref()
                    .and_then(|k| k.identity.email.as_deref())
                    .and_then(email_name)
                    .as_deref()
                    .unwrap_or("Claude Code"),
                None,
            ),
            agent: AgentKind::CLAUDE_CODE_ID.into(),
            host: None,
            config_home: String::new(),
            is_default: false,
            login_store: None,
        });
        let slot = self.claude_slot(&profile.id)?;
        let store = slot.to_string_lossy().into_owned();
        Self::forget_claude_identity(&slot);
        claude::copy_login(Store::Default, Store::Slot(&store), home)?;
        claude::snapshot_identity(home, &store);
        let id = profile.id.clone();
        self.adopt_claude_slot(profile, home, &slot)?;
        Ok(id)
    }

    /// A new profile for `agent` and its sign-in tab. The profile is named
    /// after the login's email once the sign-in lands.
    pub(super) fn account_add(&self, params: Option<Value>) -> Result<Value, ControlError> {
        let p: AgentAccountAgent = decode(params)?;
        self.add_account_at(&p.agent, &Homes::detect()?)
    }

    pub(super) fn add_account_at(&self, agent: &str, homes: &Homes) -> Result<Value, ControlError> {
        let p = AgentAccountAgent {
            agent: agent.to_owned(),
        };
        if !matches!(
            p.agent.as_str(),
            AgentKind::CODEX_ID | AgentKind::CLAUDE_CODE_ID
        ) {
            return Err(ControlError::bad_request(
                "Account switching supports Claude Code and Codex",
            ));
        }
        let accounts = self.accounts.lock().map_err(poisoned)?;
        let catalog = accounts.catalog()?;
        let home = if p.agent == AgentKind::CODEX_ID {
            &homes.codex
        } else {
            &homes.claude
        };
        let profile = AgentAccountProfile {
            id: new_profile_id(&catalog, &p.agent),
            label: unique_label(&catalog, &p.agent, agent_title(&p.agent), None),
            agent: p.agent.clone(),
            host: None,
            config_home: home.to_string_lossy().into_owned(),
            is_default: false,
            login_store: None,
        };
        accounts.upsert(profile.clone())?;
        drop(accounts);
        let slot = if p.agent == AgentKind::CODEX_ID {
            self.codex_slot(&profile.id)?
        } else {
            self.claude_slot(&profile.id)?
        };
        let _ = claude::write_private(&slot.join(PENDING_NAME), b"");
        let params = Some(json!({ "id": profile.id }));
        let opened = if p.agent == AgentKind::CODEX_ID {
            self.codex_account_login(params)
        } else {
            self.claude_account_login(params)
        };
        if opened.is_err() {
            // Nothing was signed in: leave no empty profile behind.
            let _ = self.accounts.lock().map_err(poisoned)?.remove(&profile.id);
        }
        opened
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn jwt(claims: Value) -> String {
        let encode = |v: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
        };
        format!("{}.{}.sig", encode(&json!({"alg":"none"})), encode(&claims))
    }

    /// A Codex `auth.json` as `codex login` writes it.
    pub(crate) fn codex_auth(email: &str, user: &str, workspace: &str, refresh: &str) -> Vec<u8> {
        let id = jwt(json!({
            "email": email,
            "sub": format!("sub-{user}"),
            "https://api.openai.com/auth": {
                "chatgpt_user_id": user,
                "chatgpt_account_id": workspace,
                "chatgpt_plan_type": "pro",
            }
        }));
        serde_json::to_vec(&json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {"id_token": id, "access_token": "fixture-access", "refresh_token": refresh, "account_id": workspace},
        }))
        .unwrap()
    }

    fn sandbox(tmp: &Path) -> (Arc<ControlServer>, Homes) {
        let server = super::super::tests::server(tmp);
        let homes = Homes {
            claude: tmp.join("home/.claude"),
            codex: tmp.join("home/.codex"),
        };
        for dir in [&homes.claude, &homes.codex] {
            fs::create_dir_all(dir).unwrap();
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        (server, homes)
    }

    #[test]
    fn identities_tell_accounts_and_workspaces_apart() {
        let known = codex_identity(&codex_auth("me@example.test", "user-1", "ws-1", "rt")).unwrap();
        assert_eq!(known.key.as_deref(), Some("codex:user-1:ws-1"));
        assert_eq!(known.identity.email.as_deref(), Some("me@example.test"));
        assert_eq!(known.identity.plan.as_deref(), Some("pro"));
        let refreshed =
            codex_identity(&codex_auth("me@example.test", "user-1", "ws-1", "rt-2")).unwrap();
        assert!(
            known.same_account(&refreshed),
            "a refresh is the same account"
        );
        let other_workspace =
            codex_identity(&codex_auth("me@example.test", "user-1", "ws-2", "rt")).unwrap();
        assert!(
            !known.same_account(&other_workspace),
            "a workspace is its own account"
        );

        let api = codex_identity(br#"{"OPENAI_API_KEY":"sk-fixture"}"#).unwrap();
        assert!(api.key.unwrap().starts_with("codex:apikey:"));

        let claude = claude_identity(&json!({
            "accountUuid": "acct-1", "organizationUuid": "org-1",
            "emailAddress": "me@example.test", "organizationName": "Mine"
        }))
        .unwrap();
        assert_eq!(claude.key.as_deref(), Some("claude:acct-1:org-1"));
        let by_email = Known {
            key: None,
            identity: AgentAccountIdentity {
                email: Some("ME@example.test".into()),
                ..Default::default()
            },
        };
        assert!(
            claude.same_account(&by_email),
            "an email alone still matches"
        );
    }

    #[test]
    fn generated_names_come_from_the_email_and_never_collide() {
        let profile = |id: &str, label: &str, agent: &str| AgentAccountProfile {
            id: id.into(),
            label: label.into(),
            agent: agent.into(),
            host: None,
            config_home: "~/.codex".into(),
            is_default: false,
            login_store: None,
        };
        let catalog = AgentAccountCatalog {
            profiles: vec![
                profile("a", "mihai", "codex"),
                profile("b", "Mihai 2", "codex"),
                profile("c", "work", "claude-code"),
            ],
        };
        let name = email_name("mihai@icloud.com").unwrap();
        assert_eq!(unique_label(&catalog, "codex", &name, None), "mihai 3");
        assert_eq!(unique_label(&catalog, "codex", &name, Some("a")), "mihai");
        assert_eq!(
            unique_label(&catalog, "codex", "work", None),
            "work",
            "names are per Agent"
        );
        assert_eq!(unique_label(&catalog, "codex", " \u{7}", None), "Codex");
        assert_eq!(
            unique_label(&catalog, "codex", &"x".repeat(90), None).len(),
            NAME_LIMIT
        );
    }

    #[test]
    fn the_codex_login_in_use_is_shown_saved_once_and_matched_to_its_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, homes) = sandbox(tmp.path());
        codex::write(
            &homes.codex.join("auth.json"),
            &codex_auth("mihai@example.test", "u1", "ws1", "rt-1"),
        )
        .unwrap();

        let overview = server.overview_at(&homes).unwrap();
        let live = overview.live(AgentKind::CODEX_ID).unwrap();
        assert_eq!(live.profile_id, None, "not saved yet");
        assert_eq!(live.identity.email.as_deref(), Some("mihai@example.test"));

        let id = server.adopt_codex(homes.codex.clone()).unwrap();
        let overview = server.overview_at(&homes).unwrap();
        let profile = overview
            .catalog
            .profiles
            .iter()
            .find(|p| p.id == id)
            .unwrap();
        assert_eq!(profile.label, "mihai");
        assert!(profile.is_default, "it is the login every Codex tab uses");
        assert!(overview.is_live(profile));
        let login = overview.login(&id).unwrap();
        assert!(login.signed_in);
        assert_eq!(login.identity.plan.as_deref(), Some("pro"));
        assert_eq!(
            login.login_file.as_deref(),
            Some(&*homes.codex.join("auth.json").to_string_lossy()),
            "the shared file is fresher than the slot"
        );
        assert!(!serde_json::to_string(&overview).unwrap().contains("rt-1"));

        // Saving again refreshes that profile instead of adding another.
        codex::write(
            &homes.codex.join("auth.json"),
            &codex_auth("mihai@example.test", "u1", "ws1", "rt-2"),
        )
        .unwrap();
        assert_eq!(server.adopt_codex(homes.codex.clone()).unwrap(), id);
        let slot = server.codex_slot(&id).unwrap().join("auth.json");
        assert!(
            String::from_utf8(codex::read(&slot).unwrap().unwrap())
                .unwrap()
                .contains("rt-2")
        );
        assert_eq!(
            server.overview_at(&homes).unwrap().catalog.profiles.len(),
            1
        );
    }

    #[test]
    fn a_profile_added_by_signing_in_is_named_after_its_email() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, homes) = sandbox(tmp.path());
        server
            .agent_catalog
            .lock()
            .unwrap()
            .configure(
                None,
                "codex",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some("/usr/bin/true".into()),
                    show_in_quick_create: Some(true),
                },
            )
            .unwrap();
        let record: diri_proto::SessionRecord =
            serde_json::from_value(server.add_account_at("codex", &homes).unwrap()).unwrap();
        assert_eq!(record.kind, AgentKind::SHELL, "a sign-in tab");
        let overview = server.overview_at(&homes).unwrap();
        let profile = overview.catalog.profiles[0].clone();
        assert_eq!(profile.label, "Codex");
        assert!(!overview.login(&profile.id).unwrap().signed_in);

        // `codex login` lands the account in the profile's slot.
        let slot = server.codex_slot(&profile.id).unwrap();
        codex::write(
            &slot.join("auth.json"),
            &codex_auth("work@corp.test", "u2", "ws2", "rt"),
        )
        .unwrap();
        let _ = server.session_kill(Some(json!({"sessionID": record.id})));
        let overview = server.overview_at(&homes).unwrap();
        assert_eq!(overview.catalog.profiles[0].label, "work");
        assert!(overview.login(&profile.id).unwrap().signed_in);
        assert!(!slot.join(PENDING_NAME).exists());

        // A name the user picked afterwards is theirs.
        let mut renamed = overview.catalog.profiles[0].clone();
        renamed.label = "Day job".into();
        server.accounts.lock().unwrap().upsert(renamed).unwrap();
        let overview = server.overview_at(&homes).unwrap();
        assert_eq!(overview.catalog.profiles[0].label, "Day job");
    }

    #[test]
    fn claudes_answer_completes_the_record_of_the_same_account_only() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, homes) = sandbox(tmp.path());
        let executable = tmp.path().join("claude");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s' '{\"loggedIn\":true,\"email\":\"one@example.test\",\"orgName\":\"One\"}'\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        server
            .agent_catalog
            .lock()
            .unwrap()
            .configure(
                None,
                "claude-code",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some(executable.to_string_lossy().into_owned()),
                    show_in_quick_create: Some(true),
                },
            )
            .unwrap();
        claude::write_private(
            &homes.claude.parent().unwrap().join(".claude.json"),
            br#"{"oauthAccount":{"emailAddress":"one@example.test","accountUuid":"acct-1","displayName":"One"}}"#,
        )
        .unwrap();
        let account = server
            .claude_identity_from_status("/unused", &homes.claude)
            .unwrap();
        assert_eq!(
            account["accountUuid"], "acct-1",
            "fields Diri does not know are kept"
        );
        assert_eq!(account["organizationName"], "One");
    }

    #[test]
    fn naming_after_the_email_never_brings_back_a_removed_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, homes) = sandbox(tmp.path());
        server
            .agent_catalog
            .lock()
            .unwrap()
            .configure(
                None,
                "codex",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some("/usr/bin/true".into()),
                    show_in_quick_create: Some(true),
                },
            )
            .unwrap();
        let record: diri_proto::SessionRecord =
            serde_json::from_value(server.add_account_at("codex", &homes).unwrap()).unwrap();
        let _ = server.session_kill(Some(json!({"sessionID": record.id})));
        // The overview read the catalog, then the profile was removed while
        // it was still asking who the login belongs to.
        let mut stale = server.accounts.lock().unwrap().catalog().unwrap();
        let id = stale.profiles[0].id.clone();
        server
            .dispatch(Method::ACCOUNT_PROFILES_REMOVE, Some(json!({"id": id})))
            .unwrap();
        let known = Known {
            key: None,
            identity: AgentAccountIdentity {
                email: Some("side@corp.test".into()),
                ..Default::default()
            },
        };
        assert!(!server.name_pending_profiles(&mut stale, &[(id.clone(), known)]));
        let catalog = server.accounts.lock().unwrap().catalog().unwrap();
        assert!(catalog.profiles.iter().all(|p| p.id != id), "stays removed");
    }

    #[test]
    fn a_name_chosen_while_signing_in_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, homes) = sandbox(tmp.path());
        server
            .agent_catalog
            .lock()
            .unwrap()
            .configure(
                None,
                "codex",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some("/usr/bin/true".into()),
                    show_in_quick_create: Some(true),
                },
            )
            .unwrap();
        let record: diri_proto::SessionRecord =
            serde_json::from_value(server.add_account_at("codex", &homes).unwrap()).unwrap();
        let _ = server.session_kill(Some(json!({"sessionID": record.id})));
        let mut profile = server.overview_at(&homes).unwrap().catalog.profiles[0].clone();
        profile.label = "Side project".into();
        server
            .dispatch(
                Method::ACCOUNT_PROFILES_SAVE,
                Some(serde_json::to_value(&profile).unwrap()),
            )
            .unwrap();
        let slot = server.codex_slot(&profile.id).unwrap();
        codex::write(
            &slot.join("auth.json"),
            &codex_auth("side@corp.test", "u3", "ws3", "rt"),
        )
        .unwrap();
        let overview = server.overview_at(&homes).unwrap();
        assert_eq!(overview.catalog.profiles[0].label, "Side project");
        assert_eq!(
            overview
                .login(&profile.id)
                .unwrap()
                .identity
                .email
                .as_deref(),
            Some("side@corp.test")
        );
    }

    #[test]
    fn the_claude_login_in_use_is_matched_by_its_account_record() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, homes) = sandbox(tmp.path());
        claude::write_private(
            &homes.claude.parent().unwrap().join(".claude.json"),
            br#"{"oauthAccount":{"accountUuid":"a1","organizationUuid":"o1","emailAddress":"me@example.test","organizationName":"Mine"}}"#,
        )
        .unwrap();
        claude::write_private(
            &homes.claude.join(".credentials.json"),
            b"{\"claudeAiOauth\":{}}",
        )
        .unwrap();
        let overview = server.overview_at(&homes).unwrap();
        let live = overview.live(AgentKind::CLAUDE_CODE_ID).unwrap();
        assert_eq!(live.profile_id, None);
        assert_eq!(live.identity.organization.as_deref(), Some("Mine"));

        // A profile whose saved account record names the same account.
        let slot = tmp.path().join("claude-logins/mine");
        fs::create_dir_all(slot.parent().unwrap()).unwrap();
        claude::write_private(
            &claude::snapshot_path(&slot.to_string_lossy()),
            br#"{"accountUuid":"a1","organizationUuid":"o1","emailAddress":"me@example.test"}"#,
        )
        .unwrap();
        server
            .accounts
            .lock()
            .unwrap()
            .upsert(AgentAccountProfile {
                id: "mine".into(),
                label: "Mine".into(),
                agent: "claude-code".into(),
                host: None,
                config_home: homes.claude.to_string_lossy().into_owned(),
                is_default: false,
                login_store: Some(slot.to_string_lossy().into_owned()),
            })
            .unwrap();
        let overview = server.overview_at(&homes).unwrap();
        assert_eq!(
            overview
                .live(AgentKind::CLAUDE_CODE_ID)
                .unwrap()
                .profile_id
                .as_deref(),
            Some("mine")
        );
        assert!(overview.login("mine").unwrap().signed_in);
    }

    #[test]
    fn a_claude_sign_in_is_described_by_claude_once_and_remembered() {
        let tmp = tempfile::tempdir().unwrap();
        let (server, homes) = sandbox(tmp.path());
        let executable = tmp.path().join("claude");
        let calls = tmp.path().join("calls");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\necho \"$CLAUDE_SECURESTORAGE_CONFIG_DIR\" >> '{}'\nprintf '%s' '{{\"loggedIn\":true,\"email\":\"two@example.test\",\"orgName\":\"Two\",\"subscriptionType\":\"max\"}}'\n",
                calls.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        server
            .agent_catalog
            .lock()
            .unwrap()
            .configure(
                None,
                "claude-code",
                crate::agent_catalog::AgentPreference {
                    executable_path: Some(executable.to_string_lossy().into_owned()),
                    show_in_quick_create: Some(true),
                },
            )
            .unwrap();
        let slot = tmp.path().join("claude-logins/two");
        fs::create_dir_all(&slot).unwrap();
        fs::set_permissions(&slot, fs::Permissions::from_mode(0o700)).unwrap();
        claude::write_private(&slot.join(".credentials.json"), b"{\"claudeAiOauth\":{}}").unwrap();
        server
            .accounts
            .lock()
            .unwrap()
            .upsert(AgentAccountProfile {
                id: "two".into(),
                label: "Two".into(),
                agent: "claude-code".into(),
                host: None,
                config_home: homes.claude.to_string_lossy().into_owned(),
                is_default: false,
                login_store: Some(slot.to_string_lossy().into_owned()),
            })
            .unwrap();
        for _ in 0..2 {
            let overview = server.overview_at(&homes).unwrap();
            let identity = &overview.login("two").unwrap().identity;
            assert_eq!(identity.email.as_deref(), Some("two@example.test"));
            assert_eq!(identity.plan.as_deref(), Some("max"));
        }
        assert_eq!(
            fs::read_to_string(&calls).unwrap().lines().count(),
            1,
            "asked once, then read from beside the store"
        );
        assert_eq!(
            fs::metadata(slot.join(REPORTED_IDENTITY))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
