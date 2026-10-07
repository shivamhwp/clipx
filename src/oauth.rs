//! OAuth (PKCE) login and token refresh for Claude and Codex subscriptions.

use crate::app::App;
use crate::store::{Account, AccountFile, Provider, default_label, new_account};
use crate::util::{b64url, jwt_claims, now, random_bytes, random_hex};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const CLAUDE_AUTHORIZE: &str = "https://claude.ai/oauth/authorize";
/// Manual-code page: shows `code#state` for the user to paste back. Works on remote boxes.
pub const CLAUDE_REDIRECT: &str = "https://platform.claude.com/oauth/code/callback";
pub const CLAUDE_SCOPE: &str = "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_AUTHORIZE: &str = "https://auth.openai.com/oauth/authorize";
pub const CODEX_REDIRECT: &str = "http://localhost:1455/auth/callback";
pub const CODEX_CALLBACK_PORT: u16 = 1455;

const FLOW_TTL: u64 = 15 * 60;

#[derive(Clone, Debug)]
pub struct PendingFlow {
    pub provider: Provider,
    pub verifier: String,
    pub state: String,
    pub created: u64,
}

pub struct StartedFlow {
    pub id: String,
    pub url: String,
    pub hint: &'static str,
}

pub fn start(app: &Arc<App>, provider: Provider) -> StartedFlow {
    let verifier = b64url(&random_bytes::<32>());
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let state = b64url(&random_bytes::<24>());
    let (base, params): (&str, Vec<(&str, &str)>) = match provider {
        Provider::Claude => (
            CLAUDE_AUTHORIZE,
            vec![
                ("code", "true"),
                ("client_id", CLAUDE_CLIENT_ID),
                ("response_type", "code"),
                ("redirect_uri", CLAUDE_REDIRECT),
                ("scope", CLAUDE_SCOPE),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("state", &state),
            ],
        ),
        Provider::Codex => (
            CODEX_AUTHORIZE,
            vec![
                ("client_id", CODEX_CLIENT_ID),
                ("response_type", "code"),
                ("redirect_uri", CODEX_REDIRECT),
                ("scope", "openid email profile offline_access"),
                ("state", &state),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("prompt", "login"),
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
            ],
        ),
    };
    let url = url::Url::parse_with_params(base, &params).expect("static url").to_string();
    let id = random_hex(8);
    let mut flows = app.flows.lock().unwrap();
    flows.retain(|_, f| now() - f.created < FLOW_TTL);
    flows.insert(id.clone(), PendingFlow { provider, verifier, state: state.clone(), created: now() });
    drop(flows);
    let hint = match provider {
        Provider::Claude => "sign in, then paste the code shown on the page",
        Provider::Codex => {
            spawn_codex_callback_listener(app.clone(), id.clone());
            "sign in. if this box is not the machine with the browser, the final page fails to load: copy its full url (localhost:1455/auth/callback?code=…) and paste it here"
        }
    };
    StartedFlow { id, url, hint }
}

/// Accepts a full callback URL, `code#state`, `code=..&state=..`, or a bare code.
pub fn parse_callback(input: &str) -> (String, Option<String>) {
    let input = input.trim();
    let query = if let Ok(u) = url::Url::parse(input) {
        u.query().map(String::from)
    } else if input.contains("code=") {
        Some(input.trim_start_matches('?').to_string())
    } else {
        None
    };
    if let Some(q) = query {
        let mut code = String::new();
        let mut state = None;
        for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
            match k.as_ref() {
                "code" => code = v.into_owned(),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        if let Some((c, s)) = code.clone().split_once('#') {
            return (c.to_string(), Some(s.to_string()));
        }
        return (code, state);
    }
    match input.split_once('#') {
        Some((c, s)) => (c.to_string(), Some(s.to_string())),
        None => (input.to_string(), None),
    }
}

pub async fn complete(app: &Arc<App>, flow_id: &str, callback: &str) -> Result<Arc<Account>, String> {
    let flow = app.flows.lock().unwrap().get(flow_id).cloned().ok_or("login expired or unknown; start again")?;
    let (code, state) = parse_callback(callback);
    if code.is_empty() {
        return Err("no authorization code found in what you pasted".into());
    }
    if let Some(s) = &state
        && s != &flow.state {
            return Err("state does not match this login; start again".into());
        }
    let file = match flow.provider {
        Provider::Claude => exchange_claude(app, &flow, &code).await?,
        Provider::Codex => exchange_codex(app, &flow, &code).await?,
    };
    app.flows.lock().unwrap().remove(flow_id);
    let acc = app.store.upsert(file).map_err(|e| e.to_string())?;
    tracing::info!("logged in {} account {}", flow.provider.as_str(), acc.label());
    Ok(acc)
}

pub fn flow_done(app: &App, flow_id: &str) -> bool {
    !app.flows.lock().unwrap().contains_key(flow_id)
}

fn claude_headers(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    req.header("accept", "application/json, text/plain, */*")
        .header("content-type", "application/json")
        .header("user-agent", "axios/1.15.2")
}

async fn read_json(resp: reqwest::Response, what: &str) -> Result<Value, (u16, String)> {
    let status = resp.status().as_u16();
    let body = resp.bytes().await.map_err(|e| (0, format!("{what}: {e}")))?;
    if !(200..300).contains(&status) {
        let text = String::from_utf8_lossy(&body);
        return Err((status, format!("{what} failed ({status}): {}", text.chars().take(400).collect::<String>())));
    }
    serde_json::from_slice(&body).map_err(|e| (status, format!("{what}: bad json: {e}")))
}

async fn exchange_claude(app: &App, flow: &PendingFlow, code: &str) -> Result<AccountFile, String> {
    let url = app.cfg.read().unwrap().upstream.claude_token_url.clone();
    let body = json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": CLAUDE_REDIRECT,
        "client_id": CLAUDE_CLIENT_ID,
        "code_verifier": flow.verifier,
        "state": flow.state,
    });
    let resp = claude_headers(app.http.post(url)).body(body.to_string()).send().await.map_err(|e| e.to_string())?;
    let v = read_json(resp, "claude token exchange").await.map_err(|e| e.1)?;
    let mut file = new_account(Provider::Claude, v["access_token"].as_str().ok_or("no access_token in response")?.to_string());
    file.refresh_token = v["refresh_token"].as_str().map(String::from);
    file.expires_at = now() + v["expires_in"].as_u64().unwrap_or(28800);
    file.email = v["account"]["email_address"].as_str().map(String::from);
    file.account_uuid = v["account"]["uuid"].as_str().map(String::from);
    file.org = v["organization"]["name"].as_str().map(String::from);
    file.label = default_label(Provider::Claude, file.email.as_deref());
    Ok(file)
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

fn apply_codex_identity(file: &mut AccountFile) {
    if let Some(claims) = file.id_token.as_deref().and_then(jwt_claims) {
        if let Some(e) = claims["email"].as_str() {
            file.email = Some(e.to_string());
        }
        let auth = &claims["https://api.openai.com/auth"];
        if let Some(a) = auth["chatgpt_account_id"].as_str() {
            file.account_id = Some(a.to_string());
        }
        if let Some(p) = auth["chatgpt_plan_type"].as_str() {
            file.plan = Some(p.to_string());
        }
    }
}

fn jwt_exp(token: &str) -> Option<u64> {
    jwt_claims(token)?["exp"].as_u64()
}

async fn exchange_codex(app: &App, flow: &PendingFlow, code: &str) -> Result<AccountFile, String> {
    let url = app.cfg.read().unwrap().upstream.codex_token_url.clone();
    let body = form(&[
        ("grant_type", "authorization_code"),
        ("client_id", CODEX_CLIENT_ID),
        ("code", code),
        ("redirect_uri", CODEX_REDIRECT),
        ("code_verifier", &flow.verifier),
    ]);
    let resp = app
        .http
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let v = read_json(resp, "codex token exchange").await.map_err(|e| e.1)?;
    let access = v["access_token"].as_str().ok_or("no access_token in response")?.to_string();
    let mut file = new_account(Provider::Codex, access.clone());
    file.refresh_token = v["refresh_token"].as_str().map(String::from);
    file.id_token = v["id_token"].as_str().map(String::from);
    file.expires_at = v["expires_in"].as_u64().map(|s| now() + s).or_else(|| jwt_exp(&access)).unwrap_or(now() + 3600);
    apply_codex_identity(&mut file);
    file.label = default_label(Provider::Codex, file.email.as_deref());
    Ok(file)
}

/// Refresh if the token expires within `margin` seconds. Single-flight per account.
pub async fn ensure_fresh(app: &App, acc: &Account, margin: u64) -> Result<(), String> {
    let needs = |acc: &Account| {
        let f = acc.file.read().unwrap();
        f.expires_at != 0 && f.expires_at <= now() + margin && !f.no_refresh && f.refresh_token.is_some()
    };
    if !needs(acc) {
        return Ok(());
    }
    let _guard = acc.refresh_lock.lock().await;
    if !needs(acc) {
        return Ok(());
    }
    refresh(app, acc).await
}

pub async fn refresh(app: &App, acc: &Account) -> Result<(), String> {
    if acc.file.read().unwrap().linked.is_some() {
        return match app.store.reload_linked(acc)? {
            true => Ok(()),
            false => Err("waiting for the linked file to get a new token".into()),
        };
    }
    let (provider, refresh_token, no_refresh) = {
        let f = acc.file.read().unwrap();
        (f.provider, f.refresh_token.clone(), f.no_refresh)
    };
    if no_refresh {
        return Err("refresh disabled for this account".into());
    }
    let refresh_token = refresh_token.ok_or("account has no refresh token")?;
    let upstream = app.cfg.read().unwrap().upstream.clone();
    let result = match provider {
        Provider::Claude => {
            let body = json!({
                "client_id": CLAUDE_CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "scope": CLAUDE_SCOPE,
            });
            match claude_headers(app.http.post(&upstream.claude_token_url)).body(body.to_string()).send().await {
                Ok(r) => read_json(r, "claude refresh").await,
                Err(e) => Err((0, e.to_string())),
            }
        }
        Provider::Codex => {
            let body = form(&[
                ("client_id", CODEX_CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh_token),
                ("scope", "openid profile email"),
            ]);
            match app
                .http
                .post(&upstream.codex_token_url)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("accept", "application/json")
                .body(body)
                .send()
                .await
            {
                Ok(r) => read_json(r, "codex refresh").await,
                Err(e) => Err((0, e.to_string())),
            }
        }
    };
    let v = match result {
        Ok(v) => v,
        Err((status, msg)) => {
            let mut s = acc.state.lock().unwrap();
            s.last_error = Some(msg.clone());
            // 400/401 means the refresh token is dead: the user must log in again.
            if status == 400 || status == 401 || status == 403 {
                s.needs_login = true;
            }
            tracing::warn!("{} refresh failed: {msg}", acc.label());
            return Err(msg);
        }
    };
    {
        let mut f = acc.file.write().unwrap();
        if let Some(a) = v["access_token"].as_str() {
            f.access_token = a.to_string();
        }
        if let Some(r) = v["refresh_token"].as_str().filter(|r| !r.is_empty()) {
            f.refresh_token = Some(r.to_string());
        }
        if let Some(i) = v["id_token"].as_str() {
            f.id_token = Some(i.to_string());
        }
        f.expires_at = v["expires_in"].as_u64().map(|s| now() + s).or_else(|| jwt_exp(&f.access_token)).unwrap_or(now() + 3600);
        f.last_refresh = now();
        if provider == Provider::Codex {
            apply_codex_identity(&mut f);
        }
    }
    {
        let mut s = acc.state.lock().unwrap();
        s.needs_login = false;
        s.last_error = None;
    }
    app.store.save_account(acc).map_err(|e| e.to_string())?;
    tracing::info!("refreshed {}", acc.label());
    Ok(())
}

/// Refresh tokens that expire within 10 minutes. Runs every minute.
pub async fn refresh_loop(app: Arc<App>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tick.tick().await;
        for acc in app.store.list() {
            if acc.file.read().unwrap().linked.is_some() {
                // Cheap: one small file read a minute. Also revives an account the owner re-logged in.
                if let Err(e) = app.store.reload_linked(&acc) {
                    tracing::warn!("{}: {e}", acc.label());
                }
                continue;
            }
            let skip = {
                let f = acc.file.read().unwrap();
                f.disabled || acc.state.lock().unwrap().needs_login
            };
            if !skip {
                let _ = ensure_fresh(&app, &acc, 600).await;
            }
        }
    }
}

/// While a Codex login is pending, catch the browser redirect to localhost:1455 when the
/// browser runs on this machine. Remote users paste the URL instead.
fn spawn_codex_callback_listener(app: Arc<App>, flow_id: String) {
    tokio::spawn(async move {
        let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", CODEX_CALLBACK_PORT)).await else {
            return;
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(FLOW_TTL);
        loop {
            if flow_done(&app, &flow_id) {
                return;
            }
            let accepted = tokio::time::timeout_at(deadline.min(tokio::time::Instant::now() + std::time::Duration::from_secs(5)), listener.accept()).await;
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            let Ok(Ok((mut sock, _))) = accepted else { continue };
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
            if !path.starts_with("/auth/callback") {
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                continue;
            }
            let result = complete(&app, &flow_id, &format!("http://localhost{path}")).await;
            let msg = match &result {
                Ok(acc) => format!("logged in as {}. you can close this tab.", acc.label()),
                Err(e) => format!("login failed: {e}"),
            };
            let page = format!("<!doctype html><meta charset=utf-8><title>clipx</title><body style=\"font:16px monospace;padding:40px\">{}</body>", html_escape(&msg));
            let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}", page.len());
            let _ = sock.write_all(resp.as_bytes()).await;
            if result.is_ok() {
                return;
            }
        }
    });
}

pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::parse_callback;

    #[test]
    fn callbacks() {
        assert_eq!(parse_callback("abc#xyz"), ("abc".into(), Some("xyz".into())));
        assert_eq!(parse_callback("http://localhost:1455/auth/callback?code=c1&state=s1"), ("c1".into(), Some("s1".into())));
        assert_eq!(parse_callback("  rawcode "), ("rawcode".into(), None));
        assert_eq!(parse_callback("code=a&state=b"), ("a".into(), Some("b".into())));
    }
}
