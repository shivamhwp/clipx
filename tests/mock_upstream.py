"""Fake Anthropic + ChatGPT Codex + OAuth endpoints for clipx end-to-end tests.

Token behaviour (Bearer value):
  claude-ok*          normal responses
  claude-limited      429 with a unified reset 120s out
  claude-dead         401
  claude-old          401 until refreshed (refresh gives claude-ok-refreshed)
  codex-ok*           normal responses
  codex-expired       401 (clipx should refresh first because expires_at is past)
Every request is appended to LOG and readable at GET /_log, cleared by POST /_reset.
"""

import json
import sys
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = []
LOCK = threading.Lock()


def sse(event, data):
    return f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, *a):
        pass

    def body(self):
        n = int(self.headers.get("content-length") or 0)
        return self.rfile.read(n) if n else b""

    def send(self, status, obj, headers=None, ctype="application/json"):
        data = obj if isinstance(obj, bytes) else json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(data)))
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def record(self, raw):
        with LOCK:
            LOG.append({"path": self.path, "headers": {k.lower(): v for k, v in self.headers.items()}, "body": raw.decode("utf-8", "replace")})

    def token(self):
        return (self.headers.get("authorization") or "").removeprefix("Bearer ").strip()

    def do_GET(self):
        if self.path == "/_log":
            with LOCK:
                return self.send(200, LOG)
        self.record(b"")
        if self.path.startswith("/v1/models"):
            return self.send(200, {"data": [{"id": "claude-test-1", "type": "model"}, {"id": "claude-test-2", "type": "model"}]})
        if self.path.startswith("/codex/models"):
            return self.send(200, {"models": [{"slug": "gpt-test-codex"}]})
        self.send(404, {"error": "nope"})

    def do_POST(self):
        raw = self.body()
        if self.path == "/_reset":
            with LOCK:
                LOG.clear()
            return self.send(200, {})
        self.record(raw)
        if self.path == "/oauth/claude":
            body = json.loads(raw)
            if body.get("refresh_token") == "claude-rt-good":
                return self.send(200, {"access_token": "claude-ok-refreshed", "refresh_token": "claude-rt-good-2", "expires_in": 28800})
            return self.send(400, {"error": "invalid_grant"})
        if self.path == "/oauth/codex":
            form = dict(urllib.parse.parse_qsl(raw.decode()))
            if form.get("refresh_token") == "codex-rt-good":
                return self.send(200, {"access_token": "codex-ok-refreshed", "refresh_token": "codex-rt-good-2", "expires_in": 3600})
            return self.send(400, {"error": "invalid_grant"})
        if self.path.startswith("/v1/messages"):
            return self.claude(raw)
        if self.path.startswith("/codex/responses"):
            return self.codex(raw)
        self.send(404, {"error": "nope"})

    def claude(self, raw):
        tok = self.token()
        quota = {
            "anthropic-ratelimit-unified-5h-utilization": "0.42",
            "anthropic-ratelimit-unified-5h-reset": str(int(time.time()) + 3600),
            "anthropic-ratelimit-unified-7d-utilization": "0.10",
            "request-id": "req_mock",
        }
        if tok == "claude-limited":
            return self.send(429, {"type": "error", "error": {"type": "rate_limit_error", "message": "limited"}}, {"anthropic-ratelimit-unified-reset": str(int(time.time()) + 120), "anthropic-ratelimit-unified-status": "rejected"})
        if tok in ("claude-dead", "claude-old"):
            return self.send(401, {"type": "error", "error": {"type": "authentication_error", "message": "invalid token"}})
        if not tok.startswith("claude-ok"):
            return self.send(401, {"type": "error", "error": {"type": "authentication_error", "message": "unknown token " + tok}})
        if "count_tokens" in self.path:
            return self.send(200, {"input_tokens": 12})
        req = json.loads(raw)
        if req.get("max_tokens") == 7:
            return self.send(400, {"type": "error", "error": {"type": "invalid_request_error", "message": "bad max_tokens"}})
        text = f"hello from {tok}"
        if not req.get("stream"):
            return self.send(200, {"id": "msg_1", "type": "message", "role": "assistant", "model": req["model"], "content": [{"type": "text", "text": text}], "stop_reason": "end_turn", "usage": {"input_tokens": 11, "output_tokens": 5, "cache_read_input_tokens": 3}}, quota)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        for k, v in quota.items():
            self.send_header(k, v)
        self.end_headers()
        delay = float(req.get("metadata", {}).get("delay", 0)) if isinstance(req.get("metadata"), dict) else 0
        w = self.wfile
        w.write(sse("message_start", {"type": "message_start", "message": {"id": "msg_1", "model": req["model"], "usage": {"input_tokens": 11, "output_tokens": 1}}}))
        w.write(sse("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}))
        for part in [text[:5], text[5:]]:
            w.write(sse("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": part}}))
            w.flush()
            if delay:
                time.sleep(delay)
        w.write(sse("content_block_stop", {"type": "content_block_stop", "index": 0}))
        w.write(sse("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}))
        w.write(sse("message_stop", {"type": "message_stop"}))
        w.flush()

    def codex(self, raw):
        tok = self.token()
        if not tok.startswith("codex-ok"):
            return self.send(401, {"error": {"message": "bad token " + tok}})
        if self.headers.get("chatgpt-account-id") != "acct-123":
            return self.send(400, {"error": {"message": "missing chatgpt-account-id"}})
        req = json.loads(raw)
        if req.get("store") is not False or req.get("stream") is not True:
            return self.send(400, {"error": {"message": "store must be false and stream true"}})
        quota = {"x-codex-primary-used-percent": "37", "x-codex-primary-window-minutes": "300", "x-codex-primary-reset-after-seconds": "999"}
        self.send_response(200)
        # The real ChatGPT backend sends its stream without a content-type.
        for k, v in quota.items():
            self.send_header(k, v)
        self.end_headers()
        w = self.wfile
        item = {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "codex says hi"}]}
        w.write(sse("response.created", {"type": "response.created", "response": {"id": "resp_1"}}))
        if req.get("tools"):
            fc = {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": req["tools"][0]["name"], "arguments": ""}
            w.write(sse("response.output_item.added", {"type": "response.output_item.added", "item": fc}))
            w.write(sse("response.function_call_arguments.delta", {"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"x\":1}"}))
            fc_done = dict(fc, arguments="{\"x\":1}")
            w.write(sse("response.output_item.done", {"type": "response.output_item.done", "item": fc_done}))
            out = [fc_done]
        else:
            w.write(sse("response.output_text.delta", {"type": "response.output_text.delta", "delta": "codex says "}))
            w.write(sse("response.output_text.delta", {"type": "response.output_text.delta", "delta": "hi"}))
            w.write(sse("response.output_item.done", {"type": "response.output_item.done", "item": item}))
            out = []
        w.write(sse("response.completed", {"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "output": out, "usage": {"input_tokens": 20, "output_tokens": 4, "input_tokens_details": {"cached_tokens": 8}}}}))
        w.flush()


if __name__ == "__main__":
    port = int(sys.argv[1])
    ThreadingHTTPServer.request_queue_size = 4096
    srv = ThreadingHTTPServer(("127.0.0.1", port), H)
    srv.daemon_threads = True
    srv.serve_forever()
