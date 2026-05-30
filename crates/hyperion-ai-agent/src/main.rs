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
use std::sync::{Arc, Mutex};

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

/// Per-turn budget: how many tool-call rounds the agent may take.
const MAX_TURNS: usize = 8;

const SYSTEM_PROMPT: &str = "You are an in-game AI assistant on a Minecraft server (the Hyperion \
engine). A player is talking to you through chat. Keep replies short — one or two sentences — \
because they appear in the Minecraft chat box.\n\nTools:\n- `say`: speak to the player.\n- \
`place_block`: place ONE block at integer world coordinates (x, y, z).\n- \
`fill`: fill a whole cuboid between two corners with one block — use this for walls, floors, \
towers, boxes, and clearing areas (block=\"air\"). Prefer `fill` over many `place_block` calls.\n\n\
Block names look like \"stone\", \"oak_planks\", \"glass\", \"glowstone\". The player's current \
coordinates are given to you each turn so you can build relative to where they stand (e.g. a few \
blocks in front of and beside them, not inside them).\n\nYour final response text is also shown \
to the player automatically. Be helpful and concise.";

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
}

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

/// Drive one player turn to completion, continuing that session's history.
async fn handle_turn(
    provider: Arc<dyn LlmProvider>,
    model: String,
    sessions: Sessions,
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
                tokio::spawn(handle_turn(
                    provider,
                    model,
                    sessions,
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
            Ok(InMsg::Evict { .. }) => {}
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
