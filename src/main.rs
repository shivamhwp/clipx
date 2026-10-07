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
mod translate;
mod tunnel;
mod usage;
mod util;

use axum::Router;
use axum::routing::any;
use clap::{Parser, Subcommand};
use config::{Config, Strategy, TunnelMode};
use serde_json::{Value, json};
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
                let mut v: Vec<PathBuf> = std::fs::read_dir(&path).map_err(|e| e.to_string())?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
                v.sort();
                v
            } else {
                vec![path]
            };
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
                    let abs = std::fs::canonicalize(&f).map_err(|e| e.to_string())?;
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
            println!("{n} account(s) imported");
            Ok(())
        }
        Cmd::Keys { action } => {
            let api = Api::new(home)?;
            match action.unwrap_or(KeysCmd::List) {
                KeysCmd::List => {
                    let s = api.get("/state").await?;
                    for k in s["keys"].as_array().into_iter().flatten() {
                        println!("{}  {:<16} {}…  used {}", k["id"].as_str().unwrap_or(""), k["name"].as_str().unwrap_or(""), k["prefix"].as_str().unwrap_or(""), ago(k["last_used"].as_u64().unwrap_or(0)));
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
        Cmd::Uninstall => {
            service::uninstall(home);
            println!("service removed. your data is still in {}", home.display());
            Ok(())
        }
        Cmd::Serve | Cmd::Relay { .. } => unreachable!(),
    }
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

async fn login(home: &Path, provider: &str) -> Res {
    let api = Api::new(home)?;
    let r = api.post("/login/start", json!({"provider": provider})).await?;
    let flow = r["flow_id"].as_str().unwrap_or("").to_string();
    println!("open this url in a browser and sign in:\n\n  {}\n", r["url"].as_str().unwrap_or(""));
    println!("{}", r["hint"].as_str().unwrap_or(""));
    print!("paste here: ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    // Read the paste on a thread so a local browser callback can finish the flow meanwhile.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);
    std::thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            let _ = tx.blocking_send(line);
        }
    });
    loop {
        tokio::select! {
            line = rx.recv() => {
                let line = line.unwrap_or_default();
                if line.trim().is_empty() {
                    return Err("nothing pasted".into());
                }
                let r = api.post("/login/complete", json!({"flow_id": flow, "callback": line.trim()})).await?;
                println!("added {}", r["account"]["label"].as_str().unwrap_or(""));
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                if api.get(&format!("/login/status/{flow}")).await?["done"] == true {
                    println!("\nlogged in through the browser callback");
                    return Ok(());
                }
            }
        }
    }
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
    let base = base_url(&state, false);
    println!("local        {}", state["local_url"].as_str().unwrap_or(""));
    println!();
    println!("dashboard    {base}/#token={}", cfg.admin_token);
    println!("admin token  {}", cfg.admin_token);
    match &new_key {
        Some(k) => println!("api key      {k}   (shown once)"),
        None => println!("api key      (existing keys kept; make another with `clipx keys create`)"),
    }
    println!();
    if state["accounts"].as_array().is_none_or(|a| a.is_empty()) {
        println!("next: add an account from the dashboard, or run `clipx login claude` / `clipx login codex`");
        println!();
    }
    print_env(&base, new_key.as_deref().unwrap_or("<your clipx key>"));
    Ok(())
}
