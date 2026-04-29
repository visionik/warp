#!/usr/bin/env python3
"""
Minimal ACP (Agent Client Protocol) echo agent for integration testing.

Speaks JSON-RPC 2.0 over stdin/stdout.  Use this to verify the Warp ACP
harness end-to-end without needing a real coding agent.

Usage:
  # Standalone protocol test:
  printf '<json>\n<json>\n<json>\n' | python3 echo-agent.py

  # Via Warp binary:
  warp agent run --harness acp -f config-example.yaml --prompt "hello"

All traffic is logged to stderr for easy debugging.

Handles:
  initialize          -> responds with minimal capabilities
  session/new         -> creates a random session ID
  session/prompt      -> emits an AgentMessageChunk notification echoing
                         the prompt, then responds with stopReason=end_turn
  session/cancel      -> exits cleanly
  fs/readTextFile     -> reads file from disk and returns content
  fs/writeTextFile    -> writes content to file (creates dirs as needed)
  terminal/create     -> returns a synthetic UUID terminal ID
  request/permission  -> auto-approves (first option)
  <unknown>           -> JSON-RPC -32601 error response
"""
import sys
import json
import os
import uuid


def send(msg: dict) -> None:
    line = json.dumps(msg)
    print(line, flush=True)
    _log(f"-> {line[:140]}")


def recv() -> dict | None:
    line = sys.stdin.readline()
    if not line:
        return None
    msg = json.loads(line)
    _log(f"<- {json.dumps(msg)[:140]}")
    return msg


def respond_result(req_id, result: dict) -> None:
    send({"jsonrpc": "2.0", "id": req_id, "result": result})


def respond_error(req_id, code: int, message: str) -> None:
    send({"jsonrpc": "2.0", "id": req_id,
          "error": {"code": code, "message": message}})


def _log(text: str) -> None:
    print(f"[acp-echo] {text}", file=sys.stderr, flush=True)


def main() -> None:
    _log("starting — waiting for initialize")
    terminals: dict[str, str] = {}  # terminal_id -> command

    while True:
        msg = recv()
        if msg is None:
            break

        req_id = msg.get("id")
        method = msg.get("method", "")
        params = msg.get("params") or {}

        # ── Core session lifecycle ───────────────────────────────────────

        if method == "initialize":
            respond_result(req_id, {
                "protocolVersion": params.get("protocolVersion", 1),
                "agentCapabilities": {
                    "loadSession": False,
                    "mcpCapabilities": {},
                    "promptCapabilities": {},
                    "sessionCapabilities": {},
                },
                "agentInfo": {"name": "acp-echo-agent", "version": "0.1.0"},
                "authMethods": [],
            })

        elif method == "session/new":
            session_id = str(uuid.uuid4())
            _log(f"session created: {session_id}")
            respond_result(req_id, {"sessionId": session_id})

        elif method == "session/prompt":
            parts = params.get("prompt", [])
            text = " | ".join(
                p.get("content", p.get("text", "")) for p in parts
            )
            _log(f"prompt received: {text!r}")
            # Emit one agent-message-chunk notification before responding.
            send({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": params.get("sessionId"),
                    "update": {
                        "type": "agentMessageChunk",
                        "content": {
                            "content_type": "text/plain",
                            "content": f"Echo: {text}",
                        },
                    },
                },
            })
            respond_result(req_id, {"stopReason": "end_turn"})

        elif method == "session/cancel":
            # Notification (no id) — just exit.
            _log("cancelled")
            break

        # ── Client capabilities ──────────────────────────────────────────

        elif method == "fs/readTextFile":
            path = params.get("path", "")
            try:
                with open(path, "r", encoding="utf-8") as fh:
                    respond_result(req_id, {"content": fh.read()})
            except OSError as exc:
                respond_error(req_id, -32602, str(exc))

        elif method == "fs/writeTextFile":
            path = params.get("path", "")
            content = params.get("content", "")
            try:
                parent = os.path.dirname(path)
                if parent:
                    os.makedirs(parent, exist_ok=True)
                with open(path, "w", encoding="utf-8") as fh:
                    fh.write(content)
                respond_result(req_id, {})
            except OSError as exc:
                respond_error(req_id, -32602, str(exc))

        elif method == "terminal/create":
            tid = str(uuid.uuid4())
            cmd = params.get("command", "?")
            terminals[tid] = cmd
            _log(f"terminal {tid} -> {cmd!r}")
            respond_result(req_id, {"terminalId": tid})

        elif method == "request/permission":
            options = params.get("options", [])
            chosen = options[0]["id"] if options else "allow"
            tool = (params.get("toolCall") or {}).get("name", "<unknown>")
            _log(f"permission request for '{tool}' -> auto-approving '{chosen}'")
            respond_result(req_id, {"outcome": {"optionId": chosen}})

        # ── Fallback ─────────────────────────────────────────────────────

        else:
            _log(f"unknown method: {method!r}")
            if req_id is not None:
                respond_error(req_id, -32601, f"Method not found: {method}")

    _log("exiting")


if __name__ == "__main__":
    main()
