//! Shared server state and account selection.

use crate::config::{CLAUDE_CODE_VERSION, Config, Strategy};
use crate::oauth::PendingFlow;
use crate::store::{Account, Provider, QuotaWindow, Store};
use crate::usage::Stats;
use crate::util::now;
use reqwest::header::HeaderMap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

#[derive(Default, Clone, serde::Serialize)]
pub struct TunnelStatus {
    pub mode: String,
    pub connected: bool,
    pub public_url: Option<String>,
    pub error: Option<String>,
    pub since: u64,
}

pub struct App {
    pub home: PathBuf,
    pub cfg: RwLock<Config>,
    pub store: Store,
    pub stats: Stats,
    pub http: reqwest::Client,
    pub flows: Mutex<HashMap<String, PendingFlow>>,
    pub tunnel: Mutex<TunnelStatus>,
    pub tunnel_ctl: tokio::sync::watch::Sender<u64>,
    pub models_cache: Mutex<HashMap<Provider, (u64, Vec<String>)>>,
    pub started: u64,
    pub shutdown: tokio::sync::Notify,
    claude_version: RwLock<String>,
    rr: [AtomicUsize; 3],
}

impl App {
    pub fn new(home: PathBuf, cfg: Config) -> std::io::Result<Arc<Self>> {
        let store = Store::open(&home)?;
        let stats = Stats::open(&home);
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(600))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .pool_max_idle_per_host(16)
            .tcp_nodelay(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(std::io::Error::other)?;
        let version = cfg.claude_code_version.clone().unwrap_or_else(|| CLAUDE_CODE_VERSION.to_string());
        Ok(Arc::new(Self {
            home,
            cfg: RwLock::new(cfg),
            store,
            stats,
            http,
            flows: Mutex::default(),
            tunnel: Mutex::default(),
            tunnel_ctl: tokio::sync::watch::channel(0).0,
            models_cache: Mutex::default(),
            started: now(),
            shutdown: tokio::sync::Notify::new(),
            claude_version: RwLock::new(version),
            rr: [AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)],
        }))
    }

    pub fn claude_version(&self) -> String {
        self.claude_version.read().unwrap().clone()
    }

    /// Native Claude Code clients passing through tell us the current version; use the
    /// newest one seen for clients that need us to present as Claude Code.
    pub fn observe_claude_version(&self, user_agent: &str) {
        let Some(rest) = user_agent.strip_prefix("claude-cli/") else { return };
        let seen = rest.split([' ', '(']).next().unwrap_or("");
        if seen.is_empty() || !seen.chars().all(|c| c.is_ascii_digit() || c == '.') {
            return;
        }
        if version_gt(seen, &self.claude_version.read().unwrap()) {
            *self.claude_version.write().unwrap() = seen.to_string();
        }
    }

    pub fn save_config(&self) -> std::io::Result<()> {
        self.cfg.read().unwrap().save(&self.home)
    }

    /// Pick an account for `provider`. `pinned` restricts to one account by id or label.
    pub fn pick(&self, provider: Provider, pinned: Option<&str>, tried: &[String]) -> Result<Arc<Account>, PickError> {
        let t = now();
        if let Some(name) = pinned {
            let acc = self.store.get(name).ok_or_else(|| PickError::NoSuchAccount(name.to_string()))?;
            if acc.provider() != provider {
                return Err(PickError::WrongProvider(name.to_string(), acc.provider()));
            }
            if tried.contains(&acc.id()) {
                return Err(PickError::Exhausted(provider));
            }
            if !acc.usable(t) {
                return Err(PickError::Unavailable(acc.label(), acc.status()));
            }
            return Ok(acc);
        }
        let mut candidates: Vec<Arc<Account>> = self
            .store
            .list()
            .into_iter()
            .filter(|a| a.provider() == provider && a.usable(t) && !tried.contains(&a.id()))
            .collect();
        if candidates.is_empty() {
            let any = self.store.list().into_iter().any(|a| a.provider() == provider);
            return Err(if any { PickError::Exhausted(provider) } else { PickError::NoAccounts(provider) });
        }
        let strategy = self.cfg.read().unwrap().strategy;
        match strategy {
            Strategy::FillFirst => {
                candidates.sort_by_key(|a| std::cmp::Reverse(a.file.read().unwrap().priority));
                Ok(candidates.swap_remove(0))
            }
            Strategy::RoundRobin => {
                // Highest priority tier only, rotated.
                let top = candidates.iter().map(|a| a.file.read().unwrap().priority).max().unwrap_or(0);
                candidates.retain(|a| a.file.read().unwrap().priority == top);
                let i = self.rr[provider as usize].fetch_add(1, Ordering::Relaxed) % candidates.len();
                Ok(candidates.swap_remove(i))
            }
        }
    }

    /// Earliest time any account of this provider comes back from cooldown.
    pub fn next_available(&self, provider: Provider) -> Option<u64> {
        self.store
            .list()
            .iter()
            .filter(|a| a.provider() == provider && !a.file.read().unwrap().disabled)
            .map(|a| a.state.lock().unwrap().cooldown_until)
            .filter(|&c| c > now())
            .min()
    }
}

#[derive(Debug)]
pub enum PickError {
    NoAccounts(Provider),
    Exhausted(Provider),
    NoSuchAccount(String),
    WrongProvider(String, Provider),
    Unavailable(String, &'static str),
}

impl std::fmt::Display for PickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PickError::NoAccounts(p) => write!(f, "no {} accounts. add one in the clipx dashboard or with `clipx login {}`", p.as_str(), p.as_str()),
            PickError::Exhausted(p) => write!(f, "every {} account is cooling down, disabled or needs login", p.as_str()),
            PickError::NoSuchAccount(n) => write!(f, "no account named {n}"),
            PickError::WrongProvider(n, p) => write!(f, "account {n} is a {} account", p.as_str()),
            PickError::Unavailable(n, s) => match *s {
                "needs-login" => write!(f, "account {n} is logged out. sign in again from the clipx dashboard"),
                "cooling" => write!(f, "account {n} hit its usage limit and is resting"),
                s => write!(f, "account {n} is {s}"),
            },
        }
    }
}

pub fn version_gt(a: &str, b: &str) -> bool {
    let parse = |s: &str| s.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    parse(a) > parse(b)
}

fn header_f64(h: &HeaderMap, name: &str) -> Option<f64> {
    h.get(name)?.to_str().ok()?.trim().parse().ok()
}

/// Read subscription usage windows from upstream response headers.
pub fn quota_from_headers(provider: Provider, h: &HeaderMap) -> Vec<QuotaWindow> {
    let mut out = Vec::new();
    match provider {
        Provider::Claude => {
            // anthropic-ratelimit-unified-<window>-utilization (0..1) and -<window>-reset (unix)
            const P: &str = "anthropic-ratelimit-unified-";
            for (name, _) in h.iter() {
                let n = name.as_str();
                let Some(window) = n.strip_prefix(P).and_then(|r| r.strip_suffix("-utilization")) else { continue };
                let Some(u) = header_f64(h, n) else { continue };
                let reset = header_f64(h, &format!("{P}{window}-reset")).map(|r| r as u64);
                out.push(QuotaWindow { name: window.to_string(), used_pct: (u * 100.0).clamp(0.0, 100.0), resets_at: reset });
            }
        }
        Provider::Codex => {
            for window in ["primary", "secondary"] {
                let Some(pct) = header_f64(h, &format!("x-codex-{window}-used-percent")) else { continue };
                let minutes = header_f64(h, &format!("x-codex-{window}-window-minutes"));
                if minutes == Some(0.0) {
                    continue; // unused window
                }
                let reset = header_f64(h, &format!("x-codex-{window}-reset-at"))
                    .map(|r| r as u64)
                    .or_else(|| header_f64(h, &format!("x-codex-{window}-reset-after-seconds")).map(|s| now() + s as u64));
                let name = match minutes {
                    Some(m) if m >= 1440.0 => format!("{}d", (m / 1440.0).round()),
                    Some(m) if m >= 60.0 => format!("{}h", (m / 60.0).round()),
                    Some(m) => format!("{m}m"),
                    None => window.to_string(),
                };
                out.push(QuotaWindow { name, used_pct: pct.clamp(0.0, 100.0), resets_at: reset });
            }
        }
        // Code Assist does not send per-account usage-window headers.
        Provider::Gemini => {}
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// How long to rest an account after a 429.
pub fn cooldown_secs(provider: Provider, h: &HeaderMap, body: &[u8]) -> u64 {
    let t = now();
    if let Some(secs) = h.get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok())
        && secs > 0 {
            return secs.min(7 * 86400);
        }
    match provider {
        Provider::Claude => {
            if let Some(reset) = header_f64(h, "anthropic-ratelimit-unified-reset") {
                let reset = reset as u64;
                if reset > t {
                    return (reset - t).min(7 * 86400);
                }
            }
        }
        Provider::Codex => {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
                let e = &v["error"];
                if let Some(s) = e["resets_in_seconds"].as_u64() {
                    return s.clamp(1, 7 * 86400);
                }
                if let Some(at) = e["resets_at"].as_u64().filter(|&at| at > t) {
                    return (at - t).min(7 * 86400);
                }
            }
            for w in ["primary", "secondary"] {
                if header_f64(h, &format!("x-codex-{w}-used-percent")).unwrap_or(0.0) >= 100.0
                    && let Some(s) = header_f64(h, &format!("x-codex-{w}-reset-after-seconds")) {
                        return (s as u64).clamp(1, 7 * 86400);
                    }
            }
        }
        Provider::Gemini => {
            // 429 RESOURCE_EXHAUSTED carries a google.rpc.RetryInfo detail, e.g. "retryDelay": "37s".
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
                for d in v["error"]["details"].as_array().into_iter().flatten() {
                    if d["@type"] == "type.googleapis.com/google.rpc.RetryInfo"
                        && let Some(secs) = d["retryDelay"].as_str().and_then(|s| s.trim_end_matches('s').parse::<f64>().ok())
                            && secs > 0.0 {
                                return (secs.ceil() as u64).clamp(1, 7 * 86400);
                            }
                }
            }
        }
    }
    60
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn claude_quota() {
        let h = headers(&[
            ("anthropic-ratelimit-unified-5h-utilization", "0.25"),
            ("anthropic-ratelimit-unified-5h-reset", "1900000000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.5"),
        ]);
        let q = quota_from_headers(Provider::Claude, &h);
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].name, "5h");
        assert_eq!(q[0].used_pct, 25.0);
        assert_eq!(q[0].resets_at, Some(1900000000));
    }

    #[test]
    fn codex_quota_and_cooldown() {
        let h = headers(&[
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-after-seconds", "1200"),
        ]);
        let q = quota_from_headers(Provider::Codex, &h);
        assert_eq!(q[0].name, "5h");
        assert_eq!(cooldown_secs(Provider::Codex, &h, b"{}"), 1200);
        assert_eq!(cooldown_secs(Provider::Codex, &HeaderMap::new(), br#"{"error":{"resets_in_seconds":77}}"#), 77);
    }

    #[test]
    fn versions() {
        assert!(version_gt("2.1.300", "2.1.291"));
        assert!(!version_gt("2.1.29", "2.1.291"));
    }
}
