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
}
