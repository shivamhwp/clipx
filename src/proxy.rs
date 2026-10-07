//! The API proxy: client auth, account rotation, upstream calls, streaming with usage taps.

use crate::app::{App, PickError, cooldown_secs, quota_from_headers};
use crate::cloak;
use crate::gemini;
use crate::oauth;
use crate::sse;
use crate::store::{Account, AccountFile, Provider};
use crate::translate;
use crate::usage::{RequestLog, Tokens};
use crate::util::now;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::Instant;

const MAX_BODY: usize = 64 << 20;
pub const CODEX_VERSION: &str = "0.160.1";

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Anthropic,
    OpenAI,
    Gemini,
}

fn error(style: Style, status: StatusCode, kind: &str, msg: &str) -> Response {
    let body = match style {
        Style::Anthropic => json!({"type": "error", "error": {"type": kind, "message": msg}}),
        Style::OpenAI => json!({"error": {"message": msg, "type": kind, "code": status.as_u16()}}),
        Style::Gemini => json!({"error": {"code": status.as_u16(), "message": msg, "status": kind}}),
    };
    (status, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

fn presented_key(h: &HeaderMap) -> Option<&str> {
    if let Some(v) = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        return Some(v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")).unwrap_or(v).trim());
    }
    ["x-api-key", "api-key", "x-goog-api-key"].iter().find_map(|k| h.get(*k).and_then(|v| v.to_str().ok()).map(str::trim))
}

/// Gemini-compatible clients may pass the key as `?key=` instead of a header.
fn key_from_query(query: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes()).find(|(k, _)| k == "key").map(|(_, v)| v.into_owned())
}

pub fn provider_for_model(model: &str) -> Option<Provider> {
    let m = model.to_ascii_lowercase();
    if m.starts_with("claude") || ["opus", "sonnet", "haiku", "fable"].iter().any(|k| m.contains(k)) {
        Some(Provider::Claude)
    } else if m.starts_with("gpt") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") || m.contains("codex") {
        Some(Provider::Codex)
    } else if m.starts_with("gemini") {
        Some(Provider::Gemini)
    } else {
        None
    }
}

/// `model@label` pins a request to one account when `label` names an account.
fn split_pin(app: &App, model: &str) -> (String, Option<String>) {
    if let Some((m, label)) = model.rsplit_once('@')
        && app.store.get(label).is_some() {
            return (m.to_string(), Some(label.to_string()));
        }
    (model.to_string(), None)
}

struct Ctx {
    app: Arc<App>,
    key: String,
    pin: Option<String>,
    headers: HeaderMap,
    query: Option<String>,
    path: String,
    start: Instant,
}

pub async fn entry(State(app): State<Arc<App>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let mut path = parts.uri.path().to_string();
    let mut pin = None;
    if let Some(rest) = path.strip_prefix("/a/") {
        let (name, tail) = rest.split_once('/').unwrap_or((rest, ""));
        pin = Some(url::form_urlencoded::parse(name.as_bytes()).map(|(k, _)| k.into_owned()).next().unwrap_or_default());
        path = format!("/{tail}");
    }
    let trimmed = path.trim_end_matches('/');
    let style = if trimmed.contains("/messages") {
        Style::Anthropic
    } else if trimmed.starts_with("/v1beta/") {
        Style::Gemini
    } else {
        Style::OpenAI
    };
    let presented = presented_key(&parts.headers).map(str::to_string).or_else(|| parts.uri.query().and_then(key_from_query));
    let Some(key) = presented.and_then(|k| app.store.check_key(&k)) else {
        let kind = if style == Style::Gemini { "UNAUTHENTICATED" } else { "authentication_error" };
        return error(style, StatusCode::UNAUTHORIZED, kind, "missing or invalid clipx api key");
    };
    if trimmed == "/v1/models" || trimmed == "/models" {
        return models(&app, pin.as_deref()).await;
    }
    if parts.method != Method::POST {
        return error(style, StatusCode::METHOD_NOT_ALLOWED, "invalid_request_error", "use POST");
    }
    let body = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(e) => return error(style, StatusCode::PAYLOAD_TOO_LARGE, "invalid_request_error", &e.to_string()),
    };
    app.stats.in_flight.fetch_add(1, Ordering::Relaxed);
    let _guard = InFlight(app.clone());
    let ctx = Ctx { app, key, pin, headers: parts.headers, query: parts.uri.query().map(String::from), path: trimmed.to_string(), start: Instant::now() };
    match trimmed {
        "/v1/messages" => messages(ctx, body, false).await,
        "/v1/messages/count_tokens" => messages(ctx, body, true).await,
        "/v1/chat/completions" | "/chat/completions" => chat(ctx, body).await,
        p if p == "/v1/responses" || p == "/responses" => responses(ctx, body, "").await,
        p if p.starts_with("/v1/responses/") || p.starts_with("/responses/") => {
            let rest = p.split_once("/responses").map(|(_, r)| r.to_string()).unwrap_or_default();
            responses(ctx, body, &rest).await
        }
        p if p.starts_with("/v1beta/models/") => gemini_native(ctx, body, p.trim_start_matches("/v1beta/")).await,
        _ => error(style, StatusCode::NOT_FOUND, "not_found_error", &format!("clipx does not serve {trimmed}")),
    }
}

struct InFlight(Arc<App>);
impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(serde::Deserialize, Default)]
struct Peek {
    #[serde(default)]
    model: String,
    #[serde(default)]
    stream: bool,
}

// ------------------------------------------------------------------ claude

const STRIP_REQ: &[&str] = &[
    "host", "authorization", "x-api-key", "api-key", "content-length", "connection", "keep-alive", "transfer-encoding", "te",
    "upgrade", "proxy-authorization", "proxy-connection", "accept-encoding", "cookie", "forwarded", "x-forwarded-for",
    "x-forwarded-host", "x-forwarded-proto", "x-real-ip", "origin", "referer", "x-clipx-tunnel",
];

fn claude_headers(client: &HeaderMap, native: bool, f: &AccountFile, version: &str, session: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    let mut betas: Vec<String> = Vec::new();
    if native {
        for (k, v) in client {
            let name = k.as_str();
            if STRIP_REQ.contains(&name) || name.starts_with("cf-") {
                continue;
            }
            if name == "anthropic-beta" {
                betas.extend(v.to_str().unwrap_or("").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
                continue;
            }
            h.append(k.clone(), v.clone());
        }
        if !betas.iter().any(|b| b == "oauth-2025-04-20") {
            betas.push("oauth-2025-04-20".into());
        }
    } else {
        betas.extend(cloak::BASE_BETAS.iter().map(|s| s.to_string()));
        for v in client.get_all("anthropic-beta") {
            for b in v.to_str().unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if !betas.iter().any(|x| x == b) {
                    betas.push(b.to_string());
                }
            }
        }
        let set = |h: &mut HeaderMap, k: &'static str, v: &str| {
            if let Ok(v) = HeaderValue::from_str(v) {
                h.insert(k, v);
            }
        };
        set(&mut h, "user-agent", &format!("claude-cli/{version} (external, cli)"));
        set(&mut h, "x-app", "cli");
        set(&mut h, "anthropic-dangerous-direct-browser-access", "true");
        set(&mut h, "x-stainless-lang", "js");
        set(&mut h, "x-stainless-runtime", "node");
        set(&mut h, "x-stainless-retry-count", "0");
        set(&mut h, "x-stainless-timeout", "600");
        set(&mut h, "x-claude-code-session-id", session);
        set(&mut h, "accept", "application/json");
    }
    if !h.contains_key("anthropic-version") {
        h.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }
    if let Ok(v) = HeaderValue::from_str(&betas.join(",")) {
        h.insert("anthropic-beta", v);
    }
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", f.access_token)) {
        h.insert(header::AUTHORIZATION, v);
    }
    h
}

enum ClaudeBody {
    Raw(Bytes),
    Cloak(Value),
}

impl ClaudeBody {
    fn render(&self, f: &AccountFile, version: &str, session: &str) -> Bytes {
        match self {
            ClaudeBody::Raw(b) => b.clone(),
            ClaudeBody::Cloak(v) => {
                let mut v = v.clone();
                Bytes::from(cloak::cloak_body(&mut v, version, &f.device_id, f.account_uuid.as_deref().unwrap_or(""), session))
            }
        }
    }
}

async fn messages(ctx: Ctx, body: Bytes, count: bool) -> Response {
    let ua = ctx.headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    ctx.app.observe_claude_version(&ua);
    let native = cloak::is_native(&ua, &body);
    let Ok(peek) = serde_json::from_slice::<Peek>(&body) else {
        return error(Style::Anthropic, StatusCode::BAD_REQUEST, "invalid_request_error", "body is not valid JSON");
    };
    let (model, model_pin) = split_pin(&ctx.app, &peek.model);
    if provider_for_model(&model) == Some(Provider::Codex) {
        return error(
            Style::Anthropic,
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!("{model} is a Codex model; call it through /v1/responses or /v1/chat/completions"),
        );
    }
    let plan = if native && model_pin.is_none() {
        ClaudeBody::Raw(body)
    } else {
        let mut v: Value = serde_json::from_slice(&body).unwrap_or_default();
        v["model"] = json!(model);
        if native { ClaudeBody::Raw(Bytes::from(serde_json::to_vec(&v).unwrap_or_default())) } else { ClaudeBody::Cloak(v) }
    };
    let pin = ctx.pin.clone().or(model_pin);
    let base = ctx.app.cfg.read().unwrap().upstream.claude_api.clone();
    let url = format!(
        "{base}/v1/messages{}?{}",
        if count { "/count_tokens" } else { "" },
        ctx.query.clone().unwrap_or_else(|| "beta=true".into())
    );
    let version = ctx.app.claude_version();
    let session = crate::util::uuid_v4();
    let app = ctx.app.clone();
    let client_headers = ctx.headers.clone();
    let sent = send(&ctx, Provider::Claude, pin.as_deref(), Style::Anthropic, &model, |f| {
        let h = claude_headers(&client_headers, native, f, &version, &session);
        app.http.post(&url).headers(h).body(plan.render(f, &version, &session))
    })
    .await;
    match sent {
        Ok((acc, resp)) => passthrough(Recorder::new(&ctx, Provider::Claude, acc, &model), resp, peek.stream),
        Err(r) => r,
    }
}

// ------------------------------------------------------------------ codex

fn codex_headers(client: &HeaderMap, f: &AccountFile, accept_sse: bool) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in client {
        let n = k.as_str();
        let keep = matches!(n, "version" | "originator" | "session_id" | "session-id" | "thread-id" | "conversation_id" | "openai-beta")
            || n.starts_with("x-codex-")
            || n.starts_with("x-openai-")
            || n == "x-client-request-id"
            || (n == "user-agent" && v.to_str().is_ok_and(|s| s.starts_with("codex")));
        if keep {
            h.append(k.clone(), v.clone());
        }
    }
    if !h.contains_key("originator") {
        h.insert("originator", HeaderValue::from_static("codex_cli_rs"));
    }
    if !h.contains_key(header::USER_AGENT) {
        let ua = format!("codex_cli_rs/{CODEX_VERSION} ({} ; {}) clipx", std::env::consts::OS, std::env::consts::ARCH);
        h.insert(header::USER_AGENT, HeaderValue::from_str(&ua).unwrap_or(HeaderValue::from_static("codex_cli_rs")));
    }
    if let Some(id) = f.account_id.as_deref().and_then(|a| HeaderValue::from_str(a).ok()) {
        h.insert("chatgpt-account-id", id);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", f.access_token)) {
        h.insert(header::AUTHORIZATION, v);
    }
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(header::ACCEPT, HeaderValue::from_static(if accept_sse { "text/event-stream" } else { "application/json" }));
    h
}

async fn responses(ctx: Ctx, body: Bytes, rest: &str) -> Response {
    let Ok(mut v) = serde_json::from_slice::<Value>(&body) else {
        return error(Style::OpenAI, StatusCode::BAD_REQUEST, "invalid_request_error", "body is not valid JSON");
    };
    let (model, model_pin) = split_pin(&ctx.app, v["model"].as_str().unwrap_or(""));
    if provider_for_model(&model) == Some(Provider::Claude) {
        return error(Style::OpenAI, StatusCode::BAD_REQUEST, "invalid_request_error", &format!("{model} is a Claude model; call it through /v1/messages or /v1/chat/completions"));
    }
    v["model"] = json!(model);
    let client_stream = v["stream"] == true;
    let main = rest.is_empty();
    if let Some(o) = v.as_object_mut() {
        if main {
            o.insert("stream".into(), json!(true));
            o.insert("store".into(), json!(false));
            o.entry("instructions").or_insert(json!(""));
            // The ChatGPT backend only accepts a list; the public API also takes a string.
            if let Some(text) = o.get("input").and_then(Value::as_str).map(String::from) {
                o.insert("input".into(), json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}]));
            }
        }
        for k in ["max_output_tokens", "max_tokens", "temperature", "top_p", "previous_response_id", "prompt_cache_retention", "safety_identifier", "stream_options", "user"] {
            o.remove(k);
        }
    }
    let payload = Bytes::from(serde_json::to_vec(&v).unwrap_or_default());
    let pin = ctx.pin.clone().or(model_pin);
    let url = format!("{}/responses{rest}", ctx.app.cfg.read().unwrap().upstream.codex_api);
    let app = ctx.app.clone();
    let client_headers = ctx.headers.clone();
    let sent = send(&ctx, Provider::Codex, pin.as_deref(), Style::OpenAI, &model, |f| {
        app.http.post(&url).headers(codex_headers(&client_headers, f, main)).body(payload.clone())
    })
    .await;
    let (acc, resp) = match sent {
        Ok(x) => x,
        Err(r) => return r,
    };
    let rec = Recorder::new(&ctx, Provider::Codex, acc, &model);
    if main && !client_stream {
        return match collect_codex(resp, rec).await {
            Ok(r) => json_response(StatusCode::OK, &r),
            Err(e) => error(Style::OpenAI, StatusCode::BAD_GATEWAY, "upstream_error", &e),
        };
    }
    passthrough(rec, resp, main && client_stream)
}

/// Read a Codex SSE stream to the end and return the final `response` object.
async fn collect_codex(resp: reqwest::Response, rec: Recorder) -> Result<Value, String> {
    let mut parser = sse::Parser::default();
    let mut events = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    let mut tokens = Tokens::default();
    let mut final_resp = None;
    let mut failure = None;
    let mut stream = Box::pin(resp.bytes_stream());
    use futures_util::StreamExt;
    let mut handle = |events: &mut Vec<sse::Event>| {
        for ev in events.drain(..) {
            absorb_usage(&mut tokens, &ev.data);
            let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { continue };
            match v["type"].as_str().unwrap_or("") {
                "response.output_item.done" => items.push(v["item"].clone()),
                "response.completed" | "response.incomplete" | "response.done" => final_resp = Some(v["response"].clone()),
                "response.failed" | "error" => failure = Some(v.to_string()),
                _ => {}
            }
        }
    };
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(b) => parser.push(&b, &mut events),
            Err(e) => {
                rec.finish(tokens, Some(e.to_string()));
                return Err(e.to_string());
            }
        }
        handle(&mut events);
    }
    parser.finish(&mut events);
    handle(&mut events);
    match final_resp {
        Some(mut r) => {
            if r["output"].as_array().is_none_or(|o| o.is_empty()) && !items.is_empty() {
                r["output"] = Value::Array(items);
            }
            rec.finish(tokens, None);
            Ok(r)
        }
        None => {
            let e = failure.unwrap_or_else(|| "upstream stream ended without a response".into());
            rec.finish(tokens, Some(e.clone()));
            Err(e)
        }
    }
}

// ------------------------------------------------------------------ chat completions

async fn chat(ctx: Ctx, body: Bytes) -> Response {
    let Ok(mut req) = serde_json::from_slice::<Value>(&body) else {
        return error(Style::OpenAI, StatusCode::BAD_REQUEST, "invalid_request_error", "body is not valid JSON");
    };
    let (model, model_pin) = split_pin(&ctx.app, req["model"].as_str().unwrap_or(""));
    req["model"] = json!(model);
    let Some(provider) = provider_for_model(&model) else {
        return error(Style::OpenAI, StatusCode::BAD_REQUEST, "invalid_request_error", &format!("unknown model {model:?}; use a claude-* or gpt-* model"));
    };
    let stream = req["stream"] == true;
    let include_usage = req["stream_options"]["include_usage"] == true;
    let pin = ctx.pin.clone().or(model_pin);
    let app = ctx.app.clone();
    let client_headers = ctx.headers.clone();
    match provider {
        Provider::Claude => {
            let plan = ClaudeBody::Cloak(translate::chat_to_claude(&req));
            let url = format!("{}/v1/messages?beta=true", app.cfg.read().unwrap().upstream.claude_api);
            let version = app.claude_version();
            let session = crate::util::uuid_v4();
            let sent = send(&ctx, provider, pin.as_deref(), Style::OpenAI, &model, |f| {
                app.http.post(&url).headers(claude_headers(&client_headers, false, f, &version, &session)).body(plan.render(f, &version, &session))
            })
            .await;
            let (acc, resp) = match sent {
                Ok(x) => x,
                Err(r) => return r,
            };
            let rec = Recorder::new(&ctx, provider, acc, &model);
            if stream {
                return translated(rec, resp, Translator::Claude(translate::ClaudeToChatStream::new(&model, include_usage)));
            }
            match resp.bytes().await {
                Ok(b) => {
                    let v: Value = serde_json::from_slice(&b).unwrap_or_default();
                    let mut t = Tokens::default();
                    t.absorb(&v["usage"]);
                    rec.finish(t, None);
                    json_response(StatusCode::OK, &translate::claude_to_chat(&v, &model))
                }
                Err(e) => {
                    rec.finish(Tokens::default(), Some(e.to_string()));
                    error(Style::OpenAI, StatusCode::BAD_GATEWAY, "upstream_error", &e.to_string())
                }
            }
        }
        Provider::Codex => {
            let payload = Bytes::from(translate::chat_to_codex(&req).to_string());
            let url = format!("{}/responses", app.cfg.read().unwrap().upstream.codex_api);
            let sent = send(&ctx, provider, pin.as_deref(), Style::OpenAI, &model, |f| {
                app.http.post(&url).headers(codex_headers(&client_headers, f, true)).body(payload.clone())
            })
            .await;
            let (acc, resp) = match sent {
                Ok(x) => x,
                Err(r) => return r,
            };
            let rec = Recorder::new(&ctx, provider, acc, &model);
            if stream {
                return translated(rec, resp, Translator::Codex(translate::CodexToChatStream::new(&model, include_usage)));
            }
            match collect_codex(resp, rec).await {
                Ok(r) => json_response(StatusCode::OK, &translate::codex_to_chat(&r, &model)),
                Err(e) => error(Style::OpenAI, StatusCode::BAD_GATEWAY, "upstream_error", &e),
            }
        }
        Provider::Gemini => {
            let inner = gemini::chat_to_gemini(&req);
            let user_prompt_id = crate::util::uuid_v4();
            let method = if stream { "streamGenerateContent" } else { "generateContent" };
            let url = format!("{}/v1internal:{method}{}", app.cfg.read().unwrap().upstream.gemini_api, if stream { "?alt=sse" } else { "" });
            let sent = send(&ctx, provider, pin.as_deref(), Style::OpenAI, &model, |f| {
                let payload = gemini::wrap_envelope(&model, f.project_id.as_deref(), &user_prompt_id, inner.clone());
                app.http.post(&url).bearer_auth(&f.access_token).header(header::CONTENT_TYPE, "application/json").body(payload.to_string())
            })
            .await;
            let (acc, resp) = match sent {
                Ok(x) => x,
                Err(r) => return r,
            };
            let rec = Recorder::new(&ctx, provider, acc, &model);
            if stream {
                return gemini::chat_stream_response(rec, resp, &model, include_usage);
            }
            match resp.bytes().await {
                Ok(b) => {
                    let v: Value = serde_json::from_slice(&b).unwrap_or_default();
                    let inner = gemini::unwrap_envelope(&v);
                    let mut t = Tokens::default();
                    gemini::absorb_usage(&mut t, &inner["usageMetadata"]);
                    rec.finish(t, None);
                    json_response(StatusCode::OK, &gemini::gemini_to_chat(&inner, &model))
                }
                Err(e) => {
                    rec.finish(Tokens::default(), Some(e.to_string()));
                    error(Style::OpenAI, StatusCode::BAD_GATEWAY, "upstream_error", &e.to_string())
                }
            }
        }
    }
}

// ------------------------------------------------------------------ native gemini

/// `rest` is e.g. "models/gemini-2.5-pro:generateContent", matching the public Gemini API shape.
async fn gemini_native(ctx: Ctx, body: Bytes, rest: &str) -> Response {
    let Some(model) = rest.strip_prefix("models/") else {
        return error(Style::Gemini, StatusCode::NOT_FOUND, "NOT_FOUND", "expected models/{model}:{method}");
    };
    let Some((model, method)) = model.rsplit_once(':') else {
        return error(Style::Gemini, StatusCode::NOT_FOUND, "NOT_FOUND", "expected models/{model}:{method}");
    };
    let Ok(native_body) = serde_json::from_slice::<Value>(&body) else {
        return error(Style::Gemini, StatusCode::BAD_REQUEST, "INVALID_ARGUMENT", "body is not valid JSON");
    };
    let (model, model_pin) = split_pin(&ctx.app, model);
    let pin = ctx.pin.clone().or(model_pin);
    let app = ctx.app.clone();
    match method {
        "countTokens" => {
            let payload = gemini::count_tokens_envelope(&model, &native_body);
            let url = format!("{}/v1internal:countTokens", app.cfg.read().unwrap().upstream.gemini_api);
            let sent = send(&ctx, Provider::Gemini, pin.as_deref(), Style::Gemini, &model, |f| {
                app.http.post(&url).bearer_auth(&f.access_token).header(header::CONTENT_TYPE, "application/json").body(payload.to_string())
            })
            .await;
            let (acc, resp) = match sent {
                Ok(x) => x,
                Err(r) => return r,
            };
            let rec = Recorder::new(&ctx, Provider::Gemini, acc, &model);
            match resp.bytes().await {
                Ok(b) => {
                    let v: Value = serde_json::from_slice(&b).unwrap_or_default();
                    rec.finish(Tokens::default(), None);
                    json_response(StatusCode::OK, &gemini::count_tokens_response(&v))
                }
                Err(e) => {
                    rec.finish(Tokens::default(), Some(e.to_string()));
                    error(Style::Gemini, StatusCode::BAD_GATEWAY, "INTERNAL", &e.to_string())
                }
            }
        }
        "generateContent" | "streamGenerateContent" => {
            let stream = method == "streamGenerateContent";
            let user_prompt_id = crate::util::uuid_v4();
            let url = format!("{}/v1internal:{method}{}", app.cfg.read().unwrap().upstream.gemini_api, if stream { "?alt=sse" } else { "" });
            let sent = send(&ctx, Provider::Gemini, pin.as_deref(), Style::Gemini, &model, |f| {
                let payload = gemini::wrap_envelope(&model, f.project_id.as_deref(), &user_prompt_id, native_body.clone());
                app.http.post(&url).bearer_auth(&f.access_token).header(header::CONTENT_TYPE, "application/json").body(payload.to_string())
            })
            .await;
            let (acc, resp) = match sent {
                Ok(x) => x,
                Err(r) => return r,
            };
            let rec = Recorder::new(&ctx, Provider::Gemini, acc, &model);
            if stream {
                gemini::native_stream_response(rec, resp)
            } else {
                match resp.bytes().await {
                    Ok(b) => {
                        let v: Value = serde_json::from_slice(&b).unwrap_or_default();
                        let inner = gemini::unwrap_envelope(&v);
                        let mut t = Tokens::default();
                        gemini::absorb_usage(&mut t, &inner["usageMetadata"]);
                        rec.finish(t, None);
                        json_response(StatusCode::OK, &inner)
                    }
                    Err(e) => {
                        rec.finish(Tokens::default(), Some(e.to_string()));
                        error(Style::Gemini, StatusCode::BAD_GATEWAY, "INTERNAL", &e.to_string())
                    }
                }
            }
        }
        _ => error(Style::Gemini, StatusCode::NOT_FOUND, "NOT_FOUND", &format!("unknown method {method}")),
    }
}

// ------------------------------------------------------------------ upstream send with rotation

/// Send to an upstream account, rotating through accounts on rate limits, auth failures and
/// server errors. Client errors (400, 404, 413, …) are returned to the caller unchanged.
async fn send<F>(ctx: &Ctx, provider: Provider, pin: Option<&str>, style: Style, model: &str, build: F) -> Result<(Arc<Account>, reqwest::Response), Response>
where
    F: Fn(&AccountFile) -> reqwest::RequestBuilder,
{
    let app = &ctx.app;
    let max = app.cfg.read().unwrap().retries.max(1) as usize;
    let mut tried: Vec<String> = Vec::new();
    let mut last: Option<(StatusCode, HeaderMap, Bytes)> = None;
    let mut last_net: Option<String> = None;
    let mut refreshed: Vec<String> = Vec::new();
    let mut attempts = 0;
    while attempts < max {
        let acc = match app.pick(provider, pin, &tried) {
            Ok(a) => a,
            Err(e) => {
                if let Some((s, h, b)) = last {
                    return Err(upstream_error_response(s, &h, b));
                }
                if let Some(e) = last_net {
                    return Err(error(style, StatusCode::BAD_GATEWAY, "api_error", &format!("upstream unreachable: {e}")));
                }
                let status = match e {
                    PickError::Exhausted(_) | PickError::Unavailable(..) => StatusCode::TOO_MANY_REQUESTS,
                    PickError::NoAccounts(_) => StatusCode::SERVICE_UNAVAILABLE,
                    _ => StatusCode::BAD_REQUEST,
                };
                let mut r = error(style, status, if status == StatusCode::TOO_MANY_REQUESTS { "rate_limit_error" } else { "api_error" }, &e.to_string());
                if let Some(at) = app.next_available(provider)
                    && let Ok(v) = HeaderValue::from_str(&(at.saturating_sub(now())).max(1).to_string()) {
                        r.headers_mut().insert(header::RETRY_AFTER, v);
                    }
                return Err(r);
            }
        };
        attempts += 1;
        let id = acc.id();
        if let Err(e) = oauth::ensure_fresh(app, &acc, 60).await
            && acc.state.lock().unwrap().needs_login {
                tracing::warn!("{} needs login: {e}", acc.label());
                tried.push(id);
                continue;
            }
        let file = acc.file.read().unwrap().clone();
        let result = build(&file).send().await;
        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!("{} {model}: network error: {msg}", file.label);
                note_failure(&acc, &msg, 0);
                last_net = Some(msg);
                tried.push(id);
                continue;
            }
        };
        let status = resp.status();
        let quota = quota_from_headers(provider, resp.headers());
        if !quota.is_empty() {
            let mut s = acc.state.lock().unwrap();
            s.quota = quota;
            s.quota_at = now();
        }
        if status.is_success() {
            let mut s = acc.state.lock().unwrap();
            s.last_used = now();
            s.last_error = None;
            drop(s);
            return Ok((acc, resp));
        }
        let headers = resp.headers().clone();
        let body = resp.bytes().await.unwrap_or_default();
        let snippet = String::from_utf8_lossy(&body[..body.len().min(300)]).to_string();
        tracing::warn!("{} {model}: upstream {status}: {snippet}", file.label);
        match status.as_u16() {
            401 | 403 => {
                // Try one forced refresh on the same account before moving on.
                if !refreshed.contains(&id) && !file.no_refresh && file.refresh_token.is_some() {
                    refreshed.push(id.clone());
                    let _guard = acc.refresh_lock.lock().await;
                    if acc.file.read().unwrap().access_token == file.access_token && oauth::refresh(app, &acc).await.is_ok() {
                        attempts -= 1;
                        continue;
                    }
                }
                note_failure(&acc, &format!("{status}: {snippet}"), if status == 403 { 300 } else { 0 });
                if status == 401 {
                    acc.state.lock().unwrap().needs_login = true;
                }
            }
            429 => {
                let secs = cooldown_secs(provider, &headers, &body);
                note_failure(&acc, &format!("rate limited for {secs}s: {snippet}"), secs);
            }
            408 | 500 | 502 | 503 | 504 | 520..=599 => note_failure(&acc, &format!("{status}: {snippet}"), 0),
            _ => {
                acc.state.lock().unwrap().failures += 1;
                Recorder::new(ctx, provider, acc.clone(), model).finish(Tokens::default(), Some(format!("{status}: {snippet}")));
                return Err(upstream_error_response(status, &headers, body));
            }
        }
        last = Some((status, headers, body));
        tried.push(id);
    }
    match last {
        Some((s, h, b)) => Err(upstream_error_response(s, &h, b)),
        None => Err(error(style, StatusCode::BAD_GATEWAY, "api_error", &format!("upstream unreachable: {}", last_net.unwrap_or_default()))),
    }
}

fn note_failure(acc: &Account, msg: &str, cooldown: u64) {
    let mut s = acc.state.lock().unwrap();
    s.failures += 1;
    s.last_error = Some(msg.chars().take(400).collect());
    if cooldown > 0 {
        s.cooldown_until = now() + cooldown;
    }
}

const STRIP_RESP: &[&str] = &["connection", "keep-alive", "transfer-encoding", "content-length", "content-encoding", "set-cookie", "alt-svc", "server", "via", "strict-transport-security"];

fn copy_response_headers(src: &HeaderMap, dst: &mut HeaderMap) {
    for (k, v) in src {
        let n = k.as_str();
        if STRIP_RESP.contains(&n) || n.starts_with("cf-") || n.starts_with("x-envoy") {
            continue;
        }
        dst.append(k.clone(), v.clone());
    }
}

fn upstream_error_response(status: StatusCode, headers: &HeaderMap, body: Bytes) -> Response {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = status;
    copy_response_headers(headers, r.headers_mut());
    r
}

fn json_response(status: StatusCode, v: &Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

// ------------------------------------------------------------------ recording + streaming

pub(crate) struct Recorder {
    app: Arc<App>,
    acc: Arc<Account>,
    key: String,
    provider: Provider,
    model: String,
    path: String,
    start: Instant,
    status: u16,
}

impl Recorder {
    fn new(ctx: &Ctx, provider: Provider, acc: Arc<Account>, model: &str) -> Self {
        Self { app: ctx.app.clone(), acc, key: ctx.key.clone(), provider, model: model.to_string(), path: ctx.path.clone(), start: ctx.start, status: 200 }
    }

    pub(crate) fn finish(&self, t: Tokens, error: Option<String>) {
        {
            let mut s = self.acc.state.lock().unwrap();
            s.requests += 1;
            s.input_tokens += t.input;
            s.output_tokens += t.output;
            if error.is_some() {
                s.failures += 1;
            }
        }
        self.app.stats.record(RequestLog {
            ts: now(),
            key: self.key.clone(),
            provider: self.provider.as_str().to_string(),
            account: self.acc.label(),
            model: self.model.clone(),
            path: self.path.clone(),
            status: self.status,
            ms: self.start.elapsed().as_millis() as u64,
            input: t.input,
            output: t.output,
            cache_read: t.cache_read,
            error,
        });
    }
}

fn absorb_usage(t: &mut Tokens, data: &str) {
    if !data.contains("\"usage\"") {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else { return };
    for p in ["/usage", "/message/usage", "/response/usage"] {
        if let Some(u) = v.pointer(p).filter(|u| u.is_object()) {
            t.absorb(u);
        }
    }
}

type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// Passes upstream bytes through untouched while reading token usage on the side.
struct Tap {
    inner: ByteStream,
    /// None until known: some upstreams (the ChatGPT backend) send streams without a content-type.
    sse: Option<bool>,
    parser: sse::Parser,
    events: Vec<sse::Event>,
    buf: Vec<u8>,
    tokens: Tokens,
    rec: Option<Recorder>,
    /// The final event went through; a hang-up after it is not a failure.
    complete: bool,
}

fn is_final_event(ev: &sse::Event) -> bool {
    matches!(ev.event.as_deref(), Some("message_stop" | "response.completed" | "response.incomplete" | "response.done")) || ev.data == "[DONE]"
}

impl Tap {
    fn feed(&mut self, b: &[u8]) {
        if self.sse.is_none() {
            let start = b.iter().position(|c| !c.is_ascii_whitespace());
            match start {
                Some(i) => self.sse = Some(b[i..].starts_with(b"event:") || b[i..].starts_with(b"data:") || b[i..].starts_with(b":")),
                None => return,
            }
        }
        if self.sse == Some(true) {
            self.parser.push(b, &mut self.events);
            for ev in self.events.drain(..) {
                self.complete |= is_final_event(&ev);
                absorb_usage(&mut self.tokens, &ev.data);
            }
        } else if self.buf.len() < 8 << 20 {
            self.buf.extend_from_slice(b);
        }
    }
    fn done(&mut self, error: Option<String>) {
        if let Some(rec) = self.rec.take() {
            if self.sse == Some(true) {
                // A client may hang up right after the final event, before its blank line.
                self.parser.finish(&mut self.events);
                for ev in self.events.drain(..) {
                    self.complete |= is_final_event(&ev);
                    absorb_usage(&mut self.tokens, &ev.data);
                }
            } else {
                if let Ok(v) = serde_json::from_slice::<Value>(&self.buf)
                    && v["usage"].is_object() {
                        self.tokens.absorb(&v["usage"]);
                    }
            }
            rec.finish(self.tokens, if self.complete { None } else { error });
        }
    }
}

impl Stream for Tap {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(b))) => {
                self.feed(&b);
                Poll::Ready(Some(Ok(b)))
            }
            Poll::Ready(Some(Err(e))) => {
                self.done(Some(e.to_string()));
                Poll::Ready(Some(Err(std::io::Error::other(e))))
            }
            Poll::Ready(None) => {
                self.done(None);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.done(Some("client disconnected".into()));
    }
}

/// `stream_hint`: the request asked for a stream, so an untyped body is treated as SSE.
fn passthrough(mut rec: Recorder, resp: reqwest::Response, stream_hint: bool) -> Response {
    let status = resp.status();
    rec.status = status.as_u16();
    let ctype = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(|c| c.contains("event-stream"));
    let mut headers = HeaderMap::new();
    copy_response_headers(resp.headers(), &mut headers);
    if ctype.is_none() && stream_hint && status.is_success() {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    }
    let sse = ctype.or((stream_hint && status.is_success()).then_some(true));
    if sse == Some(true) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        headers.insert(HeaderName::from_static("x-accel-buffering"), HeaderValue::from_static("no"));
    }
    let tap = Tap {
        inner: Box::pin(resp.bytes_stream()),
        sse,
        parser: sse::Parser::default(),
        events: Vec::new(),
        buf: Vec::new(),
        tokens: Tokens::default(),
        rec: Some(rec),
        complete: false,
    };
    let mut r = Response::new(Body::from_stream(tap));
    *r.status_mut() = status;
    *r.headers_mut() = headers;
    r
}

enum Translator {
    Claude(translate::ClaudeToChatStream),
    Codex(translate::CodexToChatStream),
}

impl Translator {
    fn on_event(&mut self, ev: &sse::Event, out: &mut String) {
        match self {
            Translator::Claude(t) => t.on_event(ev, out),
            Translator::Codex(t) => t.on_event(ev, out),
        }
    }
    fn finish(&mut self, out: &mut String) {
        match self {
            Translator::Claude(t) => t.finish(out),
            Translator::Codex(t) => t.finish(out),
        }
    }
}

/// Converts an upstream SSE stream into chat.completion.chunk frames.
struct Translate {
    inner: ByteStream,
    parser: sse::Parser,
    events: Vec<sse::Event>,
    t: Translator,
    tokens: Tokens,
    rec: Option<Recorder>,
    ended: bool,
}

impl Translate {
    fn done(&mut self, error: Option<String>) {
        if let Some(rec) = self.rec.take() {
            rec.finish(self.tokens, error);
        }
    }
    fn drain(&mut self, out: &mut String) {
        for ev in std::mem::take(&mut self.events) {
            absorb_usage(&mut self.tokens, &ev.data);
            self.t.on_event(&ev, out);
        }
    }
}

impl Stream for Translate {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.ended {
                return Poll::Ready(None);
            }
            let mut out = String::new();
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(b))) => {
                    let this = &mut *self;
                    this.parser.push(&b, &mut this.events);
                    this.drain(&mut out);
                    if !out.is_empty() {
                        return Poll::Ready(Some(Ok(Bytes::from(out))));
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    self.ended = true;
                    let msg = e.to_string();
                    out.push_str(&sse::frame(None, &json!({"error": {"message": msg, "type": "upstream_error"}}).to_string()));
                    out.push_str("data: [DONE]\n\n");
                    self.done(Some(msg));
                    return Poll::Ready(Some(Ok(Bytes::from(out))));
                }
                Poll::Ready(None) => {
                    self.ended = true;
                    let this = &mut *self;
                    this.parser.finish(&mut this.events);
                    this.drain(&mut out);
                    this.t.finish(&mut out);
                    this.done(None);
                    return Poll::Ready(if out.is_empty() { None } else { Some(Ok(Bytes::from(out))) });
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Drop for Translate {
    fn drop(&mut self) {
        self.done(Some("client disconnected".into()));
    }
}

fn translated(rec: Recorder, resp: reqwest::Response, t: Translator) -> Response {
    let s = Translate {
        inner: Box::pin(resp.bytes_stream()),
        parser: sse::Parser::default(),
        events: Vec::new(),
        t,
        tokens: Tokens::default(),
        rec: Some(rec),
        ended: false,
    };
    let mut r = Response::new(Body::from_stream(s));
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(HeaderName::from_static("x-accel-buffering"), HeaderValue::from_static("no"));
    r
}

// ------------------------------------------------------------------ models

const CLAUDE_MODELS: &[&str] = &["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-1", "claude-haiku-4-5-20251001"];
const CODEX_MODELS: &[&str] = &["gpt-6.1-sol", "gpt-6-sol", "gpt-5.6-sol", "gpt-5.5"];
/// From the Gemini CLI source (packages/core/src/config/models.ts): DEFAULT_GEMINI_MODEL,
/// BASE_GEMINI_FLASH_MODEL, LATEST_GEMINI_FLASH_MODEL, BASE_GEMINI_FLASH_LITE_MODEL.
const GEMINI_MODELS: &[&str] = &["gemini-2.5-pro", "gemini-3.5-flash", "gemini-3.8-flash", "gemini-3.1-flash-lite"];

async fn upstream_models(app: &Arc<App>, provider: Provider) -> Vec<String> {
    if let Some((at, list)) = app.models_cache.lock().unwrap().get(&provider)
        && now() - at < 3600 && !list.is_empty() {
            return list.clone();
        }
    let fallback = || -> Vec<String> {
        match provider {
            Provider::Claude => CLAUDE_MODELS.iter().map(|s| s.to_string()).collect(),
            Provider::Codex => CODEX_MODELS.iter().map(|s| s.to_string()).collect(),
            Provider::Gemini => GEMINI_MODELS.iter().map(|s| s.to_string()).collect(),
        }
    };
    // Code Assist has no models-listing method, so Gemini always uses the fallback list.
    if provider == Provider::Gemini {
        let list = fallback();
        app.models_cache.lock().unwrap().insert(provider, (now(), list.clone()));
        return list;
    }
    let Ok(acc) = app.pick(provider, None, &[]) else { return fallback() };
    let _ = oauth::ensure_fresh(app, &acc, 60).await;
    let f = acc.file.read().unwrap().clone();
    let upstream = app.cfg.read().unwrap().upstream.clone();
    let req = match provider {
        Provider::Claude => app
            .http
            .get(format!("{}/v1/models?limit=100", upstream.claude_api))
            .headers(claude_headers(&HeaderMap::new(), false, &f, &app.claude_version(), &crate::util::uuid_v4())),
        Provider::Codex => app.http.get(format!("{}/models?client_version={CODEX_VERSION}", upstream.codex_api)).headers(codex_headers(&HeaderMap::new(), &f, false)),
        Provider::Gemini => unreachable!(),
    };
    let list: Vec<String> = match req.timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) if r.status().is_success() => {
            let v: Value = r.json().await.unwrap_or_default();
            let arr = v["data"].as_array().or_else(|| v["models"].as_array()).cloned().unwrap_or_default();
            arr.iter().filter_map(|m| m["id"].as_str().or_else(|| m["slug"].as_str()).map(String::from)).collect()
        }
        _ => Vec::new(),
    };
    let list = if list.is_empty() { fallback() } else { list };
    app.models_cache.lock().unwrap().insert(provider, (now(), list.clone()));
    list
}

async fn models(app: &Arc<App>, pin: Option<&str>) -> Response {
    let mut data = Vec::new();
    let accounts = app.store.list();
    for provider in [Provider::Claude, Provider::Codex, Provider::Gemini] {
        let mine: Vec<_> = accounts.iter().filter(|a| a.provider() == provider && !a.file.read().unwrap().disabled).collect();
        if mine.is_empty() {
            continue;
        }
        if let Some(p) = pin
            && !mine.iter().any(|a| a.id() == p || a.label().eq_ignore_ascii_case(p)) {
                continue;
            }
        let owner = match provider {
            Provider::Claude => "anthropic",
            Provider::Codex => "openai",
            Provider::Gemini => "google",
        };
        for id in upstream_models(app, provider).await {
            data.push(json!({"id": id, "object": "model", "type": "model", "display_name": id, "created": 0, "created_at": "2025-01-01T00:00:00Z", "owned_by": owner}));
        }
    }
    let first = data.first().map(|m| m["id"].clone());
    let last = data.last().map(|m| m["id"].clone());
    json_response(StatusCode::OK, &json!({"object": "list", "data": data, "has_more": false, "first_id": first, "last_id": last}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_routing() {
        assert_eq!(provider_for_model("claude-opus-5-5"), Some(Provider::Claude));
        assert_eq!(provider_for_model("gpt-5.6-codex"), Some(Provider::Codex));
        assert_eq!(provider_for_model("o3"), Some(Provider::Codex));
        assert_eq!(provider_for_model("gemini-2.5-pro"), Some(Provider::Gemini));
        assert_eq!(provider_for_model("llama"), None);
    }

    #[test]
    fn key_extraction() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer sk-1"));
        assert_eq!(presented_key(&h), Some("sk-1"));
        let mut h = HeaderMap::new();
        h.insert("x-api-key", HeaderValue::from_static("sk-2"));
        assert_eq!(presented_key(&h), Some("sk-2"));
    }
}
