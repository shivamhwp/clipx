//! Remote access. A box keeps one outbound WebSocket to a relay; the relay forwards public
//! HTTP requests over it as multiplexed streams. Or, with no relay, a Cloudflare quick tunnel.
//!
//! Frame: [kind u8][stream id u32 BE][payload]

use crate::app::App;
use crate::config::TunnelMode;
use crate::util::{now, sha256_hex};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as WsMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tower::ServiceExt;

pub const REQ_HEAD: u8 = 1;
pub const REQ_DATA: u8 = 2;
pub const REQ_END: u8 = 3;
pub const RES_HEAD: u8 = 4;
pub const RES_DATA: u8 = 5;
pub const RES_END: u8 = 6;
pub const RESET: u8 = 7;

const MAX_FRAME_DATA: usize = 64 * 1024;
/// Frames buffered per stream. The shared socket reader never waits on one stream: a
/// stream whose reader falls this far behind is reset instead of stalling the others.
const STREAM_BUFFER: usize = 256;

pub fn encode(kind: u8, id: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.push(kind);
    v.extend_from_slice(&id.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

pub fn decode(b: &[u8]) -> Option<(u8, u32, &[u8])> {
    if b.len() < 5 {
        return None;
    }
    Some((b[0], u32::from_be_bytes([b[1], b[2], b[3], b[4]]), &b[5..]))
}

#[derive(Serialize, Deserialize)]
struct ReqHead {
    m: String,
    p: String,
    h: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize)]
struct ResHead {
    s: u16,
    h: Vec<(String, String)>,
}

const HOP: &[&str] = &["connection", "keep-alive", "transfer-encoding", "te", "upgrade", "proxy-connection", "content-length", "host"];

fn header_pairs(h: &HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .filter(|(k, _)| !HOP.contains(&k.as_str()))
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect()
}

fn apply_pairs(dst: &mut HeaderMap, pairs: &[(String, String)]) {
    for (k, v) in pairs {
        if let (Ok(k), Ok(v)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            dst.append(k, v);
        }
    }
}

fn set_status(app: &App, f: impl FnOnce(&mut crate::app::TunnelStatus)) {
    f(&mut app.tunnel.lock().unwrap());
}

// =================================================================== agent side

/// Keeps the configured tunnel running and restarts it when the config changes.
pub async fn supervise(app: Arc<App>, router: Router) {
    let mut ctl = app.tunnel_ctl.subscribe();
    let mut served: Option<(u16, bool)> = None;
    loop {
        let connect = app.cfg.read().unwrap().connect.clone();
        // Leaving Tailscale mode, or moving it, takes our mount down. Other mounts stay.
        let wanted = (connect.mode == TunnelMode::Tailscale).then(|| (connect.ts_port.unwrap_or(443), connect.tailnet_only));
        if let Some(old) = served.take().filter(|o| Some(*o) != wanted) {
            tailscale_off(old.0, old.1).await;
        }
        served = wanted;
        set_status(&app, |s| {
            *s = crate::app::TunnelStatus { mode: format!("{:?}", connect.mode).to_lowercase(), since: now(), ..Default::default() };
        });
        let run = async {
            match connect.mode {
                TunnelMode::Off => std::future::pending::<()>().await,
                TunnelMode::Relay => relay_agent(app.clone(), router.clone()).await,
                TunnelMode::Cloudflare => cloudflare(app.clone()).await,
                TunnelMode::Tailscale => tailscale(app.clone()).await,
            }
        };
        tokio::select! {
            _ = run => {}
            _ = ctl.changed() => {}
        }
    }
}

fn ws_url(relay: &str, name: &str) -> String {
    let base = relay.trim_end_matches('/');
    let base = if let Some(r) = base.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = base.strip_prefix("http://") {
        format!("ws://{r}")
    } else if base.starts_with("ws") {
        base.to_string()
    } else {
        format!("wss://{base}")
    };
    format!("{base}/_clipx/connect?name={name}")
}

async fn relay_agent(app: Arc<App>, router: Router) {
    let mut backoff = 1u64;
    loop {
        let (relay, name, key, secret) = {
            let mut cfg = app.cfg.write().unwrap();
            let c = &mut cfg.connect;
            let mut changed = false;
            if c.name.is_none() {
                c.name = Some(default_name());
                changed = true;
            }
            if c.agent_key.is_none() {
                c.agent_key = Some(crate::util::random_token("ak-"));
                changed = true;
            }
            let out = (c.relay.clone(), c.name.clone().unwrap(), c.agent_key.clone().unwrap(), c.relay_secret.clone());
            drop(cfg);
            if changed {
                let _ = app.save_config();
            }
            out
        };
        let Some(relay) = relay else {
            set_status(&app, |s| s.error = Some("no relay url configured".into()));
            return std::future::pending().await;
        };
        let started = std::time::Instant::now();
        match agent_session(&app, &router, &ws_url(&relay, &name), &key, secret.as_deref()).await {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("tunnel: {e}");
                set_status(&app, |s| {
                    s.connected = false;
                    s.error = Some(e);
                });
            }
        }
        if started.elapsed() > Duration::from_secs(30) {
            backoff = 1;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

fn default_name() -> String {
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let host: String = host.trim().to_ascii_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(20).collect();
    let host = if host.is_empty() { "box".to_string() } else { host };
    format!("{host}-{}", crate::util::random_hex(3))
}

struct AgentStream {
    body: Option<mpsc::Sender<Result<Bytes, std::io::Error>>>,
    task: tokio::task::AbortHandle,
}

async fn agent_session(app: &Arc<App>, router: &Router, url: &str, key: &str, secret: Option<&str>) -> Result<(), String> {
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let mut req = url.into_client_request().map_err(|e| e.to_string())?;
    req.headers_mut().insert("authorization", HeaderValue::from_str(&format!("Bearer {key}")).map_err(|e| e.to_string())?);
    if let Some(s) = secret {
        req.headers_mut().insert("x-clipx-relay-secret", HeaderValue::from_str(s).map_err(|e| e.to_string())?);
    }
    let (ws, resp) = tokio::time::timeout(Duration::from_secs(20), tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| "relay connect timed out".to_string())?
        .map_err(|e| match e {
            tokio_tungstenite::tungstenite::Error::Http(r) => {
                let body = r.body().as_ref().map(|b| String::from_utf8_lossy(b).to_string()).unwrap_or_default();
                format!("relay refused ({}): {body}", r.status())
            }
            other => other.to_string(),
        })?;
    drop(resp);
    let (mut sink, mut source) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });
    let pinger = {
        let tx = out_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(20)).await;
                if tx.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        })
    };
    let streams: Arc<Mutex<HashMap<u32, AgentStream>>> = Arc::default();
    let result = loop {
        let next = tokio::time::timeout(Duration::from_secs(75), source.next()).await;
        let msg = match next {
            Err(_) => break Err("relay went silent".to_string()),
            Ok(None) => break Err("relay closed the connection".to_string()),
            Ok(Some(Err(e))) => break Err(e.to_string()),
            Ok(Some(Ok(m))) => m,
        };
        match msg {
            Message::Text(t) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t)
                    && let Some(u) = v["url"].as_str() {
                        tracing::info!("tunnel connected: {u}");
                        set_status(app, |s| {
                            s.connected = true;
                            s.public_url = Some(u.to_string());
                            s.error = None;
                            s.since = now();
                        });
                    }
            }
            Message::Binary(b) => {
                let Some((kind, id, payload)) = decode(&b) else { continue };
                match kind {
                    REQ_HEAD => {
                        let Ok(head) = serde_json::from_slice::<ReqHead>(payload) else { continue };
                        // Room for a whole request body (the proxy accepts up to 64 MB) before the handler reads it.
                        let (btx, brx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1100);
                        let task = tokio::spawn(serve_stream(router.clone(), id, head, brx, out_tx.clone(), streams.clone()));
                        streams.lock().unwrap().insert(id, AgentStream { body: Some(btx), task: task.abort_handle() });
                    }
                    REQ_DATA => {
                        let tx = streams.lock().unwrap().get(&id).and_then(|s| s.body.clone());
                        if let Some(tx) = tx
                            && tx.try_send(Ok(Bytes::copy_from_slice(payload))).is_err() {
                                if let Some(s) = streams.lock().unwrap().remove(&id) {
                                    s.task.abort();
                                }
                                let _ = out_tx.try_send(Message::Binary(encode(RESET, id, b"").into()));
                            }
                    }
                    REQ_END => {
                        if let Some(s) = streams.lock().unwrap().get_mut(&id) {
                            s.body = None;
                        }
                    }
                    RESET => {
                        if let Some(s) = streams.lock().unwrap().remove(&id) {
                            s.task.abort();
                        }
                    }
                    _ => {}
                }
            }
            Message::Close(_) => break Err("relay closed the connection".to_string()),
            _ => {}
        }
    };
    pinger.abort();
    for (_, s) in streams.lock().unwrap().drain() {
        s.task.abort();
    }
    drop(out_tx);
    writer.abort();
    set_status(app, |s| s.connected = false);
    result
}

async fn serve_stream(
    router: Router,
    id: u32,
    head: ReqHead,
    body: mpsc::Receiver<Result<Bytes, std::io::Error>>,
    out: mpsc::Sender<tokio_tungstenite::tungstenite::Message>,
    streams: Arc<Mutex<HashMap<u32, AgentStream>>>,
) {
    use tokio_tungstenite::tungstenite::Message;
    let body_stream = futures_util::stream::unfold(body, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    let mut builder = axum::http::Request::builder().method(head.m.as_str()).uri(head.p.as_str());
    if let Some(h) = builder.headers_mut() {
        apply_pairs(h, &head.h);
        h.insert("x-clipx-tunnel", HeaderValue::from_static("1"));
    }
    let send = |kind: u8, payload: &[u8]| Message::Binary(encode(kind, id, payload).into());
    let resp = match builder.body(Body::from_stream(body_stream)) {
        Ok(req) => router.oneshot(req).await.unwrap_or_else(|e| match e {}),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    let (parts, body) = resp.into_parts();
    let rh = ResHead { s: parts.status.as_u16(), h: header_pairs(&parts.headers) };
    if out.send(send(RES_HEAD, &serde_json::to_vec(&rh).unwrap_or_default())).await.is_err() {
        return;
    }
    let mut data = body.into_data_stream();
    while let Some(chunk) = data.next().await {
        match chunk {
            Ok(b) => {
                for part in b.chunks(MAX_FRAME_DATA) {
                    if out.send(send(RES_DATA, part)).await.is_err() {
                        return;
                    }
                }
            }
            Err(_) => {
                let _ = out.send(send(RESET, b"")).await;
                streams.lock().unwrap().remove(&id);
                return;
            }
        }
    }
    let _ = out.send(send(RES_END, b"")).await;
    streams.lock().unwrap().remove(&id);
}

// ---------------------------------------------------------------- tailscale serve / funnel

fn tailscale_bin() -> Option<PathBuf> {
    let mac = PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
    std::env::var_os("PATH")
        .and_then(|paths| std::env::split_paths(&paths).map(|d| d.join("tailscale")).find(|p| p.exists()))
        .or_else(|| mac.exists().then_some(mac))
}

/// Run a tailscale command with a deadline. Funnel can stop and wait for someone to
/// approve it in the admin console; the deadline turns that into an error with the link.
async fn ts(args: &[&str]) -> Result<String, String> {
    let bin = tailscale_bin().ok_or("tailscale is not installed")?;
    let child = tokio::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("running tailscale: {e}"))?;
    let out = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output()).await;
    let out = match out {
        Ok(o) => o.map_err(|e| e.to_string())?,
        Err(_) => return Err(format!("tailscale {} did not finish; it may be waiting for approval in the tailscale admin console", args[0])),
    };
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        return Ok(text);
    }
    let text = text.trim();
    if text.contains("Access denied") || text.contains("permission") {
        return Err(format!("{text}. allow your user once with: sudo tailscale set --operator=$USER"));
    }
    Err(text.chars().take(400).collect())
}

/// This machine's name on the tailnet, e.g. box.tail1234.ts.net.
async fn tailscale_host() -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_str(&ts(&["status", "--json"]).await?).map_err(|e| e.to_string())?;
    if v["BackendState"] != "Running" {
        return Err(format!("tailscale is {}; run `tailscale up`", v["BackendState"].as_str().unwrap_or("not running")));
    }
    let host = v["Self"]["DNSName"].as_str().unwrap_or("").trim_end_matches('.');
    if host.is_empty() {
        return Err("tailscale has no DNS name for this machine; turn on MagicDNS".into());
    }
    Ok(host.to_string())
}

fn ts_url(host: &str, port: u16) -> String {
    if port == 443 { format!("https://{host}") } else { format!("https://{host}:{port}") }
}

/// Mount clipx at `/` on the chosen HTTPS port and keep it there.
async fn tailscale(app: Arc<App>) {
    let (port, tailnet_only, local) = {
        let cfg = app.cfg.read().unwrap();
        (cfg.connect.ts_port.unwrap_or(443), cfg.connect.tailnet_only, cfg.port())
    };
    let target = format!("http://127.0.0.1:{local}");
    let https = format!("--https={port}");
    loop {
        let result = async {
            let host = tailscale_host().await?;
            // Re-apply only when our mount is missing, so a healthy setup is left alone.
            let status: serde_json::Value = serde_json::from_str(&ts(&["serve", "status", "--json"]).await?).unwrap_or_default();
            let hp = format!("{host}:{port}");
            let mounted = status["Web"][&hp]["Handlers"]["/"]["Proxy"].as_str() == Some(target.as_str());
            let funneled = status["AllowFunnel"][&hp].as_bool() == Some(true);
            if !mounted || funneled == tailnet_only {
                let verb = if tailnet_only { "serve" } else { "funnel" };
                ts(&[verb, "--bg", "--yes", &https, &target]).await?;
            }
            Ok::<String, String>(ts_url(&host, port))
        }
        .await;
        match result {
            Ok(url) => set_status(&app, |s| {
                if !s.connected {
                    tracing::info!("tailscale serving {url}");
                    s.since = now();
                }
                s.connected = true;
                s.public_url = Some(url);
                s.error = None;
            }),
            Err(e) => set_status(&app, |s| {
                s.connected = false;
                s.error = Some(e);
            }),
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

async fn tailscale_off(port: u16, tailnet_only: bool) {
    let verb = if tailnet_only { "serve" } else { "funnel" };
    if let Err(e) = ts(&[verb, "--yes", &format!("--https={port}"), "--set-path", "/", "off"]).await {
        tracing::warn!("removing tailscale mount: {e}");
    }
}

// ---------------------------------------------------------------- cloudflare quick tunnel

async fn cloudflare(app: Arc<App>) {
    let port = app.cfg.read().unwrap().port();
    let mut backoff = 2u64;
    loop {
        let bin = match find_or_fetch_cloudflared(&app).await {
            Ok(b) => b,
            Err(e) => {
                set_status(&app, |s| s.error = Some(e));
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
        };
        let child = tokio::process::Command::new(&bin)
            .args(["tunnel", "--no-autoupdate", "--url", &format!("http://127.0.0.1:{port}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                set_status(&app, |s| s.error = Some(format!("starting cloudflared: {e}")));
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        let stderr = child.stderr.take().unwrap();
        let app2 = app.clone();
        let reader = tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(url) = find_trycloudflare(&line) {
                    tracing::info!("tunnel connected: {url}");
                    set_status(&app2, |s| {
                        s.connected = true;
                        s.public_url = Some(url);
                        s.error = None;
                        s.since = now();
                    });
                }
            }
        });
        let status = child.wait().await;
        reader.abort();
        set_status(&app, |s| {
            s.connected = false;
            s.error = Some(format!("cloudflared exited: {status:?}"));
        });
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}

fn find_trycloudflare(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    let end = rest.find(|c: char| c.is_whitespace() || c == '|').unwrap_or(rest.len());
    let url = &rest[..end];
    url.ends_with(".trycloudflare.com").then(|| url.to_string())
}

/// The cloudflared T3 Code downloads for T3 Connect, at
/// `~/.t3/tools/cloudflared/<version>/<platform>/cloudflared`. Newest version wins.
fn t3_cloudflared() -> Option<PathBuf> {
    let root = crate::util::home_dir().join(".t3/tools/cloudflared");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(root).ok()?.flatten().map(|e| e.path()).collect();
    versions.sort();
    versions.into_iter().rev().find_map(|v| {
        std::fs::read_dir(v).ok()?.flatten().map(|e| e.path().join("cloudflared")).find(|p| p.is_file())
    })
}

pub async fn find_or_fetch_cloudflared(app: &App) -> Result<PathBuf, String> {
    let local = app.home.join("bin").join("cloudflared");
    if local.exists() {
        return Ok(local);
    }
    if let Some(p) = std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|d| d.join("cloudflared")).find(|p| p.exists())) {
        return Ok(p);
    }
    if let Some(p) = t3_cloudflared() {
        return Ok(p);
    }
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        other => return Err(format!("no cloudflared build for {other}; install cloudflared yourself")),
    };
    let (asset, tgz) = match std::env::consts::OS {
        "linux" => (format!("cloudflared-linux-{arch}"), false),
        "macos" => (format!("cloudflared-darwin-{arch}.tgz"), true),
        other => return Err(format!("no cloudflared build for {other}")),
    };
    let url = format!("https://github.com/cloudflare/cloudflared/releases/latest/download/{asset}");
    tracing::info!("downloading cloudflared from {url}");
    set_status(app, |s| s.error = Some("downloading cloudflared…".into()));
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::limited(10)).build().map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().await.map_err(|e| format!("download cloudflared: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download cloudflared: http {}", resp.status()));
    }
    std::fs::create_dir_all(local.parent().unwrap()).map_err(|e| e.to_string())?;
    // Stream to disk so a 40 MB download does not sit in memory.
    let tmp = local.with_extension("part");
    {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
        let mut body = resp.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| format!("download cloudflared: {e}"))?;
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
        file.flush().await.map_err(|e| e.to_string())?;
    }
    if tgz {
        let ok = std::process::Command::new("tar").arg("-xzf").arg(&tmp).arg("-C").arg(local.parent().unwrap()).status().map(|s| s.success()).unwrap_or(false);
        let _ = std::fs::remove_file(&tmp);
        if !ok {
            return Err("could not unpack cloudflared".into());
        }
    } else {
        std::fs::rename(&tmp, &local).map_err(|e| e.to_string())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o755));
    }
    Ok(local)
}

// =================================================================== relay side

pub struct RelayOpts {
    pub listen: String,
    pub domain: Option<String>,
    pub public_url: Option<String>,
    pub secret: Option<String>,
    pub data: PathBuf,
}

struct RelayStream {
    head: Option<oneshot::Sender<ResHead>>,
    body: mpsc::Sender<Result<Bytes, std::io::Error>>,
}

struct Agent {
    conn: u64,
    tx: mpsc::Sender<WsMsg>,
    streams: Mutex<HashMap<u32, RelayStream>>,
    next: AtomicU32,
}

pub struct Relay {
    opts: RelayOpts,
    agents: RwLock<HashMap<String, Arc<Agent>>>,
    claims: Mutex<HashMap<String, String>>,
    conn_seq: AtomicU32,
}

impl Relay {
    fn claims_path(&self) -> PathBuf {
        self.opts.data.join("claims.json")
    }
    fn save_claims(&self) {
        let data = serde_json::to_vec_pretty(&*self.claims.lock().unwrap()).unwrap_or_default();
        if let Err(e) = crate::util::write_private(&self.claims_path(), &data) {
            tracing::warn!("saving relay claims: {e}");
        }
    }
}

pub fn relay_router(opts: RelayOpts) -> Router {
    let claims = std::fs::read(opts.data.join("claims.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let relay = Arc::new(Relay { opts, agents: RwLock::default(), claims: Mutex::new(claims), conn_seq: AtomicU32::new(1) });
    Router::new()
        .route("/_clipx/connect", axum::routing::get(connect_ws))
        .route("/_clipx/health", axum::routing::get(|| async { "ok" }))
        .fallback(public)
        .with_state(relay)
}

fn valid_name(n: &str) -> bool {
    (1..=40).contains(&n.len()) && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') && !n.starts_with('-')
}

#[derive(Deserialize)]
struct ConnectQ {
    name: String,
}

async fn connect_ws(State(relay): State<Arc<Relay>>, Query(q): Query<ConnectQ>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let name = q.name.to_ascii_lowercase();
    if !valid_name(&name) {
        return (StatusCode::BAD_REQUEST, "name must be 1-40 chars of a-z, 0-9 and -").into_response();
    }
    let key = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("").trim().to_string();
    if key.len() < 16 {
        return (StatusCode::UNAUTHORIZED, "agent key required").into_response();
    }
    let hash = sha256_hex(key.as_bytes());
    {
        let mut claims = relay.claims.lock().unwrap();
        match claims.get(&name) {
            Some(h) if crate::util::ct_eq(h.as_bytes(), hash.as_bytes()) => {}
            Some(_) => return (StatusCode::FORBIDDEN, format!("name {name} belongs to another box; pick another name")).into_response(),
            None => {
                if let Some(secret) = &relay.opts.secret {
                    let given = headers.get("x-clipx-relay-secret").and_then(|v| v.to_str().ok()).unwrap_or("");
                    if !crate::util::ct_eq(given.as_bytes(), secret.as_bytes()) {
                        return (StatusCode::FORBIDDEN, "this relay needs a relay secret to register new names").into_response();
                    }
                }
                claims.insert(name.clone(), hash);
                drop(claims);
                relay.save_claims();
            }
        }
    }
    let url = public_url(&relay, &name, &headers);
    ws.max_message_size(16 << 20).on_upgrade(move |socket| agent_conn(relay, name, url, socket))
}

fn public_url(relay: &Relay, name: &str, headers: &HeaderMap) -> String {
    if let Some(d) = &relay.opts.domain {
        return format!("https://{name}.{d}/");
    }
    if let Some(p) = &relay.opts.public_url {
        return format!("{}/t/{name}/", p.trim_end_matches('/'));
    }
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("localhost");
    let proto = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).unwrap_or("http");
    format!("{proto}://{host}/t/{name}/")
}

async fn agent_conn(relay: Arc<Relay>, name: String, url: String, socket: WebSocket) {
    let (mut sink, mut source) = socket.split();
    let (tx, mut rx) = mpsc::channel::<WsMsg>(256);
    let conn = relay.conn_seq.fetch_add(1, Ordering::Relaxed) as u64;
    let agent = Arc::new(Agent { conn, tx: tx.clone(), streams: Mutex::default(), next: AtomicU32::new(1) });
    if let Some(old) = relay.agents.write().unwrap().insert(name.clone(), agent.clone()) {
        let _ = old.tx.try_send(WsMsg::Close(None));
    }
    tracing::info!("agent {name} connected ({url})");
    let _ = tx.send(WsMsg::Text(serde_json::json!({"url": url}).to_string().into())).await;
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let close = matches!(m, WsMsg::Close(_));
            if sink.send(m).await.is_err() || close {
                break;
            }
        }
    });
    let pinger = {
        let tx = tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(25)).await;
                if tx.send(WsMsg::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        })
    };
    loop {
        let next = tokio::time::timeout(Duration::from_secs(90), source.next()).await;
        let Ok(Some(Ok(msg))) = next else { break };
        let WsMsg::Binary(b) = msg else {
            if matches!(msg, WsMsg::Close(_)) {
                break;
            }
            continue;
        };
        let Some((kind, id, payload)) = decode(&b) else { continue };
        match kind {
            RES_HEAD => {
                let head = serde_json::from_slice::<ResHead>(payload).ok();
                let sender = agent.streams.lock().unwrap().get_mut(&id).and_then(|s| s.head.take());
                if let (Some(h), Some(s)) = (head, sender) {
                    let _ = s.send(h);
                }
            }
            RES_DATA => {
                let body = agent.streams.lock().unwrap().get(&id).map(|s| s.body.clone());
                if let Some(body) = body
                    && body.try_send(Ok(Bytes::copy_from_slice(payload))).is_err() {
                        agent.streams.lock().unwrap().remove(&id);
                        let _ = agent.tx.try_send(WsMsg::Binary(encode(RESET, id, b"").into()));
                    }
            }
            RES_END => {
                agent.streams.lock().unwrap().remove(&id);
            }
            RESET => {
                if let Some(s) = agent.streams.lock().unwrap().remove(&id) {
                    let _ = s.body.try_send(Err(std::io::Error::other("box reset the stream")));
                }
            }
            _ => {}
        }
    }
    pinger.abort();
    writer.abort();
    agent.streams.lock().unwrap().clear();
    let mut agents = relay.agents.write().unwrap();
    if agents.get(&name).is_some_and(|a| a.conn == conn) {
        agents.remove(&name);
    }
    tracing::info!("agent {name} disconnected");
}

/// Response body that tells the box to stop when the public client goes away.
struct RelayBody {
    rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
    agent: Arc<Agent>,
    id: u32,
    done: bool,
}

impl Stream for RelayBody {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let r = self.rx.poll_recv(cx);
        if let Poll::Ready(None) = r {
            self.done = true;
        }
        r
    }
}

impl Drop for RelayBody {
    fn drop(&mut self) {
        if !self.done && self.agent.streams.lock().unwrap().remove(&self.id).is_some() {
            let _ = self.agent.tx.try_send(WsMsg::Binary(encode(RESET, self.id, b"").into()));
        }
    }
}

async fn public(State(relay): State<Arc<Relay>>, req: Request) -> Response {
    let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("").split(':').next().unwrap_or("").to_ascii_lowercase();
    let path_q = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| "/".into());
    let (name, path) = match relay.opts.domain.as_deref().and_then(|d| host.strip_suffix(&format!(".{d}"))) {
        Some(sub) => (sub.to_string(), path_q.clone()),
        None => {
            let Some(rest) = path_q.strip_prefix("/t/") else {
                let n = relay.agents.read().unwrap().len();
                return (StatusCode::OK, format!("clipx relay. {n} box(es) online.\n")).into_response();
            };
            let cut = rest.find(['/', '?']).unwrap_or(rest.len());
            let (name, tail) = rest.split_at(cut);
            if tail.is_empty() || tail.starts_with('?') {
                return (StatusCode::PERMANENT_REDIRECT, [(header::LOCATION, format!("/t/{name}/{tail}"))]).into_response();
            }
            (name.to_string(), tail.to_string())
        }
    };
    let Some(agent) = relay.agents.read().unwrap().get(&name).cloned() else {
        return (StatusCode::BAD_GATEWAY, format!("box {name} is not connected\n")).into_response();
    };
    let (parts, body) = req.into_parts();
    let mut headers = header_pairs(&parts.headers);
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case("x-clipx-tunnel"));
    headers.push(("x-forwarded-host".into(), host.clone()));
    let id = agent.next.fetch_add(1, Ordering::Relaxed);
    let head = ReqHead { m: parts.method.to_string(), p: path, h: headers };
    let (head_tx, head_rx) = oneshot::channel();
    let (body_tx, body_rx) = mpsc::channel(STREAM_BUFFER);
    agent.streams.lock().unwrap().insert(id, RelayStream { head: Some(head_tx), body: body_tx });
    let guard = RelayBody { rx: body_rx, agent: agent.clone(), id, done: false };
    if agent.tx.send(WsMsg::Binary(encode(REQ_HEAD, id, &serde_json::to_vec(&head).unwrap_or_default()).into())).await.is_err() {
        return (StatusCode::BAD_GATEWAY, "box connection dropped\n").into_response();
    }
    let tx = agent.tx.clone();
    tokio::spawn(async move {
        let mut data = body.into_data_stream();
        while let Some(Ok(chunk)) = data.next().await {
            for part in chunk.chunks(MAX_FRAME_DATA) {
                if tx.send(WsMsg::Binary(encode(REQ_DATA, id, part).into())).await.is_err() {
                    return;
                }
            }
        }
        let _ = tx.send(WsMsg::Binary(encode(REQ_END, id, b"").into())).await;
    });
    let head = match tokio::time::timeout(Duration::from_secs(900), head_rx).await {
        Ok(Ok(h)) => h,
        _ => {
            drop(guard); // tells the box to drop the stream
            return (StatusCode::GATEWAY_TIMEOUT, "box did not answer\n").into_response();
        }
    };
    let mut resp = Response::new(Body::from_stream(guard));
    *resp.status_mut() = StatusCode::from_u16(head.s).unwrap_or(StatusCode::BAD_GATEWAY);
    apply_pairs(resp.headers_mut(), &head.h);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames() {
        let f = encode(RES_DATA, 7, b"hi");
        assert_eq!(decode(&f), Some((RES_DATA, 7, &b"hi"[..])));
        assert!(decode(b"abc").is_none());
    }

    #[test]
    fn urls() {
        assert_eq!(ws_url("https://r.example.com/", "box"), "wss://r.example.com/_clipx/connect?name=box");
        assert_eq!(ws_url("http://127.0.0.1:9", "a"), "ws://127.0.0.1:9/_clipx/connect?name=a");
        assert_eq!(
            find_trycloudflare("2026 INF |  https://foo-bar-baz.trycloudflare.com                    |"),
            Some("https://foo-bar-baz.trycloudflare.com".into())
        );
        assert!(valid_name("my-box-1") && !valid_name("Bad_Name") && !valid_name("-x"));
    }
}
