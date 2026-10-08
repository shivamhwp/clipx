mod admin;
mod app;
mod cloak;
mod config;
mod gemini;
mod oauth;
mod proxy;
mod service;
mod sse;
mod store;
mod t3;
mod translate;
mod tunnel;
mod update;
mod usage;
mod util;

use axum::Router;
use axum::routing::any;
use clap::{Parser, Subcommand};
use config::{Config, Strategy, TunnelMode};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "clipx", version, about = "One proxy for your Claude and ChatGPT subscriptions, with remote access and a dashboard.")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Set everything up: config, first API key, background service, remote access.
    Setup(SetupArgs),
    /// Run the server in the foreground.
    Serve,
    /// Show status, accounts and the URLs to use.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Add an account by logging in (claude, codex, or gemini).
    Login { provider: String },
    /// List accounts.
    Accounts,
    /// Import accounts from a CLIProxyAPI auth file or directory, or a clipx account file.
    Import {
        path: PathBuf,
        /// Never refresh imported tokens (use when another tool still refreshes them).
        #[arg(long)]
        no_refresh: bool,
        /// Keep following the source file: clipx re-reads it for new tokens and never
        /// refreshes them itself. For logins CLIProxyAPI keeps refreshing.
        #[arg(long, conflicts_with = "no_refresh")]
        link: bool,
    },
    /// Manage client API keys.
    Keys {
        #[command(subcommand)]
        action: Option<KeysCmd>,
    },
    /// Configure remote access: off, tailscale, cloudflare, or relay <url>.
    Connect {
        mode: String,
        relay: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        secret: Option<String>,
        /// Tailscale: HTTPS port (Funnel allows 443, 8443 and 10000).
        #[arg(long)]
        port: Option<u16>,
        /// Tailscale: keep it inside your tailnet instead of on the internet.
        #[arg(long)]
        tailnet_only: bool,
    },
    /// Print environment snippets for Claude Code, Codex and OpenAI SDKs.
    Env {
        /// Use the local address even when a tunnel is up.
        #[arg(long)]
        local: bool,
    },
    /// Run a relay that boxes connect to (put it behind TLS, e.g. Caddy).
    Relay {
        #[arg(long, default_value = "0.0.0.0:8080")]
        listen: String,
        /// Serve boxes at <name>.<domain> (needs wildcard DNS). Without it, boxes live at /t/<name>/.
        #[arg(long)]
        domain: Option<String>,
        /// Public base URL of this relay, e.g. https://relay.example.com
        #[arg(long)]
        public_url: Option<String>,
        /// Require this secret before a new box can claim a name.
        #[arg(long, env = "CLIPX_RELAY_SECRET")]
        secret: Option<String>,
        #[arg(long)]
        data: Option<PathBuf>,
    },
    Start,
    Stop,
    Restart,
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
    /// Add clipx to T3 Code as "Claude (clipx)" and "ChatGPT (clipx)": on (default), off,
    /// or show (the values to enter in T3 yourself).
    T3 {
        action: Option<String>,
        /// T3 runs on this machine but clipx runs elsewhere: the clipx address T3 should use,
        /// like http://127.0.0.1:8318. Asks for clipx's admin token (or reads CLIPX_ADMIN_TOKEN).
        #[arg(long)]
        server: Option<String>,
    },
    /// Update clipx to the latest release and restart it.
    Update {
        /// A release tag such as v0.2.0, instead of the latest.
        #[arg(long)]
        version: Option<String>,
    },
    /// Remove the background service (keeps ~/.clipx).
    Uninstall,
}

#[derive(Subcommand)]
enum KeysCmd {
    List,
    Create { name: Option<String> },
    Revoke { id: String },
}

#[derive(clap::Args)]
struct SetupArgs {
    #[arg(long)]
    port: Option<u16>,
    /// Listen on all interfaces instead of 127.0.0.1.
    #[arg(long)]
    public: bool,
    /// Remote access: tailscale, cloudflare, relay, or off. Default: tailscale when it is
    /// running on this machine, otherwise cloudflare.
    #[arg(long)]
    tunnel: Option<String>,
    /// Tailscale: HTTPS port (Funnel allows 443, 8443 and 10000).
    #[arg(long)]
    ts_port: Option<u16>,
    /// Relay URL (implies --tunnel relay).
    #[arg(long)]
    relay: Option<String>,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    relay_secret: Option<String>,
    /// Import CLIProxyAPI auth files from this directory.
    #[arg(long)]
    import: Option<PathBuf>,
    /// Write config only; do not install or start a service.
    #[arg(long)]
    no_service: bool,
    /// Do not add clipx to T3 Code.
    #[arg(long)]
    no_t3: bool,
}

fn main() {
    let cli = Cli::parse();
    let home = config::home();
    let cmd = cli.cmd.unwrap_or(Cmd::Status { json: false });
    let result = match cmd {
        Cmd::Serve => serve(&home),
        Cmd::Relay { listen, domain, public_url, secret, data } => {
            let data = data.unwrap_or_else(|| util::home_dir().join(".clipx-relay"));
            relay(tunnel::RelayOpts { listen, domain, public_url, secret, data })
        }
        other => client_runtime().block_on(run_client(&home, other)),
    };
    if let Err(e) = result {
        eprintln!("clipx: {e}");
        std::process::exit(1);
    }
}

type Res = Result<(), String>;

fn init_logs() {
    let filter = std::env::var("CLIPX_LOG").unwrap_or_else(|_| "info".into());
    let level = filter.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt().with_max_level(level).with_target(false).init();
}

fn server_runtime(workers: Option<usize>) -> tokio::runtime::Runtime {
    let n = workers.unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).min(4));
    tokio::runtime::Builder::new_multi_thread().worker_threads(n.max(1)).max_blocking_threads(4).enable_all().build().expect("tokio runtime")
}

fn client_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime")
}

pub fn build_router(app: Arc<app::App>) -> Router {
    Router::new()
        .route("/v1/{*rest}", any(proxy::entry))
        .route("/v1beta/{*rest}", any(proxy::entry))
        .route("/a/{*rest}", any(proxy::entry))
        .route("/responses", any(proxy::entry))
        .route("/responses/{*rest}", any(proxy::entry))
        .route("/chat/completions", any(proxy::entry))
        .route("/models", any(proxy::entry))
        .merge(admin::router(app.clone()))
        .with_state(app)
}

fn serve(home: &Path) -> Res {
    init_logs();
    let cfg = Config::load(home).map_err(|e| e.to_string())?;
    let rt = server_runtime(cfg.workers);
    rt.block_on(async {
        let listen = cfg.listen.clone();
        let app = app::App::new(home.to_path_buf(), cfg).map_err(|e| e.to_string())?;
        let router = build_router(app.clone());
        tokio::spawn(oauth::refresh_loop(app.clone()));
        tokio::spawn(tunnel::supervise(app.clone(), router.clone()));
        tokio::spawn(t3::sync_loop(app.clone()));
        let flusher = app.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                t.tick().await;
                flusher.stats.flush();
            }
        });
        let listener = tokio::net::TcpListener::bind(&listen).await.map_err(|e| format!("listen on {listen}: {e}"))?;
        tracing::info!("clipx {} listening on {listen} ({} accounts)", env!("CARGO_PKG_VERSION"), app.store.list().len());
        let stopper = app.clone();
        let stop = async move {
            tokio::select! { _ = shutdown() => {}, _ = stopper.shutdown.notified() => {} }
        };
        axum::serve(listener, router).with_graceful_shutdown(stop).await.map_err(|e| e.to_string())?;
        app.stats.flush();
        let _ = app.store.save_keys();
        Ok(())
    })
}

fn relay(opts: tunnel::RelayOpts) -> Res {
    init_logs();
    server_runtime(None).block_on(async {
        let listen = opts.listen.clone();
        std::fs::create_dir_all(&opts.data).map_err(|e| e.to_string())?;
        let listener = tokio::net::TcpListener::bind(&listen).await.map_err(|e| format!("listen on {listen}: {e}"))?;
        tracing::info!("clipx relay listening on {listen}");
        axum::serve(listener, tunnel::relay_router(opts)).with_graceful_shutdown(shutdown()).await.map_err(|e| e.to_string())
    })
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

// ------------------------------------------------------------------ CLI client

struct Api {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Api {
    fn new(home: &Path) -> Result<Self, String> {
        let cfg = Config::load(home).map_err(|e| e.to_string())?;
        Ok(Self { base: cfg.local_url(), token: cfg.admin_token, http: reqwest::Client::new() })
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
        let mut req = self.http.request(method, format!("{}/api{path}", self.base)).bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_connect() { format!("clipx is not running at {}. start it with `clipx start`", self.base) } else { e.to_string() }
        })?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(v["error"].as_str().map(String::from).unwrap_or_else(|| format!("http {status}")));
        }
        Ok(v)
    }
    async fn get(&self, path: &str) -> Result<Value, String> {
        self.call(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
    async fn patch(&self, path: &str, body: Value) -> Result<Value, String> {
        self.call(reqwest::Method::PATCH, path, Some(body)).await
    }
    async fn delete(&self, path: &str) -> Result<Value, String> {
        self.call(reqwest::Method::DELETE, path, None).await
    }

    async fn wait_ready(&self, secs: u64) -> bool {
        for _ in 0..secs * 5 {
            if self.http.get(format!("{}/healthz", self.base)).send().await.is_ok_and(|r| r.status().is_success()) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        false
    }
}

fn ago(ts: u64) -> String {
    if ts == 0 {
        return "never".into();
    }
    let d = util::now().saturating_sub(ts);
    match d {
        0..60 => format!("{d}s ago"),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

fn until(ts: u64) -> String {
    let d = ts.saturating_sub(util::now());
    match d {
        0..60 => format!("{d}s"),
        60..3600 => format!("{}m", d / 60),
        3600..86400 => format!("{}h{}m", d / 3600, d % 3600 / 60),
        _ => format!("{}d{}h", d / 86400, d % 86400 / 3600),
    }
}

fn print_accounts(state: &Value) {
    let accounts = state["accounts"].as_array().cloned().unwrap_or_default();
    if accounts.is_empty() {
        println!("  no accounts yet. add one: clipx login claude   (or codex)");
        return;
    }
    for a in accounts {
        let quota: Vec<String> = a["quota"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|q| format!("{} {:.0}%", q["name"].as_str().unwrap_or(""), q["used_pct"].as_f64().unwrap_or(0.0)))
            .collect();
        let mut status = a["status"].as_str().unwrap_or("").to_string();
        if status == "cooling" {
            status = format!("cooling {}", until(a["cooldown_until"].as_u64().unwrap_or(0)));
        }
        println!(
            "  {:<24} {:<7} {:<16} {:>6} req  {}",
            a["label"].as_str().unwrap_or(""),
            a["provider"].as_str().unwrap_or(""),
            status,
            a["requests"],
            if quota.is_empty() { String::new() } else { format!("used {}", quota.join(", ")) }
        );
        if let Some(e) = a["last_error"].as_str() {
            println!("  {:<24} last error: {}", "", e.chars().take(120).collect::<String>());
        }
    }
}

/// A quick, synchronous check used to pick setup's default remote access.
fn tailscale_running() -> bool {
    std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .is_some_and(|v| v["BackendState"] == "Running")
}

fn base_url(state: &Value, local: bool) -> String {
    let public = state["tunnel"]["public_url"].as_str().filter(|_| state["tunnel"]["connected"] == true && !local);
    public.map(|u| u.trim_end_matches('/').to_string()).unwrap_or_else(|| state["local_url"].as_str().unwrap_or("").to_string())
}

fn print_env(base: &str, key: &str) {
    println!("claude code:");
    println!("  export ANTHROPIC_BASE_URL={base}");
    println!("  export ANTHROPIC_AUTH_TOKEN={key}");
    println!();
    println!("codex (~/.codex/config.toml):");
    println!("  model_provider = \"clipx\"");
    println!("  [model_providers.clipx]");
    println!("  name = \"clipx\"");
    println!("  base_url = \"{base}/v1\"");
    println!("  env_key = \"CLIPX_API_KEY\"");
    println!("  wire_api = \"responses\"");
    println!("  # then: export CLIPX_API_KEY={key}");
    println!();
    println!("openai-compatible apps:");
    println!("  base url  {base}/v1");
    println!("  api key   {key}");
    println!();
    println!("gemini-compatible apps:");
    println!("  base url  {base}/v1beta");
    println!("  api key   {key}   (as x-goog-api-key, or ?key=)");
}

async fn run_client(home: &Path, cmd: Cmd) -> Res {
    match cmd {
        Cmd::Setup(args) => setup(home, args).await,
        Cmd::Status { json } => {
            if !config::config_path(home).exists()
                && let Some(link) = T3Link::load(home)
            {
                println!("clipx does not run here. T3 Code here uses the clipx at {}; `clipx t3` refreshes it.", link.server);
                return Ok(());
            }
            let api = Api::new(home)?;
            let s = api.get("/state").await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s).unwrap());
                return Ok(());
            }
            let rss = s["rss"].as_u64().unwrap_or(0) as f64 / 1048576.0;
            println!("clipx {}  up {}  {:.1} MB  {} requests ({} failed)", s["version"].as_str().unwrap_or(""), until(util::now() + s["uptime"].as_u64().unwrap_or(0)), rss, s["total_requests"], s["total_failures"]);
            println!("local      {}", s["local_url"].as_str().unwrap_or(""));
            let t = &s["tunnel"];
            match (t["mode"].as_str().unwrap_or("off"), t["connected"] == true) {
                ("off", _) => println!("remote     off (turn on: clipx connect cloudflare)"),
                (_, true) => println!("remote     {}", t["public_url"].as_str().unwrap_or("")),
                (m, false) => println!("remote     {m}: connecting… {}", t["error"].as_str().unwrap_or("")),
            }
            println!("dashboard  {}/  (admin token in {})", base_url(&s, false), config::config_path(home).display());
            println!("routing    {}", s["strategy"].as_str().unwrap_or(""));
            if s["t3"]["enabled"] == true {
                let t = api.get("/t3").await?;
                let added: Vec<&str> = t["added"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                println!("T3 Code    {}", if added.is_empty() { "on; providers appear once you add an account".into() } else { added.join(", ") });
            }
            println!("accounts");
            print_accounts(&s);
            Ok(())
        }
        Cmd::Accounts => {
            let s = Api::new(home)?.get("/state").await?;
            print_accounts(&s);
            Ok(())
        }
        Cmd::Login { provider } => login(home, &provider).await,
        Cmd::Import { path, no_refresh, link } => {
            let api = Api::new(home)?;
            let files: Vec<PathBuf> = if path.is_dir() {
                json_files(&path)
            } else {
                vec![path]
            };
            let n = import_files(&api, files, no_refresh, link).await;
            println!("{n} account(s) imported");
            Ok(())
        }
        Cmd::Keys { action } => {
            let api = Api::new(home)?;
            match action.unwrap_or(KeysCmd::List) {
                KeysCmd::List => {
                    let s = api.get("/state").await?;
                    let keys = s["keys"].as_array().cloned().unwrap_or_default();
                    let w = keys.iter().map(|k| k["name"].as_str().unwrap_or("").chars().count()).max().unwrap_or(0).max(16);
                    for k in keys {
                        println!("{}  {:<w$} {}…  used {}", k["id"].as_str().unwrap_or(""), k["name"].as_str().unwrap_or(""), k["prefix"].as_str().unwrap_or(""), ago(k["last_used"].as_u64().unwrap_or(0)));
                    }
                }
                KeysCmd::Create { name } => {
                    let r = api.post("/keys", json!({"name": name.unwrap_or_default()})).await?;
                    println!("{}", r["key"].as_str().unwrap_or(""));
                    eprintln!("save it now; clipx stores only a hash.");
                }
                KeysCmd::Revoke { id } => {
                    api.delete(&format!("/keys/{id}")).await?;
                    println!("revoked {id}");
                }
            }
            Ok(())
        }
        Cmd::Connect { mode, relay, name, secret, port, tailnet_only } => {
            let api = Api::new(home)?;
            let mut body = json!({"relay": relay, "name": name, "relay_secret": secret});
            body["mode"] = json!(match mode.as_str() {
                "off" | "none" => "off",
                "cloudflare" | "cf" => "cloudflare",
                "tailscale" | "ts" => {
                    body["ts_port"] = json!(port);
                    body["tailnet_only"] = json!(tailnet_only);
                    "tailscale"
                }
                "relay" => "relay",
                url if url.starts_with("http") => {
                    body["relay"] = json!(url);
                    "relay"
                }
                other => return Err(format!("unknown mode {other}; use off, tailscale, cloudflare, or relay <url>")),
            });
            connect(&api, body).await
        }
        Cmd::Env { local } => {
            let s = Api::new(home)?.get("/state").await?;
            print_env(&base_url(&s, local), "<your clipx key, see `clipx keys create`>");
            Ok(())
        }
        Cmd::Start => {
            service::start(home)?;
            let ok = Api::new(home)?.wait_ready(10).await;
            println!("{}", if ok { "clipx is running" } else { "started, but it is not answering yet; see `clipx logs`" });
            Ok(())
        }
        Cmd::Stop => stop_all(home).await,
        Cmd::Restart => {
            if matches!(service::installed(), service::Kind::Detached) {
                stop_all(home).await?;
            }
            service::restart(home)?;
            let ok = Api::new(home)?.wait_ready(10).await;
            println!("{}", if ok { "clipx restarted" } else { "restarted, but it is not answering yet; see `clipx logs`" });
            Ok(())
        }
        Cmd::Logs { follow } => {
            service::logs(home, follow);
            Ok(())
        }
        Cmd::T3 { action, server } => {
            let action = action.unwrap_or_else(|| "on".into());
            if server.is_some() || (!config::config_path(home).exists() && T3Link::path(home).exists()) {
                return t3_remote(home, &action, server).await;
            }
            let api = Api::new(home)?;
            match action.as_str() {
                "on" | "add" => {
                    let t = api.post("/t3", json!({"enabled": true})).await?;
                    print_t3(&t);
                    if t3_linked() == Some(false) {
                        println!("this machine is not on T3 Connect yet. run `t3 connect` to reach it from app.t3.codes");
                    }
                }
                "off" | "remove" => {
                    api.post("/t3", json!({"enabled": false})).await?;
                    println!("removed clipx from T3 Code");
                }
                "show" => {
                    let t = api.get("/t3").await?;
                    println!("To add clipx to T3 Code yourself: Settings, Providers, add a provider, then enter these.");
                    println!("Make a key with `clipx keys create t3` and use it in place of <your clipx key>.");
                    for m in t["manual"].as_array().into_iter().flatten() {
                        println!();
                        println!("{}  (type: {})", m["name"].as_str().unwrap_or(""), m["driver"].as_str().unwrap_or(""));
                        for e in m["environment"].as_array().into_iter().flatten() {
                            println!("  {}={}{}", e["name"].as_str().unwrap_or(""), e["value"].as_str().unwrap_or(""), if e["sensitive"] == true { "   (secret)" } else { "" });
                        }
                        if let Some(a) = m["launch_args"].as_str() {
                            println!("  launch arguments: {a}");
                        }
                    }
                }
                other => return Err(format!("unknown action {other}; use on, off or show")),
            }
            Ok(())
        }
        Cmd::Update { version } => {
            println!("checking for a new clipx");
            let Some((exe, v)) = update::install(version).await? else {
                println!("clipx {} is up to date", env!("CARGO_PKG_VERSION"));
                return Ok(());
            };
            println!("updated clipx to {v}");
            // The new binary restarts the server and refreshes T3, so its own code does it.
            let run = |args: &[&str]| std::process::Command::new(&exe).args(args).env("CLIPX_HOME", home).status().is_ok_and(|s| s.success());
            let running = match Api::new(home) {
                Ok(api) => api.get("/state").await.is_ok(),
                Err(_) => false,
            };
            if running && !run(&["restart"]) {
                return Err("the new clipx did not restart; see `clipx logs`".into());
            }
            if T3Link::path(home).exists() && !run(&["t3"]) {
                return Err("could not refresh T3 Code; run `clipx t3`".into());
            }
            Ok(())
        }
        Cmd::Uninstall => {
            if let Ok(api) = Api::new(home)
                && api.get("/state").await.is_ok_and(|s| s["t3"]["enabled"] == true)
            {
                match api.post("/t3", json!({"enabled": false})).await {
                    Ok(_) => println!("removed clipx from T3 Code"),
                    Err(e) => eprintln!("could not remove clipx from T3 Code: {e}"),
                }
            }
            service::uninstall(home);
            println!("service removed. your data is still in {}", home.display());
            Ok(())
        }
        Cmd::Serve | Cmd::Relay { .. } => unreachable!(),
    }
}

/// A clipx on another machine that this machine's T3 uses, saved by `clipx t3 --server`.
#[derive(serde::Serialize, serde::Deserialize)]
struct T3Link {
    server: String,
    token: String,
}

impl T3Link {
    fn path(home: &Path) -> PathBuf {
        home.join("t3-link.json")
    }
    fn load(home: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(Self::path(home)).ok()?).ok()
    }
    fn api(&self) -> Api {
        Api { base: self.server.clone(), token: self.token.clone(), http: reqwest::Client::new() }
    }
}

/// `clipx t3` for a T3 on this machine and a clipx somewhere else. There is no clipx server
/// here to keep T3 in step, so this runs once; `clipx update` runs it again.
async fn t3_remote(home: &Path, action: &str, server: Option<String>) -> Res {
    let link = match server {
        Some(s) => {
            let token = match std::env::var("CLIPX_ADMIN_TOKEN").ok().filter(|t| !t.is_empty()) {
                Some(t) => t,
                None => prompt_secret("clipx admin token (admin_token in ~/.clipx/config.toml on that machine): ").await.filter(|t| !t.is_empty()).ok_or("no admin token given")?,
            };
            T3Link { server: s.trim_end_matches('/').to_string(), token }
        }
        None => T3Link::load(home).ok_or("no clipx to use; run `clipx t3 --server <clipx address>`")?,
    };
    let api = link.api();
    let state = api.get("/state").await.map_err(|e| format!("{}: {e}", link.server))?;
    // Keys get this machine's name, so they never clash with a T3 next to that clipx.
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .or_else(|| std::process::Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).to_string()))
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "another machine".into());
    let key_name = |p: store::Provider| format!("{} on {host}", t3::key_name(p));
    let ours = |name: &str| t3::PROVIDERS.iter().any(|p| name == key_name(*p));
    let mut settings = t3::read()?;
    match action {
        "on" | "add" => {
            if !t3::installed() {
                return Err(format!("T3 Code is not installed here (no {})", t3::home().display()));
            }
            let accounts: Vec<store::Provider> = state["accounts"].as_array().into_iter().flatten().filter_map(|a| serde_json::from_value(a["provider"].clone()).ok()).collect();
            let want: Vec<store::Provider> = t3::PROVIDERS.into_iter().filter(|p| accounts.contains(p)).collect();
            let keys = state["keys"].as_array().cloned().unwrap_or_default();
            let names: Vec<String> = keys.iter().filter_map(|k| k["name"].as_str().map(String::from)).collect();
            let mut fresh = Vec::new();
            for p in t3::needs_key(&settings, &want, &names, key_name) {
                for k in keys.iter().filter(|k| k["name"] == key_name(p).as_str()) {
                    api.delete(&format!("/keys/{}", k["id"].as_str().unwrap_or(""))).await?;
                }
                let r = api.post("/keys", json!({"name": key_name(p)})).await?;
                fresh.push((p, r["key"].as_str().unwrap_or("").to_string(), r["id"].as_str().unwrap_or("").to_string()));
            }
            let before = settings.clone();
            let new_keys: Vec<(store::Provider, String)> = fresh.iter().map(|(p, k, _)| (*p, k.clone())).collect();
            t3::apply(&mut settings, &link.server, &want, &new_keys, &t3::programs());
            if settings != before
                && let Err(e) = t3::write(&settings)
            {
                for (_, _, id) in &fresh {
                    let _ = api.delete(&format!("/keys/{id}")).await;
                }
                return Err(e);
            }
            util::write_private(&T3Link::path(home), &serde_json::to_vec_pretty(&link).unwrap()).map_err(|e| e.to_string())?;
            let added: Vec<&str> = t3::added(&settings).iter().map(|p| t3::display_name(*p)).collect();
            if added.is_empty() {
                println!("T3 Code      the clipx at {} has no Claude or ChatGPT accounts yet; run this again after adding one", link.server);
            } else {
                println!("T3 Code      added {}, using the clipx at {}", added.join(" and "), link.server);
            }
            println!("clipx does not run here, so run `clipx t3` again after adding a new kind of account. `clipx update` does it for you.");
        }
        "off" | "remove" => {
            let before = settings.clone();
            t3::apply(&mut settings, "", &[], &[], &[]);
            if settings != before {
                t3::write(&settings)?;
            }
            for k in state["keys"].as_array().into_iter().flatten().filter(|k| k["name"].as_str().is_some_and(ours)) {
                api.delete(&format!("/keys/{}", k["id"].as_str().unwrap_or(""))).await?;
            }
            let _ = std::fs::remove_file(T3Link::path(home));
            println!("removed clipx from T3 Code");
        }
        other => return Err(format!("unknown action {other}; use on or off")),
    }
    Ok(())
}

/// Stop through the service manager, then ask any server still answering to exit.
async fn stop_all(home: &Path) -> Res {
    service::stop(home);
    let api = Api::new(home)?;
    if api.get("/state").await.is_ok() {
        let _ = api.post("/shutdown", json!({})).await;
        for _ in 0..50 {
            if api.get("/state").await.is_err() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    if api.get("/state").await.is_ok() {
        return Err("clipx is still answering; it may run under another user or service".into());
    }
    println!("stopped");
    Ok(())
}

async fn connect(api: &Api, body: Value) -> Res {
    api.patch("/connect", body.clone()).await?;
    if body["mode"] == "off" {
        println!("remote access off");
        return Ok(());
    }
    println!("connecting…");
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let s = api.get("/state").await?;
        if s["tunnel"]["connected"] == true {
            println!("remote access on: {}", s["tunnel"]["public_url"].as_str().unwrap_or(""));
            return Ok(());
        }
    }
    let s = api.get("/state").await?;
    Err(format!("not connected yet: {}", s["tunnel"]["error"].as_str().unwrap_or("no answer")))
}

/// Read one line from the terminal on a thread, so the caller can wait on other things too.
/// Returns None at end of input.
fn read_line() -> tokio::sync::oneshot::Receiver<Option<String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let got = std::io::stdin().read_line(&mut line).ok().filter(|n| *n > 0).map(|_| line.trim().to_string());
        let _ = tx.send(got);
    });
    rx
}

async fn prompt(q: &str) -> Option<String> {
    print!("{q}");
    std::io::stdout().flush().ok();
    read_line().await.ok().flatten()
}

/// Like `prompt`, without echoing what is typed.
async fn prompt_secret(q: &str) -> Option<String> {
    let tty = std::io::stdin().is_terminal();
    let stty = |arg: &str| {
        let _ = std::process::Command::new("stty").arg(arg).stdin(std::process::Stdio::inherit()).status();
    };
    if tty {
        stty("-echo");
    }
    let got = prompt(q).await;
    if tty {
        stty("echo");
        println!();
    }
    got
}

async fn confirm(q: &str) -> bool {
    prompt(&format!("{q} [Y/n] ")).await.is_some_and(|a| !a.to_ascii_lowercase().starts_with('n'))
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    v.sort();
    v
}

async fn import_files(api: &Api, files: Vec<PathBuf>, no_refresh: bool, link: bool) -> usize {
    let mut n = 0;
    for f in files {
        let Ok(mut v) = std::fs::read(&f).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string())) else {
            eprintln!("skip {}: not json", f.display());
            continue;
        };
        if no_refresh {
            v["no_refresh"] = json!(true);
        }
        if link {
            let abs = std::fs::canonicalize(&f).unwrap_or(f.clone());
            v["linked"] = json!(abs.to_string_lossy());
        }
        match api.post("/accounts/import", v).await {
            Ok(r) => {
                for a in r["accounts"].as_array().into_iter().flatten() {
                    println!("imported {} ({})", a["label"].as_str().unwrap_or(""), a["provider"].as_str().unwrap_or(""));
                    n += 1;
                }
            }
            Err(e) => eprintln!("skip {}: {e}", f.display()),
        }
    }
    n
}

async fn login(home: &Path, provider: &str) -> Res {
    let api = Api::new(home)?;
    let r = api.post("/login/start", json!({"provider": provider})).await?;
    let flow = r["flow_id"].as_str().unwrap_or("").to_string();
    println!("open this url in a browser and sign in:\n\n  {}\n", r["url"].as_str().unwrap_or(""));
    println!("{}", r["hint"].as_str().unwrap_or(""));
    print!("paste here: ");
    std::io::stdout().flush().ok();
    // A local browser callback can finish the flow while we wait for a paste.
    let mut line = read_line();
    loop {
        tokio::select! {
            got = &mut line => {
                let Some(text) = got.ok().flatten().filter(|t| !t.is_empty()) else {
                    return Err("nothing pasted".into());
                };
                let r = api.post("/login/complete", json!({"flow_id": flow, "callback": text})).await?;
                println!("added {}", r["account"]["label"].as_str().unwrap_or(""));
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                if api.get(&format!("/login/status/{flow}")).await?["done"] == true {
                    println!("\nsigned in through the browser. press Enter to continue");
                    let _ = line.await;
                    return Ok(());
                }
            }
        }
    }
}

/// Offer CLIProxyAPI's logins, then let the user sign in to as many accounts as they like.
async fn add_accounts(home: &Path, api: &Api) -> Res {
    let cpa = util::home_dir().join(".cli-proxy-api");
    let files = json_files(&cpa);
    let state = api.get("/state").await?;
    let has_linked = state["accounts"].as_array().into_iter().flatten().any(|a| a["linked"].is_string());
    if !files.is_empty()
        && !has_linked
        && confirm(&format!(
            "\nCLIProxyAPI has {} login(s) in {}. Use them in clipx too? clipx reads them and never refreshes them, so CLIProxyAPI keeps working.",
            files.len(),
            cpa.display()
        ))
        .await
    {
        import_files(api, files, false, true).await;
    }
    loop {
        let s = api.get("/state").await?;
        println!("\naccounts");
        if s["accounts"].as_array().is_none_or(|a| a.is_empty()) {
            println!("  none yet");
        } else {
            print_accounts(&s);
        }
        let Some(pick) = prompt("add an account: 1 Claude, 2 ChatGPT, 3 Gemini, or press Enter when done: ").await else {
            return Ok(());
        };
        let provider = match pick.to_ascii_lowercase().as_str() {
            "" => return Ok(()),
            "1" | "claude" => "claude",
            "2" | "chatgpt" | "codex" => "codex",
            "3" | "gemini" => "gemini",
            _ => {
                println!("type 1, 2 or 3, or press Enter to finish");
                continue;
            }
        };
        println!();
        if let Err(e) = login(home, provider).await {
            eprintln!("{e}");
        }
    }
}

fn run_sh(script: &str, env: &[(&str, &str)]) -> bool {
    std::process::Command::new("sh").args(["-c", script]).envs(env.iter().copied()).status().is_ok_and(|s| s.success())
}

/// T3 Code needs libatomic on Linux, which slim images leave out. Offer to install it.
async fn install_libatomic() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let found = ["/lib", "/usr/lib", "/lib64", "/usr/lib64"].iter().any(|d| {
        std::fs::read_dir(d).into_iter().flatten().flatten().any(|e| {
            let p = e.path();
            p.join("libatomic.so.1").exists() || p.file_name().is_some_and(|n| n == "libatomic.so.1")
        })
    });
    if found {
        return;
    }
    let cmd = [
        ("apt-get", "apt-get install -y libatomic1"),
        ("dnf", "dnf install -y libatomic"),
        ("yum", "yum install -y libatomic"),
        ("zypper", "zypper install -y libatomic1"),
        ("pacman", "pacman -S --noconfirm gcc-libs"),
        ("apk", "apk add libatomic"),
    ]
    .into_iter()
    .find(|(bin, _)| util::which(bin).is_some())
    .map(|(_, c)| c);
    let Some(cmd) = cmd else {
        println!("note: T3 Code needs libatomic.so.1, which is not installed. install it with your package manager");
        return;
    };
    let root = std::process::Command::new("id").arg("-u").output().is_ok_and(|o| o.stdout.starts_with(b"0"));
    let sudo = if root { String::new() } else { "sudo ".into() };
    let update = if cmd.starts_with("apt-get") { format!("{sudo}apt-get update -qq && ") } else { String::new() };
    let full = format!("{update}{sudo}{cmd}");
    if confirm(&format!("T3 Code needs libatomic, which is not installed. Install it now ({full})?")).await && !run_sh(&full, &[]) {
        eprintln!("installing libatomic failed; T3 Code may not start");
    }
}

/// Whether this machine is on T3 Connect. None when the `t3` command is missing or did not answer.
fn t3_linked() -> Option<bool> {
    let out = std::process::Command::new(t3::cli()?).args(["connect", "status", "--json"]).stderr(std::process::Stdio::null()).output().ok()?;
    serde_json::from_slice::<Value>(&out.stdout).ok().map(|v| v["linked"] == true)
}

fn print_t3(t: &Value) {
    let added: Vec<&str> = t["added"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if added.is_empty() {
        println!("T3 Code      on. clipx adds its providers there once you add an account");
    } else {
        println!("T3 Code      added {}", added.join(" and "));
    }
}

/// Everything T3 needs to run clipx's providers, then T3 Connect so app.t3.codes can reach
/// this machine.
async fn setup_t3(api: &Api, interactive: bool) -> Result<Option<Value>, String> {
    let state = api.get("/state").await?;
    let has = |p: &str| state["accounts"].as_array().into_iter().flatten().any(|a| a["provider"] == p);
    let tools = [
        ("claude", "Claude Code", "curl -fsSL https://claude.ai/install.sh | bash", &[][..]),
        ("codex", "Codex", "curl -fsSL https://chatgpt.com/codex/install.sh | sh", &[("CODEX_NON_INTERACTIVE", "1")][..]),
    ];
    for (provider, (bin, name, script, env)) in ["claude", "codex"].into_iter().zip(tools) {
        if !has(provider) || util::which(bin).is_some() {
            continue;
        }
        if interactive && confirm(&format!("\nT3 runs these accounts through {name}, which is not installed. Install it?")).await {
            if !run_sh(script, env) {
                eprintln!("installing {name} failed; install it yourself, T3 needs it");
            }
        } else {
            println!("note: T3 needs {name} for these accounts. install it with: {script}");
        }
    }
    if interactive {
        install_libatomic().await;
    }
    let t = api.post("/t3", json!({"enabled": true})).await?;
    if !interactive {
        return Ok(Some(t));
    }
    let Some(cli) = t3::cli() else {
        return Ok(Some(t));
    };
    match t3_linked() {
        Some(false) => {
            println!("\nNow T3 Connect, so you can use this machine from app.t3.codes.");
            println!("If T3 asks to install its relay client, answer y.\n");
            let _ = std::process::Command::new(&cli).arg("connect").status();
        }
        Some(true) if !t3::running() => {
            let _ = std::process::Command::new(&cli).args(["service", "install"]).status();
        }
        _ => {}
    }
    Ok(Some(t))
}

async fn setup(home: &Path, args: SetupArgs) -> Res {
    std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let fresh = !config::config_path(home).exists();
    let mut cfg = if fresh { Config::new() } else { Config::load(home).map_err(|e| e.to_string())? };
    if let Some(p) = args.port {
        let host = cfg.listen.rsplit_once(':').map(|(h, _)| h.to_string()).unwrap_or_else(|| "127.0.0.1".into());
        cfg.listen = format!("{host}:{p}");
    }
    if args.public {
        cfg.listen = format!("0.0.0.0:{}", cfg.port());
    }
    let tunnel = match (args.tunnel.as_deref(), &args.relay) {
        (_, Some(_)) | (Some("relay"), _) => Some(TunnelMode::Relay),
        (Some("off" | "none"), _) => Some(TunnelMode::Off),
        (Some("cloudflare" | "cf"), _) => Some(TunnelMode::Cloudflare),
        (Some("tailscale" | "ts"), _) => Some(TunnelMode::Tailscale),
        (Some(other), _) => return Err(format!("unknown tunnel {other}; use tailscale, cloudflare, relay or off")),
        (None, None) if fresh => Some(if tailscale_running() { TunnelMode::Tailscale } else { TunnelMode::Cloudflare }),
        _ => None,
    };
    let auto_tunnel = fresh && args.tunnel.is_none() && args.relay.is_none();
    if let Some(p) = args.ts_port {
        cfg.connect.ts_port = Some(p);
    }
    if let Some(t) = tunnel {
        cfg.connect.mode = t;
    }
    if let Some(r) = args.relay {
        cfg.connect.relay = Some(r.trim_end_matches('/').to_string());
    }
    if cfg.connect.mode == TunnelMode::Relay && cfg.connect.relay.is_none() {
        return Err("--tunnel relay needs --relay <url>".into());
    }
    if let Some(n) = args.name {
        cfg.connect.name = Some(n.to_ascii_lowercase());
    }
    if let Some(s) = args.relay_secret {
        cfg.connect.relay_secret = Some(s);
    }
    if fresh {
        cfg.strategy = Strategy::RoundRobin;
    }
    cfg.save(home).map_err(|e| e.to_string())?;

    // The first key is created offline so it can be printed exactly once.
    let mut new_key = None;
    {
        let store = store::Store::open(home).map_err(|e| e.to_string())?;
        if store.keys.read().unwrap().is_empty() {
            let (_, plain) = store.create_key("default").map_err(|e| e.to_string())?;
            new_key = Some(plain);
        }
    }

    println!("clipx home   {}", home.display());
    if args.no_service {
        println!("config written. run `clipx serve` to start in the foreground.");
        if let Some(k) = &new_key {
            println!("api key      {k}");
        }
        return Ok(());
    }
    let how = service::install_and_start(home)?;
    println!("running as   {how}");
    let api = Api::new(home)?;
    if !api.wait_ready(15).await {
        return Err("clipx did not start; see `clipx logs`".into());
    }
    if let Some(dir) = args.import {
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let p = entry.map_err(|e| e.to_string())?.path();
            if p.extension().is_some_and(|x| x == "json")
                && let Ok(v) = std::fs::read(&p).map_err(|e| e.to_string()).and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()))
                    && let Ok(r) = api.post("/accounts/import", v).await {
                        for a in r["accounts"].as_array().into_iter().flatten() {
                            println!("imported     {}", a["label"].as_str().unwrap_or(""));
                        }
                    }
        }
    }
    let mut state = api.get("/state").await?;
    if cfg.connect.mode != TunnelMode::Off {
        print!("remote       connecting");
        use std::io::Write;
        for i in 0..90 {
            if state["tunnel"]["connected"] == true {
                break;
            }
            // Tailscale was only a guess; if Funnel is not allowed here, use a quick tunnel.
            if auto_tunnel && i == 30 && cfg.connect.mode == TunnelMode::Tailscale {
                print!(" ({}; trying cloudflare)", state["tunnel"]["error"].as_str().unwrap_or("tailscale did not answer"));
                api.patch("/connect", json!({"mode": "cloudflare"})).await?;
            }
            print!(".");
            std::io::stdout().flush().ok();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            state = api.get("/state").await?;
        }
        println!();
        if state["tunnel"]["connected"] == true {
            println!("remote       {}", state["tunnel"]["public_url"].as_str().unwrap_or(""));
        } else {
            println!("remote       not up yet ({}); it keeps retrying", state["tunnel"]["error"].as_str().unwrap_or("no answer"));
        }
    }
    let interactive = std::io::stdin().is_terminal();
    let mut use_t3 = !args.no_t3 && t3::installed();
    if !args.no_t3 && !use_t3 && interactive && confirm("\nT3 Code is not installed. Install it? clipx adds your accounts to it, and app.t3.codes reaches it from anywhere.").await {
        install_libatomic().await;
        use_t3 = run_sh("curl -fsSL https://t3.codes/install.sh | sh", &[]) && t3::installed();
        if !use_t3 {
            eprintln!("T3 Code did not install; carrying on without it");
        }
    }
    if interactive {
        add_accounts(home, &api).await?;
    }
    let t3_state = if use_t3 {
        setup_t3(&api, interactive).await.unwrap_or_else(|e| {
            eprintln!("could not add clipx to T3 Code: {e}");
            None
        })
    } else {
        None
    };

    let state = api.get("/state").await?;
    let base = base_url(&state, false);
    println!();
    println!("clipx        {}", state["local_url"].as_str().unwrap_or(""));
    if state["tunnel"]["connected"] == true {
        println!("remote       {}", state["tunnel"]["public_url"].as_str().unwrap_or(""));
    }
    println!("dashboard    {base}/#token={}", cfg.admin_token);
    println!("admin token  {}", cfg.admin_token);
    match &new_key {
        Some(k) => println!("api key      {k}   (shown once)"),
        None => println!("api key      (existing keys kept; make another with `clipx keys create`)"),
    }
    let n = state["accounts"].as_array().map_or(0, Vec::len);
    println!("accounts     {n}");
    if let Some(t) = &t3_state {
        let t = api.get("/t3").await.unwrap_or_else(|_| t.clone());
        print_t3(&t);
        println!();
        match t3_linked() {
            Some(true) => println!("Open https://app.t3.codes and sign in. This machine is there; pick Claude (clipx) or ChatGPT (clipx) in a new thread."),
            _ => println!("Run `t3 connect`, then open https://app.t3.codes and sign in to use this machine from anywhere."),
        }
        println!("Add more accounts any time with `clipx login claude`, `clipx login codex` or `clipx login gemini`, or from the dashboard.");
        println!("Using clipx from other apps: `clipx env`.");
    } else {
        println!();
        if n == 0 {
            println!("next: add an account from the dashboard, or run `clipx login claude` / `clipx login codex`");
            println!();
        }
        print_env(&base, new_key.as_deref().unwrap_or("<your clipx key>"));
    }
    Ok(())
}
