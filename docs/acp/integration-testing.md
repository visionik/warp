# ACP Harness — Integration Testing Guide

This guide explains how to run the Warp binary locally against a real (or minimal
stub) ACP agent to verify end-to-end harness behaviour.

## Prerequisites

| Requirement | Notes |
|---|---|
| `cargo build` in this repo | Produces `./target/debug/warp` |
| `python3` on `PATH` | For the echo agent stub |
| `warp-channel-config` on `PATH` | See [Channel config stub](#channel-config-stub) |
| Warp account (logged in) | Required for `create_agent_task` call |

## Quick start

```bash
# 1. Build
cargo build --bin warp

# 2. Run the echo agent against the binary
./target/debug/warp agent run \
  --harness acp \
  -f docs/acp/config-example.yaml \
  --prompt "hello from ACP"
```

On success you should see the agent task start, the echo agent receive the
prompt, respond with `stopReason: "end_turn"`, and the process exit cleanly.

---

## Channel config stub

Non-bundled debug builds call the external `warp-channel-config` binary at
startup (see `app/src/bin/channel_config.rs`).  This binary lives in a private
Warp repository and is not available to external contributors.

The stub at `docs/acp/warp-channel-config-stub.py` emits the minimum valid
JSON that the binary expects.  Put it on `PATH` under the name
`warp-channel-config`:

```bash
ln -sf "$(pwd)/docs/acp/warp-channel-config-stub.py" /usr/local/bin/warp-channel-config
# — or —
PATH="$(pwd)/docs/acp:$PATH" ./target/debug/warp agent run ...
```

> **Note:** The stub configures the `WarpLocal` channel with production server
> URLs.  Authentication (`warp login`) is still required because `agent run`
> calls `create_agent_task` on the Warp server.

---

## Echo agent

`docs/acp/echo-agent.py` is a self-contained ACP server that:

* Speaks JSON-RPC 2.0 over `stdin`/`stdout`
* Handles every ACP method Warp's harness sends:
  `initialize`, `session/new`, `session/prompt`, `session/cancel`,
  `fs/readTextFile`, `fs/writeTextFile`, `terminal/create`,
  `request/permission`
* Echoes the prompt text back as an `AgentMessageChunk` notification
* Prints all traffic to `stderr` for easy debugging

### Standalone protocol test (no Warp login needed)

You can verify the JSON-RPC exchange directly without starting Warp:

```bash
printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}' \
  '{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"s1","prompt":[{"type":"text","text":"hello"}]}}' \
| python3 docs/acp/echo-agent.py
```

Expected output on stdout (newline-delimited JSON):
```
{"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1, ...}}
{"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "<uuid>"}}
{"jsonrpc": "2.0", "method": "session/update", ...}
{"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}}
```

---

## Config file format

```yaml
# docs/acp/config-example.yaml
harness:
  type: acp
  command: python3 /path/to/echo-agent.py   # any ACP-compliant binary
```

`harness.command` is parsed with shell quoting (`shlex`) so you can pass
arguments:

```yaml
harness:
  type: acp
  command: node /path/to/my-agent/index.js --verbose
```

---

## Feature flags

`FeatureFlag::AcpHarness` is in `DOGFOOD_FLAGS` (enabled for `WarpDev` channel
builds and confirmed active in local `WarpLocal` debug builds).

`FeatureFlag::AgentHarness` must also be enabled; it is also in
`DOGFOOD_FLAGS`.  If you see:

```
unexpected argument '--harness' found
```

it means `AgentHarness` is not enabled for your channel.  Temporarily add
`FeatureFlag::AgentHarness` to `RELEASE_FLAGS` in
`crates/warp_features/src/lib.rs` to bypass this for local testing.

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `'warp-channel-config' was not found on PATH` | Missing channel config tool | Add `docs/acp/` to `PATH` |
| `You are not logged in` | No Warp session | Run `warp login` |
| `ACP harness requires a 'command'` | Config file missing `harness.command` | Check `docs/acp/config-example.yaml` |
| `'<binary>' was not found on PATH` | Agent binary not installed | Verify `harness.command` path |
| Agent hangs / no response | Agent not speaking ACP protocol | Test standalone (see above) |

---

## Code paths exercised

When `warp agent run --harness acp -f config.yaml --prompt "..."` runs:

```
agent_sdk/mod.rs
  └── harness_kind(Harness::Acp, Some("<command>"))
        └── AcpHarness::new("<command>")
              └── AcpHarness::validate()          ← checks binary on PATH
                    └── AcpHarnessRunner::start()  ← tokio::spawn background task
                          └── run_acp_session()
                                ├── initialize
                                ├── session/new
                                ├── session/prompt  ← handles interleaved client requests
                                └── drop → kill/wait
```

See `app/src/ai/agent_sdk/driver/harness/acp.rs` and
`app/src/ai/agent_sdk/driver/harness/acp-tests.rs` for implementation and
unit tests.
