//! Hyperion AI agent sidecar (long-running).
//!
//! Started **once** by the Hyperion server at boot and kept alive for the
//! process lifetime. It reads requests from **stdin** (one JSON line per
//! player prompt) and writes game actions to **stdout** (one JSON line each).
//! All diagnostics go to **stderr** so they never pollute the action stream.
//!
//! ```text
//!   stdin   {"session":7,"prompt":"build a tower"}
//!              │  iacoder Runtime.run(), with this session's history
//!              ▼
//!   stdout  {"session":7,"type":"say","text":"On it!"}
//! ```
//!
//! `session` is an opaque id assigned by Hyperion, stable per player
//! connection. We key per-session conversation history on it so each player
//! gets multi-turn context. The LLM provider is built **once** at startup.
//!
//! Runs in its own crate on the stable toolchain because Hyperion pins an old
//! nightly that can't compile iacoder's dependency tree. Process isolation
//! sidesteps that entirely.
//!
//! Configuration (environment):
//! - `ANTHROPIC_API_KEY` (required unless offline) — Anthropic API key.
//! - `HYPERION_AI_MODEL`  (optional) — model id, default `claude-sonnet-4-5`.
//! - `HYPERION_AI_OFFLINE`(optional) — if set, skip the LLM and echo prompts.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use iacoder_agent::{AllowAll, RunRequest, Runtime};
use iacoder_core::{
    Config, LlmProvider, Message, SamplingParams, Tool, ToolAnnotations, ToolContext,
    ToolDefinition, ToolError, ToolOutput, build_model,
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

/// Per-turn budget: how many tool-call rounds the agent may take. Higher than a
/// pure-chat agent because the observe→build→verify loop (read the world, place
/// blocks, read again, fix) legitimately needs several rounds.
const MAX_TURNS: usize = 14;

/// How long a world-read tool waits for Hyperion to answer before giving up.
const QUERY_TIMEOUT: Duration = Duration::from_secs(8);

const SYSTEM_PROMPT: &str = "You are an in-game AI assistant on a Minecraft server (the Hyperion \
engine). A player is talking to you through chat. Keep replies short — one or two sentences — \
because they appear in the Minecraft chat box.\n\nTools:\n- `say`: speak to the player.\n- \
`place_block`: place ONE block at integer world coordinates (x, y, z).\n- \
`fill`: fill a whole cuboid between two corners with one block — use this for walls, floors, \
towers, boxes, and clearing areas (block=\"air\"). Prefer `fill` over many `place_block` calls.\n- \
`get_block`: read the block at one coordinate. Returns its name (or that it is air/unloaded).\n- \
`scan_region`: read a whole cuboid and list its non-air blocks. Use it to SEE the terrain before \
you build (so you don't build inside a hill or float in the air) and to CHECK your own work after \
building, then fix mistakes.\n\nIf you are asked to build something whose shape you are unsure \
of, you may use the web search and web fetch tools first to learn what it typically looks like — \
its structure, proportions, and materials — then translate that into blocks.\n\nMANDATORY build \
procedure — follow it EVERY time, even for \
simple builds:\n1. BEFORE placing any block, you MUST call `scan_region` over the build area to \
see the ground and surroundings. Never build blind: do NOT call `place_block` or `fill` until \
you have scanned.\n2. Build relative to what the scan showed — sit the structure on the ground, \
not inside a hill or floating in the air.\n3. AFTER building, call `scan_region` again on the \
same area to verify it looks right, and fix any mistakes before you finish.\n\nBlock names look like \
\"stone\", \"oak_planks\", \"glass\", \"glowstone\". The player's current coordinates are given to \
you each turn so you can build relative to where they stand (e.g. a few blocks in front of and \
beside them, not inside them).\n\nYour final response text is also shown to the player \
automatically. Be helpful and concise.";

/// Per-session conversation history, keyed by Hyperion's session id. Stored as
/// iacoder's transcript so the next turn can continue it via `prior_history`.
type Sessions = Arc<Mutex<HashMap<u64, Arc<Vec<Message>>>>>;

/// One line read from stdin. Tagged so we can tell a new turn from a request
/// to evict a disconnected player's session.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum InMsg {
    Turn {
        session: u64,
        prompt: String,
        /// The player's current position `[x, y, z]`, if known. Injected into
        /// the prompt so the agent can build relative to where they stand.
        #[serde(default)]
        pos: Option<[f32; 3]>,
    },
    Evict {
        session: u64,
    },
    /// Hyperion's answer to a `get_block`/`scan_region` query, routed back to
    /// the awaiting tool call by `request_id`.
    QueryResult {
        request_id: u64,
        #[serde(default)]
        blocks: Vec<BlockInfo>,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        error: Option<String>,
    },
}

/// One block read back from the world, used by the read tools.
#[derive(Debug, Clone, Deserialize)]
struct BlockInfo {
    x: i32,
    y: i32,
    z: i32,
    block: String,
}

/// Hyperion's reply to a world-read query (carried inside [`InMsg::QueryResult`]).
#[derive(Debug)]
struct QueryResult {
    blocks: Vec<BlockInfo>,
    truncated: bool,
    error: Option<String>,
}

/// World-read queries awaiting Hyperion's reply, keyed by `request_id`. A tool
/// call inserts a oneshot here, emits the query, then awaits the receiver; the
/// stdin loop completes it when `QueryResult` arrives.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<QueryResult>>>>;

/// Internal carrier for one turn's data (built from [`InMsg::Turn`]).
#[derive(Debug)]
struct TurnRequest {
    session: u64,
    prompt: String,
    pos: Option<[f32; 3]>,
}

/// The action payload — internally tagged so kinds can grow.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ActionKind {
    Say {
        text: String,
    },
    /// A transient progress line shown to the player in a muted style — not the
    /// agent "speaking", just a "what I'm doing now" beat (searching, scanning).
    Status {
        text: String,
    },
    PlaceBlock {
        x: i32,
        y: i32,
        z: i32,
        block: String,
    },
    Fill {
        x1: i32,
        y1: i32,
        z1: i32,
        x2: i32,
        y2: i32,
        z2: i32,
        block: String,
    },
    /// Read one block. Hyperion replies with an `InMsg::QueryResult` tagged
    /// with the same `request_id`.
    GetBlock {
        request_id: u64,
        x: i32,
        y: i32,
        z: i32,
    },
    /// Read a whole cuboid; Hyperion replies with its non-air blocks.
    ScanRegion {
        request_id: u64,
        x1: i32,
        y1: i32,
        z1: i32,
        x2: i32,
        y2: i32,
        z2: i32,
    },
}

/// One stdout line: a session id plus a flattened action.
#[derive(Debug, Serialize)]
struct OutLine {
    session: u64,
    #[serde(flatten)]
    kind: ActionKind,
}

/// Emit one action as a JSON line on stdout (the channel Hyperion reads).
fn emit(session: u64, kind: ActionKind) {
    match serde_json::to_string(&OutLine { session, kind }) {
        Ok(line) => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            // Ignore write errors: if Hyperion closed the pipe we just stop.
            let _ = writeln!(lock, "{line}");
            let _ = lock.flush();
        }
        Err(e) => tracing::error!("failed to serialize action: {e}"),
    }
}

/// Emit a transient progress line to the player (muted style on the Hyperion
/// side). Used by tool wrappers so the player sees the agent searching/scanning
/// instead of staring at a silent chat box during multi-second work.
fn emit_status(session: u64, text: impl Into<String>) {
    emit(session, ActionKind::Status { text: text.into() });
}

/// Wraps another tool so a short status beat is shown to the player the moment
/// the agent invokes it (e.g. "🔍 Searching the web…"), then delegates. Used to
/// announce iacoder's own web tools, which Hyperion can't otherwise observe.
#[derive(Debug)]
struct Announce {
    inner: iacoder_core::BoxedTool,
    session: u64,
    status: String,
}

#[async_trait]
impl Tool for Announce {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }

    async fn call(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        emit_status(self.session, self.status.clone());
        self.inner.call(args, ctx).await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SayInput {
    /// The chat message to send to the player.
    message: String,
}

/// The canonical "act on the game" tool. Emits a `say` action tagged with the
/// session it belongs to, which Hyperion routes back to the right player.
#[derive(Debug)]
struct SayTool {
    session: u64,
}

#[async_trait]
impl Tool for SayTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "say".into(),
            description: "Send a chat message to the player who is talking to you. Use this \
                          whenever you want to say something to them in-game."
                .into(),
            input_schema: schema_for!(SayInput),
            annotations: ToolAnnotations {
                readonly: false,
                ..Default::default()
            },
        }
    }

    async fn call(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let input: SayInput = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("say: {e}")))?;
        emit(self.session, ActionKind::Say { text: input.message });
        Ok(ToolOutput::text("Message delivered to the player."))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PlaceBlockInput {
    /// Integer world X coordinate.
    x: i32,
    /// Integer world Y coordinate (height; ground is usually around 64).
    y: i32,
    /// Integer world Z coordinate.
    z: i32,
    /// Block type name, e.g. "stone", "oak_planks", "glass", "glowstone".
    block: String,
}

/// Place a single block in the world. To build a structure, call once per
/// block. Hyperion validates the block name and applies it to the world.
#[derive(Debug)]
struct PlaceBlockTool {
    session: u64,
}

#[async_trait]
impl Tool for PlaceBlockTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "place_block".into(),
            description: "Place one block at integer world coordinates (x, y, z). `block` is a \
                          block name like \"stone\", \"oak_planks\", \"glass\", or \"glowstone\". \
                          Call once per block to build structures."
                .into(),
            input_schema: schema_for!(PlaceBlockInput),
            annotations: ToolAnnotations {
                readonly: false,
                ..Default::default()
            },
        }
    }

    async fn call(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let input: PlaceBlockInput = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("place_block: {e}")))?;
        let summary = format!(
            "Placed {} at ({}, {}, {}).",
            input.block, input.x, input.y, input.z
        );
        emit(
            self.session,
            ActionKind::PlaceBlock {
                x: input.x,
                y: input.y,
                z: input.z,
                block: input.block,
            },
        );
        Ok(ToolOutput::text(summary))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FillInput {
    /// First corner X.
    x1: i32,
    /// First corner Y.
    y1: i32,
    /// First corner Z.
    z1: i32,
    /// Opposite corner X.
    x2: i32,
    /// Opposite corner Y.
    y2: i32,
    /// Opposite corner Z.
    z2: i32,
    /// Block type name, e.g. "stone", "glass", "oak_planks". Use "air" to clear.
    block: String,
}

/// Fill the whole cuboid between two corners (inclusive) with one block —
/// the efficient way to build walls, floors, towers, and clear areas. Far
/// cheaper than many `place_block` calls. Hyperion caps the volume.
#[derive(Debug)]
struct FillTool {
    session: u64,
}

#[async_trait]
impl Tool for FillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "fill".into(),
            description: "Fill the cuboid region between two corners (x1,y1,z1)-(x2,y2,z2), \
                          inclusive, with a single block. Use this for walls, floors, towers, \
                          boxes, and clearing areas (block=\"air\"). Much better than many \
                          place_block calls. The server caps very large regions."
                .into(),
            input_schema: schema_for!(FillInput),
            annotations: ToolAnnotations {
                readonly: false,
                ..Default::default()
            },
        }
    }

    async fn call(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let input: FillInput = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("fill: {e}")))?;
        let summary = format!(
            "Filling ({},{},{})-({},{},{}) with {}.",
            input.x1, input.y1, input.z1, input.x2, input.y2, input.z2, input.block
        );
        emit(
            self.session,
            ActionKind::Fill {
                x1: input.x1,
                y1: input.y1,
                z1: input.z1,
                x2: input.x2,
                y2: input.y2,
                z2: input.z2,
                block: input.block,
            },
        );
        Ok(ToolOutput::text(summary))
    }
}

/// Emit a world-read query and await Hyperion's reply (or time out). Shared by
/// the `get_block` and `scan_region` tools.
async fn run_query(
    pending: &Pending,
    next_id: &AtomicU64,
    session: u64,
    make: impl FnOnce(u64) -> ActionKind,
) -> Result<QueryResult, String> {
    let request_id = next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    match pending.lock() {
        Ok(mut map) => {
            map.insert(request_id, tx);
        }
        Err(e) => return Err(format!("pending map poisoned: {e}")),
    }
    emit(session, make(request_id));
    match tokio::time::timeout(QUERY_TIMEOUT, rx).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(_)) => Err("world read failed (reply channel closed)".to_owned()),
        Err(_) => {
            // Don't leak the pending entry on timeout.
            if let Ok(mut map) = pending.lock() {
                map.remove(&request_id);
            }
            Err("world read timed out (server busy, or persistence disabled?)".to_owned())
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetBlockInput {
    /// Integer world X coordinate.
    x: i32,
    /// Integer world Y coordinate.
    y: i32,
    /// Integer world Z coordinate.
    z: i32,
}

/// Read the block at one world coordinate (round-trips to Hyperion).
#[derive(Debug)]
struct GetBlockTool {
    session: u64,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

#[async_trait]
impl Tool for GetBlockTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "get_block".into(),
            description: "Read the block at integer world coordinates (x, y, z). Returns the \
                          block name, or that the position is air or unloaded. Use it to look \
                          before you build."
                .into(),
            input_schema: schema_for!(GetBlockInput),
            annotations: ToolAnnotations {
                readonly: true,
                ..Default::default()
            },
        }
    }

    async fn call(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let input: GetBlockInput = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("get_block: {e}")))?;
        let (x, y, z) = (input.x, input.y, input.z);
        let text = match run_query(&self.pending, &self.next_id, self.session, |request_id| {
            ActionKind::GetBlock { request_id, x, y, z }
        })
        .await
        {
            Ok(result) => match result.error {
                Some(err) => format!("({x}, {y}, {z}): {err}"),
                None => match result.blocks.first() {
                    Some(b) => format!("Block at ({x}, {y}, {z}) is `{}`.", b.block),
                    None => format!("Block at ({x}, {y}, {z}) is air."),
                },
            },
            Err(err) => err,
        };
        Ok(ToolOutput::text(text))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ScanRegionInput {
    /// First corner X.
    x1: i32,
    /// First corner Y.
    y1: i32,
    /// First corner Z.
    z1: i32,
    /// Opposite corner X.
    x2: i32,
    /// Opposite corner Y.
    y2: i32,
    /// Opposite corner Z.
    z2: i32,
}

/// Read a whole cuboid and list its non-air blocks (round-trips to Hyperion).
#[derive(Debug)]
struct ScanRegionTool {
    session: u64,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

#[async_trait]
impl Tool for ScanRegionTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "scan_region".into(),
            description: "Read the cuboid between two corners (x1,y1,z1)-(x2,y2,z2), inclusive, \
                          and list its non-air blocks (air is omitted). Use it to understand the \
                          terrain before building and to verify your build afterwards. The server \
                          caps very large regions."
                .into(),
            input_schema: schema_for!(ScanRegionInput),
            annotations: ToolAnnotations {
                readonly: true,
                ..Default::default()
            },
        }
    }

    async fn call(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let i: ScanRegionInput = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs(format!("scan_region: {e}")))?;
        emit_status(self.session, "👀 Looking at the area…");
        let text = match run_query(&self.pending, &self.next_id, self.session, |request_id| {
            ActionKind::ScanRegion {
                request_id,
                x1: i.x1,
                y1: i.y1,
                z1: i.z1,
                x2: i.x2,
                y2: i.y2,
                z2: i.z2,
            }
        })
        .await
        {
            Ok(result) => {
                tracing::debug!(
                    "scan_region ({},{},{})-({},{},{}) -> {} block(s){}",
                    i.x1,
                    i.y1,
                    i.z1,
                    i.x2,
                    i.y2,
                    i.z2,
                    result.blocks.len(),
                    if result.truncated { " (truncated)" } else { "" }
                );
                if let Some(err) = result.error {
                    err
                } else if result.blocks.is_empty() {
                    "Region is all air (or unloaded).".to_owned()
                } else {
                    let mut s = format!("{} non-air block(s):\n", result.blocks.len());
                    for b in &result.blocks {
                        s.push_str(&format!("({}, {}, {}) {}\n", b.x, b.y, b.z, b.block));
                    }
                    if result.truncated {
                        s.push_str("… (truncated; scan a smaller region for the rest)\n");
                    }
                    s
                }
            }
            Err(err) => err,
        };
        Ok(ToolOutput::text(text))
    }
}

/// Drive one player turn to completion, continuing that session's history.
async fn handle_turn(
    provider: Arc<dyn LlmProvider>,
    model: String,
    sessions: Sessions,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    req: TurnRequest,
) {
    let session = req.session;

    // Snapshot this session's history (drop the lock before the await).
    let prior = {
        let guard = match sessions.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::error!("session lock poisoned: {e}");
                return;
            }
        };
        guard.get(&session).cloned()
    };

    let tools = vec![
        Arc::new(SayTool { session }) as iacoder_core::BoxedTool,
        Arc::new(PlaceBlockTool { session }) as iacoder_core::BoxedTool,
        Arc::new(FillTool { session }) as iacoder_core::BoxedTool,
        Arc::new(GetBlockTool {
            session,
            pending: Arc::clone(&pending),
            next_id: Arc::clone(&next_id),
        }) as iacoder_core::BoxedTool,
        Arc::new(ScanRegionTool {
            session,
            pending: Arc::clone(&pending),
            next_id: Arc::clone(&next_id),
        }) as iacoder_core::BoxedTool,
        // iacoder's built-in web tools, so the agent can research what to build.
        // `web_search` needs a TAVILY_API_KEY or BRAVE_API_KEY in the environment
        // (read by `from_env_and_config`); `web_fetch` needs no key. Wrapped in
        // `Announce` so the player sees a progress beat when they run.
        Arc::new(Announce {
            inner: Arc::new(iacoder_tools::WebSearchTool::from_env_and_config(
                &iacoder_tools::SearchBackendsConfig::default(),
            )),
            session,
            status: "🔍 Searching the web…".to_owned(),
        }) as iacoder_core::BoxedTool,
        Arc::new(Announce {
            inner: Arc::new(iacoder_tools::WebFetchTool::new()),
            session,
            status: "📖 Reading a page…".to_owned(),
        }) as iacoder_core::BoxedTool,
    ];
    let agent = Runtime::new(provider, tools, Arc::new(AllowAll), MAX_TURNS);

    // Position changes between turns, so inject it into the user prompt (not
    // the cached system prompt) each turn.
    let user_prompt = match req.pos {
        Some([x, y, z]) => format!(
            "[Context: the player is standing at block coordinates x={}, y={}, z={}. When they \
             say \"here\" or ask you to build, place blocks at or near these coordinates.]\n\n{}",
            x.floor() as i32,
            y.floor() as i32,
            z.floor() as i32,
            req.prompt
        ),
        None => req.prompt,
    };

    let run_req = RunRequest {
        model,
        // `system` only seeds a fresh transcript; ignored when continuing one.
        system: if prior.is_none() {
            Some(SYSTEM_PROMPT.to_owned())
        } else {
            None
        },
        user_prompt,
        user_attachments: Vec::new(),
        prior_history: prior,
        prior_tool_meta: None,
        memory_messages: Vec::new(),
        memory_snapshot: None,
        cwd: camino::Utf8PathBuf::from("."),
        cancel: tokio_util::sync::CancellationToken::new(),
        sampling: SamplingParams::default(),
        history_budget: None,
    };

    match agent.run(run_req, None).await {
        Ok(result) => {
            let text = result.assistant_text.trim();
            if !text.is_empty() {
                emit(session, ActionKind::Say { text: text.to_owned() });
            }
            // Persist the updated transcript for this session's next turn.
            if let Ok(mut guard) = sessions.lock() {
                guard.insert(session, result.final_history);
            }
        }
        Err(e) => {
            tracing::error!("agent run failed (session {session}): {e}");
            emit(
                session,
                ActionKind::Say {
                    text: format!("§csorry, something went wrong: {e}"),
                },
            );
        }
    }
}

/// Read request lines from stdin forever, spawning a task per turn. Different
/// sessions run concurrently; the provider is shared across all of them.
async fn serve(provider: Arc<dyn LlmProvider>, model: String) -> anyhow::Result<()> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU64::new(1));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: InMsg = match serde_json::from_str(line) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!("ignoring unparseable request line: {e}: {line}");
                continue;
            }
        };
        match msg {
            InMsg::Turn {
                session,
                prompt,
                pos,
            } => {
                let provider = Arc::clone(&provider);
                let model = model.clone();
                let sessions = Arc::clone(&sessions);
                let pending = Arc::clone(&pending);
                let next_id = Arc::clone(&next_id);
                tokio::spawn(handle_turn(
                    provider,
                    model,
                    sessions,
                    pending,
                    next_id,
                    TurnRequest {
                        session,
                        prompt,
                        pos,
                    },
                ));
            }
            InMsg::Evict { session } => {
                if let Ok(mut map) = sessions.lock() {
                    map.remove(&session);
                }
                tracing::debug!("evicted session {session}");
            }
            InMsg::QueryResult {
                request_id,
                blocks,
                truncated,
                error,
            } => {
                let sender = pending.lock().ok().and_then(|mut map| map.remove(&request_id));
                if let Some(tx) = sender {
                    // Receiver gone (tool timed out already) is fine to ignore.
                    let _ = tx.send(QueryResult {
                        blocks,
                        truncated,
                        error,
                    });
                } else {
                    tracing::debug!("query result for unknown/expired request {request_id}");
                }
            }
        }
    }

    tracing::info!("stdin closed; sidecar shutting down");
    Ok(())
}

/// Offline mode: no LLM, just echo each prompt back as a chat action. Lets you
/// verify the Hyperion <-> sidecar plumbing end to end without an API key.
async fn serve_offline() -> anyhow::Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<InMsg>(line) {
            Ok(InMsg::Turn {
                session, prompt, ..
            }) => emit(
                session,
                ActionKind::Say {
                    text: format!("(offline) you said: {prompt}"),
                },
            ),
            Ok(InMsg::Evict { .. } | InMsg::QueryResult { .. }) => {}
            Err(_) => {}
        }
    }
    Ok(())
}

/// Resolve the LLM backend (built once, shared by every turn — the cold-start
/// cost we save by staying resident).
///
/// Preferred path: an iacoder `config.toml` baked into the image (path from
/// `HYPERION_AI_CONFIG`, default `/config.toml`). It selects provider + model
/// and resolves API keys via `${ENV:VAR}`, so secrets stay in the environment,
/// not in the image. The model is `HYPERION_AI_MODEL`, else the config's
/// `default_model` / `default_models.chat`.
///
/// Fallback (no config file): env-only Anthropic, for native dev convenience.
fn resolve_backend() -> anyhow::Result<(Arc<dyn LlmProvider>, String)> {
    let config_path =
        std::env::var("HYPERION_AI_CONFIG").unwrap_or_else(|_| "/config.toml".to_owned());
    let path = camino::Utf8Path::new(&config_path);

    if path.exists() {
        let config = Config::from_file(path)
            .with_context(|| format!("loading AI config from {path}"))?;
        let selection = std::env::var("HYPERION_AI_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| config.default_model.clone())
            .or_else(|| config.default_models.chat.clone())
            .context("no model selected: set HYPERION_AI_MODEL or default_model in config.toml")?;
        let built = build_model(&config, &selection)
            .map_err(|e| anyhow::anyhow!("build_model({selection}) failed: {e}"))?;
        tracing::info!("using config {path} (selection: {selection}, model: {})", built.model_id);
        Ok((built.provider, built.model_id))
    } else {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .context("no config.toml at HYPERION_AI_CONFIG and ANTHROPIC_API_KEY is not set")?;
        let model =
            std::env::var("HYPERION_AI_MODEL").unwrap_or_else(|_| "claude-sonnet-4-5".to_owned());
        let provider =
            iacoder_core::provider::factory::build_provider_with_key("anthropic", None, &api_key)
                .context("failed to build Anthropic provider")?;
        tracing::info!("no config.toml; using env Anthropic (model: {model})");
        Ok((provider, model))
    }
}

async fn run() -> anyhow::Result<()> {
    if std::env::var("HYPERION_AI_OFFLINE").is_ok() {
        tracing::info!("starting in OFFLINE mode (no LLM)");
        return serve_offline().await;
    }

    let (provider, model) = resolve_backend()?;
    tracing::info!("sidecar ready (model: {model})");
    serve(provider, model).await
}

#[tokio::main]
async fn main() {
    // Diagnostics to stderr only — stdout is the action channel.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    if let Err(e) = run().await {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}
