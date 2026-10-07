"""End-to-end test: real clipx binary against a fake upstream, plus a local relay.

Run: python3 tests/e2e.py [path/to/clipx]
"""

import http.client
import json
import os
import re
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target/release/clipx")
MOCK, PORT, RELAY = 18901, 18902, 18903
HOME = tempfile.mkdtemp(prefix="clipx-e2e-")
ENV = dict(os.environ, CLIPX_HOME=HOME, CLIPX_CLAUDE_API=f"http://127.0.0.1:{MOCK}", CLIPX_CLAUDE_TOKEN_URL=f"http://127.0.0.1:{MOCK}/oauth/claude",
           CLIPX_CODEX_API=f"http://127.0.0.1:{MOCK}/codex", CLIPX_CODEX_TOKEN_URL=f"http://127.0.0.1:{MOCK}/oauth/codex",
           CLIPX_GEMINI_API=f"http://127.0.0.1:{MOCK}", CLIPX_GEMINI_TOKEN_URL=f"http://127.0.0.1:{MOCK}/oauth/gemini",
           CLIPX_GEMINI_USERINFO_URL=f"http://127.0.0.1:{MOCK}/oauth2/v2/userinfo", CLIPX_LOG="warn")
# A fake `tailscale` on PATH, so remote access through Tailscale can be tested offline.
FAKE_BIN = os.path.join(HOME, "fakebin")
os.makedirs(FAKE_BIN)
with open(os.path.join(FAKE_BIN, "tailscale"), "w") as f:
    f.write(f"""#!{sys.executable}
import json, os, sys
d = {HOME!r}
state, log = os.path.join(d, "ts-state.json"), os.path.join(d, "ts.log")
a = sys.argv[1:]
open(log, "a").write(" ".join(a) + "\\n")
if a[:2] == ["status", "--json"]:
    print(json.dumps({{"BackendState": "Running", "Self": {{"DNSName": "box.tailnet.ts.net."}}}}))
elif a[:2] == ["serve", "status"]:
    print(open(state).read() if os.path.exists(state) else "{{}}")
elif a[0] in ("funnel", "serve") and a[-1] == "off":
    port = next(x for x in a if x.startswith("--https=")).split("=")[1]
    st = json.load(open(state)) if os.path.exists(state) else {{}}
    st.get("Web", {{}}).get("box.tailnet.ts.net:" + port, {{}}).get("Handlers", {{}}).pop("/", None)
    json.dump(st, open(state, "w"))
elif a[0] in ("funnel", "serve"):
    port = next(x for x in a if x.startswith("--https=")).split("=")[1]
    hp = "box.tailnet.ts.net:" + port
    st = {{"Web": {{hp: {{"Handlers": {{"/": {{"Proxy": a[-1]}}}}}}}}, "AllowFunnel": {{hp: a[0] == "funnel"}}}}
    json.dump(st, open(state, "w"))
""")
os.chmod(os.path.join(FAKE_BIN, "tailscale"), 0o755)
ENV["PATH"] = FAKE_BIN + os.pathsep + ENV.get("PATH", "")
PROCS = []
PASSED = []


def check(name, cond, detail=""):
    if not cond:
        print(f"FAIL {name} {detail}")
        cleanup()
        sys.exit(1)
    PASSED.append(name)
    print(f"ok   {name}")


def cleanup():
    for p in PROCS:
        if p.poll() is None:
            p.send_signal(signal.SIGTERM)
            try:
                p.wait(5)
            except subprocess.TimeoutExpired:
                p.kill()


def wait_port(port, secs=10):
    for _ in range(secs * 20):
        try:
            socket.create_connection(("127.0.0.1", port), 0.2).close()
            return True
        except OSError:
            time.sleep(0.05)
    return False


def req(method, path, body=None, headers=None, port=PORT, raw=False, timeout=30):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    data = json.dumps(body).encode() if isinstance(body, (dict, list)) else body
    h = {"content-type": "application/json"}
    h.update(headers or {})
    c.request(method, path, body=data, headers=h)
    r = c.getresponse()
    out = r.read()
    c.close()
    if raw:
        return r.status, dict((k.lower(), v) for k, v in r.getheaders()), out
    try:
        return r.status, json.loads(out) if out else None
    except ValueError:
        return r.status, out.decode("utf-8", "replace")


def sse_text(body):
    out = ""
    for line in body.decode().splitlines():
        if line.startswith("data: {"):
            d = json.loads(line[6:])
            if d.get("type") == "content_block_delta":
                out += d["delta"].get("text", "")
    return out


def mock_log():
    return req("GET", "/_log", port=MOCK)[1]


def mock_reset():
    req("POST", "/_reset", {}, port=MOCK)


def last_upstream(prefix):
    return [e for e in mock_log() if e["path"].startswith(prefix)][-1]


def start_serve():
    p = subprocess.Popen([BIN, "serve"], env=ENV, stdout=subprocess.DEVNULL, stderr=open(os.path.join(HOME, "serve.log"), "a"))
    PROCS.append(p)
    check("server starts", wait_port(PORT))
    return p


def rss_kb(pid, field="VmRSS"):
    for line in open(f"/proc/{pid}/status"):
        if line.startswith(field + ":"):
            return int(line.split()[1])
    return 0


try:
    mock = subprocess.Popen([sys.executable, os.path.join(ROOT, "tests/mock_upstream.py"), str(MOCK)])
    PROCS.append(mock)
    check("mock upstream starts", wait_port(MOCK))

    out = subprocess.run([BIN, "setup", "--no-service", "--tunnel", "off", "--port", str(PORT)], env=ENV, capture_output=True, text=True)
    check("setup writes config", out.returncode == 0, out.stderr)
    KEY = re.search(r"api key\s+(sk-clipx-\S+)", out.stdout).group(1)
    ADMIN = re.search(r'admin_token = "([^"]+)"', open(os.path.join(HOME, "config.toml")).read()).group(1)
    A = {"authorization": "Bearer " + ADMIN}
    K = {"authorization": "Bearer " + KEY}
    server = start_serve()

    accounts = [
        {"id": "aaa", "provider": "claude", "label": "claude-a", "email": "a@x", "access_token": "claude-limited", "expires_at": 0},
        {"id": "bbb", "provider": "claude", "label": "claude-b", "email": "b@x", "access_token": "claude-ok-b", "expires_at": 0},
        {"id": "ccc", "provider": "claude", "label": "claude-c", "email": "c@x", "access_token": "claude-old", "refresh_token": "claude-rt-good", "expires_at": 0},
        {"id": "xxx", "provider": "codex", "label": "codex-x", "email": "x@x", "access_token": "codex-expired", "refresh_token": "codex-rt-good", "expires_at": 1, "account_id": "acct-123"},
    ]
    s, r = req("POST", "/api/accounts/import", accounts, A)
    check("import accounts", s == 200 and len(r["accounts"]) == 4, r)
    # CLIProxyAPI format import
    s, r = req("POST", "/api/accounts/import", {"type": "claude", "access_token": "claude-ok-cpa", "refresh_token": "r", "email": "cpa@x", "expired": "2099-01-01T00:00:00+05:30", "no_refresh": True}, A)
    check("import cliproxyapi file", s == 200 and r["accounts"][0]["no_refresh"] and r["accounts"][0]["expires_at"] == 4070889000, r)
    req("PATCH", "/api/accounts/" + r["accounts"][0]["id"], {"disabled": True}, A)

    s, r = req("GET", "/api/state")
    check("admin api needs token", s == 401)
    s, r = req("POST", "/v1/messages", {"model": "claude-x", "messages": []})
    check("proxy needs api key", s == 401 and r["type"] == "error")
    s, r = req("POST", "/v1/messages", {"model": "claude-x", "messages": []}, {"x-api-key": "sk-clipx-wrong"})
    check("proxy rejects wrong key", s == 401)

    s, r = req("GET", "/v1/models", headers=K)
    ids = [m["id"] for m in r["data"]]
    check("models from upstream", "claude-test-1" in ids and "gpt-test-codex" in ids, ids)

    # Native Claude Code passthrough: body bytes untouched, auth swapped, oauth beta added.
    native = json.dumps({"model": "claude-sonnet-x", "max_tokens": 50, "stream": True, "system": [{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.999.abc; cc_entrypoint=cli; cch=12345;"}], "messages": [{"role": "user", "content": "hi"}]}).encode()
    seen_tokens = set()
    for i in range(4):
        mock_reset()
        s, h, body = req("POST", "/v1/messages?beta=true", native, dict(K, **{"user-agent": "claude-cli/2.1.999 (external, cli)", "anthropic-beta": "claude-code-20250219,fine-grained-tool-streaming-2025-05-14"}), raw=True)
        check(f"native stream {i} ok", s == 200 and sse_text(body).startswith("hello from") and h.get("content-type", "").startswith("text/event-stream"), (s, body[:300]))
        up = last_upstream("/v1/messages")
        seen_tokens.add(up["headers"]["authorization"])
        check(f"native body untouched {i}", up["body"].encode() == native)
        check(f"native beta {i}", "oauth-2025-04-20" in up["headers"]["anthropic-beta"] and "fine-grained-tool-streaming" in up["headers"]["anthropic-beta"], up["headers"]["anthropic-beta"])
        check(f"native ua kept {i}", up["headers"]["user-agent"].startswith("claude-cli/2.1.999"))
        check(f"no client key leaks upstream {i}", KEY not in json.dumps(up))
        check(f"quota headers pass to client {i}", "anthropic-ratelimit-unified-5h-utilization" in h)
    check("rotation spread over healthy accounts", seen_tokens == {"Bearer claude-ok-b", "Bearer claude-ok-refreshed"}, seen_tokens)

    s, st = req("GET", "/api/state", headers=A)
    accs = {a["label"]: a for a in st["accounts"]}
    check("429 account cooling", accs["claude-a"]["status"] == "cooling" and accs["claude-a"]["cooldown_until"] > time.time() + 60, accs["claude-a"])
    check("401 account refreshed", accs["claude-c"]["status"] == "ready" and accs["claude-c"]["last_refresh"] > 0, accs["claude-c"])
    saved = json.load(open(os.path.join(HOME, "accounts", "ccc.json")))
    check("refreshed token persisted", saved["access_token"] == "claude-ok-refreshed" and saved["refresh_token"] == "claude-rt-good-2", saved)
    check("quota read from headers", any(q["name"] == "5h" and abs(q["used_pct"] - 42) < 0.01 for q in accs["claude-b"]["quota"]), accs["claude-b"]["quota"])
    check("claude code version learned", st["claude_version"] == "2.1.999", st["claude_version"])

    # Non-native client gets Claude Code identity.
    mock_reset()
    s, r = req("POST", "/v1/messages", {"model": "claude-sonnet-x", "max_tokens": 100, "system": "be nice", "messages": [{"role": "user", "content": "hello there friend"}]}, dict(K, **{"user-agent": "python-httpx/0.27"}))
    check("cloaked request ok", s == 200 and r["content"][0]["text"].startswith("hello from"), r)
    up = last_upstream("/v1/messages")
    b = json.loads(up["body"])
    check("billing block first", b["system"][0]["text"].startswith("x-anthropic-billing-header: cc_version=2.1.999.") and "cch=00000" not in b["system"][0]["text"], b["system"][0])
    check("agent prompt second", b["system"][1]["text"] == "You are Claude Code, Anthropic's official CLI for Claude." and b["system"][2]["text"] == "be nice")
    uid = json.loads(b["metadata"]["user_id"])
    check("metadata user id", len(uid["device_id"]) == 64 and len(uid["session_id"]) == 36, uid)
    check("cloak headers", up["headers"]["user-agent"].startswith("claude-cli/2.1.999") and up["headers"]["x-app"] == "cli" and "claude-code-20250219" in up["headers"]["anthropic-beta"], up["headers"])

    # Client errors come back as-is and do not rotate.
    mock_reset()
    s, r = req("POST", "/v1/messages", {"model": "claude-x", "max_tokens": 7, "messages": [{"role": "user", "content": "x"}]}, K)
    check("400 returned unchanged", s == 400 and r["error"]["message"] == "bad max_tokens", r)
    check("400 not retried", len([e for e in mock_log() if e["path"].startswith("/v1/messages")]) == 1)

    s, r = req("POST", "/v1/messages/count_tokens", {"model": "claude-x", "messages": [{"role": "user", "content": "x"}]}, K)
    check("count_tokens", s == 200 and r["input_tokens"] == 12, r)

    # Pinning by path and by model suffix.
    mock_reset()
    s, r = req("POST", "/a/claude-b/v1/messages", {"model": "claude-x", "max_tokens": 10, "messages": [{"role": "user", "content": "x"}]}, K)
    check("path pin", s == 200 and last_upstream("/v1/messages")["headers"]["authorization"] == "Bearer claude-ok-b")
    s, r = req("POST", "/v1/messages", {"model": "claude-x@claude-c", "max_tokens": 10, "messages": [{"role": "user", "content": "x"}]}, K)
    up = last_upstream("/v1/messages")
    check("model@label pin", s == 200 and up["headers"]["authorization"] == "Bearer claude-ok-refreshed" and json.loads(up["body"])["model"] == "claude-x")
    s, r = req("POST", "/a/claude-a/v1/messages", {"model": "claude-x", "max_tokens": 10, "messages": [{"role": "user", "content": "x"}]}, K)
    check("pinned cooling account refuses", s == 429, (s, r))
    off = next(a["label"] for a in req("GET", "/api/state", headers=A)[1]["accounts"] if a["status"] == "disabled")
    s, h, body = req("POST", f"/a/{off}/v1/messages", {"model": "claude-x", "max_tokens": 10, "messages": [{"role": "user", "content": "x"}]}, K, raw=True)
    check("pinned disabled account is not a rate limit", s == 503 and b"rate_limit" not in body and "retry-after" not in h, (s, body))
    s, recent = req("GET", "/api/requests", headers=A)
    check("failed requests reach the request log", recent[0]["status"] == 503 and recent[0]["account"] == off and recent[1]["status"] == 429 and recent[1]["account"] == "claude-a", recent[:2])

    # Chat completions -> Claude
    s, r = req("POST", "/v1/chat/completions", {"model": "claude-x", "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "hi"}]}, K)
    check("chat->claude", s == 200 and r["choices"][0]["message"]["content"].startswith("hello from") and r["usage"]["prompt_tokens"] == 14, r)
    s, h, body = req("POST", "/v1/chat/completions", {"model": "claude-x", "stream": True, "stream_options": {"include_usage": True}, "messages": [{"role": "user", "content": "hi"}]}, K, raw=True)
    text = "".join(json.loads(l[6:])["choices"][0]["delta"].get("content", "") for l in body.decode().splitlines() if l.startswith("data: {") and json.loads(l[6:])["choices"])
    check("chat->claude stream", s == 200 and text.startswith("hello from") and body.decode().rstrip().endswith("data: [DONE]") and '"completion_tokens":5' in body.decode(), body[:400])

    # Chat completions -> Codex (expired token refreshes first)
    mock_reset()
    s, r = req("POST", "/v1/chat/completions", {"model": "gpt-5", "messages": [{"role": "user", "content": "hi"}]}, K)
    check("chat->codex", s == 200 and r["choices"][0]["message"]["content"] == "codex says hi" and r["usage"]["prompt_tokens"] == 20, r)
    check("expired codex token refreshed before use", last_upstream("/codex/responses")["headers"]["authorization"] == "Bearer codex-ok-refreshed" and json.load(open(os.path.join(HOME, "accounts", "xxx.json")))["refresh_token"] == "codex-rt-good-2")
    s, h, body = req("POST", "/v1/chat/completions", {"model": "gpt-5", "stream": True, "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object"}}}], "messages": [{"role": "user", "content": "hi"}]}, K, raw=True)
    check("chat->codex tool stream", s == 200 and '"name":"f"' in body.decode() and '"finish_reason":"tool_calls"' in body.decode(), body[:500])

    # Responses API
    s, r = req("POST", "/v1/responses", {"model": "gpt-5", "input": "hi", "max_output_tokens": 5}, K)
    check("responses non-stream aggregates", s == 200 and r["output"][0]["content"][0]["text"] == "codex says hi", r)
    up = json.loads(last_upstream("/codex/responses")["body"])
    check("responses body fixed for codex", up["store"] is False and up["stream"] is True and "max_output_tokens" not in up and up["instructions"] == "")
    s, h, body = req("POST", "/v1/responses", {"model": "gpt-5", "input": "hi", "stream": True}, dict(K, originator="codex_cli_rs", **{"user-agent": "codex_cli_rs/0.160.1"}), raw=True)
    check("responses stream passthrough", s == 200 and b"response.completed" in body and h.get("x-codex-primary-used-percent") == "37")
    check("untyped codex stream gets a content-type", h.get("content-type") == "text/event-stream", h.get("content-type"))
    time.sleep(0.2)
    last = req("GET", "/api/requests?limit=1", headers=A)[1][0]
    check("usage read from untyped codex stream", last["input"] == 20 and last["output"] == 4, last)
    up = last_upstream("/codex/responses")["headers"]
    check("codex headers", up["originator"] == "codex_cli_rs" and up["user-agent"].startswith("codex_cli_rs/0.160.1") and up["chatgpt-account-id"] == "acct-123")

    # Claude Code with a GPT model: /v1/messages routes to Codex and translates both ways.
    mock_reset()
    s, r = req("POST", "/v1/messages", {"model": "gpt-5", "max_tokens": 50, "messages": [{"role": "user", "content": "hi"}]}, K)
    check(
        "messages->codex non-stream",
        s == 200 and r["type"] == "message" and r["role"] == "assistant" and r["content"][0]["text"] == "codex says hi" and r["stop_reason"] == "end_turn",
        r,
    )
    check("messages->codex usage shape", r["usage"]["input_tokens"] == 20 and r["usage"]["output_tokens"] == 4 and r["usage"]["cache_read_input_tokens"] == 8, r["usage"])
    up = json.loads(last_upstream("/codex/responses")["body"])
    check("messages->codex request fixed for codex", up["store"] is False and up["stream"] is True and "max_tokens" not in up)

    s, h, body = req("POST", "/v1/messages", {"model": "gpt-5", "max_tokens": 50, "stream": True, "messages": [{"role": "user", "content": "hi"}]}, K, raw=True)
    text = body.decode()
    check(
        "messages->codex stream shape",
        s == 200 and "event: message_start" in text and "event: message_stop" in text and '"type":"text_delta","text":"codex says "' in text and h["content-type"].startswith("text/event-stream"),
        text[:600],
    )

    # Tool call round trip: Anthropic tool shape in, Anthropic tool_use block out.
    mock_reset()
    s, h, body = req(
        "POST",
        "/v1/messages",
        {"model": "gpt-5", "max_tokens": 50, "tools": [{"name": "f", "input_schema": {"type": "object"}}], "messages": [{"role": "user", "content": "use the tool"}], "stream": True},
        K,
        raw=True,
    )
    text = body.decode()
    check(
        "messages->codex tool call stream",
        s == 200 and '"type":"tool_use","id":"call_1","name":"f"' in text and '"partial_json":"{\\"x\\":1}"' in text and '"stop_reason":"tool_use"' in text,
        text[:800],
    )
    up = json.loads(last_upstream("/codex/responses")["body"])
    check("messages->codex tool request shape", up["tools"][0] == {"type": "function", "name": "f", "description": "", "parameters": {"type": "object"}, "strict": False} and up["input"][0]["role"] == "user")

    # count_tokens for a GPT model is a local estimate; no upstream call at all.
    mock_reset()
    s, r = req("POST", "/v1/messages/count_tokens", {"model": "gpt-5", "messages": [{"role": "user", "content": "how many tokens is this, roughly"}]}, K)
    check("messages count_tokens gpt model", s == 200 and isinstance(r["input_tokens"], int) and r["input_tokens"] > 0, r)
    check("count_tokens does not call codex upstream", len([e for e in mock_log() if e["path"].startswith("/codex/responses")]) == 0)

    # Usage still gets recorded for the translated path.
    time.sleep(0.2)
    last = req("GET", "/api/requests?limit=1", headers=A)[1][0]
    check("usage recorded for messages->codex", last["model"] == "gpt-5" and last["input"] == 20 and last["output"] == 4, last)

    time.sleep(0.3)
    s, u = req("GET", "/api/usage?days=7", headers=A)
    check("usage by account", u["by_account"]["claude-b"]["requests"] >= 2 and u["by_account"]["codex-x"]["output"] >= 4, u["by_account"])
    s, st = req("GET", "/api/state", headers=A)
    check("codex quota parsed", any(q["name"] == "5h" and q["used_pct"] == 37 for q in {a["label"]: a for a in st["accounts"]}["codex-x"]["quota"]))

    # ---------------- gemini
    accounts_g = [
        {"id": "gaa", "provider": "gemini", "label": "gemini-a", "email": "a@g", "access_token": "gemini-limited", "project_id": "proj-a", "expires_at": 0},
        {"id": "gbb", "provider": "gemini", "label": "gemini-b", "email": "b@g", "access_token": "gemini-ok-b", "project_id": "proj-b", "expires_at": 0},
        {"id": "gcc", "provider": "gemini", "label": "gemini-c", "email": "c@g", "access_token": "gemini-old", "refresh_token": "gemini-rt-good", "project_id": "proj-c", "expires_at": 0},
    ]
    s, r = req("POST", "/api/accounts/import", accounts_g, A)
    check("import gemini accounts", s == 200 and len(r["accounts"]) == 3, r)

    # CLIProxyAPI gemini-cli auth file format: OAuth2 token nested under "token".
    cpa_gemini = {
        "type": "gemini",
        "token": {"access_token": "gemini-ok-cpa", "refresh_token": "gemini-rt-cpa", "expiry": "2099-01-01T00:00:00Z"},
        "project_id": "proj-cpa",
        "email": "cpa@g",
    }
    s, r = req("POST", "/api/accounts/import", cpa_gemini, A)
    check("import cliproxyapi gemini file", s == 200 and r["accounts"][0]["email"] == "cpa@g" and r["accounts"][0]["provider"] == "gemini", r)
    req("PATCH", "/api/accounts/" + r["accounts"][0]["id"], {"disabled": True}, A)

    s, r = req("GET", "/v1/models", headers=K)
    check("gemini models listed", any(m["id"].startswith("gemini") for m in r["data"]), r)

    mock_reset()
    s, r = req("POST", "/v1/chat/completions", {"model": "gemini-x", "messages": [{"role": "user", "content": "hi"}]}, K)
    check("chat->gemini", s == 200 and r["choices"][0]["message"]["content"].startswith("hello from") and r["usage"]["prompt_tokens"] == 15, r)

    s, h, body = req("POST", "/v1/chat/completions", {"model": "gemini-x", "stream": True, "stream_options": {"include_usage": True}, "messages": [{"role": "user", "content": "hi"}]}, K, raw=True)
    btext = body.decode()
    text = "".join(json.loads(l[6:])["choices"][0]["delta"].get("content", "") for l in btext.splitlines() if l.startswith("data: {") and json.loads(l[6:])["choices"])
    check("chat->gemini stream", s == 200 and text.startswith("hello from") and btext.rstrip().endswith("data: [DONE]") and '"completion_tokens":7' in btext, body[:400])

    # Native Gemini API
    mock_reset()
    s, r = req("POST", "/v1beta/models/gemini-x:generateContent", {"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}, K)
    check("native generateContent", s == 200 and r["candidates"][0]["content"]["parts"][0]["text"].startswith("hello from") and r["usageMetadata"]["promptTokenCount"] == 15, r)

    s, h, body = req("POST", "/v1beta/models/gemini-x:streamGenerateContent?alt=sse", {"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}, K, raw=True)
    check("native streamGenerateContent", s == 200 and b'"text":"hello' in body and b"finishReason" in body, body[:300])

    s, r = req("POST", "/v1beta/models/gemini-x:countTokens", {"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}, K)
    check("native countTokens", s == 200 and r["totalTokens"] == 9, r)

    s, r = req("POST", "/v1beta/models/gemini-x:generateContent", {"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}, {"x-goog-api-key": KEY})
    check("x-goog-api-key auth", s == 200, r)

    s, r = req("GET", "/v1/models?key=" + KEY)
    check("?key= auth", s == 200, r)

    # 429 cooldown + rotation, and refresh of an expired access token.
    mock_reset()
    seen = set()
    for i in range(3):
        s, r = req("POST", "/v1/chat/completions", {"model": "gemini-x", "messages": [{"role": "user", "content": "x"}]}, K)
        check(f"gemini rotation {i} ok", s == 200, r)
        seen.add(last_upstream("/v1internal:")["headers"]["authorization"])
    check("gemini rotation avoids limited account", "Bearer gemini-limited" not in seen, seen)

    s, st = req("GET", "/api/state", headers=A)
    accs = {a["label"]: a for a in st["accounts"]}
    check("gemini 429 account cooling", accs["gemini-a"]["status"] == "cooling" and accs["gemini-a"]["cooldown_until"] > time.time() + 30, accs["gemini-a"])
    check("gemini 401 account refreshed", accs["gemini-c"]["status"] == "ready" and accs["gemini-c"]["last_refresh"] > 0, accs["gemini-c"])
    saved = json.load(open(os.path.join(HOME, "accounts", "gcc.json")))
    check("gemini refreshed token persisted", saved["access_token"] == "gemini-ok-refreshed" and saved["refresh_token"] == "gemini-rt-good-2", saved)

    time.sleep(0.2)
    s, u = req("GET", "/api/usage?days=7", headers=A)
    gemini_requests = u["by_account"].get("gemini-b", {}).get("requests", 0) + u["by_account"].get("gemini-c", {}).get("requests", 0)
    check("gemini usage recorded", gemini_requests >= 1, u["by_account"])

    # Full login flow (start -> paste callback -> token exchange -> userinfo -> loadCodeAssist/onboardUser).
    s, r = req("POST", "/api/login/start", {"provider": "gemini"}, A)
    check("gemini login start", s == 200 and "accounts.google.com" in r["url"] and "127.0.0.1:1456" in r["hint"], r)
    state = urllib.parse.parse_qs(urllib.parse.urlparse(r["url"]).query)["state"][0]
    s, r = req("POST", "/api/login/complete", {"flow_id": r["flow_id"], "callback": f"http://127.0.0.1:1456/oauth2callback?code=gemini-code-good&state={state}"}, A)
    check("gemini login completes", s == 200 and r["account"]["email"] == "new-gemini-login@x" and r["account"]["provider"] == "gemini", r)
    new_acc = json.load(open(os.path.join(HOME, "accounts", r["account"]["id"] + ".json")))
    check("gemini login ran code assist setup", new_acc["project_id"] == "proj-new-login" and new_acc["tier"] == "Free", new_acc)

    s, r = req("POST", "/api/login/start", {"provider": "claude"}, A)
    s, r = req("POST", "/api/login/complete", {"flow_id": r["flow_id"], "callback": "used-code"}, A)
    check("rejected login code says what to do", s >= 400 and r["error"].startswith("That code didn't work"), (s, r))

    # Keys
    s, r = req("POST", "/api/keys", {"name": "second"}, A)
    k2 = r["key"]
    s, _ = req("GET", "/v1/models", headers={"authorization": "Bearer " + k2})
    check("new key works", s == 200)
    req("DELETE", "/api/keys/" + r["id"], headers=A)
    s, _ = req("GET", "/v1/models", headers={"authorization": "Bearer " + k2})
    check("revoked key rejected", s == 401)

    s, h, body = req("GET", "/", raw=True)
    check("dashboard served", s == 200 and b"<title>clipx</title>" in body)

    # ---------------- linked account: follows another tool's auth file
    linked = os.path.join(HOME, "cpa-claude-linked.json")
    json.dump({"type": "claude", "access_token": "claude-stale-linked", "refresh_token": "theirs", "email": "linked@x"}, open(linked, "w"))
    s, r = req("POST", "/api/accounts/import", dict(json.load(open(linked)), linked=linked), A)
    lacc = r["accounts"][0]
    check("import linked account", s == 200 and lacc["linked"] == linked and lacc["no_refresh"], r)
    json.dump({"type": "claude", "access_token": "claude-ok-linked", "refresh_token": "theirs-2", "email": "linked@x"}, open(linked, "w"))
    mock_reset()
    s, r = req("POST", f"/a/{lacc['label']}/v1/messages", {"model": "claude-x", "max_tokens": 5, "messages": [{"role": "user", "content": "hi"}]}, K)
    toks = [e["headers"].get("authorization") for e in mock_log() if e["path"].startswith("/v1/messages")]
    check("linked account reloads its file after a 401", s == 200 and toks[-1] == "Bearer claude-ok-linked", (s, toks))
    check("linked account never calls the token endpoint", not any("/oauth/" in e["path"] for e in mock_log()))
    s, st = req("GET", "/api/state", headers=A)
    check("linked account ready", {a["id"]: a for a in st["accounts"]}[lacc["id"]]["status"] == "ready")
    req("DELETE", "/api/accounts/" + lacc["id"], headers=A)

    # ---------------- tailscale serve / funnel (fake tailscale binary)
    s, r = req("PATCH", "/api/connect", {"mode": "tailscale", "ts_port": 10000}, A)
    for _ in range(50):
        st = req("GET", "/api/state", headers=A)[1]
        if st["tunnel"]["connected"]:
            break
        time.sleep(0.1)
    check("tailscale funnel connects", st["tunnel"]["public_url"] == "https://box.tailnet.ts.net:10000", st["tunnel"])
    tslog = open(os.path.join(HOME, "ts.log")).read()
    check("tailscale funnel mounts clipx at /", f"funnel --bg --yes --https=10000 http://127.0.0.1:{PORT}" in tslog, tslog)
    req("PATCH", "/api/connect", {"mode": "off"}, A)
    time.sleep(0.5)
    tslog = open(os.path.join(HOME, "ts.log")).read()
    check("leaving tailscale removes only our mount", "funnel --yes --https=10000 --set-path / off" in tslog, tslog)

    # ---------------- relay
    relay = subprocess.Popen([BIN, "relay", "--listen", f"127.0.0.1:{RELAY}", "--data", os.path.join(HOME, "relay")], env=dict(ENV, CLIPX_LOG="warn"), stderr=open(os.path.join(HOME, "relay.log"), "a"))
    PROCS.append(relay)
    check("relay starts", wait_port(RELAY))
    s, r = req("PATCH", "/api/connect", {"mode": "relay", "relay": f"http://127.0.0.1:{RELAY}", "name": "e2e"}, A)
    check("connect via api", s == 200, r)
    for _ in range(50):
        s, st = req("GET", "/api/state", headers=A)
        if st["tunnel"]["connected"]:
            break
        time.sleep(0.1)
    check("tunnel connected", st["tunnel"]["connected"] and st["tunnel"]["public_url"] == f"http://127.0.0.1:{RELAY}/t/e2e/", st["tunnel"])
    s, h, body = req("GET", "/t/e2e", port=RELAY, raw=True)
    check("relay redirects to slash", s == 308 and h["location"] == "/t/e2e/")
    s, h, body = req("GET", "/t/e2e/", port=RELAY, raw=True)
    check("dashboard through relay", s == 200 and b"<title>clipx</title>" in body)
    s, st = req("GET", "/t/e2e/api/state", headers=A, port=RELAY)
    check("admin api through relay", s == 200 and st["via_tunnel"] is True)
    s, r = req("PATCH", "/t/e2e/api/connect", {"mode": "off"}, A, port=RELAY)
    check("cannot cut own tunnel from tunnel", s == 400)
    s, h, body = req("POST", "/t/e2e/v1/messages", {"model": "claude-x", "max_tokens": 10, "stream": True, "messages": [{"role": "user", "content": "x"}]}, K, port=RELAY, raw=True)
    check("streaming through relay", s == 200 and b"message_stop" in body and h["content-type"].startswith("text/event-stream"), (s, body[:200]))
    big = {"model": "claude-x", "max_tokens": 10, "messages": [{"role": "user", "content": "y" * 300000}]}
    s, r = req("POST", "/t/e2e/v1/messages", big, K, port=RELAY)
    check("large body through relay", s == 200 and len(json.loads(last_upstream("/v1/messages")["body"])["messages"][0]["content"]) == 300000)
    huge = {"model": "claude-x", "max_tokens": 10, "messages": [{"role": "user", "content": "z" * 20_000_000}]}
    s, r = req("POST", "/t/e2e/v1/messages", huge, K, port=RELAY, timeout=60)
    check("20 MB body through relay", s == 200, (s, str(r)[:200]))
    s, r = req("POST", "/t/e2e/v1/chat/completions", {"model": "gpt-5", "messages": [{"role": "user", "content": "hi"}]}, K, port=RELAY)
    check("chat through relay", s == 200 and r["choices"][0]["message"]["content"] == "codex says hi")
    s, _ = req("GET", "/t/nobody/", port=RELAY)
    check("unknown box 502", s == 502)

    # Another box cannot steal the name.
    c = socket.create_connection(("127.0.0.1", RELAY))
    c.sendall(b"GET /_clipx/connect?name=e2e HTTP/1.1\r\nHost: x\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nAuthorization: Bearer ak-someone-else-entirely\r\n\r\n")
    head = c.recv(4096).decode()
    c.close()
    check("relay protects claimed names", head.startswith("HTTP/1.1 403"), head[:80])

    # Client hangs up mid-stream through the relay; box keeps working.
    c = http.client.HTTPConnection("127.0.0.1", RELAY, timeout=10)
    c.request("POST", "/t/e2e/v1/messages", body=json.dumps({"model": "claude-x", "max_tokens": 10, "stream": True, "metadata": {"delay": 1.0}, "messages": [{"role": "user", "content": "x"}]}), headers=dict(K, **{"content-type": "application/json"}))
    rr = c.getresponse()
    rr.read(50)
    c.close()
    time.sleep(0.3)
    s, r = req("POST", "/t/e2e/v1/messages", {"model": "claude-x", "max_tokens": 10, "messages": [{"role": "user", "content": "x"}]}, K, port=RELAY)
    check("relay survives client hangup", s == 200)

    # Box reconnects when the relay restarts.
    relay.send_signal(signal.SIGTERM)
    relay.wait(5)
    relay = subprocess.Popen([BIN, "relay", "--listen", f"127.0.0.1:{RELAY}", "--data", os.path.join(HOME, "relay")], env=ENV, stderr=open(os.path.join(HOME, "relay.log"), "a"))
    PROCS.append(relay)
    wait_port(RELAY)
    ok = False
    for _ in range(80):
        s, _ = req("GET", "/t/e2e/healthz", port=RELAY)
        if s == 200:
            ok = True
            break
        time.sleep(0.1)
    check("box reconnects after relay restart", ok)

    # ---------------- load + memory
    idle = rss_kb(server.pid)
    errors = []
    payload = json.dumps({"model": "claude-x", "max_tokens": 10, "stream": True, "metadata": {"delay": 0.2}, "messages": [{"role": "user", "content": "x"}]})

    def worker(n):
        for _ in range(n):
            try:
                c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=30)
                c.request("POST", "/v1/messages", body=payload, headers=dict(K, **{"content-type": "application/json"}))
                r = c.getresponse()
                b = r.read()
                if r.status != 200 or b"message_stop" not in b:
                    errors.append((r.status, b[:100]))
                c.close()
            except Exception as e:
                errors.append(repr(e))

    t0 = time.time()
    threads = [threading.Thread(target=worker, args=(5,)) for _ in range(200)]
    [t.start() for t in threads]
    peak = 0
    while any(t.is_alive() for t in threads):
        peak = max(peak, rss_kb(server.pid))
        time.sleep(0.05)
    took = time.time() - t0
    check("1000 streamed requests at 200 concurrency", not errors, errors[:3])
    print(f"     idle rss {idle/1024:.1f} MB, peak rss during load {peak/1024:.1f} MB, 1000 streams (each ~0.4s) in {took:.1f}s")

    # Restart keeps state.
    server.send_signal(signal.SIGTERM)
    server.wait(10)
    server = start_serve()
    s, st = req("GET", "/api/state", headers=A)
    s, u = req("GET", "/api/usage?days=1", headers=A)
    total = sum(t["requests"] for t in u["by_account"].values())
    check("state survives restart", len(st["accounts"]) == 10 and total > 1000, (len(st["accounts"]), total))

    print(f"\nall {len(PASSED)} checks passed")
finally:
    cleanup()
