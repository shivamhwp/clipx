//! Accounts and client API keys, persisted as small JSON files under the clipx home.

use crate::util::{now, random_hex, sha256_hex, write_private};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
    Gemini,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Gemini => "gemini",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "anthropic" => Some(Provider::Claude),
            "codex" | "openai" | "chatgpt" => Some(Provider::Codex),
            "gemini" | "google" | "gemini-cli" => Some(Provider::Gemini),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AccountFile {
    pub id: String,
    pub provider: Provider,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// Unix seconds when the access token expires (0 = unknown).
    #[serde(default)]
    pub expires_at: u64,
    /// ChatGPT account id (Codex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Claude account uuid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Gemini Code Assist project id, assigned at login by loadCodeAssist/onboardUser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Gemini Code Assist tier (e.g. "free-tier", "standard-tier").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub priority: i32,
    /// Never refresh this token (useful when another tool owns the refresh token).
    #[serde(default)]
    pub no_refresh: bool,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub last_refresh: u64,
    #[serde(default = "device_id")]
    pub device_id: String,
}

fn device_id() -> String {
    random_hex(32)
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct QuotaWindow {
    pub name: String,
    /// 0..=100
    pub used_pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

#[derive(Default, Debug)]
pub struct AccountState {
    pub cooldown_until: u64,
    pub needs_login: bool,
    pub last_error: Option<String>,
    pub last_used: u64,
    pub requests: u64,
    pub failures: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub quota: Vec<QuotaWindow>,
    pub quota_at: u64,
}

pub struct Account {
    pub file: RwLock<AccountFile>,
    pub state: Mutex<AccountState>,
    pub refresh_lock: tokio::sync::Mutex<()>,
}

impl Account {
    pub fn new(file: AccountFile) -> Arc<Self> {
        Arc::new(Self { file: RwLock::new(file), state: Mutex::default(), refresh_lock: tokio::sync::Mutex::new(()) })
    }
    pub fn id(&self) -> String {
        self.file.read().unwrap().id.clone()
    }
    pub fn label(&self) -> String {
        self.file.read().unwrap().label.clone()
    }
    pub fn provider(&self) -> Provider {
        self.file.read().unwrap().provider
    }

    pub fn status(&self) -> &'static str {
        let f = self.file.read().unwrap();
        let s = self.state.lock().unwrap();
        if f.disabled {
            "disabled"
        } else if s.needs_login {
            "needs-login"
        } else if s.cooldown_until > now() {
            "cooling"
        } else {
            "ready"
        }
    }

    pub fn usable(&self, at: u64) -> bool {
        let f = self.file.read().unwrap();
        let s = self.state.lock().unwrap();
        !f.disabled && !s.needs_login && s.cooldown_until <= at
    }

    pub fn summary(&self) -> Value {
        let status = self.status();
        let f = self.file.read().unwrap();
        let s = self.state.lock().unwrap();
        json!({
            "id": f.id,
            "provider": f.provider,
            "label": f.label,
            "email": f.email,
            "plan": f.plan,
            "org": f.org,
            "status": status,
            "disabled": f.disabled,
            "priority": f.priority,
            "no_refresh": f.no_refresh,
            "expires_at": f.expires_at,
            "last_refresh": f.last_refresh,
            "created_at": f.created_at,
            "cooldown_until": s.cooldown_until,
            "last_error": s.last_error,
            "last_used": s.last_used,
            "requests": s.requests,
            "failures": s.failures,
            "input_tokens": s.input_tokens,
            "output_tokens": s.output_tokens,
            "quota": s.quota,
            "quota_at": s.quota_at,
        })
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ApiKey {
    pub id: String,
    pub name: String,
    pub hash: String,
    /// First characters of the key so users can tell keys apart.
    pub prefix: String,
    pub created_at: u64,
    #[serde(default)]
    pub last_used: u64,
}

pub struct Store {
    dir: PathBuf,
    pub accounts: RwLock<Vec<Arc<Account>>>,
    pub keys: RwLock<Vec<ApiKey>>,
}

impl Store {
    pub fn open(home: &Path) -> std::io::Result<Self> {
        let dir = home.to_path_buf();
        std::fs::create_dir_all(dir.join("accounts"))?;
        let mut accounts = Vec::new();
        for entry in std::fs::read_dir(dir.join("accounts"))? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<AccountFile>(&b).map_err(|e| e.to_string())) {
                Ok(f) => accounts.push(Account::new(f)),
                Err(e) => tracing::warn!("skipping {}: {e}", path.display()),
            }
        }
        accounts.sort_by_key(|a| a.file.read().unwrap().created_at);
        let keys = std::fs::read(dir.join("keys.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Ok(Self { dir, accounts: RwLock::new(accounts), keys: RwLock::new(keys) })
    }

    pub fn account_path(&self, id: &str) -> PathBuf {
        self.dir.join("accounts").join(format!("{id}.json"))
    }

    pub fn save_account(&self, account: &Account) -> std::io::Result<()> {
        let f = account.file.read().unwrap().clone();
        let data = serde_json::to_vec_pretty(&f).map_err(std::io::Error::other)?;
        write_private(&self.account_path(&f.id), &data)
    }

    pub fn list(&self) -> Vec<Arc<Account>> {
        self.accounts.read().unwrap().clone()
    }

    pub fn get(&self, id_or_label: &str) -> Option<Arc<Account>> {
        let list = self.accounts.read().unwrap();
        list.iter()
            .find(|a| a.file.read().unwrap().id == id_or_label)
            .or_else(|| list.iter().find(|a| a.file.read().unwrap().label.eq_ignore_ascii_case(id_or_label)))
            .cloned()
    }

    /// Insert or replace an account. An existing account with the same provider and
    /// identity (email / account id) is updated in place so re-logins keep their label.
    pub fn upsert(&self, mut file: AccountFile) -> std::io::Result<Arc<Account>> {
        // Imported files are untrusted: the id becomes a file name and the label shows in the dashboard.
        if !valid_id(&file.id) {
            file.id = random_hex(6);
        }
        file.label = clean_label(&file.label);
        let existing = self.list().into_iter().find(|a| {
            let f = a.file.read().unwrap();
            f.provider == file.provider
                && ((f.email.is_some() && f.email == file.email && f.account_id == file.account_id) || f.id == file.id)
        });
        if let Some(acc) = existing {
            {
                let mut f = acc.file.write().unwrap();
                file.id = f.id.clone();
                file.label = f.label.clone();
                file.priority = f.priority;
                file.disabled = f.disabled;
                file.created_at = f.created_at;
                file.device_id = f.device_id.clone();
                *f = file;
            }
            {
                let mut s = acc.state.lock().unwrap();
                s.needs_login = false;
                s.last_error = None;
                s.cooldown_until = 0;
            }
            self.save_account(&acc)?;
            return Ok(acc);
        }
        file.label = self.unique_label(&file.label);
        let acc = Account::new(file);
        self.save_account(&acc)?;
        self.accounts.write().unwrap().push(acc.clone());
        Ok(acc)
    }

    fn unique_label(&self, base: &str) -> String {
        let base = if base.trim().is_empty() { "account" } else { base.trim() };
        let taken: Vec<String> = self.list().iter().map(|a| a.label().to_ascii_lowercase()).collect();
        if !taken.contains(&base.to_ascii_lowercase()) {
            return base.to_string();
        }
        (2..).map(|n| format!("{base}-{n}")).find(|l| !taken.contains(&l.to_ascii_lowercase())).unwrap()
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut list = self.accounts.write().unwrap();
        let before = list.len();
        list.retain(|a| a.file.read().unwrap().id != id);
        let removed = list.len() != before;
        if removed {
            let _ = std::fs::remove_file(self.account_path(id));
        }
        removed
    }

    pub fn rename(&self, id: &str, label: &str) -> Result<(), String> {
        let label = label.trim();
        if label.is_empty() || label.len() > 40 || clean_label(label) != label {
            return Err("label must be 1-40 characters of letters, digits, '.', '_' or '-'".into());
        }
        if self.list().iter().any(|a| a.id() != id && a.label().eq_ignore_ascii_case(label)) {
            return Err(format!("label {label} is taken"));
        }
        let acc = self.get(id).ok_or("no such account")?;
        acc.file.write().unwrap().label = label.to_string();
        self.save_account(&acc).map_err(|e| e.to_string())
    }

    // ---- API keys ----

    pub fn save_keys(&self) -> std::io::Result<()> {
        let keys = self.keys.read().unwrap().clone();
        write_private(&self.dir.join("keys.json"), &serde_json::to_vec_pretty(&keys).map_err(std::io::Error::other)?)
    }

    /// Create a key and return (metadata, plaintext). The plaintext is never stored.
    pub fn create_key(&self, name: &str) -> std::io::Result<(ApiKey, String)> {
        let plain = crate::util::random_token("sk-clipx-");
        let key = ApiKey {
            id: random_hex(6),
            name: if name.trim().is_empty() { "default".into() } else { name.trim().into() },
            hash: sha256_hex(plain.as_bytes()),
            prefix: plain[..14].to_string(),
            created_at: now(),
            last_used: 0,
        };
        self.keys.write().unwrap().push(key.clone());
        self.save_keys()?;
        Ok((key, plain))
    }

    pub fn revoke_key(&self, id: &str) -> bool {
        let removed = {
            let mut keys = self.keys.write().unwrap();
            let before = keys.len();
            keys.retain(|k| k.id != id);
            keys.len() != before
        };
        if removed {
            let _ = self.save_keys();
        }
        removed
    }

    /// Returns the key name when `presented` matches a stored key.
    pub fn check_key(&self, presented: &str) -> Option<String> {
        let hash = sha256_hex(presented.as_bytes());
        let mut keys = self.keys.write().unwrap();
        let k = keys.iter_mut().find(|k| crate::util::ct_eq(k.hash.as_bytes(), hash.as_bytes()))?;
        k.last_used = now();
        Some(k.name.clone())
    }
}

/// Convert a CLIProxyAPI auth file (claude-*.json / codex-*.json / gemini-*.json) into an account.
pub fn from_cliproxy(v: &Value) -> Option<AccountFile> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    let provider = Provider::parse(&s("type")?)?;
    if provider == Provider::Gemini {
        return from_cliproxy_gemini(v);
    }
    let access_token = s("access_token")?;
    let email = s("email");
    let expires_at = s("expired").and_then(|e| crate::util::parse_rfc3339(&e)).unwrap_or(0);
    let id_token = s("id_token");
    let mut plan = s("plan_type");
    let mut account_id = s("account_id");
    if provider == Provider::Codex
        && let Some(claims) = id_token.as_deref().and_then(crate::util::jwt_claims) {
            let auth = &claims["https://api.openai.com/auth"];
            account_id = account_id.or_else(|| auth["chatgpt_account_id"].as_str().map(String::from));
            plan = plan.or_else(|| auth["chatgpt_plan_type"].as_str().map(String::from));
        }
    let label = default_label(provider, email.as_deref());
    Some(AccountFile {
        id: random_hex(6),
        provider,
        label,
        email,
        access_token,
        refresh_token: s("refresh_token"),
        id_token,
        expires_at,
        account_id,
        account_uuid: s("account_uuid"),
        org: s("organization_name"),
        plan,
        project_id: None,
        tier: None,
        disabled: v.get("disabled").and_then(Value::as_bool).unwrap_or(false),
        priority: v.get("priority").and_then(Value::as_i64).unwrap_or(0) as i32,
        no_refresh: false,
        created_at: now(),
        last_refresh: s("last_refresh").and_then(|e| crate::util::parse_rfc3339(&e)).unwrap_or(0),
        device_id: device_id(),
    })
}

/// Convert a CLIProxyAPI gemini-cli auth file into an account. Its shape differs from the
/// claude/codex files: the OAuth token fields sit nested under "token" (it is written straight
/// from a Go `oauth2.Token`), alongside a top-level "project_id" and "email".
fn from_cliproxy_gemini(v: &Value) -> Option<AccountFile> {
    let token = v.get("token")?;
    let t = |k: &str| token.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    let access_token = t("access_token")?;
    let email = v.get("email").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    let project_id = v.get("project_id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
    Some(AccountFile {
        id: random_hex(6),
        provider: Provider::Gemini,
        label: default_label(Provider::Gemini, email.as_deref()),
        email,
        access_token,
        refresh_token: t("refresh_token"),
        id_token: None,
        expires_at: t("expiry").and_then(|e| crate::util::parse_rfc3339(&e)).unwrap_or(0),
        account_id: None,
        account_uuid: None,
        org: None,
        plan: None,
        project_id,
        tier: None,
        disabled: v.get("disabled").and_then(Value::as_bool).unwrap_or(false),
        priority: v.get("priority").and_then(Value::as_i64).unwrap_or(0) as i32,
        no_refresh: false,
        created_at: now(),
        last_refresh: 0,
        device_id: device_id(),
    })
}

fn valid_id(id: &str) -> bool {
    (1..=32).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Labels appear in URLs (`/a/<label>`) and the dashboard: keep them to a safe charset.
pub fn clean_label(label: &str) -> String {
    let l: String = label.trim().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')).take(40).collect();
    if l.is_empty() { "account".into() } else { l }
}

pub fn default_label(provider: Provider, email: Option<&str>) -> String {
    let user = email.and_then(|e| e.split('@').next()).unwrap_or("account");
    let user: String = user.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.').collect();
    format!("{}-{}", provider.as_str(), if user.is_empty() { "account" } else { &user })
}

pub fn new_account(provider: Provider, access_token: String) -> AccountFile {
    AccountFile {
        id: random_hex(6),
        provider,
        label: default_label(provider, None),
        email: None,
        access_token,
        refresh_token: None,
        id_token: None,
        expires_at: 0,
        account_id: None,
        account_uuid: None,
        org: None,
        plan: None,
        project_id: None,
        tier: None,
        disabled: false,
        priority: 0,
        no_refresh: false,
        created_at: now(),
        last_refresh: now(),
        device_id: device_id(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untrusted_import_is_cleaned() {
        let dir = std::env::temp_dir().join(format!("clipx-store-{}", random_hex(4)));
        let store = Store::open(&dir).unwrap();
        let mut f = new_account(Provider::Claude, "t".into());
        f.id = "../../evil".into();
        f.label = "x');alert(1);//".into();
        let acc = store.upsert(f).unwrap();
        let file = acc.file.read().unwrap().clone();
        assert!(valid_id(&file.id));
        assert_eq!(file.label, "xalert1");
        assert!(store.account_path(&file.id).starts_with(dir.join("accounts")));
        assert!(store.rename(&file.id, "a'b").is_err());
        assert!(store.rename(&file.id, "work-1").is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
