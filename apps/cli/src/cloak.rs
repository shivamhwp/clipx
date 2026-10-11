//! Make a non-Claude-Code request look like Claude Code, which subscription tokens require.
//! Native Claude Code requests skip this and pass through untouched.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const AGENT_PROMPT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const FINGERPRINT_SALT: &str = "59cf53e54c78";
const CCH_SEED: u64 = 0x4D659218E32A3268;

/// Betas a Claude Code OAuth session sends on every request.
pub const BASE_BETAS: &[&str] = &[
    "claude-code-20250219",
    "oauth-2025-04-20",
    "interleaved-thinking-2025-05-14",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
];

/// True when the request already comes from Claude Code.
pub fn is_native(user_agent: &str, body: &[u8]) -> bool {
    user_agent.starts_with("claude-cli/") || contains(body, b"x-anthropic-billing-header")
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// SHA256(salt + text[4] + text[7] + text[20] + version)[:3], indexing UTF-16 code units.
pub fn fingerprint(message_text: &str, version: &str) -> String {
    let units: Vec<u16> = message_text.encode_utf16().collect();
    let sampled: Vec<u16> = [4usize, 7, 20].iter().map(|&i| units.get(i).copied().unwrap_or(b'0' as u16)).collect();
    let input = format!("{FINGERPRINT_SALT}{}{version}", String::from_utf16_lossy(&sampled));
    hex::encode(Sha256::digest(input.as_bytes()))[..3].to_string()
}

fn first_user_text(body: &Value) -> String {
    let Some(msgs) = body["messages"].as_array() else { return String::new() };
    let Some(m) = msgs.iter().find(|m| m["role"] == "user") else { return String::new() };
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .find(|p| p["type"] == "text")
            .and_then(|p| p["text"].as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Rewrite a Messages body in place: billing block + agent prompt first in `system`,
/// a Claude Code style `metadata.user_id`. Returns signed JSON bytes.
pub fn cloak_body(body: &mut Value, version: &str, device_id: &str, account_uuid: &str, session_id: &str) -> Vec<u8> {
    let text = first_user_text(body);
    let billing = format!(
        "x-anthropic-billing-header: cc_version={version}.{}; cc_entrypoint=cli; cch=00000;",
        fingerprint(&text, version)
    );
    let mut system = vec![json!({"type": "text", "text": billing}), json!({"type": "text", "text": AGENT_PROMPT})];
    match body.get("system") {
        Some(Value::String(s)) if !s.is_empty() => system.push(json!({"type": "text", "text": s})),
        Some(Value::Array(blocks)) => {
            system.extend(blocks.iter().filter(|b| b["text"].as_str() != Some(AGENT_PROMPT)).cloned());
        }
        _ => {}
    }
    if let Some(obj) = body.as_object_mut() {
        obj.insert("system".into(), Value::Array(system));
        let user_id = json!({"device_id": device_id, "account_uuid": account_uuid, "session_id": session_id}).to_string();
        let meta = obj.entry("metadata").or_insert_with(|| json!({}));
        if let Some(m) = meta.as_object_mut() {
            m.insert("user_id".into(), Value::String(user_id));
        }
    }
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    sign(bytes)
}

/// Fill the `cch=00000` placeholder in system[0] with the xxh64 body signature.
pub fn sign(mut body: Vec<u8>) -> Vec<u8> {
    let Some(off) = cch_offset(&body) else { return body };
    body[off..off + 5].copy_from_slice(b"00000");
    let Some(normalized) = normalize(&body) else { return body };
    let h = xxhash_rust::xxh64::xxh64(&normalized, CCH_SEED) & 0xFFFFF;
    body[off..off + 5].copy_from_slice(format!("{h:05x}").as_bytes());
    body
}

fn cch_offset(body: &[u8]) -> Option<usize> {
    let marker = b"x-anthropic-billing-header:";
    let start = body.windows(marker.len()).position(|w| w == marker)?;
    let rel = body[start..].windows(4).position(|w| w == b"cch=")?;
    let digits = start + rel + 4;
    if body.get(digits + 5) == Some(&b';') { Some(digits) } else { None }
}

/// Hash view of the body: string values of every `model` key emptied, and the
/// `max_tokens`, `fallbacks` and `fallback_credit_token` members removed.
/// Mirrors Claude Code's byte-level normalisation (no reserialisation).
pub fn normalize(body: &[u8]) -> Option<Vec<u8>> {
    let mut s = Scanner { b: body, pos: 0, edits: Vec::new() };
    s.value(true)?;
    s.ws();
    if s.pos != body.len() {
        return None;
    }
    s.edits.sort_by_key(|e| e.0);
    let mut out = Vec::with_capacity(body.len());
    let mut last = 0;
    for (start, end) in s.edits {
        if start < last {
            return None;
        }
        out.extend_from_slice(&body[last..start]);
        last = end;
    }
    out.extend_from_slice(&body[last..]);
    Some(out)
}

struct Member {
    start: usize,
    end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    excluded: bool,
}

struct Scanner<'a> {
    b: &'a [u8],
    pos: usize,
    edits: Vec<(usize, usize)>,
}

impl Scanner<'_> {
    fn ws(&mut self) {
        while matches!(self.b.get(self.pos), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.b.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn edit(&mut self, start: usize, end: usize) {
        if start < end {
            self.edits.push((start, end));
        }
    }
    fn string(&mut self) -> Option<(usize, usize)> {
        if self.b.get(self.pos) != Some(&b'"') {
            return None;
        }
        let start = self.pos;
        self.pos += 1;
        while self.pos < self.b.len() {
            match self.b[self.pos] {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Some((start, self.pos));
                }
                _ => self.pos += 1,
            }
        }
        None
    }
    fn value(&mut self, collect: bool) -> Option<()> {
        self.ws();
        match *self.b.get(self.pos)? {
            b'{' => self.object(collect),
            b'[' => self.array(collect),
            b'"' => self.string().map(|_| ()),
            _ => {
                let start = self.pos;
                while let Some(c) = self.b.get(self.pos) {
                    if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                        break;
                    }
                    self.pos += 1;
                }
                (self.pos > start).then_some(())
            }
        }
    }
    fn array(&mut self, collect: bool) -> Option<()> {
        self.pos += 1;
        self.ws();
        if self.eat(b']') {
            return Some(());
        }
        loop {
            self.value(collect)?;
            self.ws();
            if self.eat(b',') {
                continue;
            }
            return self.eat(b']').then_some(());
        }
    }
    fn object(&mut self, collect: bool) -> Option<()> {
        self.pos += 1;
        self.ws();
        if self.eat(b'}') {
            return Some(());
        }
        let mut members = Vec::new();
        let mut comma_before = None;
        loop {
            self.ws();
            let member_start = self.pos;
            let (ks, ke) = self.string()?;
            self.ws();
            if !self.eat(b':') {
                return None;
            }
            self.ws();
            let key = &self.b[ks..ke];
            let excluded = collect && matches!(key, b"\"max_tokens\"" | b"\"fallbacks\"" | b"\"fallback_credit_token\"");
            if collect && key == b"\"model\"" && self.b.get(self.pos) == Some(&b'"') {
                let (vs, ve) = self.string()?;
                self.edit(vs + 1, ve - 1);
            } else {
                self.value(collect && !excluded)?;
            }
            let member_end = self.pos;
            self.ws();
            let comma_after = if self.eat(b',') { Some(self.pos - 1) } else { None };
            members.push(Member { start: member_start, end: member_end, comma_before, comma_after, excluded });
            if comma_after.is_some() {
                comma_before = comma_after;
                continue;
            }
            if !self.eat(b'}') {
                return None;
            }
            break;
        }
        if collect {
            let mut i = 0;
            while i < members.len() {
                if !members[i].excluded {
                    i += 1;
                    continue;
                }
                let mut j = i;
                while j + 1 < members.len() && members[j + 1].excluded {
                    j += 1;
                }
                let (a, b) = if j + 1 < members.len() {
                    (members[i].start, members[j].comma_after.unwrap() + 1)
                } else if i > 0 && j > i {
                    (members[i].start, members[j].end)
                } else if i > 0 {
                    (members[i].comma_before.unwrap(), members[j].end)
                } else {
                    (members[i].start, members[j].end)
                };
                self.edit(a, b);
                i = j + 1;
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_drops_excluded_and_models() {
        let b = br#"{"model":"claude-x","max_tokens":10,"messages":[{"role":"user","content":"hi"}],"stream":true}"#;
        let n = normalize(b).unwrap();
        assert_eq!(String::from_utf8(n).unwrap(), r#"{"model":"","messages":[{"role":"user","content":"hi"}],"stream":true}"#);
        let tail = br#"{"a":1,"max_tokens":10}"#;
        assert_eq!(normalize(tail).unwrap(), br#"{"a":1}"#);
        let only = br#"{"max_tokens":10}"#;
        assert_eq!(normalize(only).unwrap(), b"{}");
    }

    #[test]
    fn sign_fills_placeholder() {
        let mut body = json!({"model":"m","max_tokens":5,"messages":[{"role":"user","content":"hello world, this is a test"}]});
        let out = cloak_body(&mut body, "2.1.291", &"ab".repeat(32), "", "8f4b1c2e-0000-4000-8000-000000000000");
        let v: Value = serde_json::from_slice(&out).unwrap();
        let billing = v["system"][0]["text"].as_str().unwrap();
        assert!(billing.starts_with("x-anthropic-billing-header: cc_version=2.1.291."));
        assert!(!billing.contains("cch=00000;"));
        assert_eq!(v["system"][1]["text"], AGENT_PROMPT);
        // Re-signing the signed body is stable.
        assert_eq!(sign(out.clone()), out);
    }

    #[test]
    fn fingerprint_shape() {
        assert_eq!(fingerprint("", "2.1.291").len(), 3);
        assert_ne!(fingerprint("hello world, this is a test", "2.1.291"), fingerprint("", "2.1.291"));
    }
}
