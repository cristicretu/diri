#!/usr/bin/env python3
"""Scripted model API for driving real Agent CLIs: one port, three wire formats.

  POST /v1/chat/completions  OpenAI chat completions (OpenCode)
  POST /v1/responses         OpenAI Responses (Codex)
  POST /v1/messages          Anthropic Messages (Claude Code)

Every reply is streamed when the client asks for a stream. The reply to the
latest user text is scripted from it:

  LINES n  -> n numbered reply lines, enough to push earlier turns into the
              terminal's history
  RUNCMD   -> one shell tool call printing a marker, then "DONECMD" once the
              tool result arrives
  other    -> the first ALLCAPS word of the prompt, or OK

A request without tools (titles, summaries) gets a short title. Requests are
logged to argv[2]. The server is also the CLIs' HTTP(S) proxy: it logs and
refuses every request for another host, so the test sees, and blocks, any
network a CLI attempts beyond the model API. Binds 127.0.0.1 only.
"""
import json, re, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
LOG = open(sys.argv[2], "a", buffering=1)


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for part in content:
            if isinstance(part, dict) and part.get("type") in (None, "text", "input_text", "output_text"):
                parts.append(part.get("text", ""))
        return " ".join(parts)
    return ""


def last_user(messages):
    for message in reversed(messages):
        if message.get("role") == "user":
            text = text_of(message.get("content")).strip()
            # Claude Code and Codex prepend context blocks to the first turn;
            # the typed prompt is the last text part.
            if text:
                return text.splitlines()[-1].strip()
    return ""


def reply_for(text, tools):
    if not tools:
        return "Fake title"
    match = re.search(r"\bLINES (\d+)\b", text)
    if match:
        count = min(int(match.group(1)), 400)
        return "\n\n".join(f"Reply line {i} for this turn." for i in range(1, count + 1))
    return next(iter(re.findall(r"\b[A-Z]{4,}\b", text)), "OK")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def refuse(self):
        LOG.write(f"PROXY {self.command} {self.path}\n")
        self.send_response(403)
        self.send_header("content-length", "0")
        self.send_header("connection", "close")
        self.end_headers()
        self.close_connection = True

    def do_CONNECT(self):
        self.refuse()

    def proxied(self):
        return self.path.startswith("http://") or self.path.startswith("https://")

    def do_HEAD(self):
        if self.proxied():
            return self.refuse()
        self.send_response(200)
        self.send_header("content-length", "0")
        self.end_headers()

    def do_GET(self):
        if self.proxied():
            return self.refuse()
        LOG.write(f"GET {self.path}\n")
        self.reply_json({"object": "list", "data": [{"id": "fake-model", "object": "model"}], "models": []})

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        raw = self.rfile.read(length) or b"{}"
        if self.proxied():
            return self.refuse()
        try:
            body = json.loads(raw)
        except ValueError:
            body = {}
        path = self.path.split("?")[0]
        if path.endswith("/count_tokens"):
            return self.reply_json({"input_tokens": 10})
        if path.endswith("/responses"):
            items = [i for i in body.get("input", []) if isinstance(i, dict)]
            messages = [i for i in items if i.get("type", "message") == "message"]
            tool_result = bool(items) and items[-1].get("type", "").endswith("_call_output")
        else:
            messages = body.get("messages", [])
            last = messages[-1] if messages else {}
            content = last.get("content")
            tool_result = last.get("role") == "tool" or (
                isinstance(content, list)
                and any(isinstance(p, dict) and p.get("type") == "tool_result" for p in content))
        text = last_user(messages)
        tools = bool(body.get("tools"))
        LOG.write(f"POST {path} tools={tools} tool_result={tool_result} last_user={text[:80]!r}\n")
        call = tools and not tool_result and "RUNCMD" in text
        reply = "The command ran. DONECMD" if tool_result else reply_for(text, tools)
        time.sleep(0.3)
        if path.endswith("/responses"):
            return self.openai_responses(body, reply, call)
        if path.endswith("/messages"):
            return self.anthropic(body, reply, call)
        return self.chat(body, reply, call)

    # OpenAI chat completions
    def chat(self, body, reply, call):
        if call:
            arguments = json.dumps({"command": MARKER_COMMAND, "description": "Print a marker"})
            self.begin_stream()
            self.data(self.chunk(body, {"role": "assistant", "tool_calls": [{
                "index": 0, "id": "call_diri_1", "type": "function",
                "function": {"name": "bash", "arguments": arguments}}]}, None))
            self.data(self.chunk(body, {}, "tool_calls"))
            self.write_chunk("data: [DONE]\n\n")
            self.write_chunk("")
            return
        if not body.get("stream"):
            return self.reply_json({
                "id": "chatcmpl-diri", "object": "chat.completion", "created": int(time.time()),
                "model": body.get("model", "fake-model"),
                "choices": [{"index": 0, "message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
            })
        self.begin_stream()
        for i, piece in enumerate(pieces(reply)):
            delta = {"content": piece}
            if i == 0:
                delta["role"] = "assistant"
            self.data(self.chunk(body, delta, None))
        last = self.chunk(body, {}, "stop")
        last["usage"] = {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        self.data(last)
        self.write_chunk("data: [DONE]\n\n")
        self.write_chunk("")

    def chunk(self, body, delta, finish):
        return {
            "id": "chatcmpl-diri", "object": "chat.completion.chunk", "created": int(time.time()),
            "model": body.get("model", "fake-model"),
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        }

    # OpenAI Responses
    def openai_responses(self, body, reply, call):
        response = {"id": "resp_diri", "object": "response", "created_at": int(time.time()),
                    "model": body.get("model", "fake-model"), "status": "in_progress", "output": []}
        item = {"type": "message", "id": "msg_diri", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": reply, "annotations": []}]}
        usage = {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 5,
                 "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 15}
        if call:
            name, arguments = codex_shell_call(body.get("tools", []))
            item = {"type": "function_call", "id": "fc_diri", "call_id": "call_diri_1", "status": "completed",
                    "name": name, "arguments": arguments}
            self.begin_stream()
            self.event("response.created", {"type": "response.created", "response": response})
            self.event("response.output_item.done", {"type": "response.output_item.done", "output_index": 0,
                       "item": item})
            self.event("response.completed", {"type": "response.completed",
                       "response": {**response, "status": "completed", "output": [item], "usage": usage}})
            self.write_chunk("")
            return
        self.begin_stream()
        self.event("response.created", {"type": "response.created", "response": response})
        self.event("response.output_item.added", {"type": "response.output_item.added", "output_index": 0,
                   "item": {**item, "status": "in_progress", "content": []}})
        for piece in pieces(reply):
            self.event("response.output_text.delta", {"type": "response.output_text.delta", "item_id": "msg_diri",
                       "output_index": 0, "content_index": 0, "delta": piece})
        self.event("response.output_item.done", {"type": "response.output_item.done", "output_index": 0, "item": item})
        self.event("response.completed", {"type": "response.completed",
                   "response": {**response, "status": "completed", "output": [item], "usage": usage}})
        self.write_chunk("")

    # Anthropic Messages
    def anthropic(self, body, reply, call):
        message = {"id": "msg_diri", "type": "message", "role": "assistant", "model": body.get("model", "fake"),
                   "content": [], "stop_reason": None, "stop_sequence": None,
                   "usage": {"input_tokens": 10, "output_tokens": 1}}
        if not body.get("stream"):
            return self.reply_json({**message, "content": [{"type": "text", "text": reply}],
                                    "stop_reason": "end_turn"})
        self.begin_stream()
        self.event("message_start", {"type": "message_start", "message": message})
        if call:
            self.event("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {
                "type": "tool_use", "id": "toolu_diri_1", "name": "Bash", "input": {}}})
            self.event("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {
                "type": "input_json_delta",
                "partial_json": json.dumps({"command": MARKER_COMMAND, "description": "Print a marker"})}})
            self.event("content_block_stop", {"type": "content_block_stop", "index": 0})
            self.event("message_delta", {"type": "message_delta",
                       "delta": {"stop_reason": "tool_use", "stop_sequence": None}, "usage": {"output_tokens": 5}})
            self.event("message_stop", {"type": "message_stop"})
            self.write_chunk("")
            return
        self.event("content_block_start", {"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}})
        for piece in pieces(reply):
            self.event("content_block_delta", {"type": "content_block_delta", "index": 0,
                       "delta": {"type": "text_delta", "text": piece}})
        self.event("content_block_stop", {"type": "content_block_stop", "index": 0})
        self.event("message_delta", {"type": "message_delta",
                   "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 5}})
        self.event("message_stop", {"type": "message_stop"})
        self.write_chunk("")

    def begin_stream(self):
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()

    def data(self, value):
        self.write_chunk(f"data: {json.dumps(value)}\n\n")

    def event(self, name, value):
        self.write_chunk(f"event: {name}\ndata: {json.dumps(value)}\n\n")

    def write_chunk(self, data):
        raw = data.encode()
        self.wfile.write(f"{len(raw):x}\r\n".encode() + raw + b"\r\n")
        self.wfile.flush()

    def reply_json(self, value):
        raw = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)


MARKER_COMMAND = "echo diri-tool-output"


def codex_shell_call(tools):
    """Codex's shell tool name and argument shape vary by release."""
    for tool in tools:
        name = tool.get("name") or tool.get("function", {}).get("name", "")
        if name in ("exec_command", "shell", "shell_command", "local_shell"):
            props = (tool.get("parameters") or tool.get("function", {}).get("parameters") or {}).get("properties", {})
            if "cmd" in props:
                return name, json.dumps({"cmd": MARKER_COMMAND})
            if props.get("command", {}).get("type") == "array":
                return name, json.dumps({"command": ["bash", "-lc", MARKER_COMMAND]})
            return name, json.dumps({"command": MARKER_COMMAND})
    return "shell", json.dumps({"command": ["bash", "-lc", MARKER_COMMAND]})


def pieces(reply):
    """A handful of deltas, so the CLI renders a genuine stream."""
    lines = reply.split("\n")
    step = max(1, len(lines) // 8)
    return ["\n".join(lines[i:i + step]) + ("\n" if i + step < len(lines) else "")
            for i in range(0, len(lines), step)]


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
