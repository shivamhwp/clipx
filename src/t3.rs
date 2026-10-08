//! T3 Code integration. clipx adds itself to T3 Code as "Claude (clipx)" (Claude Code
//! pointed at clipx) and "ChatGPT (clipx)" (Codex pointed at clipx) by editing T3's
//! settings.json. T3 watches that file, so the providers show up without a restart, and
//! it moves the keys into its own secret store the next time it saves its settings.

use crate::app::App;
use crate::store::Provider;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

/// Providers T3 can run through clipx. Gemini has no agent in T3 that speaks to clipx yet.
pub const PROVIDERS: [Provider; 2] = [Provider::Claude, Provider::Codex];

pub fn instance_id(p: Provider) -> &'static str {
    match p {
        Provider::Codex => "clipx-codex",
        _ => "clipx-claude",
    }
}

fn display_name(p: Provider) -> &'static str {
    match p {
        Provider::Codex => "ChatGPT (clipx)",
        _ => "Claude (clipx)",
    }
}

fn key_var(p: Provider) -> &'static str {
    match p {
        Provider::Codex => "CLIPX_API_KEY",
        _ => "ANTHROPIC_AUTH_TOKEN",
    }
}

/// Name of the clipx API key each T3 provider uses, so revoking it in clipx is noticed.
pub fn key_name(p: Provider) -> &'static str {
    match p {
        Provider::Codex => "T3 Code (ChatGPT)",
        _ => "T3 Code (Claude)",
    }
}

/// Models picked for new threads and for T3's titles and commit messages, when the user
/// has not chosen any.
fn default_models(p: Provider) -> (&'static str, &'static str) {
    match p {
        Provider::Codex => ("gpt-6.1-sol", "gpt-6-luna"),
        _ => ("claude-opus-5-5", "claude-haiku-5-5"),
    }
}

pub fn home() -> PathBuf {
    match std::env::var_os("T3CODE_HOME").filter(|v| !v.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => crate::util::home_dir().join(".t3"),
    }
}

pub fn settings_path() -> PathBuf {
    home().join("userdata/settings.json")
}

/// The `t3` command: on PATH, in ~/.local/bin, or the version T3's runtime marks active.
pub fn cli() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join("t3"))
        .chain([crate::util::home_dir().join(".local/bin/t3")]);
    let runtime = home().join("runtime");
    let active = std::fs::read(runtime.join("service-state.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["activeVersion"].as_str().map(|v| runtime.join("versions").join(v).join("t3")));
    let mut versions: Vec<PathBuf> = std::fs::read_dir(runtime.join("versions")).into_iter().flatten().flatten().map(|e| e.path().join("t3")).collect();
    versions.sort();
    on_path.chain(active).chain(versions.into_iter().rev()).find(|p| p.is_file())
}

/// T3 Code is on this machine, either as the CLI or as the desktop app's data folder.
pub fn installed() -> bool {
    cli().is_some() || home().join("userdata").is_dir()
}

/// A T3 server is running on this machine.
pub fn running() -> bool {
    let pid = std::fs::read(home().join("userdata/server-runtime.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["pid"].as_u64());
    pid.is_some_and(|pid| {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

pub fn read() -> Result<Value, String> {
    let path = settings_path();
    match std::fs::read(&path) {
        Ok(b) if b.iter().all(u8::is_ascii_whitespace) => Ok(json!({})),
        Ok(b) => serde_json::from_slice(&b).map_err(|e| format!("{} is not valid JSON ({e}); fix it or remove it", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// Write T3's settings in one step, keeping the file's permissions. The first time clipx
/// changes the file it keeps a copy next to it.
pub fn write(v: &Value) -> Result<(), String> {
    let link = settings_path();
    let path = std::fs::canonicalize(&link).unwrap_or(link);
    let dir = path.parent().ok_or("settings.json has no folder")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let backup = dir.join("settings.json.before-clipx");
    if path.exists() && !backup.exists() {
        std::fs::copy(&path, &backup).map_err(|e| format!("back up {}: {e}", path.display()))?;
    }
    let mut data = serde_json::to_vec_pretty(v).map_err(|e| e.to_string())?;
    data.push(b'\n');
    let perms = std::fs::metadata(&path).map(|m| m.permissions()).ok();
    let tmp = dir.join(format!(".settings.json.clipx-{}", crate::util::random_hex(4)));
    std::fs::write(&tmp, &data).map_err(|e| e.to_string())?;
    if let Some(p) = perms {
        let _ = std::fs::set_permissions(&tmp, p);
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("write {}: {e}", path.display())
    })
}

fn instance<'a>(settings: &'a Value, p: Provider) -> Option<&'a Value> {
    settings["providerInstances"].get(instance_id(p))
}

/// Providers clipx has added to these settings.
pub fn added(settings: &Value) -> Vec<Provider> {
    PROVIDERS.into_iter().filter(|p| instance(settings, *p).is_some()).collect()
}

/// The provider already holds a key, inline or moved into T3's secret store.
pub fn has_key(settings: &Value, p: Provider) -> bool {
    instance(settings, p)
        .and_then(|i| i["environment"].as_array())
        .into_iter()
        .flatten()
        .any(|e| e["name"] == key_var(p) && (e["valueRedacted"] == true || e["value"].as_str().is_some_and(|v| !v.is_empty())))
}

/// How Codex is pointed at clipx. Codex reads each value as TOML and keeps it as a plain
/// string when it is not valid TOML, which every value here is.
fn codex_args(base: &str) -> String {
    format!(
        "-c model_provider=clipx -c model_providers.clipx.name=clipx -c model_providers.clipx.base_url={base}/v1 -c model_providers.clipx.env_key={} -c model_providers.clipx.wire_api=responses",
        key_var(Provider::Codex)
    )
}

/// Environment variables clipx manages for a provider. `key` is None when the existing
/// key entry should stay as it is.
fn managed_env(p: Provider, base: &str, key: Option<&str>) -> Vec<Value> {
    let mut env = Vec::new();
    if p == Provider::Claude {
        env.push(json!({"name": "ANTHROPIC_BASE_URL", "value": base, "sensitive": false}));
    }
    if let Some(k) = key {
        env.push(json!({"name": key_var(p), "value": k, "sensitive": true}));
    }
    env
}

/// Make T3's clipx providers match `want`: add or update those, remove the others.
/// `keys` holds a new key for each provider that needs one; providers without an entry
/// keep the key T3 already has. Settings the user changed on these providers are kept.
pub fn apply(settings: &mut Value, base: &str, want: &[Provider], keys: &[(Provider, String)]) {
    if !settings.is_object() {
        *settings = json!({});
    }
    let root = settings.as_object_mut().unwrap();
    let instances = root.entry("providerInstances").or_insert_with(|| json!({}));
    if !instances.is_object() {
        *instances = json!({});
    }
    let instances = instances.as_object_mut().unwrap();
    for p in PROVIDERS {
        let id = instance_id(p);
        if !want.contains(&p) {
            instances.shift_remove(id);
            continue;
        }
        let mut inst = instances.get(id).and_then(Value::as_object).cloned().unwrap_or_default();
        let new_key = keys.iter().find(|(kp, _)| *kp == p).map(|(_, k)| k.as_str());
        let managed = managed_env(p, base, new_key);
        // Update managed variables where they are, so an unchanged sync rewrites nothing.
        let mut env: Vec<Value> = inst.get("environment").and_then(Value::as_array).cloned().unwrap_or_default();
        for m in managed {
            match env.iter_mut().find(|e| e["name"] == m["name"]) {
                Some(e) => *e = m,
                None => env.push(m),
            }
        }
        inst.insert("driver".into(), json!(if p == Provider::Codex { "codex" } else { "claudeAgent" }));
        inst.entry("displayName").or_insert(json!(display_name(p)));
        inst.entry("enabled").or_insert(json!(true));
        inst.insert("environment".into(), Value::Array(env));
        let mut config = inst.get("config").and_then(Value::as_object).cloned().unwrap_or_default();
        if p == Provider::Codex {
            config.insert("launchArgs".into(), json!(codex_args(base)));
        }
        inst.insert("config".into(), Value::Object(config));
        instances.insert(id.into(), Value::Object(inst));
    }
    let ours = |sel: &Value| PROVIDERS.iter().any(|p| sel["instanceId"] == instance_id(*p));
    let first = PROVIDERS.into_iter().find(|p| want.contains(p));
    for (field, text) in [("defaultModelSelection", false), ("textGenerationModelSelection", true)] {
        let current = root.get(field).cloned().unwrap_or(Value::Null);
        let stale = ours(&current) && !want.iter().any(|p| current["instanceId"] == instance_id(*p));
        if current.is_null() || stale {
            match first {
                Some(p) => {
                    let (main, small) = default_models(p);
                    root.insert(field.into(), json!({"instanceId": instance_id(p), "model": if text { small } else { main }}));
                }
                None => {
                    root.shift_remove(field);
                }
            }
        }
    }
}

/// Values for setting the providers up by hand in T3, with `key` in place of a real key.
pub fn manual(base: &str, key: &str) -> Value {
    json!(PROVIDERS
        .iter()
        .map(|p| {
            let mut v = json!({
                "id": instance_id(*p),
                "name": display_name(*p),
                "driver": if *p == Provider::Codex { "Codex" } else { "Claude" },
                "environment": managed_env(*p, base, Some(key)),
            });
            if *p == Provider::Codex {
                v["launch_args"] = json!(codex_args(base));
            }
            v
        })
        .collect::<Vec<_>>())
}

/// Providers with at least one account in clipx, in the order T3 should list them.
pub fn wanted(app: &App) -> Vec<Provider> {
    let accounts = app.store.list();
    PROVIDERS.into_iter().filter(|p| accounts.iter().any(|a| a.provider() == *p)).collect()
}

/// Bring T3's settings in line with clipx: one provider per kind of account clipx has.
/// Creates a key for each provider that has none (or whose key was revoked in clipx).
/// Returns the providers now in T3, and whether the file changed.
pub fn sync(app: &App) -> Result<(Vec<Provider>, bool), String> {
    let mut settings = read()?;
    let before = settings.clone();
    let base = app.cfg.read().unwrap().local_url();
    let want = wanted(app);
    let names: Vec<String> = app.store.keys.read().unwrap().iter().map(|k| k.name.clone()).collect();
    let mut fresh = Vec::new();
    for p in want.iter().copied() {
        if has_key(&settings, p) && names.iter().any(|n| n == key_name(p)) {
            continue;
        }
        let stale: Vec<String> = app.store.keys.read().unwrap().iter().filter(|k| k.name == key_name(p)).map(|k| k.id.clone()).collect();
        for id in stale {
            app.store.revoke_key(&id);
        }
        let (_, plain) = app.store.create_key(key_name(p)).map_err(|e| e.to_string())?;
        fresh.push((p, plain));
    }
    apply(&mut settings, &base, &want, &fresh);
    if settings == before {
        return Ok((want, false));
    }
    if let Err(e) = write(&settings) {
        // T3 never got these keys, so do not leave them working.
        let made: Vec<String> = app.store.keys.read().unwrap().iter().filter(|k| fresh.iter().any(|(p, _)| k.name == key_name(*p))).map(|k| k.id.clone()).collect();
        for id in made {
            app.store.revoke_key(&id);
        }
        return Err(e);
    }
    Ok((want, true))
}

/// Take clipx back out of T3 and revoke the keys it made for T3.
pub fn remove(app: &App) -> Result<(), String> {
    let mut settings = read()?;
    if !added(&settings).is_empty() || ["defaultModelSelection", "textGenerationModelSelection"].iter().any(|f| PROVIDERS.iter().any(|p| settings[*f]["instanceId"] == instance_id(*p))) {
        apply(&mut settings, "", &[], &[]);
        write(&settings)?;
    }
    let ids: Vec<String> = app.store.keys.read().unwrap().iter().filter(|k| PROVIDERS.iter().any(|p| k.name == key_name(*p))).map(|k| k.id.clone()).collect();
    for id in ids {
        app.store.revoke_key(&id);
    }
    Ok(())
}

/// While the T3 integration is on, keep T3 in step with clipx's accounts and address.
pub async fn sync_loop(app: Arc<App>) {
    let mut last_err = String::new();
    loop {
        // Skip while T3 is gone, so an uninstalled T3 does not get its folder back.
        if app.cfg.read().unwrap().t3 && installed() {
            let a = app.clone();
            let r = tokio::task::spawn_blocking(move || sync(&a)).await.unwrap_or_else(|e| Err(e.to_string()));
            match r {
                Ok((want, true)) => {
                    tracing::info!("updated T3 Code: {}", want.iter().map(|p| display_name(*p)).collect::<Vec<_>>().join(", "));
                    last_err.clear();
                }
                Ok(_) => last_err.clear(),
                Err(e) if e != last_err => {
                    tracing::warn!("could not update T3 Code: {e}");
                    last_err = e;
                }
                Err(_) => {}
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

/// Status for the dashboard and `clipx status`.
pub fn status(app: &App) -> Value {
    let settings = read();
    let base = app.cfg.read().unwrap().local_url();
    json!({
        "enabled": app.cfg.read().unwrap().t3,
        "installed": installed(),
        "running": running(),
        "settings": settings_path(),
        "added": settings.as_ref().map(|s| added(s).iter().map(|p| display_name(*p)).collect::<Vec<_>>()).unwrap_or_default(),
        "error": settings.err(),
        "manual": manual(&base, "<your clipx key>"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_value<'a>(s: &'a Value, p: Provider, name: &str) -> Option<&'a Value> {
        s["providerInstances"][instance_id(p)]["environment"].as_array()?.iter().find(|e| e["name"] == name)
    }

    #[test]
    fn adds_providers_without_touching_the_rest() {
        let mut s = json!({
            "theme": "dark",
            "defaultModelSelection": {"instanceId": "claude-p", "model": "claude-opus-5-5"},
            "providerInstances": {"claude-p": {"driver": "claudeAgent", "environment": []}},
        });
        apply(&mut s, "http://127.0.0.1:8318", &PROVIDERS, &[(Provider::Claude, "sk-a".into()), (Provider::Codex, "sk-b".into())]);
        assert_eq!(s["theme"], "dark");
        assert_eq!(s["defaultModelSelection"]["instanceId"], "claude-p");
        assert_eq!(s["textGenerationModelSelection"]["instanceId"], "clipx-claude");
        assert!(s["providerInstances"]["claude-p"].is_object());
        assert_eq!(env_value(&s, Provider::Claude, "ANTHROPIC_BASE_URL").unwrap()["value"], "http://127.0.0.1:8318");
        assert_eq!(env_value(&s, Provider::Claude, "ANTHROPIC_AUTH_TOKEN").unwrap()["value"], "sk-a");
        assert_eq!(env_value(&s, Provider::Codex, "CLIPX_API_KEY").unwrap()["sensitive"], true);
        assert!(s["providerInstances"]["clipx-codex"]["config"]["launchArgs"].as_str().unwrap().contains("base_url=http://127.0.0.1:8318/v1"));
        assert!(has_key(&s, Provider::Claude));
    }

    #[test]
    fn keeps_a_key_t3_moved_to_its_secret_store_and_user_changes() {
        let mut s = json!({});
        apply(&mut s, "http://127.0.0.1:8318", &[Provider::Claude], &[(Provider::Claude, "sk-a".into())]);
        // What T3 does on its next save, plus a user rename and an extra variable.
        let inst = &mut s["providerInstances"]["clipx-claude"];
        inst["displayName"] = json!("My Claude");
        inst["environment"][1] = json!({"name": "ANTHROPIC_AUTH_TOKEN", "value": "", "sensitive": true, "valueRedacted": true});
        inst["environment"].as_array_mut().unwrap().push(json!({"name": "CLAUDE_CODE_EFFORT_LEVEL", "value": "high", "sensitive": false}));
        let saved = s.clone();
        apply(&mut s, "http://127.0.0.1:8318", &[Provider::Claude], &[]);
        assert_eq!(s, saved, "a second sync with nothing new must not change the file");
        apply(&mut s, "http://127.0.0.1:9000", &[Provider::Claude], &[]);
        assert_eq!(s["providerInstances"]["clipx-claude"]["displayName"], "My Claude");
        assert_eq!(env_value(&s, Provider::Claude, "ANTHROPIC_AUTH_TOKEN").unwrap()["valueRedacted"], true);
        assert_eq!(env_value(&s, Provider::Claude, "ANTHROPIC_BASE_URL").unwrap()["value"], "http://127.0.0.1:9000");
        assert_eq!(env_value(&s, Provider::Claude, "CLAUDE_CODE_EFFORT_LEVEL").unwrap()["value"], "high");
        assert_eq!(s["defaultModelSelection"], json!({"instanceId": "clipx-claude", "model": "claude-opus-5-5"}));
    }

    #[test]
    fn removing_clears_selections_that_point_at_clipx() {
        let mut s = json!({"textGenerationModelSelection": {"instanceId": "codex-vm", "model": "gpt-6-luna"}});
        apply(&mut s, "http://x", &PROVIDERS, &[(Provider::Claude, "a".into()), (Provider::Codex, "b".into())]);
        assert_eq!(s["defaultModelSelection"]["instanceId"], "clipx-claude");
        apply(&mut s, "http://x", &[Provider::Codex], &[]);
        assert!(s["providerInstances"].get("clipx-claude").is_none());
        assert_eq!(s["defaultModelSelection"], json!({"instanceId": "clipx-codex", "model": "gpt-6.1-sol"}));
        apply(&mut s, "", &[], &[]);
        assert!(s.get("defaultModelSelection").is_none());
        assert_eq!(s["textGenerationModelSelection"]["instanceId"], "codex-vm");
        assert_eq!(s["providerInstances"], json!({}));
    }
}
