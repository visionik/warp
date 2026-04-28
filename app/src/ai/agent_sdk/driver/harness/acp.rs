//! ACP (Agent Client Protocol) harness implementation.
//!
//! Unlike Claude/Gemini which are TUI-based and run inside the terminal PTY,
//! the ACP harness spawns the agent as a *background* subprocess with piped
//! stdin/stdout. We then drive a JSON-RPC session over those streams using
//! the four core ACP methods:
//!   - `initialize` (negotiate protocol version + client capabilities)
//!   - `session/new` (create a session, optionally with MCP servers)
//!   - `session/prompt` (send the user prompt, receive a stop reason)
//!   - `session/cancel` (notification, sent on graceful shutdown)
//!
//! ## Client capabilities (story 3)
//!
//! While waiting for `session/prompt` to complete the agent may issue requests
//! *back* to Warp. These are handled inline by [`AcpConnection::handle_client_request`]:
//!
//! - `fs/readTextFile`  — reads a file from disk (path-sandboxed to `working_dir`)
//! - `fs/writeTextFile` — writes a file to disk (path-sandboxed)
//! - `terminal/create` — spawns a subprocess and returns a synthetic terminal ID;
//!    subsequent `terminal/getOutput`, `terminal/kill`, `terminal/release` are also handled.
//! - `request/permission` — auto-approves with the first offered option and logs the
//!    decision. Full Warp autonomy-gate integration is deferred to a follow-up story.
//!
//! The agent itself can be any executable that speaks ACP over stdio. The
//! command is supplied via [`HarnessConfig::command`] in the agent run config
//! (e.g. `"my-acp-agent"` or `"node ./build/index.js"`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// We depend on the agent-client-protocol crate as the canonical schema, but
// build request payloads as raw JSON via `serde_json::json!` because the SDK's
// schema types are `#[non_exhaustive]` (external crates can't use struct
// literal syntax to populate nested capabilities) and the wire format here is
// stable and small.
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::channel::oneshot;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use uuid::Uuid;
use warp_cli::agent::Harness;
use warp_core::command::ExitCode;
use warpui::{ModelHandle, ModelSpawner};

use crate::ai::ambient_agents::AmbientAgentTaskId;
use crate::server::server_api::ServerApi;
use crate::terminal::model::block::BlockId;
use crate::terminal::CLIAgent;
use crate::util::path::resolve_executable;

use super::super::terminal::{CommandHandle, TerminalDriver};
use super::super::{AgentDriver, AgentDriverError};
use super::{HarnessRunner, ResumePayload, SavePoint, ThirdPartyHarness};

/// Marker label for the harness in error messages.
const ACP_HARNESS_LABEL: &str = "acp";

/// JSON-RPC method names — match the ACP spec.
const METHOD_INITIALIZE: &str = "initialize";
const METHOD_SESSION_NEW: &str = "session/new";
const METHOD_SESSION_PROMPT: &str = "session/prompt";
const METHOD_SESSION_CANCEL: &str = "session/cancel";

/// Agent-to-client capability method names (inbound requests from the agent).
const METHOD_FS_READ: &str = "fs/readTextFile";
const METHOD_FS_WRITE: &str = "fs/writeTextFile";
const METHOD_TERMINAL_CREATE: &str = "terminal/create";
const METHOD_TERMINAL_GET_OUTPUT: &str = "terminal/getOutput";
const METHOD_TERMINAL_KILL: &str = "terminal/kill";
const METHOD_TERMINAL_RELEASE: &str = "terminal/release";
const METHOD_REQUEST_PERMISSION: &str = "request/permission";

/// JSON-RPC error code for "method not found" (from the JSON-RPC 2.0 spec).
const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC error code for invalid params.
const JSONRPC_INVALID_PARAMS: i64 = -32602;

/// Client identification advertised in the `initialize` request.
const CLIENT_NAME: &str = "warp";

/// Third-party harness for ACP-compatible agents.
///
/// The `command` is the full launch invocation for the ACP agent, parsed with
/// [`shlex`] so users can pass args (e.g. `"node ./acp-agent.js --verbose"`).
#[derive(Debug)]
pub(crate) struct AcpHarness {
    command: String,
}

impl AcpHarness {
    /// Build the harness from a [`HarnessConfig::command`] value.
    ///
    /// Returns a `HarnessSetupFailed` error if `command` is `None`, since the
    /// ACP harness is meaningless without an agent binary to launch.
    pub(crate) fn new(command: Option<String>) -> Result<Self, AgentDriverError> {
        let command = command
            .filter(|c| !c.trim().is_empty())
            .ok_or_else(|| AgentDriverError::HarnessSetupFailed {
                harness: ACP_HARNESS_LABEL.into(),
                reason: "ACP harness requires a `command` to be set in the agent harness config (e.g. `harness.command = \"my-acp-agent\"`)."
                    .into(),
            })?;
        Ok(Self { command })
    }

    /// Split the configured command into program + args using shell-style
    /// tokenisation. Returns an error if the command can't be parsed (e.g.
    /// unmatched quotes).
    fn split_command(&self) -> Result<(String, Vec<String>), AgentDriverError> {
        let mut tokens =
            shlex::split(&self.command).ok_or_else(|| AgentDriverError::HarnessSetupFailed {
                harness: ACP_HARNESS_LABEL.into(),
                reason: format!(
                    "ACP `command` is not a valid shell expression: {}",
                    self.command
                ),
            })?;
        if tokens.is_empty() {
            return Err(AgentDriverError::HarnessSetupFailed {
                harness: ACP_HARNESS_LABEL.into(),
                reason: "ACP `command` is empty after parsing".into(),
            });
        }
        let program = tokens.remove(0);
        Ok((program, tokens))
    }
}

#[cfg_attr(not(target_family = "wasm"), async_trait)]
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
impl ThirdPartyHarness for AcpHarness {
    fn harness(&self) -> Harness {
        Harness::Acp
    }

    fn cli_agent(&self) -> CLIAgent {
        // Story 4 will add a dedicated `CLIAgent::Acp` variant; until then ACP
        // agents map to the generic `Unknown` since they don't share session
        // state with any of the existing CLI agent integrations.
        CLIAgent::Unknown
    }

    /// Validate that the configured ACP binary is on `PATH` (or resolves as a
    /// file path). The default `validate_cli_installed` doesn't fit because
    /// the program name is per-config rather than the harness label.
    fn validate(&self) -> Result<(), AgentDriverError> {
        let (program, _) = self.split_command()?;
        if resolve_executable(&program).is_none() {
            return Err(AgentDriverError::HarnessSetupFailed {
                harness: ACP_HARNESS_LABEL.into(),
                reason: format!(
                    "ACP agent binary '{program}' was not found on PATH. \
                     Verify your `harness.command` setting points to a valid executable."
                ),
            });
        }
        Ok(())
    }

    fn build_runner(
        &self,
        prompt: &str,
        _system_prompt: Option<&str>,
        _resumption_prompt: Option<&str>,
        working_dir: &Path,
        _task_id: Option<AmbientAgentTaskId>,
        _server_api: Arc<ServerApi>,
        terminal_driver: ModelHandle<TerminalDriver>,
        _resume: Option<ResumePayload>,
    ) -> Result<Box<dyn HarnessRunner>, AgentDriverError> {
        let (program, args) = self.split_command()?;
        Ok(Box::new(AcpHarnessRunner {
            program,
            args,
            prompt: prompt.to_string(),
            working_dir: working_dir.to_path_buf(),
            terminal_driver,
            state: Mutex::new(AcpRunnerState::Preexec),
        }))
    }
}

/// Runtime state of an [`AcpHarnessRunner`].
enum AcpRunnerState {
    /// `start()` has not been called yet.
    Preexec,
    /// The session is in flight; the cancel sender lets [`HarnessRunner::exit`]
    /// nudge the background driver task to send `session/cancel` and exit.
    Running {
        #[allow(
            dead_code,
            reason = "Reserved for save_conversation upload path; future story will wire it up."
        )]
        block_id: BlockId,
        cancel_tx: Option<oneshot::Sender<()>>,
    },
    /// The session has finished or never reached `Running`.
    Finished,
}

/// Per-run state for an ACP harness.
struct AcpHarnessRunner {
    program: String,
    args: Vec<String>,
    prompt: String,
    working_dir: PathBuf,
    /// Held only so harness machinery (e.g. block-snapshot upload) can read
    /// from the terminal model in [`save_conversation`]; the ACP agent itself
    /// does *not* run inside this terminal.
    #[allow(
        dead_code,
        reason = "Reserved for save_conversation transcript upload; future story will wire it up."
    )]
    terminal_driver: ModelHandle<TerminalDriver>,
    state: Mutex<AcpRunnerState>,
}

#[cfg_attr(not(target_family = "wasm"), async_trait)]
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
impl HarnessRunner for AcpHarnessRunner {
    /// Spawn the ACP agent subprocess and drive the JSON-RPC session in a
    /// background tokio task. Returns immediately with a [`CommandHandle`]
    /// that resolves once the prompt completes (or the session fails).
    async fn start(
        &self,
        _foreground: &ModelSpawner<AgentDriver>,
    ) -> Result<CommandHandle, AgentDriverError> {
        let (exit_tx, exit_rx) = oneshot::channel::<ExitCode>();
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let block_id = BlockId::new();

        let program = self.program.clone();
        let args = self.args.clone();
        let prompt = self.prompt.clone();
        let working_dir = self.working_dir.clone();

        // Fire-and-forget background driver task. tokio::spawn requires a
        // multi-thread runtime, which the agent driver is already running on
        // (cf. `claude_code::upload_transcript`'s use of spawn_blocking).
        tokio::spawn(async move {
            let exit_code =
                match run_acp_session(&program, &args, &prompt, &working_dir, cancel_rx).await {
                    Ok(()) => ExitCode::from(0),
                    Err(error) => {
                        log::error!("ACP harness session failed: {error:#}");
                        ExitCode::from(1)
                    }
                };
            // The receiver may be gone if the agent driver was torn down — not
            // an error here.
            let _ = exit_tx.send(exit_code);
        });

        *self.state.lock() = AcpRunnerState::Running {
            block_id: block_id.clone(),
            cancel_tx: Some(cancel_tx),
        };

        Ok(CommandHandle::from_channel(exit_rx, block_id))
    }

    /// Signal the background session task to send `session/cancel` and tear
    /// down the subprocess. Idempotent; safe to call after the session has
    /// already finished.
    async fn exit(&self, _foreground: &ModelSpawner<AgentDriver>) -> Result<()> {
        let mut state = self.state.lock();
        match &mut *state {
            AcpRunnerState::Running { cancel_tx, .. } => {
                if let Some(tx) = cancel_tx.take() {
                    let _ = tx.send(());
                }
                *state = AcpRunnerState::Finished;
                Ok(())
            }
            AcpRunnerState::Preexec => {
                log::warn!("ACP harness exit() called before start()");
                Ok(())
            }
            AcpRunnerState::Finished => Ok(()),
        }
    }

    /// ACP doesn't currently expose a transcript shape we can persist locally,
    /// so we no-op until the protocol/server gains a snapshot endpoint we can
    /// upload to.
    async fn save_conversation(
        &self,
        _save_point: SavePoint,
        _foreground: &ModelSpawner<AgentDriver>,
    ) -> Result<()> {
        Ok(())
    }
}

/// Drive a single ACP session end-to-end: spawn process, initialize, create
/// session, send prompt, await stop, then cleanly close stdin.
///
/// Returns once [`session/prompt`] yields a [`PromptResponse`] (or the session
/// is cancelled / the agent exits).
async fn run_acp_session(
    program: &str,
    args: &[String],
    prompt: &str,
    working_dir: &Path,
    cancel_rx: oneshot::Receiver<()>,
) -> Result<()> {
    log::info!("Spawning ACP agent: {program} {args:?}");
    let mut child = Command::new(program)
        .args(args)
        .current_dir(working_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn ACP agent '{program}': {e}"))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("ACP agent stdin not piped"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ACP agent stdout not piped"))?;

    let mut conn: AcpConnection =
        AcpConnection::new(stdin, BufReader::new(stdout), working_dir.to_path_buf());

    // 1. initialize
    let init_params = serde_json::json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {
                "readTextFile": true,
                "writeTextFile": true,
            },
            "terminal": true,
        },
        "clientInfo": {
            "name": CLIENT_NAME,
        },
    });
    let _init_resp: Value = conn.call(METHOD_INITIALIZE, &init_params).await?;
    log::debug!("ACP initialize succeeded");

    // 2. session/new
    let new_session_params = serde_json::json!({
        "cwd": working_dir,
        "mcpServers": [],
    });
    let new_session_resp: NewSessionResponseShim =
        conn.call(METHOD_SESSION_NEW, &new_session_params).await?;
    let session_id = new_session_resp.session_id;
    log::info!("ACP session/new -> session_id={session_id}");

    // 3. session/prompt — race with cancel
    let prompt_params = serde_json::json!({
        "sessionId": session_id,
        "prompt": [
            { "type": "text", "text": prompt },
        ],
    });
    let session_id_for_cancel = session_id.clone();
    tokio::select! {
        biased;
        result = conn.call::<_, Value>(METHOD_SESSION_PROMPT, &prompt_params) => {
            let _ = result?;
            log::info!("ACP session/prompt completed");
        }
        _ = cancel_rx => {
            log::info!("ACP session/cancel requested by harness exit");
            let cancel = serde_json::json!({
                "sessionId": session_id_for_cancel,
            });
            if let Err(error) = conn.notify(METHOD_SESSION_CANCEL, &cancel).await {
                log::warn!("ACP cancel notification failed: {error:#}");
            }
        }
    }

    // Closing stdin signals end-of-stream to the agent. Best-effort.
    drop(conn);

    // Reap the child without blocking forever.
    if let Err(error) = child.kill().await {
        log::debug!("ACP child kill (post-session) returned {error}");
    }
    let _ = child.wait().await;
    Ok(())
}

/// A subprocess managed on behalf of the ACP agent via `terminal/create`.
struct AcpTerminal {
    child: Child,
    /// Accumulated stdout+stderr output captured so far.
    output: Vec<u8>,
}

/// JSON-RPC 2.0 connection over stdio. Tracks an outgoing id counter,
/// matches incoming responses by id, and handles agent-initiated capability
/// requests (fs/readTextFile, fs/writeTextFile, terminal/create, etc.).
///
/// The `W` and `R` type parameters are the write and read halves of the
/// connection. In production these are [`ChildStdin`] and
/// [`BufReader<ChildStdout>`]; in tests they are
/// `WriteHalf<DuplexStream>` / `BufReader<ReadHalf<DuplexStream>>`.
struct AcpConnection<W = ChildStdin, R = BufReader<ChildStdout>> {
    stdin: W,
    stdout: R,
    next_id: u64,
    /// Working directory for path validation and relative-path resolution.
    working_dir: PathBuf,
    /// Subprocess terminals created by the agent via `terminal/create`.
    /// Keyed by the synthetic terminal ID returned to the agent.
    terminals: HashMap<String, AcpTerminal>,
}

impl<W: AsyncWrite + Unpin, R: AsyncBufReadExt + Unpin> AcpConnection<W, R> {
    fn new(stdin: W, stdout: R, working_dir: PathBuf) -> Self {
        Self {
            stdin,
            stdout,
            next_id: 1,
            working_dir,
            terminals: HashMap::new(),
        }
    }

    /// Send a JSON-RPC request and await its response.
    ///
    /// Any agent-initiated requests or notifications interleaved on the stream
    /// while we await the response are dispatched via [`handle_client_request`].
    async fn call<P: Serialize, Resp: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<Resp> {
        let id = self.next_id;
        self.next_id += 1;
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_message(&envelope).await?;

        loop {
            let value = self.read_message().await?;
            // A JSON-RPC *response* has an `id` matching our request and no
            // `method` field. Anything else (agent request or notification)
            // is dispatched as a client capability call.
            let msg_id = value.get("id").and_then(Value::as_u64);
            let is_our_response = msg_id == Some(id) && value.get("method").is_none();
            if is_our_response {
                if let Some(error) = value.get("error") {
                    return Err(anyhow!("ACP method '{method}' returned error: {error}"));
                }
                let result = value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| anyhow!("ACP response missing `result` field"))?;
                return serde_json::from_value::<Resp>(result)
                    .map_err(|e| anyhow!("Failed to deserialize ACP response for {method}: {e}"));
            }
            // Agent-initiated request or notification — handle it.
            if let Err(err) = self.handle_client_request(&value).await {
                log::warn!("ACP client-request handler error: {err:#}");
            }
        }
    }

    /// Dispatch an agent-initiated JSON-RPC request or notification.
    ///
    /// Notifications (no `id` field) are handled silently (no response sent).
    /// Requests with an `id` always receive a response, even on error.
    async fn handle_client_request(&mut self, msg: &Value) -> Result<()> {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match method {
            METHOD_FS_READ => {
                let path_str = params
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("fs/readTextFile missing 'path'"))?;
                let safe_path = resolve_safe_path(path_str, &self.working_dir.clone())?;
                match tokio::fs::read_to_string(&safe_path).await {
                    Ok(content) => {
                        self.respond_result(id, serde_json::json!({ "content": content }))
                            .await?
                    }
                    Err(err) => {
                        self.respond_error(
                            id,
                            JSONRPC_INVALID_PARAMS,
                            &format!("Cannot read '{}': {err}", safe_path.display()),
                        )
                        .await?
                    }
                }
            }

            METHOD_FS_WRITE => {
                let path_str = params
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("fs/writeTextFile missing 'path'"))?;
                let content = params
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("fs/writeTextFile missing 'content'"))?;
                let safe_path = resolve_safe_path(path_str, &self.working_dir.clone())?;
                // Create parent directories if needed.
                if let Some(parent) = safe_path.parent() {
                    if let Err(err) = tokio::fs::create_dir_all(parent).await {
                        self.respond_error(
                            id,
                            JSONRPC_INVALID_PARAMS,
                            &format!(
                                "Cannot create directories for '{}': {err}",
                                safe_path.display()
                            ),
                        )
                        .await?;
                        return Ok(());
                    }
                }
                match tokio::fs::write(&safe_path, content).await {
                    Ok(()) => self.respond_result(id, serde_json::json!({})).await?,
                    Err(err) => {
                        self.respond_error(
                            id,
                            JSONRPC_INVALID_PARAMS,
                            &format!("Cannot write '{}': {err}", safe_path.display()),
                        )
                        .await?
                    }
                }
            }

            METHOD_TERMINAL_CREATE => {
                // Spawn a subprocess on behalf of the agent. We don't attach it
                // to any Warp terminal pane (that requires model context not
                // available here); the agent gets a synthetic ID it can use for
                // terminal/getOutput and terminal/kill.
                let command = params.get("command").and_then(Value::as_str).unwrap_or("");
                let args: Vec<&str> = params
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(|s| resolve_safe_path(s, &self.working_dir.clone()).ok())
                    .unwrap_or_default()
                    .unwrap_or_else(|| self.working_dir.clone());

                if command.is_empty() {
                    self.respond_error(
                        id,
                        JSONRPC_INVALID_PARAMS,
                        "terminal/create: 'command' is required",
                    )
                    .await?;
                    return Ok(());
                }

                let child_result = Command::new(command)
                    .args(&args)
                    .current_dir(&cwd)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn();

                match child_result {
                    Ok(child) => {
                        let terminal_id = Uuid::new_v4().to_string();
                        log::info!(
                            "ACP terminal/create: spawned '{command}' as terminal {terminal_id}"
                        );
                        self.terminals.insert(
                            terminal_id.clone(),
                            AcpTerminal {
                                child,
                                output: Vec::new(),
                            },
                        );
                        self.respond_result(id, serde_json::json!({ "terminalId": terminal_id }))
                            .await?
                    }
                    Err(err) => {
                        self.respond_error(
                            id,
                            JSONRPC_INVALID_PARAMS,
                            &format!("terminal/create: failed to spawn '{command}': {err}"),
                        )
                        .await?
                    }
                }
            }

            METHOD_TERMINAL_GET_OUTPUT => {
                let terminal_id = params
                    .get("terminalId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let term = self.terminals.get_mut(terminal_id);
                match term {
                    None => {
                        self.respond_error(
                            id,
                            JSONRPC_INVALID_PARAMS,
                            &format!("terminal/getOutput: unknown terminal '{terminal_id}'"),
                        )
                        .await?
                    }
                    Some(term) => {
                        // Drain any available stdout/stderr without blocking.
                        if let Some(stdout) = term.child.stdout.as_mut() {
                            let mut buf = [0u8; 4096];
                            while let Ok(n) = stdout.read(&mut buf).await {
                                if n == 0 {
                                    break;
                                }
                                term.output.extend_from_slice(&buf[..n]);
                            }
                        }
                        let output_str = String::from_utf8_lossy(&term.output).to_string();
                        let exit_status = term.child.try_wait().ok().flatten();
                        self.respond_result(
                            id,
                            serde_json::json!({
                                "output": output_str,
                                "exitCode": exit_status.map(|s| s.code()).unwrap_or(None),
                            }),
                        )
                        .await?
                    }
                }
            }

            METHOD_TERMINAL_KILL => {
                let terminal_id = params
                    .get("terminalId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Some(term) = self.terminals.get_mut(terminal_id) {
                    let _ = term.child.kill().await;
                }
                self.respond_result(id, serde_json::json!({})).await?
            }

            METHOD_TERMINAL_RELEASE => {
                let terminal_id = params
                    .get("terminalId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Some(mut term) = self.terminals.remove(terminal_id) {
                    let _ = term.child.kill().await;
                    let _ = term.child.wait().await;
                }
                self.respond_result(id, serde_json::json!({})).await?
            }

            METHOD_REQUEST_PERMISSION => {
                // Auto-approve: select the first offered permission option.
                // Full Warp autonomy-gate integration (showing a UI prompt and
                // waiting for user confirmation) requires model context that is
                // not available in this background task and is deferred to a
                // follow-up story.
                let options = params
                    .get("options")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let first_option_id = options
                    .first()
                    .and_then(|opt| opt.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("allow");
                let tool_call = params.get("toolCall").cloned().unwrap_or(Value::Null);
                log::info!(
                    "ACP request/permission: auto-approving '{}' (option '{first_option_id}'). \
                     Full permission-gate integration is pending.",
                    tool_call
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("<unknown>")
                );
                self.respond_result(
                    id,
                    serde_json::json!({ "outcome": { "optionId": first_option_id } }),
                )
                .await?
            }

            "" => {
                // Pure notification (no method field or empty) — nothing to respond to.
                log::debug!("ACP notification (no method): {msg}");
            }

            other => {
                log::debug!(
                    "ACP unknown client request '{other}' — responding with method-not-found"
                );
                self.respond_error(
                    id,
                    JSONRPC_METHOD_NOT_FOUND,
                    &format!("Method not found: {other}"),
                )
                .await?
            }
        }
        Ok(())
    }

    /// Send a JSON-RPC success response.
    async fn respond_result(&mut self, id: Option<Value>, result: Value) -> Result<()> {
        let Some(id) = id else {
            // Notifications don't get responses.
            return Ok(());
        };
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        self.write_message(&msg).await
    }

    /// Send a JSON-RPC error response.
    async fn respond_error(&mut self, id: Option<Value>, code: i64, message: &str) -> Result<()> {
        let Some(id) = id else {
            return Ok(());
        };
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        });
        self.write_message(&msg).await
    }

    /// Send a JSON-RPC notification (no id, no expected response).
    async fn notify<P: Serialize>(&mut self, method: &str, params: &P) -> Result<()> {
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&envelope).await
    }

    async fn write_message(&mut self, value: &Value) -> Result<()> {
        let mut text = serde_json::to_string(value)
            .map_err(|e| anyhow!("Failed to encode ACP JSON-RPC: {e}"))?;
        text.push('\n');
        self.stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|e| anyhow!("Failed to write to ACP agent stdin: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| anyhow!("Failed to flush ACP agent stdin: {e}"))
    }

    async fn read_message(&mut self) -> Result<Value> {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .await
            .map_err(|e| anyhow!("Failed to read from ACP agent stdout: {e}"))?;
        if n == 0 {
            return Err(anyhow!("ACP agent stdout closed unexpectedly"));
        }
        let trimmed = line.trim();
        serde_json::from_str(trimmed)
            .map_err(|e| anyhow!("Invalid JSON-RPC line from ACP agent: {e}: {trimmed}"))
    }
}

/// Local newtype for `NewSessionResponse` deserialization. The SDK's
/// `NewSessionResponse` is non-exhaustive and may evolve; we only need the
/// session id. Using a private shim avoids a hard coupling to the SDK's
/// constructor surface.
#[derive(Debug, Deserialize)]
struct NewSessionResponseShim {
    #[serde(rename = "sessionId")]
    session_id: String,
}

/// Validate `path` and resolve it relative to `working_dir`, refusing any
/// path that escapes the working directory via `..` components or absolute
/// paths outside of it.
///
/// Relative paths are resolved against `working_dir`; absolute paths must
/// already start with `working_dir`. Uses a lexical normalisation pass so
/// the path does not need to exist yet (needed for write-new-file operations).
pub(crate) fn resolve_safe_path(path: &str, working_dir: &Path) -> anyhow::Result<PathBuf> {
    let raw = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        working_dir.join(path)
    };
    let normalised = lexical_normalise(&raw);
    if !normalised.starts_with(working_dir) {
        anyhow::bail!(
            "Path '{path}' escapes the working directory (resolved to '{}')",
            normalised.display()
        );
    }
    Ok(normalised)
}

/// Resolve `.` and `..` components without calling `canonicalize` (which
/// requires the path to exist and follows symlinks).
fn lexical_normalise(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
#[path = "acp-tests.rs"]
mod tests;
