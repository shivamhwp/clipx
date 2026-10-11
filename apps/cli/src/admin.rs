//! Admin API and the embedded dashboard.

use crate::app::App;
use crate::config::{Strategy, TunnelMode};
use crate::oauth;
use crate::store::{AccountFile, Provider, from_cliproxy};
use crate::util::{ct_eq, now, rss_bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::Ordering;

const INDEX: &str = include_str!("../web/index.html");
const FAVICON: &str = include_str!("../web/favicon.svg");

pub fn router(app: Arc<App>) -> Router<Arc<App>> {
    let api = Router::new()
        .route("/state", get(state))
        .route("/usage", get(usage))
        .route("/requests", get(requests))
        .route("/login/start", post(login_start))
        .route("/login/complete", post(login_complete))
        .route("/login/status/{flow}", get(login_status))
        .route("/accounts/import", post(import))
        .route("/accounts/{id}", patch(update_account).delete(delete_account))
        .route("/accounts/{id}/refresh", post(refresh_account))
        .route("/keys", post(create_key))
        .route("/keys/{id}", axum::routing::delete(revoke_key))
        .route("/settings", patch(settings))
        .route("/connect", patch(connect))
        .route("/t3", get(t3_status).post(t3_set))
        .route("/shutdown", post(shutdown))
        .layer(axum::middleware::from_fn_with_state(app, require_admin));
    Router::new()
        .route("/", get(index))
        .route("/favicon.svg", get(favicon))
        .route("/healthz", get(|| async { "ok" }))
        .nest("/api", api)
}

async fn index() -> Response {
    ([(header::CACHE_CONTROL, "no-cache")], Html(INDEX)).into_response()
}

async fn favicon() -> Response {
    ([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "max-age=86400")], FAVICON).into_response()
}

async fn require_admin(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let token = app.cfg.read().unwrap().admin_token.clone();
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !ct_eq(presented.trim().as_bytes(), token.as_bytes()) {
        return err(StatusCode::UNAUTHORIZED, "admin token required");
    }
    next.run(req).await
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({"error": msg}))).into_response()
}

/// True when the request came in through a tunnel or proxy rather than from this machine.
fn tunnel_header(h: &HeaderMap) -> bool {
    ["x-clipx-tunnel", "cf-connecting-ip", "x-forwarded-for", "tailscale-funnel-request"].iter().any(|k| h.contains_key(*k))
}

async fn state(State(app): State<Arc<App>>, headers: HeaderMap) -> Json<Value> {
    let cfg = app.cfg.read().unwrap().clone();
    let tunnel = app.tunnel.lock().unwrap().clone();
    let keys: Vec<Value> = app
        .store
        .keys
        .read()
        .unwrap()
        .iter()
        .map(|k| json!({"id": k.id, "name": k.name, "prefix": k.prefix, "created_at": k.created_at, "last_used": k.last_used}))
        .collect();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "now": now(),
        "uptime": now() - app.started,
        "rss": rss_bytes(),
        "in_flight": app.stats.in_flight.load(Ordering::Relaxed),
        "total_requests": app.stats.total_requests.load(Ordering::Relaxed),
        "total_failures": app.stats.total_failures.load(Ordering::Relaxed),
        "listen": cfg.listen,
        "local_url": cfg.local_url(),
        "strategy": cfg.strategy,
        "retries": cfg.retries,
        "claude_version": app.claude_version(),
        "providers": crate::store::Provider::ALL,
        "accounts": app.store.list().iter().map(|a| a.summary()).collect::<Vec<_>>(),
        "keys": keys,
        "tunnel": tunnel,
        "connect": {"mode": cfg.connect.mode, "relay": cfg.connect.relay, "name": cfg.connect.name, "ts_port": cfg.connect.ts_port.unwrap_or(443), "tailnet_only": cfg.connect.tailnet_only},
        "via_tunnel": tunnel_header(&headers),
        "t3": {"enabled": cfg.t3, "installed": crate::t3::installed()},
        "recent": app.stats.recent(15),
    }))
}

#[derive(Deserialize)]
struct DaysQ {
    days: Option<usize>,
    limit: Option<usize>,
}

async fn usage(State(app): State<Arc<App>>, Query(q): Query<DaysQ>) -> Json<Value> {
    Json(app.stats.summary(q.days.unwrap_or(7).clamp(1, 90)))
}

async fn requests(State(app): State<Arc<App>>, Query(q): Query<DaysQ>) -> Json<Value> {
    Json(json!(app.stats.recent(q.limit.unwrap_or(100).min(200))))
}

#[derive(Deserialize)]
struct StartReq {
    provider: String,
}

async fn login_start(State(app): State<Arc<App>>, Json(r): Json<StartReq>) -> Response {
    let Some(provider) = Provider::parse(&r.provider) else {
        return err(StatusCode::BAD_REQUEST, "provider must be claude, codex, or gemini");
    };
    let f = oauth::start(&app, provider);
    Json(json!({"flow_id": f.id, "url": f.url, "hint": f.hint})).into_response()
}

#[derive(Deserialize)]
struct CompleteReq {
    flow_id: String,
    callback: String,
}

async fn login_complete(State(app): State<Arc<App>>, Json(r): Json<CompleteReq>) -> Response {
    match oauth::complete(&app, &r.flow_id, &r.callback).await {
        Ok(acc) => Json(json!({"account": acc.summary()})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e),
    }
}

async fn login_status(State(app): State<Arc<App>>, Path(flow): Path<String>) -> Json<Value> {
    Json(json!({"done": oauth::flow_done(&app, &flow)}))
}

/// Accepts a clipx account file, a CLIProxyAPI auth file, or an array of either.
async fn import(State(app): State<Arc<App>>, Json(v): Json<Value>) -> Response {
    let items = match v {
        Value::Array(a) => a,
        other => vec![other],
    };
    let mut added = Vec::new();
    for item in items {
        let no_refresh = item.get("no_refresh").and_then(Value::as_bool).unwrap_or(false);
        let file = serde_json::from_value::<AccountFile>(item.clone()).ok().or_else(|| from_cliproxy(&item));
        let Some(mut file) = file else {
            return err(StatusCode::BAD_REQUEST, "unrecognised account json (expected a clipx or CLIProxyAPI auth file)");
        };
        file.no_refresh |= no_refresh;
        if let Some(path) = item.get("linked").and_then(Value::as_str).filter(|p| !p.is_empty()) {
            file.linked = Some(path.to_string());
            file.no_refresh = true;
        }
        match app.store.upsert(file) {
            Ok(acc) => added.push(acc.summary()),
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    }
    Json(json!({"accounts": added})).into_response()
}

#[derive(Deserialize)]
struct AccountPatch {
    label: Option<String>,
    disabled: Option<bool>,
    priority: Option<i32>,
    no_refresh: Option<bool>,
    reset: Option<bool>,
}

async fn update_account(State(app): State<Arc<App>>, Path(id): Path<String>, Json(p): Json<AccountPatch>) -> Response {
    let Some(acc) = app.store.get(&id) else { return err(StatusCode::NOT_FOUND, "no such account") };
    let id = acc.id();
    if let Some(label) = p.label
        && let Err(e) = app.store.rename(&id, &label) {
            return err(StatusCode::BAD_REQUEST, &e);
        }
    {
        let mut f = acc.file.write().unwrap();
        if let Some(d) = p.disabled {
            f.disabled = d;
        }
        if let Some(pr) = p.priority {
            f.priority = pr;
        }
        if let Some(n) = p.no_refresh {
            f.no_refresh = n;
        }
    }
    if p.reset == Some(true) {
        let mut s = acc.state.lock().unwrap();
        s.cooldown_until = 0;
        s.last_error = None;
        s.needs_login = false;
    }
    if let Err(e) = app.store.save_account(&acc) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    Json(json!({"account": acc.summary()})).into_response()
}

async fn delete_account(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acc) = app.store.get(&id) else { return err(StatusCode::NOT_FOUND, "no such account") };
    app.store.remove(&acc.id());
    Json(json!({"ok": true})).into_response()
}

async fn refresh_account(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acc) = app.store.get(&id) else { return err(StatusCode::NOT_FOUND, "no such account") };
    let _g = acc.refresh_lock.lock().await;
    match oauth::refresh(&app, &acc).await {
        Ok(()) => Json(json!({"account": acc.summary()})).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, &e),
    }
}

#[derive(Deserialize)]
struct KeyReq {
    #[serde(default)]
    name: String,
}

async fn create_key(State(app): State<Arc<App>>, Json(r): Json<KeyReq>) -> Response {
    match app.store.create_key(&r.name) {
        Ok((k, plain)) => Json(json!({"id": k.id, "name": k.name, "key": plain})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn revoke_key(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    if app.store.revoke_key(&id) { Json(json!({"ok": true})).into_response() } else { err(StatusCode::NOT_FOUND, "no such key") }
}

#[derive(Deserialize)]
struct SettingsReq {
    strategy: Option<Strategy>,
    retries: Option<u32>,
}

async fn settings(State(app): State<Arc<App>>, Json(r): Json<SettingsReq>) -> Response {
    {
        let mut cfg = app.cfg.write().unwrap();
        if let Some(s) = r.strategy {
            cfg.strategy = s;
        }
        if let Some(n) = r.retries {
            cfg.retries = n.clamp(1, 10);
        }
    }
    match app.save_config() {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn t3_status(State(app): State<Arc<App>>) -> Json<Value> {
    Json(tokio::task::spawn_blocking(move || crate::t3::status(&app)).await.unwrap_or_default())
}

#[derive(Deserialize)]
struct T3Req {
    enabled: bool,
}

/// Turn the T3 Code providers on (added now, then kept in step) or off (taken out of T3).
async fn t3_set(State(app): State<Arc<App>>, Json(r): Json<T3Req>) -> Response {
    // Off first, so the background sync cannot add the providers back while they go.
    let was = std::mem::replace(&mut app.cfg.write().unwrap().t3, false);
    let a = app.clone();
    let res = tokio::task::spawn_blocking(move || {
        if r.enabled {
            if !crate::t3::installed() {
                return Err("T3 Code is not installed on this machine. Install it from t3.codes first.".to_string());
            }
            crate::t3::sync(&a).map(|_| ())
        } else {
            crate::t3::remove(&a)
        }
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    if let Err(e) = res {
        app.cfg.write().unwrap().t3 = was && r.enabled;
        return err(StatusCode::BAD_REQUEST, &e);
    }
    app.cfg.write().unwrap().t3 = r.enabled;
    if let Err(e) = app.save_config() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    Json(tokio::task::spawn_blocking(move || crate::t3::status(&app)).await.unwrap_or_default()).into_response()
}

async fn shutdown(State(app): State<Arc<App>>) -> Json<Value> {
    tracing::info!("shutdown requested through the admin api");
    app.shutdown.notify_one();
    Json(json!({"ok": true}))
}

#[derive(Deserialize)]
struct ConnectReq {
    mode: TunnelMode,
    relay: Option<String>,
    name: Option<String>,
    relay_secret: Option<String>,
    ts_port: Option<u16>,
    tailnet_only: Option<bool>,
}

async fn connect(State(app): State<Arc<App>>, headers: HeaderMap, Json(r): Json<ConnectReq>) -> Response {
    if tunnel_header(&headers) && r.mode == TunnelMode::Off {
        return err(StatusCode::BAD_REQUEST, "you are connected through the tunnel; turning it off from here would cut you off. use the cli on the box");
    }
    {
        let mut cfg = app.cfg.write().unwrap();
        if r.mode == TunnelMode::Relay {
            let Some(relay) = r.relay.filter(|s| !s.trim().is_empty()).or_else(|| cfg.connect.relay.clone()) else {
                return err(StatusCode::BAD_REQUEST, "relay url required");
            };
            cfg.connect.relay = Some(relay.trim().trim_end_matches('/').to_string());
        }
        if let Some(n) = r.name.filter(|s| !s.trim().is_empty()) {
            cfg.connect.name = Some(n.trim().to_ascii_lowercase());
        }
        if let Some(s) = r.relay_secret.filter(|s| !s.is_empty()) {
            cfg.connect.relay_secret = Some(s);
        }
        if let Some(p) = r.ts_port {
            cfg.connect.ts_port = Some(p);
        }
        if let Some(t) = r.tailnet_only {
            cfg.connect.tailnet_only = t;
        }
        cfg.connect.mode = r.mode;
    }
    if let Err(e) = app.save_config() {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    app.tunnel_ctl.send_modify(|g| *g += 1);
    Json(json!({"ok": true})).into_response()
}
