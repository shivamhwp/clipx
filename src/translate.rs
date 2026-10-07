//! OpenAI Chat Completions <-> Claude Messages and <-> Codex Responses.

use crate::sse;
use crate::util::now;
use serde_json::{Map, Value, json};

pub const DEFAULT_MAX_TOKENS: u64 = 32000;

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str().or_else(|| p.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let media = meta.strip_suffix(";base64")?;
    Some((media.to_string(), data.to_string()))
}

// ---------------------------------------------------------------- chat -> claude

fn claude_user_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) => vec![json!({"type": "text", "text": s})],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p["type"].as_str().unwrap_or("text") {
                "text" | "input_text" => Some(json!({"type": "text", "text": p["text"].as_str().unwrap_or("")})),
                "image_url" => {
                    let url = p["image_url"]["url"].as_str().or_else(|| p["image_url"].as_str())?;
                    Some(match data_url(url) {
                        Some((media, data)) => json!({"type": "image", "source": {"type": "base64", "media_type": media, "data": data}}),
                        None => json!({"type": "image", "source": {"type": "url", "url": url}}),
                    })
                }
                "file" => {
                    let (media, data) = data_url(p["file"]["file_data"].as_str()?)?;
                    Some(json!({"type": "document", "source": {"type": "base64", "media_type": media, "data": data}}))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn push_claude(msgs: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = msgs.last_mut()
        && last["role"] == role {
            last["content"].as_array_mut().unwrap().extend(blocks);
            return;
        }
    msgs.push(json!({"role": role, "content": blocks}));
}

pub fn chat_to_claude(req: &Value) -> Value {
    let mut system: Vec<Value> = Vec::new();
    let mut msgs: Vec<Value> = Vec::new();
    for m in req["messages"].as_array().into_iter().flatten() {
        match m["role"].as_str().unwrap_or("") {
            "system" | "developer" => {
                let t = text_of(&m["content"]);
                if !t.is_empty() {
                    system.push(json!({"type": "text", "text": t}));
                }
            }
            "user" => push_claude(&mut msgs, "user", claude_user_blocks(&m["content"])),
            "assistant" => {
                let mut blocks = Vec::new();
                let t = text_of(&m["content"]);
                if !t.is_empty() {
                    blocks.push(json!({"type": "text", "text": t}));
                }
                for tc in m["tool_calls"].as_array().into_iter().flatten() {
                    let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
                    let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": tc["id"].as_str().unwrap_or(""),
                        "name": tc["function"]["name"].as_str().unwrap_or(""),
                        "input": if input.is_object() { input } else { json!({}) },
                    }));
                }
                push_claude(&mut msgs, "assistant", blocks);
            }
            "tool" | "function" => {
                let id = m["tool_call_id"].as_str().unwrap_or("");
                push_claude(&mut msgs, "user", vec![json!({"type": "tool_result", "tool_use_id": id, "content": text_of(&m["content"])})]);
            }
            _ => {}
        }
    }
    let mut out = Map::new();
    out.insert("model".into(), req["model"].clone());
    let mut max_tokens = req["max_completion_tokens"].as_u64().or_else(|| req["max_tokens"].as_u64()).unwrap_or(DEFAULT_MAX_TOKENS);
    if let Some(effort) = req["reasoning_effort"].as_str().filter(|e| *e != "none" && *e != "minimal") {
        let budget = match effort {
            "low" => 2048,
            "high" => 24576,
            "xhigh" | "max" => 32000,
            _ => 8192,
        };
        max_tokens = max_tokens.max(budget + 4096);
        out.insert("thinking".into(), json!({"type": "enabled", "budget_tokens": budget}));
    } else {
        for k in ["temperature", "top_p"] {
            if req[k].is_number() {
                out.insert(k.into(), req[k].clone());
            }
        }
    }
    out.insert("max_tokens".into(), json!(max_tokens));
    if !system.is_empty() {
        out.insert("system".into(), Value::Array(system));
    }
    out.insert("messages".into(), Value::Array(msgs));
    match &req["stop"] {
        Value::String(s) => {
            out.insert("stop_sequences".into(), json!([s]));
        }
        Value::Array(a) if !a.is_empty() => {
            out.insert("stop_sequences".into(), Value::Array(a.clone()));
        }
        _ => {}
    }
    if let Some(tools) = req["tools"].as_array() {
        let tools: Vec<Value> = tools
            .iter()
            .filter(|t| t["type"] == "function")
            .map(|t| {
                let f = &t["function"];
                let mut tool = json!({"name": f["name"], "input_schema": if f["parameters"].is_object() { f["parameters"].clone() } else { json!({"type": "object", "properties": {}}) }});
                if let Some(d) = f["description"].as_str() {
                    tool["description"] = json!(d);
                }
                tool
            })
            .collect();
        if !tools.is_empty() {
            out.insert("tools".into(), Value::Array(tools));
            let choice = match &req["tool_choice"] {
                Value::String(s) if s == "none" => Some(json!({"type": "none"})),
                Value::String(s) if s == "required" => Some(json!({"type": "any"})),
                Value::Object(o) => o.get("function").and_then(|f| f["name"].as_str()).map(|n| json!({"type": "tool", "name": n})),
                _ => None,
            };
            if let Some(mut c) = choice {
                if req["parallel_tool_calls"] == false {
                    c["disable_parallel_tool_use"] = json!(true);
                }
                out.insert("tool_choice".into(), c);
            } else if req["parallel_tool_calls"] == false {
                out.insert("tool_choice".into(), json!({"type": "auto", "disable_parallel_tool_use": true}));
            }
        }
    }
    if req["stream"] == true {
        out.insert("stream".into(), json!(true));
    }
    Value::Object(out)
}

fn claude_finish(stop: &str) -> &'static str {
    match stop {
        "max_tokens" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => "stop",
    }
}

fn chat_usage(input: u64, output: u64, cached: u64) -> Value {
    json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": input + output,
        "prompt_tokens_details": {"cached_tokens": cached},
    })
}

fn claude_usage(u: &Value) -> (u64, u64, u64) {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let cached = n("cache_read_input_tokens");
    (n("input_tokens") + cached + n("cache_creation_input_tokens"), n("output_tokens"), cached)
}

pub fn claude_to_chat(resp: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for b in resp["content"].as_array().into_iter().flatten() {
        match b["type"].as_str().unwrap_or("") {
            "text" => text.push_str(b["text"].as_str().unwrap_or("")),
            "thinking" => reasoning.push_str(b["thinking"].as_str().unwrap_or("")),
            "tool_use" => tool_calls.push(json!({
                "id": b["id"],
                "type": "function",
                "function": {"name": b["name"], "arguments": b["input"].to_string()},
            })),
            _ => {}
        }
    }
    let mut message = json!({"role": "assistant", "content": if text.is_empty() && !tool_calls.is_empty() { Value::Null } else { json!(text) }});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let (i, o, c) = claude_usage(&resp["usage"]);
    json!({
        "id": format!("chatcmpl-{}", resp["id"].as_str().unwrap_or("")),
        "object": "chat.completion",
        "created": now(),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": claude_finish(resp["stop_reason"].as_str().unwrap_or(""))}],
        "usage": chat_usage(i, o, c),
    })
}

/// Shared helper for building chat.completion.chunk frames.
struct Chunker {
    id: String,
    model: String,
    created: u64,
}

impl Chunker {
    fn chunk(&self, delta: Value, finish: Option<&str>) -> String {
        let v = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        sse::frame(None, &v.to_string())
    }
    fn usage(&self, usage: Value) -> String {
        let v = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": usage,
        });
        sse::frame(None, &v.to_string())
    }
}

pub struct ClaudeToChatStream {
    c: Chunker,
    include_usage: bool,
    tool_index: std::collections::HashMap<u64, u64>,
    next_tool: u64,
    finish: Option<&'static str>,
    input: u64,
    cached: u64,
    output: u64,
    done: bool,
}

impl ClaudeToChatStream {
    pub fn new(model: &str, include_usage: bool) -> Self {
        Self {
            c: Chunker { id: format!("chatcmpl-{}", crate::util::random_hex(12)), model: model.to_string(), created: now() },
            include_usage,
            tool_index: Default::default(),
            next_tool: 0,
            finish: None,
            input: 0,
            cached: 0,
            output: 0,
            done: false,
        }
    }

    pub fn on_event(&mut self, ev: &sse::Event, out: &mut String) {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return };
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                let (i, o, c) = claude_usage(&v["message"]["usage"]);
                (self.input, self.output, self.cached) = (i, o, c);
                out.push_str(&self.c.chunk(json!({"role": "assistant", "content": ""}), None));
            }
            "content_block_start" => {
                let b = &v["content_block"];
                match b["type"].as_str().unwrap_or("") {
                    "tool_use" => {
                        let idx = self.next_tool;
                        self.next_tool += 1;
                        self.tool_index.insert(v["index"].as_u64().unwrap_or(0), idx);
                        out.push_str(&self.c.chunk(
                            json!({"tool_calls": [{"index": idx, "id": b["id"], "type": "function", "function": {"name": b["name"], "arguments": ""}}]}),
                            None,
                        ));
                    }
                    "text" => {
                        if let Some(t) = b["text"].as_str().filter(|t| !t.is_empty()) {
                            out.push_str(&self.c.chunk(json!({"content": t}), None));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let d = &v["delta"];
                match d["type"].as_str().unwrap_or("") {
                    "text_delta" => out.push_str(&self.c.chunk(json!({"content": d["text"]}), None)),
                    "thinking_delta" => out.push_str(&self.c.chunk(json!({"reasoning_content": d["thinking"]}), None)),
                    "input_json_delta" => {
                        if let Some(&idx) = self.tool_index.get(&v["index"].as_u64().unwrap_or(0)) {
                            out.push_str(&self.c.chunk(json!({"tool_calls": [{"index": idx, "function": {"arguments": d["partial_json"]}}]}), None));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    self.finish = Some(claude_finish(s));
                }
                if let Some(o) = v["usage"]["output_tokens"].as_u64() {
                    self.output = o;
                }
            }
            "message_stop" => self.end(out),
            "error" => {
                out.push_str(&sse::frame(None, &json!({"error": v["error"]}).to_string()));
                self.done = true;
                out.push_str("data: [DONE]\n\n");
            }
            _ => {}
        }
    }

    fn end(&mut self, out: &mut String) {
        if self.done {
            return;
        }
        self.done = true;
        out.push_str(&self.c.chunk(json!({}), Some(self.finish.unwrap_or("stop"))));
        if self.include_usage {
            out.push_str(&self.c.usage(chat_usage(self.input, self.output, self.cached)));
        }
        out.push_str("data: [DONE]\n\n");
    }

    pub fn finish(&mut self, out: &mut String) {
        self.end(out);
    }
}

// ---------------------------------------------------------------- chat -> codex

fn codex_content(content: &Value, role: &str) -> Vec<Value> {
    let text_type = if role == "assistant" { "output_text" } else { "input_text" };
    match content {
        Value::String(s) => vec![json!({"type": text_type, "text": s})],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p["type"].as_str().unwrap_or("text") {
                "text" | "input_text" | "output_text" => Some(json!({"type": text_type, "text": p["text"].as_str().unwrap_or("")})),
                "image_url" if role != "assistant" => {
                    let url = p["image_url"]["url"].as_str().or_else(|| p["image_url"].as_str())?;
                    Some(json!({"type": "input_image", "image_url": url}))
                }
                "file" if role != "assistant" => {
                    let f = &p["file"];
                    Some(json!({"type": "input_file", "file_data": f["file_data"], "filename": f["filename"].as_str().unwrap_or("file")}))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub fn chat_to_codex(req: &Value) -> Value {
    let mut input = Vec::new();
    for m in req["messages"].as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("");
        match role {
            "system" | "developer" | "user" => {
                let role = if role == "user" { "user" } else { "developer" };
                let content = codex_content(&m["content"], role);
                if !content.is_empty() {
                    input.push(json!({"type": "message", "role": role, "content": content}));
                }
            }
            "assistant" => {
                let content = codex_content(&m["content"], "assistant");
                if !content.is_empty() && content.iter().any(|c| c["text"].as_str().is_some_and(|t| !t.is_empty())) {
                    input.push(json!({"type": "message", "role": "assistant", "content": content}));
                }
                for tc in m["tool_calls"].as_array().into_iter().flatten() {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tc["id"],
                        "name": tc["function"]["name"],
                        "arguments": tc["function"]["arguments"].as_str().unwrap_or("{}"),
                    }));
                }
            }
            "tool" | "function" => input.push(json!({
                "type": "function_call_output",
                "call_id": m["tool_call_id"],
                "output": text_of(&m["content"]),
            })),
            _ => {}
        }
    }
    let mut out = json!({
        "model": req["model"],
        "instructions": "",
        "input": input,
        "stream": true,
        "store": false,
        "parallel_tool_calls": req["parallel_tool_calls"].as_bool().unwrap_or(true),
        "include": ["reasoning.encrypted_content"],
        "reasoning": {"effort": req["reasoning_effort"].as_str().unwrap_or("medium"), "summary": "auto"},
    });
    if let Some(tools) = req["tools"].as_array() {
        let tools: Vec<Value> = tools
            .iter()
            .filter(|t| t["type"] == "function")
            .map(|t| {
                let f = &t["function"];
                json!({
                    "type": "function",
                    "name": f["name"],
                    "description": f["description"].as_str().unwrap_or(""),
                    "parameters": if f["parameters"].is_object() { f["parameters"].clone() } else { json!({"type": "object", "properties": {}}) },
                    "strict": f["strict"].as_bool().unwrap_or(false),
                })
            })
            .collect();
        if !tools.is_empty() {
            out["tools"] = Value::Array(tools);
            out["tool_choice"] = match &req["tool_choice"] {
                Value::String(s) => json!(s),
                Value::Object(o) => o.get("function").map(|f| json!({"type": "function", "name": f["name"]})).unwrap_or(json!("auto")),
                _ => json!("auto"),
            };
        }
    }
    let rf = &req["response_format"];
    match rf["type"].as_str() {
        Some("json_schema") => {
            let s = &rf["json_schema"];
            out["text"] = json!({"format": {"type": "json_schema", "name": s["name"].as_str().unwrap_or("output"), "schema": s["schema"], "strict": s["strict"].as_bool().unwrap_or(false)}});
        }
        Some("json_object") => out["text"] = json!({"format": {"type": "json_object"}}),
        _ => {}
    }
    out
}

fn codex_usage(u: &Value) -> (u64, u64, u64) {
    (
        u["input_tokens"].as_u64().unwrap_or(0),
        u["output_tokens"].as_u64().unwrap_or(0),
        u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
    )
}

/// Build a chat.completion from a finished Responses `response` object.
pub fn codex_to_chat(resp: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for item in resp["output"].as_array().into_iter().flatten() {
        match item["type"].as_str().unwrap_or("") {
            "message" => {
                for c in item["content"].as_array().into_iter().flatten() {
                    if c["type"] == "output_text" {
                        text.push_str(c["text"].as_str().unwrap_or(""));
                    }
                }
            }
            "reasoning" => {
                for s in item["summary"].as_array().into_iter().flatten() {
                    reasoning.push_str(s["text"].as_str().unwrap_or(""));
                }
            }
            "function_call" => tool_calls.push(json!({
                "id": item["call_id"],
                "type": "function",
                "function": {"name": item["name"], "arguments": item["arguments"]},
            })),
            _ => {}
        }
    }
    let finish = if !tool_calls.is_empty() {
        "tool_calls"
    } else if resp["status"] == "incomplete" {
        "length"
    } else {
        "stop"
    };
    let mut message = json!({"role": "assistant", "content": if text.is_empty() && !tool_calls.is_empty() { Value::Null } else { json!(text) }});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let (i, o, c) = codex_usage(&resp["usage"]);
    json!({
        "id": format!("chatcmpl-{}", resp["id"].as_str().unwrap_or("")),
        "object": "chat.completion",
        "created": now(),
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": chat_usage(i, o, c),
    })
}

pub struct CodexToChatStream {
    c: Chunker,
    include_usage: bool,
    tools: std::collections::HashMap<String, u64>,
    next_tool: u64,
    started: bool,
    done: bool,
}

impl CodexToChatStream {
    pub fn new(model: &str, include_usage: bool) -> Self {
        Self {
            c: Chunker { id: format!("chatcmpl-{}", crate::util::random_hex(12)), model: model.to_string(), created: now() },
            include_usage,
            tools: Default::default(),
            next_tool: 0,
            started: false,
            done: false,
        }
    }

    fn start(&mut self, out: &mut String) {
        if !self.started {
            self.started = true;
            out.push_str(&self.c.chunk(json!({"role": "assistant", "content": ""}), None));
        }
    }

    pub fn on_event(&mut self, ev: &sse::Event, out: &mut String) {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return };
        let kind = v["type"].as_str().or(ev.event.as_deref()).unwrap_or("");
        match kind {
            "response.created" => self.start(out),
            "response.output_text.delta" => {
                self.start(out);
                out.push_str(&self.c.chunk(json!({"content": v["delta"]}), None));
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.start(out);
                out.push_str(&self.c.chunk(json!({"reasoning_content": v["delta"]}), None));
            }
            "response.output_item.added" if v["item"]["type"] == "function_call" => {
                self.start(out);
                let item = &v["item"];
                let idx = self.next_tool;
                self.next_tool += 1;
                self.tools.insert(item["id"].as_str().unwrap_or("").to_string(), idx);
                out.push_str(&self.c.chunk(
                    json!({"tool_calls": [{"index": idx, "id": item["call_id"], "type": "function", "function": {"name": item["name"], "arguments": ""}}]}),
                    None,
                ));
            }
            "response.function_call_arguments.delta" => {
                if let Some(&idx) = self.tools.get(v["item_id"].as_str().unwrap_or("")) {
                    out.push_str(&self.c.chunk(json!({"tool_calls": [{"index": idx, "function": {"arguments": v["delta"]}}]}), None));
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                self.start(out);
                let r = &v["response"];
                let finish = if self.next_tool > 0 {
                    "tool_calls"
                } else if kind == "response.incomplete" || r["status"] == "incomplete" {
                    "length"
                } else {
                    "stop"
                };
                out.push_str(&self.c.chunk(json!({}), Some(finish)));
                if self.include_usage {
                    let (i, o, c) = codex_usage(&r["usage"]);
                    out.push_str(&self.c.usage(chat_usage(i, o, c)));
                }
                out.push_str("data: [DONE]\n\n");
                self.done = true;
            }
            "response.failed" | "error" => {
                let err = if v["response"]["error"].is_object() { v["response"]["error"].clone() } else { v.get("error").cloned().unwrap_or(v.clone()) };
                out.push_str(&sse::frame(None, &json!({"error": err}).to_string()));
                out.push_str("data: [DONE]\n\n");
                self.done = true;
            }
            _ => {}
        }
    }

    pub fn finish(&mut self, out: &mut String) {
        if !self.done {
            self.done = true;
            out.push_str(&self.c.chunk(json!({}), Some("stop")));
            out.push_str("data: [DONE]\n\n");
        }
    }
}

// ---------------------------------------------------------------- messages -> codex

/// Claude Code stamps native requests with this system block. Codex has no use for
/// it, so it never reaches the Codex backend.
const BILLING_MARKER: &str = "x-anthropic-billing-header:";

fn codex_image(source: &Value) -> Option<Value> {
    match source["type"].as_str()? {
        "base64" => {
            let media = source["media_type"].as_str().unwrap_or("image/png");
            Some(json!({"type": "input_image", "image_url": format!("data:{media};base64,{}", source["data"].as_str()?)}))
        }
        "url" => Some(json!({"type": "input_image", "image_url": source["url"].as_str()?})),
        _ => None,
    }
}

fn codex_instructions(system: &Value) -> String {
    let keep = |t: &str| !t.starts_with(BILLING_MARKER);
    match system {
        Value::String(s) if keep(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).filter(|t| keep(t)).collect::<Vec<_>>().join("\n\n"),
        _ => String::new(),
    }
}

/// Split one tool_result's content into plain text and any images. Codex cannot
/// carry an image inside a `function_call_output`, so images go in a message right after.
fn tool_result_parts(content: &Value) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut images = Vec::new();
    match content {
        Value::String(s) => text.push_str(s),
        Value::Array(blocks) => {
            for b in blocks {
                match b["type"].as_str().unwrap_or("text") {
                    "text" => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(b["text"].as_str().unwrap_or(""));
                    }
                    "image" => images.extend(codex_image(&b["source"])),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    (text, images)
}

/// Turn one Anthropic message's content into Codex input items. A tool call or tool
/// result breaks out into its own item; surrounding text/image content becomes a
/// `message` item. Thinking blocks are dropped: Codex cannot take a foreign signature.
fn codex_items(role: &str, content: &Value) -> Vec<Value> {
    let mut items = Vec::new();
    let mut parts: Vec<Value> = Vec::new();
    let mut trailing_images: Vec<Value> = Vec::new();
    let text_type = if role == "assistant" { "output_text" } else { "input_text" };
    macro_rules! flush {
        () => {
            if !parts.is_empty() {
                items.push(json!({"type": "message", "role": role, "content": std::mem::take(&mut parts)}));
            }
        };
    }
    match content {
        Value::String(s) => parts.push(json!({"type": text_type, "text": s})),
        Value::Array(blocks) => {
            for b in blocks {
                match b["type"].as_str().unwrap_or("text") {
                    "text" => parts.push(json!({"type": text_type, "text": b["text"].as_str().unwrap_or("")})),
                    "image" if role != "assistant" => {
                        if let Some(img) = codex_image(&b["source"]) {
                            parts.push(img);
                        }
                    }
                    "tool_use" => {
                        flush!();
                        items.push(json!({"type": "function_call", "call_id": b["id"], "name": b["name"], "arguments": b["input"].to_string()}));
                    }
                    "tool_result" => {
                        flush!();
                        let (text, images) = tool_result_parts(&b["content"]);
                        items.push(json!({"type": "function_call_output", "call_id": b["tool_use_id"], "output": text}));
                        trailing_images.extend(images);
                    }
                    "thinking" | "redacted_thinking" => {}
                    _ => {}
                }
            }
        }
        _ => {}
    }
    flush!();
    if !trailing_images.is_empty() {
        items.push(json!({"type": "message", "role": "user", "content": trailing_images}));
    }
    items
}

fn codex_tool(t: &Value) -> Option<Value> {
    match t["type"].as_str() {
        None | Some("custom") => Some(json!({
            "type": "function",
            "name": t["name"],
            "description": t["description"].as_str().unwrap_or(""),
            "parameters": if t["input_schema"].is_object() { t["input_schema"].clone() } else { json!({"type": "object", "properties": {}}) },
            "strict": false,
        })),
        Some(ty) if ty.contains("web_search") => Some(json!({"type": "web_search"})),
        // Other Anthropic server tools (bash, code execution, ...) have no Codex equivalent.
        _ => None,
    }
}

/// `thinking: {type: enabled, budget_tokens}` / `adaptive`, or `output_config.effort`,
/// mapped to a Codex reasoning effort. None means the request did not ask to think.
pub fn thinking_effort(req: &Value) -> Option<&'static str> {
    if let Some(e) = req["output_config"]["effort"].as_str() {
        return Some(match e {
            "low" | "minimal" => "low",
            "high" | "xhigh" | "max" => "high",
            _ => "medium",
        });
    }
    match req["thinking"]["type"].as_str()? {
        "enabled" => {
            let budget = req["thinking"]["budget_tokens"].as_u64().unwrap_or(8192);
            Some(if budget <= 4096 { "low" } else if budget <= 16384 { "medium" } else { "high" })
        }
        "adaptive" => Some("medium"),
        _ => None,
    }
}

pub fn wants_thinking(req: &Value) -> bool {
    thinking_effort(req).is_some()
}

/// Anthropic Messages request -> Codex Responses request, for Claude Code (or any
/// Anthropic client) talking to a Codex/GPT model.
pub fn messages_to_codex(req: &Value) -> Value {
    let mut input = Vec::new();
    for m in req["messages"].as_array().into_iter().flatten() {
        // Claude Code puts system messages mid-conversation; the ChatGPT backend refuses
        // the system role but takes the same text as a developer message.
        let role = match m["role"].as_str().unwrap_or("user") {
            "system" | "developer" => "developer",
            r => r,
        };
        input.extend(codex_items(role, &m["content"]));
    }
    let mut out = json!({
        "model": req["model"],
        "instructions": codex_instructions(&req["system"]),
        "input": input,
        "stream": true,
        "store": false,
    });
    let choice = &req["tool_choice"];
    let disable_parallel = choice["disable_parallel_tool_use"].as_bool().unwrap_or(false);
    out["parallel_tool_calls"] = json!(req["parallel_tool_calls"].as_bool().unwrap_or(!disable_parallel));
    if let Some(tools) = req["tools"].as_array() {
        let mapped: Vec<Value> = tools.iter().filter_map(codex_tool).collect();
        if !mapped.is_empty() {
            out["tools"] = Value::Array(mapped);
            out["tool_choice"] = match choice["type"].as_str() {
                Some("any") => json!("required"),
                Some("none") => json!("none"),
                Some("tool") => json!({"type": "function", "name": choice["name"]}),
                _ => json!("auto"),
            };
        }
    }
    if let Some(effort) = thinking_effort(req) {
        out["reasoning"] = json!({"effort": effort, "summary": "auto"});
        out["include"] = json!(["reasoning.encrypted_content"]);
    }
    out
}

// ---------------------------------------------------------------- codex -> messages

/// Build an Anthropic `message` from a finished Codex Responses `response` object.
pub fn codex_to_message(resp: &Value, model: &str, emit_thinking: bool) -> Value {
    let mut content = Vec::new();
    let mut had_tool_call = false;
    for item in resp["output"].as_array().into_iter().flatten() {
        match item["type"].as_str().unwrap_or("") {
            "message" => {
                for c in item["content"].as_array().into_iter().flatten() {
                    if c["type"] == "output_text" {
                        content.push(json!({"type": "text", "text": c["text"].as_str().unwrap_or("")}));
                    }
                }
            }
            "reasoning" if emit_thinking => {
                let thinking: String = item["summary"].as_array().into_iter().flatten().filter_map(|s| s["text"].as_str()).collect();
                if !thinking.is_empty() {
                    content.push(json!({"type": "thinking", "thinking": thinking, "signature": ""}));
                }
            }
            "function_call" => {
                had_tool_call = true;
                let args = item["arguments"].as_str().unwrap_or("{}");
                let input: Value = serde_json::from_str(args).unwrap_or_else(|_| json!({}));
                content.push(json!({"type": "tool_use", "id": item["call_id"], "name": item["name"], "input": if input.is_object() { input } else { json!({}) }}));
            }
            _ => {}
        }
    }
    let stop_reason = if had_tool_call {
        "tool_use"
    } else if resp["status"] == "incomplete" {
        "max_tokens"
    } else {
        "end_turn"
    };
    let (i, o, c) = codex_usage(&resp["usage"]);
    json!({
        "id": resp["id"].as_str().unwrap_or("msg_codex"),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {"input_tokens": i, "output_tokens": o, "cache_read_input_tokens": c},
    })
}

#[derive(PartialEq, Clone, Copy)]
enum OpenKind {
    Text,
    Thinking,
    Tool,
}

fn content_delta(index: u64, delta: Value) -> String {
    sse::frame(Some("content_block_delta"), &json!({"type": "content_block_delta", "index": index, "delta": delta}).to_string())
}

/// Streams a Codex Responses SSE stream into Anthropic Messages SSE events, for
/// Claude Code talking to a Codex/GPT model.
pub struct CodexToAnthropicStream {
    id: String,
    model: String,
    emit_thinking: bool,
    started: bool,
    next_index: u64,
    open: Option<(OpenKind, u64)>,
    tool_index: std::collections::HashMap<String, u64>,
    had_tool_call: bool,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    done: bool,
}

impl CodexToAnthropicStream {
    pub fn new(model: &str, emit_thinking: bool) -> Self {
        Self {
            id: format!("msg_{}", crate::util::random_hex(12)),
            model: model.to_string(),
            emit_thinking,
            started: false,
            next_index: 0,
            open: None,
            tool_index: Default::default(),
            had_tool_call: false,
            input_tokens: 0,
            output_tokens: 0,
            cache_read: 0,
            done: false,
        }
    }

    fn start(&mut self, out: &mut String) {
        if self.started {
            return;
        }
        self.started = true;
        out.push_str(&sse::frame(
            Some("message_start"),
            &json!({"type": "message_start", "message": {
                "id": self.id, "type": "message", "role": "assistant", "model": self.model,
                "content": [], "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            }})
            .to_string(),
        ));
    }

    /// Close whatever content block is open. A thinking block gets a dummy signature
    /// first: Claude Code's content block shape expects one before the block closes.
    fn close(&mut self, out: &mut String) {
        if let Some((kind, idx)) = self.open.take() {
            if kind == OpenKind::Thinking {
                out.push_str(&content_delta(idx, json!({"type": "signature_delta", "signature": ""})));
            }
            out.push_str(&sse::frame(Some("content_block_stop"), &json!({"type": "content_block_stop", "index": idx}).to_string()));
        }
    }

    fn ensure_text(&mut self, out: &mut String) -> u64 {
        if let Some((OpenKind::Text, idx)) = self.open {
            return idx;
        }
        self.close(out);
        let idx = self.next_index;
        self.next_index += 1;
        out.push_str(&sse::frame(Some("content_block_start"), &json!({"type": "content_block_start", "index": idx, "content_block": {"type": "text", "text": ""}}).to_string()));
        self.open = Some((OpenKind::Text, idx));
        idx
    }

    fn ensure_thinking(&mut self, out: &mut String) -> u64 {
        if let Some((OpenKind::Thinking, idx)) = self.open {
            return idx;
        }
        self.close(out);
        let idx = self.next_index;
        self.next_index += 1;
        out.push_str(&sse::frame(
            Some("content_block_start"),
            &json!({"type": "content_block_start", "index": idx, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}).to_string(),
        ));
        self.open = Some((OpenKind::Thinking, idx));
        idx
    }

    fn open_tool(&mut self, out: &mut String, item_id: &str, call_id: &Value, name: &Value) {
        self.close(out);
        let idx = self.next_index;
        self.next_index += 1;
        out.push_str(&sse::frame(
            Some("content_block_start"),
            &json!({"type": "content_block_start", "index": idx, "content_block": {"type": "tool_use", "id": call_id, "name": name, "input": {}}}).to_string(),
        ));
        self.open = Some((OpenKind::Tool, idx));
        self.tool_index.insert(item_id.to_string(), idx);
    }

    pub fn on_event(&mut self, ev: &sse::Event, out: &mut String) {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else { return };
        let kind = v["type"].as_str().or(ev.event.as_deref()).unwrap_or("");
        match kind {
            "response.created" => self.start(out),
            "response.output_text.delta" => {
                self.start(out);
                let idx = self.ensure_text(out);
                out.push_str(&content_delta(idx, json!({"type": "text_delta", "text": v["delta"]})));
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if self.emit_thinking {
                    self.start(out);
                    let idx = self.ensure_thinking(out);
                    out.push_str(&content_delta(idx, json!({"type": "thinking_delta", "thinking": v["delta"]})));
                }
            }
            "response.output_item.added" if v["item"]["type"] == "function_call" => {
                self.start(out);
                self.had_tool_call = true;
                let item = &v["item"];
                self.open_tool(out, item["id"].as_str().unwrap_or(""), &item["call_id"], &item["name"]);
            }
            "response.function_call_arguments.delta" => {
                if let Some(&idx) = self.tool_index.get(v["item_id"].as_str().unwrap_or("")) {
                    out.push_str(&content_delta(idx, json!({"type": "input_json_delta", "partial_json": v["delta"]})));
                }
            }
            "response.output_item.done" => {
                let item_type = v["item"]["type"].as_str().unwrap_or("");
                let matches_open = matches!(
                    (self.open.map(|(k, _)| k), item_type),
                    (Some(OpenKind::Tool), "function_call") | (Some(OpenKind::Text), "message") | (Some(OpenKind::Thinking), "reasoning")
                );
                if matches_open {
                    self.close(out);
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                self.start(out);
                self.close(out);
                let r = &v["response"];
                let (i, o, c) = codex_usage(&r["usage"]);
                (self.input_tokens, self.output_tokens, self.cache_read) = (i, o, c);
                let incomplete = kind == "response.incomplete" || r["status"] == "incomplete";
                self.end(out, incomplete);
            }
            "response.failed" | "error" => {
                let err = if v["response"]["error"].is_object() { v["response"]["error"].clone() } else { v.get("error").cloned().unwrap_or(v.clone()) };
                out.push_str(&sse::frame(
                    Some("error"),
                    &json!({"type": "error", "error": {"type": err["type"].as_str().unwrap_or("api_error"), "message": err["message"].as_str().unwrap_or("upstream error")}}).to_string(),
                ));
                self.done = true;
            }
            _ => {}
        }
    }

    fn end(&mut self, out: &mut String, incomplete: bool) {
        if self.done {
            return;
        }
        self.done = true;
        let stop_reason = if self.had_tool_call {
            "tool_use"
        } else if incomplete {
            "max_tokens"
        } else {
            "end_turn"
        };
        out.push_str(&sse::frame(
            Some("message_delta"),
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {"input_tokens": self.input_tokens, "output_tokens": self.output_tokens, "cache_read_input_tokens": self.cache_read},
            })
            .to_string(),
        ));
        out.push_str(&sse::frame(Some("message_stop"), &json!({"type": "message_stop"}).to_string()));
    }

    /// Called when the upstream stream ends; a no-op if `response.completed` (or a
    /// failure) already closed things out.
    pub fn finish(&mut self, out: &mut String) {
        self.start(out);
        self.close(out);
        self.end(out, false);
    }

    /// Anthropic-shaped error event for a mid-stream network failure.
    pub fn on_error(&mut self, msg: &str, out: &mut String) {
        out.push_str(&sse::frame(Some("error"), &json!({"type": "error", "error": {"type": "api_error", "message": msg}}).to_string()));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn system_messages_become_developer_messages() {
        let req = serde_json::json!({"model": "gpt-5.6-sol", "messages": [
            {"role": "user", "content": "hi"},
            {"role": "system", "content": [{"type": "text", "text": "be terse"}]},
        ]});
        let out = super::messages_to_codex(&req);
        assert_eq!(out["input"][1]["role"], "developer");
        assert_eq!(out["input"][1]["content"][0]["type"], "input_text");
    }

    use super::*;

    fn events(raw: &str) -> Vec<sse::Event> {
        let mut p = sse::Parser::default();
        let mut out = Vec::new();
        p.push(raw.as_bytes(), &mut out);
        out
    }

    #[test]
    fn chat_to_claude_shapes() {
        let req = json!({
            "model": "claude-sonnet-5-5",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": [{"type": "text", "text": "hi"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}]},
                {"role": "assistant", "content": null, "tool_calls": [{"id": "t1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}}]},
                {"role": "tool", "tool_call_id": "t1", "content": "ok"},
                {"role": "tool", "tool_call_id": "t2", "content": "ok2"}
            ],
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}],
            "tool_choice": "required",
            "stop": "END",
            "stream": true
        });
        let out = chat_to_claude(&req);
        assert_eq!(out["system"][0]["text"], "be brief");
        assert_eq!(out["messages"].as_array().unwrap().len(), 3);
        assert_eq!(out["messages"][0]["content"][1]["source"]["media_type"], "image/png");
        assert_eq!(out["messages"][1]["content"][0]["input"]["a"], 1);
        assert_eq!(out["messages"][2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out["tool_choice"]["type"], "any");
        assert_eq!(out["stop_sequences"][0], "END");
        assert_eq!(out["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn claude_stream_to_chat() {
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu\",\"name\":\"f\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"a\\\"\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":7}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut t = ClaudeToChatStream::new("claude", true);
        let mut out = String::new();
        for e in events(raw) {
            t.on_event(&e, &mut out);
        }
        assert!(out.contains("\"content\":\"Hel\""));
        assert!(out.contains("\"name\":\"f\""));
        assert!(out.contains("\"arguments\":\"{\\\"a\\\"\""));
        assert!(out.contains("\"finish_reason\":\"tool_calls\""));
        assert!(out.contains("\"completion_tokens\":7"));
        assert!(out.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn codex_roundtrip() {
        let req = json!({"model": "gpt-5", "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "hi"}], "reasoning_effort": "high"});
        let out = chat_to_codex(&req);
        assert_eq!(out["input"][0]["role"], "developer");
        assert_eq!(out["input"][1]["content"][0]["type"], "input_text");
        assert_eq!(out["reasoning"]["effort"], "high");
        assert_eq!(out["store"], false);

        let raw = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"yo\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
        );
        let mut t = CodexToChatStream::new("gpt-5", true);
        let mut s = String::new();
        for e in events(raw) {
            t.on_event(&e, &mut s);
        }
        assert!(s.contains("\"content\":\"yo\""));
        assert!(s.contains("\"prompt_tokens\":3"));
        assert!(s.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn messages_to_codex_shapes() {
        let req = json!({
            "model": "gpt-5.6-sol",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.0.abc; cc_entrypoint=cli; cch=00000;"},
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                ]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "f", "input": {"a": 1}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}
            ],
            "tools": [{"name": "f", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "f"},
            "thinking": {"type": "enabled", "budget_tokens": 20000}
        });
        let out = messages_to_codex(&req);
        assert_eq!(out["instructions"], "You are Claude Code, Anthropic's official CLI for Claude.");
        assert_eq!(out["stream"], true);
        assert_eq!(out["store"], false);
        let input = out["input"].as_array().unwrap();
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert!(input[0]["content"][1]["image_url"].as_str().unwrap().starts_with("data:image/png;base64,AAAA"));
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "t1");
        assert_eq!(input[1]["arguments"], "{\"a\":1}");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "t1");
        assert_eq!(input[2]["output"], "ok");
        assert_eq!(out["tool_choice"]["type"], "function");
        assert_eq!(out["tool_choice"]["name"], "f");
        assert_eq!(out["reasoning"]["effort"], "high");
    }

    #[test]
    fn messages_to_codex_tool_result_image_follows() {
        let req = json!({
            "model": "gpt-5",
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "see"},
                    {"type": "image", "source": {"type": "url", "url": "https://x/y.png"}}
                ]}]}
            ]
        });
        let input = messages_to_codex(&req)["input"].as_array().unwrap().clone();
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["output"], "see");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"][0]["type"], "input_image");
        assert_eq!(input[1]["content"][0]["image_url"], "https://x/y.png");
    }

    #[test]
    fn messages_to_codex_drops_thinking_blocks() {
        let req = json!({"model": "gpt-5", "messages": [{"role": "assistant", "content": [
            {"type": "thinking", "thinking": "secret", "signature": "sig"},
            {"type": "text", "text": "hi"}
        ]}]});
        let input = messages_to_codex(&req)["input"].as_array().unwrap().clone();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"][0]["type"], "output_text");
        assert_eq!(input[0]["content"][0]["text"], "hi");
    }

    #[test]
    fn codex_to_message_non_streaming() {
        let resp = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hi"}]},
                {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{\"a\":1}"}
            ],
            "usage": {"input_tokens": 5, "output_tokens": 2, "input_tokens_details": {"cached_tokens": 1}}
        });
        let m = codex_to_message(&resp, "gpt-5.6-sol", false);
        assert_eq!(m["content"][0]["type"], "text");
        assert_eq!(m["content"][0]["text"], "hi");
        assert_eq!(m["content"][1]["type"], "tool_use");
        assert_eq!(m["content"][1]["id"], "call_1");
        assert_eq!(m["content"][1]["input"]["a"], 1);
        assert_eq!(m["stop_reason"], "tool_use");
        assert_eq!(m["usage"]["input_tokens"], 5);
        assert_eq!(m["usage"]["cache_read_input_tokens"], 1);
    }

    #[test]
    fn codex_to_anthropic_stream_text_and_usage() {
        let raw = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
        );
        let mut t = CodexToAnthropicStream::new("gpt-5.6-sol", false);
        let mut out = String::new();
        for e in events(raw) {
            t.on_event(&e, &mut out);
        }
        assert!(out.contains("event: message_start"));
        assert!(out.contains("\"type\":\"text_delta\",\"text\":\"hi\""));
        assert!(out.contains("\"stop_reason\":\"end_turn\""));
        assert!(out.contains("\"input_tokens\":5"));
        assert!(out.contains("\"output_tokens\":2"));
        assert!(out.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
    }

    #[test]
    fn codex_to_anthropic_stream_thinking_only_when_requested() {
        let raw = concat!(
            "event: response.reasoning_summary_text.delta\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking...\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{}}}\n\n",
        );
        let mut off = CodexToAnthropicStream::new("gpt-5", false);
        let mut out = String::new();
        for e in events(raw) {
            off.on_event(&e, &mut out);
        }
        assert!(!out.contains("thinking_delta"));

        let mut on = CodexToAnthropicStream::new("gpt-5", true);
        let mut out2 = String::new();
        for e in events(raw) {
            on.on_event(&e, &mut out2);
        }
        assert!(out2.contains("\"type\":\"thinking_delta\",\"thinking\":\"thinking...\""));
        assert!(out2.contains("signature_delta"));
    }

    #[test]
    fn codex_to_anthropic_stream_tool_call_chunked() {
        let raw = concat!(
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"f\"}}\n\n",
            "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"a\\\"\"}\n\n",
            "event: response.function_call_arguments.delta\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\":1}\"}\n\n",
            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\"}}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        );
        let mut t = CodexToAnthropicStream::new("gpt-5", false);
        let mut out = String::new();
        for e in events(raw) {
            t.on_event(&e, &mut out);
        }
        assert!(out.contains("\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"f\""));
        assert!(out.contains("\"partial_json\":\"{\\\"a\\\"\""));
        assert!(out.contains("\"partial_json\":\":1}\""));
        assert!(out.contains("\"stop_reason\":\"tool_use\""));
    }

    #[test]
    fn codex_to_anthropic_stream_incomplete_and_error() {
        let raw = concat!(
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"usage\":{}}}\n\n",
        );
        let mut t = CodexToAnthropicStream::new("gpt-5", false);
        let mut out = String::new();
        for e in events(raw) {
            t.on_event(&e, &mut out);
        }
        assert!(out.contains("\"stop_reason\":\"max_tokens\""));

        let mut e2 = CodexToAnthropicStream::new("gpt-5", false);
        let mut out2 = String::new();
        e2.on_event(&events("event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"server_error\",\"message\":\"boom\"}}}\n\n")[0], &mut out2);
        assert!(out2.contains("\"type\":\"error\""));
        assert!(out2.contains("\"message\":\"boom\""));
    }

    #[test]
    fn codex_to_anthropic_stream_finish_without_completion() {
        let raw = "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n";
        let mut t = CodexToAnthropicStream::new("gpt-5", false);
        let mut out = String::new();
        for e in events(raw) {
            t.on_event(&e, &mut out);
        }
        t.finish(&mut out);
        assert!(out.contains("content_block_stop"));
        assert!(out.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
    }
}
