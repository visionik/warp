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
//! The agent itself can be any executable that speaks ACP over stdio. The
//! command is supplied via [`HarnessConfig::command`] in the agent run config
//! (e.g. `"my-acp-agent"` or `"node ./build/index.js"`).

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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
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
            reason = "Reserved for save_conversation upload path; story 3 will wire it up."
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
        reason = "Reserved for save_conversation transcript upload; populated by story 3."
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

    let mut conn = AcpConnection::new(stdin, BufReader::new(stdout));

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

/// JSON-RPC 2.0 connection over stdio. Tracks an outgoing id counter and
/// matches incoming responses by id. Notifications/requests *from* the agent
/// (e.g. session updates, terminal create) are logged at debug for now —
/// story 3 will route them into the client capabilities surface.
struct AcpConnection {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl AcpConnection {
    fn new(stdin: ChildStdin, stdout: BufReader<ChildStdout>) -> Self {
        Self {
            stdin,
            stdout,
            next_id: 1,
        }
    }

    /// Send a JSON-RPC request and await its response. Discards any
    /// agent-initiated requests/notifications interleaved on the stream
    /// (logged but not handled, to be wired in by story 3).
    async fn call<P: Serialize, R: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<R> {
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
            // A response has an `id` field; agent-initiated requests/notifs
            // either lack `id` (notification) or have a different one.
            let msg_id = value.get("id").and_then(Value::as_u64);
            if msg_id == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(anyhow!("ACP method '{method}' returned error: {error}"));
                }
                let result = value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| anyhow!("ACP response missing `result` field"))?;
                return serde_json::from_value(result)
                    .map_err(|e| anyhow!("Failed to deserialize ACP response for {method}: {e}"));
            }
            log::debug!("ACP unsolicited message ignored: {value}");
        }
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

#[cfg(test)]
#[path = "acp-tests.rs"]
mod tests;
