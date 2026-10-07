use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 8318;
pub const CLAUDE_CODE_VERSION: &str = "2.1.291";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    #[default]
    RoundRobin,
    FillFirst,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TunnelMode {
    #[default]
    Off,
    Relay,
    Cloudflare,
    Tailscale,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ConnectConfig {
    #[serde(default)]
    pub mode: TunnelMode,
    /// Relay base URL, e.g. https://relay.example.com
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
    /// Name to claim on the relay (becomes <name>.<domain> or /t/<name>/)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Secret proving ownership of `name` on the relay. Generated locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_key: Option<String>,
    /// Shared secret the relay may require before it accepts new names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_secret: Option<String>,
    /// Tailscale: HTTPS port to serve on. Funnel allows 443, 8443 and 10000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_port: Option<u16>,
    /// Tailscale: serve inside the tailnet only, instead of on the internet with Funnel.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tailnet_only: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Upstream {
    #[serde(default = "d_claude_api")]
    pub claude_api: String,
    #[serde(default = "d_claude_token")]
    pub claude_token_url: String,
    #[serde(default = "d_codex_api")]
    pub codex_api: String,
    #[serde(default = "d_codex_token")]
    pub codex_token_url: String,
}

fn d_claude_api() -> String {
    "https://api.anthropic.com".into()
}
fn d_claude_token() -> String {
    "https://platform.claude.com/v1/oauth/token".into()
}
fn d_codex_api() -> String {
    "https://chatgpt.com/backend-api/codex".into()
}
fn d_codex_token() -> String {
    "https://auth.openai.com/oauth/token".into()
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            claude_api: d_claude_api(),
            claude_token_url: d_claude_token(),
            codex_api: d_codex_api(),
            codex_token_url: d_codex_token(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(default = "d_listen")]
    pub listen: String,
    pub admin_token: String,
    #[serde(default)]
    pub strategy: Strategy,
    /// How many accounts to try before giving up on a request.
    #[serde(default = "d_retries")]
    pub retries: u32,
    /// Claude Code version to present for non-Claude-Code clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers: Option<usize>,
    #[serde(default)]
    pub connect: ConnectConfig,
    #[serde(default, skip_serializing_if = "is_default_upstream")]
    pub upstream: Upstream,
}

fn is_default_upstream(u: &Upstream) -> bool {
    let d = Upstream::default();
    u.claude_api == d.claude_api
        && u.claude_token_url == d.claude_token_url
        && u.codex_api == d.codex_api
        && u.codex_token_url == d.codex_token_url
}

fn d_listen() -> String {
    format!("127.0.0.1:{DEFAULT_PORT}")
}
fn d_retries() -> u32 {
    3
}

impl Config {
    pub fn new() -> Self {
        Self {
            listen: d_listen(),
            admin_token: crate::util::random_token("adm-"),
            strategy: Strategy::default(),
            retries: d_retries(),
            claude_code_version: None,
            workers: None,
            connect: ConnectConfig::default(),
            upstream: Upstream::default(),
        }
    }

    pub fn load(home: &Path) -> anyhow_lite::Result<Self> {
        let path = config_path(home);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow_lite::err(format!("read {}: {e}. run `clipx setup` first", path.display())))?;
        let mut cfg: Config = toml::from_str(&text).map_err(|e| anyhow_lite::err(format!("parse {}: {e}", path.display())))?;
        cfg.apply_env();
        Ok(cfg)
    }

    pub fn save(&self, home: &Path) -> std::io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::util::write_private(&config_path(home), text.as_bytes())
    }

    fn apply_env(&mut self) {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        if let Some(v) = env("CLIPX_LISTEN") {
            self.listen = v;
        }
        if let Some(v) = env("CLIPX_CLAUDE_API") {
            self.upstream.claude_api = v;
        }
        if let Some(v) = env("CLIPX_CLAUDE_TOKEN_URL") {
            self.upstream.claude_token_url = v;
        }
        if let Some(v) = env("CLIPX_CODEX_API") {
            self.upstream.codex_api = v;
        }
        if let Some(v) = env("CLIPX_CODEX_TOKEN_URL") {
            self.upstream.codex_token_url = v;
        }
    }

    pub fn port(&self) -> u16 {
        self.listen.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
    }

    /// Address a local client can use to reach this server.
    pub fn local_url(&self) -> String {
        let host = self.listen.rsplit_once(':').map(|(h, _)| h).unwrap_or("127.0.0.1");
        let host = match host {
            "0.0.0.0" | "" | "[::]" | "::" => "127.0.0.1",
            h => h,
        };
        format!("http://{host}:{}", self.port())
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("CLIPX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::util::home_dir().join(".clipx"))
}

pub fn config_path(home: &Path) -> PathBuf {
    home.join("config.toml")
}

/// Tiny error type so the binary does not need anyhow.
pub mod anyhow_lite {
    #[derive(Debug)]
    pub struct Error(pub String);
    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for Error {}
    pub type Result<T> = std::result::Result<T, Error>;
    pub fn err(msg: impl Into<String>) -> Error {
        Error(msg.into())
    }
    impl From<std::io::Error> for Error {
        fn from(e: std::io::Error) -> Self {
            Error(e.to_string())
        }
    }
    impl From<reqwest::Error> for Error {
        fn from(e: reqwest::Error) -> Self {
            Error(e.to_string())
        }
    }
    impl From<serde_json::Error> for Error {
        fn from(e: serde_json::Error) -> Self {
            Error(e.to_string())
        }
    }
}
