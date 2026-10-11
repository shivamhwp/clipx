//! OpenAI Chat Completions <-> Gemini, and the native Gemini API's wrap/unwrap.
//!
//! clipx talks to Google's Code Assist backend, the same one the Gemini CLI uses for a Google
//! account login (see `oauth.rs`): POST `{endpoint}/v1internal:generateContent` (or
//! `:streamGenerateContent?alt=sse`, or `:countTokens`), with the body wrapped as
//! `{model, project, user_prompt_id, request: {...}}` and the response wrapped as
//! `{response: {...}, traceId, ...}`. The native `/v1beta/models/{model}:generateContent`
//! endpoints clipx exposes use that inner `request`/`response` shape directly, matching the
//! public Gemini API, so this file wraps on the way in and unwraps on the way out.

use crate::proxy::Recorder;
use crate::sse;
use crate::translate::{data_url, text_of};
use crate::usage::Tokens;
use crate::util::{now, random_hex};
use axum::body::{Body, Bytes};
use axum::http::{HeaderName, HeaderValue, header};
use axum::response::Response;
use futures_util::Stream;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};

type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

// ------------------------------------------------------------------ envelope

/// Wrap an inner Gemini `request` object (contents/systemInstruction/tools/...) in the Code
/// Assist envelope the backend expects. `project` is per-account, so this runs once per attempt.
pub fn wrap_envelope(model: &str, project: Option<&str>, user_prompt_id: &str, request: Value) -> Value {
    let mut v = json!({"model": model, "user_prompt_id": user_prompt_id, "request": request});
    if let Some(p) = project {
        v["project"] = json!(p);
    }
    v
}

/// Unwrap a Code Assist response envelope (`{"response": {...}, "traceId": ...}`) to the plain
/// `GenerateContentResponse` shape native Gemini clients expect. Falls back to the value itself
/// so a malformed or already-unwrapped body still produces something.
pub fn unwrap_envelope(v: &Value) -> Value {
    if v.get("response").is_some() { v["response"].clone() } else { v.clone() }
}

pub fn count_tokens_envelope(model: &str, native_body: &Value) -> Value {
    json!({"request": {"model": format!("models/{model}"), "contents": native_body["contents"].clone()}})
}

pub fn count_tokens_response(ca: &Value) -> Value {
    json!({"totalTokens": ca["totalTokens"].as_u64().unwrap_or(0)})
}

/// Pull token counts out of a Gemini `usageMetadata` object. Thinking tokens are counted as
/// output: they are generated tokens the account pays for, same as visible completion tokens.
pub fn absorb_usage(t: &mut Tokens, usage: &Value) {
    if let Some(v) = usage["promptTokenCount"].as_u64() {
        t.input = t.input.max(v);
    }
    let out = usage["candidatesTokenCount"].as_u64().unwrap_or(0) + usage["thoughtsTokenCount"].as_u64().unwrap_or(0);
    if out > 0 {
        t.output = t.output.max(out);
    }
    if let Some(v) = usage["cachedContentTokenCount"].as_u64() {
        t.cache_read = t.cache_read.max(v);
    }
}

fn usage_triplet(u: &Value) -> (u64, u64, u64) {
    let input = u["promptTokenCount"].as_u64().unwrap_or(0);
    let output = u["candidatesTokenCount"].as_u64().unwrap_or(0) + u["thoughtsTokenCount"].as_u64().unwrap_or(0);
    let cached = u["cachedContentTokenCount"].as_u64().unwrap_or(0);
    (input, output, cached)
}

fn chat_usage(input: u64, output: u64, cached: u64) -> Value {
    json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input + output,
        "prompt_tokens_details": {"cached_tokens": cached},
    })
}

fn gemini_finish(reason: &str, has_tool_calls: bool) -> &'static str {
    if has_tool_calls {
        return "tool_calls";
    }
    match reason {
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "BLOCKLIST" => "content_filter",
        _ => "stop",
    }
}

// ---------------------------------------------------------------- chat -> gemini

fn gemini_user_parts(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) => vec![json!({"text": s})],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p["type"].as_str().unwrap_or("text") {
                "text" | "input_text" => Some(json!({"text": p["text"].as_str().unwrap_or("")})),
                "image_url" => {
                    let url = p["image_url"]["url"].as_str().or_else(|| p["image_url"].as_str())?;
                    let (media, data) = data_url(url)?;
                    Some(json!({"inlineData": {"mimeType": media, "data": data}}))
                }
                "file" => {
                    let (media, data) = data_url(p["file"]["file_data"].as_str()?)?;
                    Some(json!({"inlineData": {"mimeType": media, "data": data}}))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn push_content(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut()
        && last["role"] == role {
            last["parts"].as_array_mut().unwrap().extend(parts);
            return;
        }
    contents.push(json!({"role": role, "parts": parts}));
}

/// Build the inner Gemini `request` object (contents/systemInstruction/tools/generationConfig)
/// from an OpenAI chat completions request. The caller wraps the result with `wrap_envelope`.
pub fn chat_to_gemini(req: &Value) -> Value {
    let mut system_parts: Vec<Value> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    // OpenAI tool results carry a tool_call_id; Gemini function responses need the function
    // name instead, so remember which name each call id belongs to as we walk the messages.
    let mut call_names: HashMap<String, String> = HashMap::new();
    for m in req["messages"].as_array().into_iter().flatten() {
        match m["role"].as_str().unwrap_or("") {
            "system" | "developer" => {
                let t = text_of(&m["content"]);
                if !t.is_empty() {
                    system_parts.push(json!({"text": t}));
                }
            }
            "user" => push_content(&mut contents, "user", gemini_user_parts(&m["content"])),
            "assistant" => {
                let mut parts = Vec::new();
                let t = text_of(&m["content"]);
                if !t.is_empty() {
                    parts.push(json!({"text": t}));
                }
                for tc in m["tool_calls"].as_array().into_iter().flatten() {
                    let id = tc["id"].as_str().unwrap_or("").to_string();
                    let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                    let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
                    let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
                    call_names.insert(id, name.clone());
                    parts.push(json!({"functionCall": {"name": name, "args": if input.is_object() { input } else { json!({}) }}}));
                }
                push_content(&mut contents, "model", parts);
            }
            "tool" | "function" => {
                let id = m["tool_call_id"].as_str().unwrap_or("");
                let name = call_names.get(id).cloned().unwrap_or_default();
                let text = text_of(&m["content"]);
                let result: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!(text));
                let response = if result.is_object() { result } else { json!({"result": result}) };
                push_content(&mut contents, "user", vec![json!({"functionResponse": {"name": name, "response": response}})]);
            }
            _ => {}
        }
    }

    let mut gen_config = Map::new();
    if let Some(effort) = req["reasoning_effort"].as_str().filter(|e| *e != "none" && *e != "minimal") {
        let budget = match effort {
            "low" => 2048,
            "high" => 24576,
            "xhigh" | "max" => 32768,
            _ => 8192,
        };
        gen_config.insert("thinkingConfig".into(), json!({"includeThoughts": true, "thinkingBudget": budget}));
    } else {
        if req["temperature"].is_number() {
            gen_config.insert("temperature".into(), req["temperature"].clone());
        }
        if req["top_p"].is_number() {
            gen_config.insert("topP".into(), req["top_p"].clone());
        }
    }
    if let Some(mt) = req["max_completion_tokens"].as_u64().or_else(|| req["max_tokens"].as_u64()) {
        gen_config.insert("maxOutputTokens".into(), json!(mt));
    }
    match &req["stop"] {
        Value::String(s) => {
            gen_config.insert("stopSequences".into(), json!([s]));
        }
        Value::Array(a) if !a.is_empty() => {
            gen_config.insert("stopSequences".into(), Value::Array(a.clone()));
        }
        _ => {}
    }

    let mut out = Map::new();
    if !system_parts.is_empty() {
        out.insert("systemInstruction".into(), json!({"parts": system_parts}));
    }
    out.insert("contents".into(), Value::Array(contents));
    if !gen_config.is_empty() {
        out.insert("generationConfig".into(), Value::Object(gen_config));
    }
    if let Some(tools) = req["tools"].as_array() {
        let decls: Vec<Value> = tools
            .iter()
            .filter(|t| t["type"] == "function")
            .map(|t| {
                let f = &t["function"];
                let mut d = json!({
                    "name": f["name"],
                    "parameters": if f["parameters"].is_object() { f["parameters"].clone() } else { json!({"type": "object", "properties": {}}) },
                });
                if let Some(desc) = f["description"].as_str() {
                    d["description"] = json!(desc);
                }
                d
            })
            .collect();
        if !decls.is_empty() {
            out.insert("tools".into(), json!([{"functionDeclarations": decls}]));
            let mode_and_names = match &req["tool_choice"] {
                Value::String(s) if s == "none" => Some(("NONE", None)),
                Value::String(s) if s == "required" => Some(("ANY", None)),
                Value::Object(o) => o.get("function").and_then(|f| f["name"].as_str()).map(|n| ("ANY", Some(n.to_string()))),
                _ => None,
            };
            if let Some((mode, name)) = mode_and_names {
                let mut fc = json!({"mode": mode});
                if let Some(n) = name {
                    fc["allowedFunctionNames"] = json!([n]);
                }
                out.insert("toolConfig".into(), json!({"functionCallingConfig": fc}));
            }
        }
    }
    Value::Object(out)
}

/// Build a chat.completion from a finished (unwrapped) Gemini `GenerateContentResponse`.
pub fn gemini_to_chat(resp: &Value, model: &str) -> Value {
    let cand = &resp["candidates"][0];
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for p in cand["content"]["parts"].as_array().into_iter().flatten() {
        if p["functionCall"].is_object() {
            tool_calls.push(json!({
                "id": format!("call_{}", random_hex(8)),
                "type": "function",
                "function": {"name": p["functionCall"]["name"], "arguments": p["functionCall"]["args"].to_string()},
            }));
        } else if p["thought"] == true {
            reasoning.push_str(p["text"].as_str().unwrap_or(""));
        } else if let Some(t) = p["text"].as_str() {
            text.push_str(t);
        }
    }
    let has_tool_calls = !tool_calls.is_empty();
    let mut message = json!({"role": "assistant", "content": if text.is_empty() && has_tool_calls { Value::Null } else { json!(text) }});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if has_tool_calls {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let finish = gemini_finish(cand["finishReason"].as_str().unwrap_or(""), has_tool_calls);
    let (i, o, c) = usage_triplet(&resp["usageMetadata"]);
    json!({
        "id": format!("chatcmpl-{}", random_hex(12)),
        "object": "chat.completion",
        "created": now(),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": chat_usage(i, o, c),
    })
}

// ---------------------------------------------------------------- streaming (chat completions)

fn chat_chunk(id: &str, model: &str, created: u64, delta: Value, finish: Option<&str>) -> String {
    let v = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    sse::frame(None, &v.to_string())
}

fn chat_usage_frame(id: &str, model: &str, created: u64, usage: Value) -> String {
    let v = json!({"id": id, "object": "chat.completion.chunk", "created": created, "model": model, "choices": [], "usage": usage});
    sse::frame(None, &v.to_string())
}

/// Converts a stream of Code Assist envelope events into chat.completion.chunk frames. Gemini
/// sends each part whole per chunk (it does not stream partial function-call JSON the way
/// Claude does), so one tool call becomes one chunk with its full name and arguments.
pub struct GeminiToChatStream {
    id: String,
    model: String,
    created: u64,
    include_usage: bool,
    started: bool,
    next_tool: u64,
    input: u64,
    output: u64,
    cached: u64,
    finish: &'static str,
    done: bool,
}

impl GeminiToChatStream {
    pub fn new(model: &str, include_usage: bool) -> Self {
        Self {
            id: format!("chatcmpl-{}", random_hex(12)),
            model: model.to_string(),
            created: now(),
            include_usage,
            started: false,
            next_tool: 0,
            input: 0,
            output: 0,
            cached: 0,
            finish: "stop",
            done: false,
        }
    }

    pub fn on_event(&mut self, ev: &sse::Event, out: &mut String) {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return };
        let resp = unwrap_envelope(&v);
        if !self.started {
            self.started = true;
            out.push_str(&chat_chunk(&self.id, &self.model, self.created, json!({"role": "assistant", "content": ""}), None));
        }
        let cand = &resp["candidates"][0];
        for p in cand["content"]["parts"].as_array().into_iter().flatten() {
            if let Some(fc) = p.get("functionCall").filter(|f| f.is_object()) {
                let idx = self.next_tool;
                self.next_tool += 1;
                out.push_str(&chat_chunk(
                    &self.id,
                    &self.model,
                    self.created,
                    json!({"tool_calls": [{"index": idx, "id": format!("call_{}", random_hex(8)), "type": "function", "function": {"name": fc["name"], "arguments": fc["args"].to_string()}}]}),
                    None,
                ));
            } else if p["thought"] == true {
                if let Some(t) = p["text"].as_str().filter(|t| !t.is_empty()) {
                    out.push_str(&chat_chunk(&self.id, &self.model, self.created, json!({"reasoning_content": t}), None));
                }
            } else if let Some(t) = p["text"].as_str().filter(|t| !t.is_empty()) {
                out.push_str(&chat_chunk(&self.id, &self.model, self.created, json!({"content": t}), None));
            }
        }
        if let Some(r) = cand["finishReason"].as_str() {
            self.finish = gemini_finish(r, self.next_tool > 0);
        }
        if resp["usageMetadata"].is_object() {
            let (i, o, c) = usage_triplet(&resp["usageMetadata"]);
            self.input = i;
            self.output = o;
            self.cached = c;
        }
    }

    pub fn finish(&mut self, out: &mut String) {
        if self.done {
            return;
        }
        self.done = true;
        out.push_str(&chat_chunk(&self.id, &self.model, self.created, json!({}), Some(self.finish)));
        if self.include_usage {
            out.push_str(&chat_usage_frame(&self.id, &self.model, self.created, chat_usage(self.input, self.output, self.cached)));
        }
        out.push_str("data: [DONE]\n\n");
    }
}

struct ChatTranslate {
    inner: ByteStream,
    parser: sse::Parser,
    events: Vec<sse::Event>,
    t: GeminiToChatStream,
    rec: Option<Recorder>,
    ended: bool,
}

impl ChatTranslate {
    fn done(&mut self, error: Option<String>) {
        if let Some(rec) = self.rec.take() {
            rec.finish(Tokens { input: self.t.input, output: self.t.output, cache_read: self.t.cached, cache_write: 0 }, error);
        }
    }
}

impl Stream for ChatTranslate {
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
                    for ev in std::mem::take(&mut this.events) {
                        this.t.on_event(&ev, &mut out);
                    }
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
                    for ev in std::mem::take(&mut this.events) {
                        this.t.on_event(&ev, &mut out);
                    }
                    this.t.finish(&mut out);
                    this.done(None);
                    return Poll::Ready(if out.is_empty() { None } else { Some(Ok(Bytes::from(out))) });
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Drop for ChatTranslate {
    fn drop(&mut self) {
        self.done(Some("client disconnected".into()));
    }
}

fn sse_headers(r: &mut Response) {
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert(HeaderName::from_static("x-accel-buffering"), HeaderValue::from_static("no"));
}

/// Builds the `/v1/chat/completions` streaming response for a Gemini account.
pub fn chat_stream_response(rec: Recorder, resp: reqwest::Response, model: &str, include_usage: bool) -> Response {
    let s = ChatTranslate { inner: Box::pin(resp.bytes_stream()), parser: sse::Parser::default(), events: Vec::new(), t: GeminiToChatStream::new(model, include_usage), rec: Some(rec), ended: false };
    let mut r = Response::new(Body::from_stream(s));
    sse_headers(&mut r);
    r
}

// ---------------------------------------------------------------- streaming (native passthrough)

/// Unwraps each Code Assist envelope event back to the plain response shape and re-emits it,
/// for clients calling the native `:streamGenerateContent` endpoint.
struct NativeUnwrap {
    inner: ByteStream,
    parser: sse::Parser,
    events: Vec<sse::Event>,
    tokens: Tokens,
    rec: Option<Recorder>,
    ended: bool,
}

impl NativeUnwrap {
    fn done(&mut self, error: Option<String>) {
        if let Some(rec) = self.rec.take() {
            rec.finish(self.tokens, error);
        }
    }
    fn drain(&mut self, out: &mut String) {
        for ev in std::mem::take(&mut self.events) {
            let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { continue };
            let inner = unwrap_envelope(&v);
            absorb_usage(&mut self.tokens, &inner["usageMetadata"]);
            out.push_str(&sse::frame(None, &inner.to_string()));
        }
    }
}

impl Stream for NativeUnwrap {
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
                    self.done(Some(e.to_string()));
                    return Poll::Ready(Some(Err(std::io::Error::other("upstream stream error"))));
                }
                Poll::Ready(None) => {
                    self.ended = true;
                    let this = &mut *self;
                    this.parser.finish(&mut this.events);
                    this.drain(&mut out);
                    this.done(None);
                    return Poll::Ready(if out.is_empty() { None } else { Some(Ok(Bytes::from(out))) });
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl Drop for NativeUnwrap {
    fn drop(&mut self) {
        self.done(Some("client disconnected".into()));
    }
}

/// Builds the native `:streamGenerateContent` response, unwrapping the Code Assist envelope.
pub fn native_stream_response(rec: Recorder, resp: reqwest::Response) -> Response {
    let s = NativeUnwrap { inner: Box::pin(resp.bytes_stream()), parser: sse::Parser::default(), events: Vec::new(), tokens: Tokens::default(), rec: Some(rec), ended: false };
    let mut r = Response::new(Body::from_stream(s));
    sse_headers(&mut r);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(raw: &str) -> Vec<sse::Event> {
        let mut p = sse::Parser::default();
        let mut out = Vec::new();
        p.push(raw.as_bytes(), &mut out);
        out
    }

    #[test]
    fn envelope_roundtrip() {
        let wrapped = wrap_envelope("gemini-x", Some("proj-1"), "up-1", json!({"contents": []}));
        assert_eq!(wrapped["model"], "gemini-x");
        assert_eq!(wrapped["project"], "proj-1");
        assert_eq!(wrapped["user_prompt_id"], "up-1");
        let no_project = wrap_envelope("gemini-x", None, "up-1", json!({}));
        assert!(no_project.get("project").is_none());
        let resp = json!({"response": {"candidates": [{"content": {"parts": [{"text": "hi"}]}}]}, "traceId": "t1"});
        let inner = unwrap_envelope(&resp);
        assert_eq!(inner["candidates"][0]["content"]["parts"][0]["text"], "hi");
    }

    #[test]
    fn count_tokens_shapes() {
        let env = count_tokens_envelope("gemini-x", &json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}));
        assert_eq!(env["request"]["model"], "models/gemini-x");
        assert_eq!(env["request"]["contents"][0]["parts"][0]["text"], "hi");
        assert_eq!(count_tokens_response(&json!({"totalTokens": 12}))["totalTokens"], 12);
    }

    #[test]
    fn chat_to_gemini_shapes() {
        let req = json!({
            "model": "gemini-x",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}]},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "t1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}}]},
                {"role": "tool", "tool_call_id": "t1", "content": "{\"ok\":true}"}
            ],
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}],
            "tool_choice": "required",
            "max_tokens": 50
        });
        let out = chat_to_gemini(&req);
        assert_eq!(out["systemInstruction"]["parts"][0]["text"], "be brief");
        assert_eq!(out["contents"].as_array().unwrap().len(), 3);
        assert_eq!(out["contents"][0]["role"], "user");
        assert_eq!(out["contents"][0]["parts"][1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(out["contents"][1]["role"], "model");
        assert_eq!(out["contents"][1]["parts"][0]["functionCall"]["name"], "f");
        assert_eq!(out["contents"][1]["parts"][0]["functionCall"]["args"]["a"], 1);
        assert_eq!(out["contents"][2]["role"], "user");
        assert_eq!(out["contents"][2]["parts"][0]["functionResponse"]["name"], "f");
        assert_eq!(out["contents"][2]["parts"][0]["functionResponse"]["response"]["ok"], true);
        assert_eq!(out["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        assert_eq!(out["generationConfig"]["maxOutputTokens"], 50);
    }

    #[test]
    fn gemini_to_chat_shapes() {
        let resp = json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "hello"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "cachedContentTokenCount": 2, "thoughtsTokenCount": 1},
        });
        let out = gemini_to_chat(&resp, "gemini-x");
        assert_eq!(out["choices"][0]["message"]["content"], "hello");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 10);
        assert_eq!(out["usage"]["completion_tokens"], 6);
        assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 2);
    }

    #[test]
    fn gemini_stream_to_chat() {
        let raw = concat!(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Hel\"}]}}]}}\n\n",
            "data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"lo\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2}}}\n\n",
        );
        let mut t = GeminiToChatStream::new("gemini-x", true);
        let mut out = String::new();
        for e in events(raw) {
            t.on_event(&e, &mut out);
        }
        t.finish(&mut out);
        assert!(out.contains("\"content\":\"Hel\""));
        assert!(out.contains("\"content\":\"lo\""));
        assert!(out.contains("\"finish_reason\":\"stop\""));
        assert!(out.contains("\"prompt_tokens\":3"));
        assert!(out.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn gemini_stream_tool_call() {
        let raw = concat!("data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"f\",\"args\":{\"a\":1}}}]},\"finishReason\":\"STOP\"}]}}\n\n");
        let mut t = GeminiToChatStream::new("gemini-x", false);
        let mut out = String::new();
        for e in events(raw) {
            t.on_event(&e, &mut out);
        }
        t.finish(&mut out);
        assert!(out.contains("\"name\":\"f\""));
        assert!(out.contains("\"arguments\":\"{\\\"a\\\":1}\""));
        assert!(out.contains("\"finish_reason\":\"tool_calls\""));
    }
}
