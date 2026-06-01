//! In-game AI assistant (Hyperion side).
//!
//! The agent runs *out of process* in the long-lived `hyperion-ai-agent`
//! sidecar (a separate crate on its own stable toolchain — see that crate's
//! README for why). This module is the thin Hyperion-side bridge.
//!
//! One sidecar process is spawned at server boot and kept alive. Two tokio
//! tasks own its pipes:
//!
//! - **writer**: receives `/ai` requests, assigns each player a stable
//!   `session` id, and writes `{"session","prompt"}` lines to the child stdin.
//! - **reader**: reads `{"session","type":"say","text"}` lines from the child
//!   stdout, resolves the session back to a [`ConnectionId`], and enqueues a
//!   [`GameAction`].
//!
//! A Bevy system ([`drain_ai_actions`]) drains the action channel every tick
//! and applies actions to the world, so all `World` mutation stays on the
//! Bevy thread.
//!
//! ```text
//!   /ai <prompt>  -> writer -> child stdin  {"session":7,"prompt":"…"}
//!                                   │  iacoder agent (per-session history)
//!   player chat  <- drain  <- reader <- child stdout {"session":7,"type":"say",…}
//! ```
//!
//! The `session` id is what gives each player **multi-turn context**: the
//! sidecar keys conversation history on it, and Hyperion keeps the
//! `session <-> connection` mapping so it can both reuse a player's session
//! and route replies back.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use bevy::{ecs::system::SystemState, prelude::*};
use clap::Parser;
use glam::{I16Vec2, IVec3};
use hyperion::net::{Compose, ConnectionId, agnostic};
use hyperion::simulation::{Position, blocks::Blocks, packet_state};
use hyperion_clap::{CommandPermission, MinecraftCommand};
use serde::{Deserialize, Serialize};
use valence_protocol::{BlockPos, BlockState, block::BlockKind, packets::play};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command as TokioCommand};
use tracing::{error, info, warn};

/// How long to wait between sidecar restart attempts.
const RESTART_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
/// Give up restarting after this many consecutive spawn failures.
const MAX_RESTART_FAILURES: u32 = 5;
/// A child that dies sooner than this after starting is treated as a startup
/// crash (bad config/key/model), not a transient fault.
const HEALTHY_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(10);
/// Give up after this many back-to-back startup crashes, to avoid an infinite
/// hot-restart loop on a misconfigured sidecar.
const MAX_RAPID_FAILURES: u32 = 5;

/// A request from a player, sent over the command channel to the writer task.
#[derive(Debug)]
struct AiRequest {
    connection: ConnectionId,
    prompt: String,
    /// The player's position when they invoked `/ai`, forwarded to the agent.
    pos: Option<[f32; 3]>,
}

/// A message to the sidecar-driving task: either a new turn or a request to
/// drop a disconnected player's session (so the sidecar frees its history and
/// Hyperion frees the session id).
#[derive(Debug)]
enum BridgeMsg {
    Turn(AiRequest),
    Evict(ConnectionId),
    /// A world-read answer to send back to the sidecar (in reply to a
    /// `get_block`/`scan_region` query), routed by `request_id`.
    QueryResult {
        request_id: u64,
        blocks: Vec<BlockInfo>,
        truncated: bool,
        error: Option<String>,
    },
}

/// One block read back from the world, serialized to the sidecar.
#[derive(Debug, Serialize)]
struct BlockInfo {
    x: i32,
    y: i32,
    z: i32,
    block: String,
}

/// An action to apply to the game world, drained on the Bevy thread.
#[derive(Debug)]
struct GameAction {
    connection: ConnectionId,
    kind: ActionKind,
}

#[derive(Debug)]
enum ActionKind {
    Say(String),
    Status(String),
    PlaceBlock { pos: IVec3, block: String },
    Fill { min: IVec3, max: IVec3, block: String },
    GetBlock { request_id: u64, pos: IVec3 },
    ScanRegion { request_id: u64, min: IVec3, max: IVec3 },
}

/// Max blocks a single `fill` may touch. Caps both abuse ("fill the world")
/// and the per-block `BlockUpdateS2c` packet burst we send to the player.
const MAX_FILL_VOLUME: i64 = 16_384;

/// Max blocks a single `scan_region` may read. Bounds the read cost and the
/// size of the result we send back to the sidecar.
const MAX_SCAN_VOLUME: i64 = 16_384;

/// Max non-air blocks returned from one `scan_region`, to bound the reply
/// payload (and the agent's token cost). Beyond this the result is truncated.
const MAX_SCAN_RESULTS: usize = 1_024;

/// One line written to the sidecar's stdin. Tagged so the sidecar can tell a
/// new turn from a session eviction.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutMsg {
    Turn {
        session: u64,
        prompt: String,
        pos: Option<[f32; 3]>,
    },
    Evict {
        session: u64,
    },
    /// Reply to a world-read query, matched on the sidecar by `request_id`.
    QueryResult {
        request_id: u64,
        blocks: Vec<BlockInfo>,
        truncated: bool,
        error: Option<String>,
    },
}

/// One action line read from the sidecar's stdout: a session id plus a
/// flattened, internally-tagged action (matches the sidecar's `OutLine`).
#[derive(Debug, Deserialize)]
struct InAction {
    session: u64,
    #[serde(flatten)]
    kind: AgentAction,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AgentAction {
    Say {
        text: String,
    },
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
    GetBlock {
        request_id: u64,
        x: i32,
        y: i32,
        z: i32,
    },
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

/// Bidirectional `session <-> connection` map. Sessions are stable per player
/// connection so the sidecar accumulates multi-turn history; assigned lazily
/// on first `/ai` use.
#[derive(Default)]
struct SessionMap {
    fwd: HashMap<ConnectionId, u64>,
    rev: HashMap<u64, ConnectionId>,
    next: u64,
}

impl SessionMap {
    /// Return this connection's session id, allocating one on first sight.
    fn assign(&mut self, connection: ConnectionId) -> u64 {
        if let Some(&id) = self.fwd.get(&connection) {
            return id;
        }
        self.next += 1;
        let id = self.next;
        self.fwd.insert(connection, id);
        self.rev.insert(id, connection);
        id
    }

    /// Resolve a session id back to its connection.
    fn resolve(&self, session: u64) -> Option<ConnectionId> {
        self.rev.get(&session).copied()
    }

    /// Forget a connection's session (on disconnect). Returns the freed id.
    fn remove(&mut self, connection: ConnectionId) -> Option<u64> {
        let id = self.fwd.remove(&connection)?;
        self.rev.remove(&id);
        Some(id)
    }
}

/// Bridge resource: channel ends plus the tokio runtime owning the tasks.
#[derive(Resource)]
pub struct AiBridge {
    /// `Some` when the sidecar spawned successfully. `None` disables `/ai`
    /// (it then replies with a friendly error rather than silently dropping).
    cmd_tx: Option<flume::Sender<BridgeMsg>>,
    /// Drained each tick to apply [`GameAction`]s.
    action_rx: flume::Receiver<GameAction>,
    /// Kept alive so the tasks run for the process lifetime.
    _runtime: Arc<tokio::runtime::Runtime>,
}

impl AiBridge {
    /// Queue an AI turn on behalf of `connection` (e.g. triggered by talking to
    /// an NPC instead of the `/ai` command). The reply streams back to that
    /// player. Returns `false` if the sidecar is unavailable or busy.
    #[must_use]
    pub fn ask(&self, connection: ConnectionId, prompt: String, pos: Option<[f32; 3]>) -> bool {
        let Some(cmd_tx) = &self.cmd_tx else {
            return false;
        };
        cmd_tx
            .try_send(BridgeMsg::Turn(AiRequest {
                connection,
                prompt,
                pos,
            }))
            .is_ok()
    }
}

/// Locate the sidecar binary. Resolution order:
/// 1. `BEDWARS_AI_AGENT_BIN` if set (the Docker image sets this to
///    `/hyperion-ai-agent`).
/// 2. Dev default `$HOME/.cargo/target/debug/hyperion-ai-agent` — a global
///    `~/.cargo/config.toml` redirects all builds to `~/.cargo/target`, so the
///    sidecar lands here. Derived from `$HOME`, so it works on any dev machine
///    (no hardcoded username).
/// 3. Bare `hyperion-ai-agent`, resolved via `PATH`, as a last resort.
fn agent_bin() -> String {
    if let Ok(path) = std::env::var("BEDWARS_AI_AGENT_BIN") {
        return path;
    }
    if let Ok(home) = std::env::var("HOME") {
        return format!("{home}/.cargo/target/debug/hyperion-ai-agent");
    }
    "hyperion-ai-agent".to_owned()
}

/// Spawn the sidecar with piped stdio.
fn spawn_child(bin: &str) -> std::io::Result<Child> {
    TokioCommand::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
}

/// Serialize one [`BridgeMsg`] and write it to the child's stdin. Returns
/// `false` only on a write/flush failure (the child is presumed dead, so the
/// supervisor should restart it). Session bookkeeping is done here.
async fn write_msg(
    stdin: &mut ChildStdin,
    sessions: &Arc<Mutex<SessionMap>>,
    msg: BridgeMsg,
) -> bool {
    let out = match msg {
        BridgeMsg::Turn(req) => {
            let session = match sessions.lock() {
                Ok(mut map) => map.assign(req.connection),
                Err(e) => {
                    error!("session map poisoned: {e}");
                    return true;
                }
            };
            OutMsg::Turn {
                session,
                prompt: req.prompt,
                pos: req.pos,
            }
        }
        BridgeMsg::Evict(connection) => {
            let removed = sessions.lock().ok().and_then(|mut map| map.remove(connection));
            let Some(session) = removed else {
                return true; // player never used /ai; nothing to evict
            };
            OutMsg::Evict { session }
        }
        BridgeMsg::QueryResult {
            request_id,
            blocks,
            truncated,
            error,
        } => OutMsg::QueryResult {
            request_id,
            blocks,
            truncated,
            error,
        },
    };

    let line = match serde_json::to_string(&out) {
        Ok(line) => line,
        Err(e) => {
            error!("failed to serialize sidecar message: {e}");
            return true;
        }
    };
    if let Err(e) = stdin.write_all(format!("{line}\n").as_bytes()).await {
        error!("failed to write to AI sidecar (is it alive?): {e}");
        return false;
    }
    if let Err(e) = stdin.flush().await {
        error!("failed to flush AI sidecar stdin: {e}");
        return false;
    }
    true
}

/// Drive one child to death. Returns `true` if the supervisor should restart
/// (the child exited), `false` if the bridge is shutting down (the request
/// channel closed).
async fn run_session(
    child: &mut Child,
    sessions: &Arc<Mutex<SessionMap>>,
    requests: &flume::Receiver<BridgeMsg>,
    actions: &flume::Sender<GameAction>,
) -> bool {
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        error!("AI sidecar stdio was not captured");
        return true;
    };

    let reader = tokio::spawn(reader_loop(stdout, Arc::clone(sessions), actions.clone()));
    let mut stdin = stdin;

    let restart = loop {
        tokio::select! {
            biased;
            status = child.wait() => {
                match status {
                    Ok(s) => warn!("AI sidecar exited with {s}"),
                    Err(e) => warn!("failed to await AI sidecar: {e}"),
                }
                break true;
            }
            msg = requests.recv_async() => {
                match msg {
                    Ok(msg) => {
                        if !write_msg(&mut stdin, sessions, msg).await {
                            break true; // pipe broke -> restart
                        }
                    }
                    Err(_) => break false, // channel closed -> shutting down
                }
            }
        }
    };

    reader.abort();
    restart
}

/// Supervisor: keep one sidecar alive, restarting it (with backoff) if it
/// dies. Sessions persist across restarts so reply routing keeps working;
/// the new process starts with empty per-session history.
async fn supervise(
    bin: String,
    first_child: Child,
    sessions: Arc<Mutex<SessionMap>>,
    requests: flume::Receiver<BridgeMsg>,
    actions: flume::Sender<GameAction>,
) {
    let mut child = first_child;
    let mut rapid_failures = 0_u32;
    loop {
        let started = std::time::Instant::now();
        if !run_session(&mut child, &sessions, &requests, &actions).await {
            info!("AI bridge shutting down");
            return;
        }

        // Crash-loop guard: a child that barely ran almost certainly died on a
        // startup error (bad config/model/key), so don't hot-restart forever.
        if started.elapsed() < HEALTHY_THRESHOLD {
            rapid_failures += 1;
            error!(
                "AI sidecar crashed right after start ({rapid_failures}/{MAX_RAPID_FAILURES}) — \
                 likely bad config/model/key; see the error above"
            );
            if rapid_failures >= MAX_RAPID_FAILURES {
                error!("AI sidecar keeps crashing on startup — giving up; /ai disabled until restart");
                return;
            }
        } else {
            rapid_failures = 0;
        }

        let mut spawn_failures = 0_u32;
        loop {
            tokio::time::sleep(RESTART_BACKOFF).await;
            match spawn_child(&bin) {
                Ok(new_child) => {
                    info!("AI sidecar restarted");
                    child = new_child;
                    break;
                }
                Err(e) => {
                    spawn_failures += 1;
                    error!("failed to restart AI sidecar ({bin}): {e}");
                    if spawn_failures >= MAX_RESTART_FAILURES {
                        error!("giving up after {spawn_failures} spawn attempts — /ai disabled");
                        return;
                    }
                }
            }
        }
    }
}

/// Reader task: parse the child's stdout actions and route them back to the
/// originating player by session id.
async fn reader_loop(
    stdout: ChildStdout,
    sessions: Arc<Mutex<SessionMap>>,
    actions: flume::Sender<GameAction>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let action: InAction = match serde_json::from_str(line) {
                    Ok(action) => action,
                    Err(e) => {
                        warn!("ignoring unparseable agent line: {e}: {line}");
                        continue;
                    }
                };
                let Some(connection) = sessions
                    .lock()
                    .ok()
                    .and_then(|map| map.resolve(action.session))
                else {
                    warn!("agent referenced unknown session {}", action.session);
                    continue;
                };
                let kind = match action.kind {
                    AgentAction::Say { text } => ActionKind::Say(text),
                    AgentAction::Status { text } => ActionKind::Status(text),
                    AgentAction::PlaceBlock { x, y, z, block } => ActionKind::PlaceBlock {
                        pos: IVec3::new(x, y, z),
                        block,
                    },
                    AgentAction::Fill {
                        x1,
                        y1,
                        z1,
                        x2,
                        y2,
                        z2,
                        block,
                    } => ActionKind::Fill {
                        min: IVec3::new(x1.min(x2), y1.min(y2), z1.min(z2)),
                        max: IVec3::new(x1.max(x2), y1.max(y2), z1.max(z2)),
                        block,
                    },
                    AgentAction::GetBlock {
                        request_id,
                        x,
                        y,
                        z,
                    } => ActionKind::GetBlock {
                        request_id,
                        pos: IVec3::new(x, y, z),
                    },
                    AgentAction::ScanRegion {
                        request_id,
                        x1,
                        y1,
                        z1,
                        x2,
                        y2,
                        z2,
                    } => ActionKind::ScanRegion {
                        request_id,
                        min: IVec3::new(x1.min(x2), y1.min(y2), z1.min(z2)),
                        max: IVec3::new(x1.max(x2), y1.max(y2), z1.max(z2)),
                    },
                };
                if actions
                    .send_async(GameAction { connection, kind })
                    .await
                    .is_err()
                {
                    warn!("action channel closed; stopping reader");
                    break;
                }
            }
            Ok(None) => {
                warn!("AI sidecar stdout closed (process exited?)");
                break;
            }
            Err(e) => {
                error!("error reading AI sidecar stdout: {e}");
                break;
            }
        }
    }
}

fn build_bridge() -> AiBridge {
    let (action_tx, action_rx) = flume::unbounded::<GameAction>();
    let (cmd_tx, cmd_rx) = flume::unbounded::<BridgeMsg>();

    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("failed to build tokio runtime for AI sidecar"),
    );

    let bin = agent_bin();

    // Spawn the first child inside the runtime context (tokio::process needs
    // the reactor). If even the first spawn fails, the binary is missing or
    // misconfigured — disable `/ai` rather than spin restarting forever.
    let spawned = runtime.block_on(async { spawn_child(&bin) });

    let cmd_tx = match spawned {
        Ok(child) => {
            let sessions = Arc::new(Mutex::new(SessionMap::default()));
            runtime.spawn(supervise(bin.clone(), child, sessions, cmd_rx, action_tx));
            info!("AI assistant ready (persistent sidecar: {bin})");
            Some(cmd_tx)
        }
        Err(e) => {
            error!("failed to spawn AI sidecar ({bin}): {e} — /ai disabled");
            None
        }
    };

    AiBridge {
        cmd_tx,
        action_rx,
        _runtime: runtime,
    }
}

/// Bevy system: apply queued game actions to the world every tick.
fn drain_ai_actions(
    bridge: Res<'_, AiBridge>,
    compose: Res<'_, Compose>,
    mut blocks: ResMut<'_, Blocks>,
) {
    for action in bridge.action_rx.try_iter() {
        match action.kind {
            ActionKind::Say(text) => {
                let packet = agnostic::chat(format!("§b[AI]§r {text}"));
                if let Err(e) = compose.unicast(&packet, action.connection) {
                    warn!("failed to deliver AI chat message: {e}");
                }
            }
            ActionKind::Status(text) => {
                // Muted style: a transient "what I'm doing" beat, not speech.
                let packet = agnostic::chat(format!("§7§o[AI] {text}§r"));
                if let Err(e) = compose.unicast(&packet, action.connection) {
                    warn!("failed to deliver AI status message: {e}");
                }
            }
            ActionKind::PlaceBlock { pos, block } => {
                place_block(&mut blocks, &compose, action.connection, pos, &block);
            }
            ActionKind::Fill { min, max, block } => {
                fill_region(&mut blocks, &compose, action.connection, min, max, &block);
            }
            ActionKind::GetBlock { request_id, pos } => {
                let (found, error) = match blocks.get_block(pos) {
                    Some(state) => (
                        vec![BlockInfo {
                            x: pos.x,
                            y: pos.y,
                            z: pos.z,
                            block: state.to_kind().to_str().to_owned(),
                        }],
                        None,
                    ),
                    None => (Vec::new(), Some("unloaded or out of bounds".to_owned())),
                };
                info!(
                    "AI get_block ({}, {}, {}) -> {}",
                    pos.x,
                    pos.y,
                    pos.z,
                    found
                        .first()
                        .map_or("unloaded", |b| b.block.as_str())
                );
                send_query_result(&bridge, request_id, found, false, error);
            }
            ActionKind::ScanRegion {
                request_id,
                min,
                max,
            } => {
                let volume = i64::from(max.x - min.x + 1)
                    * i64::from(max.y - min.y + 1)
                    * i64::from(max.z - min.z + 1);
                if volume > MAX_SCAN_VOLUME {
                    send_query_result(
                        &bridge,
                        request_id,
                        Vec::new(),
                        false,
                        Some(format!(
                            "region too large ({volume} blocks, max {MAX_SCAN_VOLUME})"
                        )),
                    );
                } else {
                    let mut found = Vec::new();
                    let mut truncated = false;
                    'scan: for x in min.x..=max.x {
                        for y in min.y..=max.y {
                            for z in min.z..=max.z {
                                let Some(state) = blocks.get_block(IVec3::new(x, y, z)) else {
                                    continue; // unloaded — skip
                                };
                                let kind = state.to_kind();
                                if matches!(
                                    kind,
                                    BlockKind::Air | BlockKind::CaveAir | BlockKind::VoidAir
                                ) {
                                    continue;
                                }
                                if found.len() >= MAX_SCAN_RESULTS {
                                    truncated = true;
                                    break 'scan;
                                }
                                found.push(BlockInfo {
                                    x,
                                    y,
                                    z,
                                    block: kind.to_str().to_owned(),
                                });
                            }
                        }
                    }
                    info!(
                        "AI scan_region ({}, {}, {})-({}, {}, {}) -> {} non-air block(s){}",
                        min.x,
                        min.y,
                        min.z,
                        max.x,
                        max.y,
                        max.z,
                        found.len(),
                        if truncated { " (truncated)" } else { "" }
                    );
                    send_query_result(&bridge, request_id, found, truncated, None);
                }
            }
        }
    }
}

/// Send a world-read answer back to the sidecar (in reply to a query action).
fn send_query_result(
    bridge: &AiBridge,
    request_id: u64,
    blocks: Vec<BlockInfo>,
    truncated: bool,
    error: Option<String>,
) {
    let Some(cmd_tx) = bridge.cmd_tx.as_ref() else {
        return;
    };
    if let Err(e) = cmd_tx.try_send(BridgeMsg::QueryResult {
        request_id,
        blocks,
        truncated,
        error,
    }) {
        warn!("failed to enqueue AI query result: {e}");
    }
}

/// Resolve a block name, or tell the player it's unknown and return `None`.
fn resolve_block(compose: &Compose, connection: ConnectionId, block: &str) -> Option<BlockState> {
    match BlockKind::from_str(block) {
        Some(kind) => Some(BlockState::from_kind(kind)),
        None => {
            let msg = agnostic::chat(format!("§c[AI] unknown block: {block}"));
            if let Err(e) = compose.unicast(&msg, connection) {
                warn!("ai chat send failed: {e}");
            }
            None
        }
    }
}

/// Broadcast a single block change to every player near it (including the one
/// who triggered it). The player didn't client-predict the change, so without
/// this nobody would see it. Skips blocks outside the broadcastable chunk
/// range rather than panicking on a bad coordinate.
fn broadcast_block(compose: &Compose, pos: IVec3, state: BlockState) {
    let (Ok(cx), Ok(cz)) = (i16::try_from(pos.x >> 4), i16::try_from(pos.z >> 4)) else {
        warn!("block at {pos:?} is outside broadcastable chunk range");
        return;
    };
    let update = play::BlockUpdateS2c {
        position: BlockPos::new(pos.x, pos.y, pos.z),
        block_id: state,
    };
    if let Err(e) = compose.broadcast_local(&update, I16Vec2::new(cx, cz)).send() {
        warn!("failed to broadcast block update: {e}");
    }
}

/// Set a single block in the world and broadcast it to nearby players.
fn place_block(
    blocks: &mut Blocks,
    compose: &Compose,
    connection: ConnectionId,
    pos: IVec3,
    block: &str,
) {
    let Some(state) = resolve_block(compose, connection, block) else {
        return;
    };
    if let Err(e) = blocks.set_block(pos, state) {
        let msg = agnostic::chat(format!("§c[AI] couldn't place {block}: {e:?}"));
        if let Err(err) = compose.unicast(&msg, connection) {
            warn!("ai chat send failed: {err}");
        }
        return;
    }
    broadcast_block(compose, pos, state);
}

/// Fill an inclusive cuboid `min..=max` with one block, capped at
/// [`MAX_FILL_VOLUME`]. Each placed block is broadcast to nearby players.
fn fill_region(
    blocks: &mut Blocks,
    compose: &Compose,
    connection: ConnectionId,
    min: IVec3,
    max: IVec3,
    block: &str,
) {
    let Some(state) = resolve_block(compose, connection, block) else {
        return;
    };

    let volume = i64::from(max.x - min.x + 1)
        * i64::from(max.y - min.y + 1)
        * i64::from(max.z - min.z + 1);
    if volume > MAX_FILL_VOLUME {
        let msg = agnostic::chat(format!(
            "§c[AI] region too large ({volume} blocks, max {MAX_FILL_VOLUME})"
        ));
        if let Err(e) = compose.unicast(&msg, connection) {
            warn!("ai chat send failed: {e}");
        }
        return;
    }

    let mut placed = 0_u32;
    let mut skipped = 0_u32;
    for x in min.x..=max.x {
        for y in min.y..=max.y {
            for z in min.z..=max.z {
                let pos = IVec3::new(x, y, z);
                match blocks.set_block(pos, state) {
                    Ok(_) => {
                        broadcast_block(compose, pos, state);
                        placed += 1;
                    }
                    Err(_) => skipped += 1,
                }
            }
        }
    }

    // No per-fill chat: a build is many fills, and one line each floods the box.
    // The agent's progress beats ("looking at the area…") and its final reply
    // tell the player what happened; the placed blocks speak for themselves.
    // Keep a server-side trace only.
    if skipped > 0 {
        tracing::debug!("AI fill {block}: placed {placed}, skipped {skipped} (out of bounds/unloaded)");
    }
}

#[derive(Parser, CommandPermission, Debug)]
#[command(name = "ai")]
#[command_permission(group = "Normal")]
pub struct AiCommand {
    /// Everything after `/ai` is the natural-language prompt.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 1..)]
    prompt: Vec<String>,
}

impl MinecraftCommand for AiCommand {
    type State = SystemState<(
        Res<'static, Compose>,
        Query<'static, 'static, (&'static ConnectionId, &'static Position)>,
        Res<'static, AiBridge>,
    )>;

    fn execute(self, world: &World, state: &mut Self::State, caller: Entity) {
        let (compose, query, bridge) = state.get(world);

        let Ok((&connection, position)) = query.get(caller) else {
            error!("ai command failed: caller is missing ConnectionId/Position");
            return;
        };
        let pos = Some([position.x, position.y, position.z]);

        let prompt = self.prompt.join(" ");
        if prompt.trim().is_empty() {
            let msg = agnostic::chat("§cUsage: /ai <what you want>");
            if let Err(e) = compose.unicast(&msg, connection) {
                warn!("ai chat send failed: {e}");
            }
            return;
        }

        let Some(cmd_tx) = bridge.cmd_tx.as_ref() else {
            let msg = agnostic::chat("§cThe AI assistant is not available on this server.");
            if let Err(e) = compose.unicast(&msg, connection) {
                warn!("ai chat send failed: {e}");
            }
            return;
        };

        match cmd_tx.try_send(BridgeMsg::Turn(AiRequest {
            connection,
            prompt,
            pos,
        })) {
            Ok(()) => {
                let msg = agnostic::chat("§7[AI] thinking…");
                if let Err(e) = compose.unicast(&msg, connection) {
                    warn!("ai chat send failed: {e}");
                }
            }
            Err(e) => {
                error!("failed to enqueue ai request: {e}");
                let msg = agnostic::chat("§cThe AI assistant is busy, try again.");
                if let Err(e) = compose.unicast(&msg, connection) {
                    warn!("ai chat send failed: {e}");
                }
            }
        }
    }
}

/// On player disconnect, drop their AI session so the sidecar frees its
/// conversation history and Hyperion frees the session id.
fn on_player_disconnect(
    trigger: Trigger<'_, OnRemove, packet_state::Play>,
    query: Query<'_, '_, &ConnectionId>,
    bridge: Res<'_, AiBridge>,
) {
    let Some(cmd_tx) = bridge.cmd_tx.as_ref() else {
        return;
    };
    let Ok(&connection) = query.get(trigger.target()) else {
        return; // no connection component; nothing to evict
    };
    if let Err(e) = cmd_tx.try_send(BridgeMsg::Evict(connection)) {
        warn!("failed to enqueue session eviction: {e}");
    }
}

pub struct AiPlugin;

impl Plugin for AiPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(build_bridge());
        app.add_systems(FixedUpdate, drain_ai_actions);
        app.add_observer(on_player_disconnect);
        AiCommand::register(app.world_mut());
    }
}
