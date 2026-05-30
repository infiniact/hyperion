# hyperion-ai-agent

Out-of-process **AI agent sidecar** for the [Hyperion](../hyperion) Minecraft
server. A player runs `/ai <prompt>` in-game; Hyperion feeds the prompt to this
**long-running** process, which drives an [`iacoder`](../../../iacoder) agent loop.
The agent's *game tools* act on the world by writing JSON actions to **stdout**,
which Hyperion reads and applies.

## Why a separate process?

Hyperion pins an **old nightly** (`nightly-2025-02-22`) for its own unstable
features. iacoder's dependency tree (e.g. `rig-core`) needs a newer compiler
(now-stable `let_chains`). The two **cannot share a toolchain**. Running the
agent as a sidecar — built here with **stable** — sidesteps the conflict and
keeps the AI a cleanly separable, independently deployable component.

```text
  Minecraft client ──/ai <prompt>──▶ Hyperion (bedwars, old nightly)
                                         │  writer task -> child stdin
                                         ▼
                              hyperion-ai-agent (this crate, stable, resident)
                                         │  iacoder Runtime.run() w/ session history
                                         │  agent calls the `say` tool
                                         ▼
                              child stdout -> reader task -> Hyperion
                              -> GameAction -> chat packet -> the player
```

The sidecar is started **once** at server boot and stays resident: the LLM
provider/HTTP client is built a single time (no per-request cold start), and
each player gets **multi-turn conversation context**.

## Protocol (newline-delimited JSON)

stdin and stdout are each a stream of one JSON object per line. stdout carries
**only** actions; all logging goes to stderr.

- **stdin** (Hyperion → sidecar), one per player prompt:
  ```json
  {"session": 7, "prompt": "build me a tower"}
  ```
- **stdout** (sidecar → Hyperion), one per action:
  ```json
  {"session": 7, "type": "say", "text": "On it!"}
  ```

`session` is an opaque id assigned by Hyperion, **stable per player
connection**. The sidecar keys conversation history on it (multi-turn);
Hyperion keeps the `session <-> connection` mapping to route replies back.

Action kinds are designed to grow (`place_block`, `give_item`, …) without
breaking the wire format — add a variant on both sides (`ActionKind` here and
`AgentAction` in `hyperion/events/bedwars/src/ai.rs`).

## Build

This crate lives in the hyperion repo at `crates/hyperion-ai-agent` but is
**excluded from the workspace** (see the root `Cargo.toml` `exclude`): it builds
with its own stable toolchain, whereas the workspace pins an old nightly. So
build it standalone, from inside the crate:

```bash
cd crates/hyperion-ai-agent
cargo build            # produces hyperion-ai-agent (stable toolchain)
```

It depends on `iacoder` by path at `../../../iacoder` (a sibling repo of
hyperion), so that directory must be present.

A global `~/.cargo/config.toml` redirects output to `~/.cargo/target`, so the
binary lands at `~/.cargo/target/debug/hyperion-ai-agent`.

## Run / test

It reads requests from stdin, so you can drive it by hand:

```bash
# Offline smoke test — no API key, echoes each prompt as a chat action.
printf '{"session":7,"prompt":"build a tower"}\n{"session":7,"prompt":"now bigger"}\n' \
  | HYPERION_AI_OFFLINE=1 hyperion-ai-agent
# -> {"session":7,"type":"say","text":"(offline) you said: build a tower"}
# -> {"session":7,"type":"say","text":"(offline) you said: now bigger"}

# Real run
export ANTHROPIC_API_KEY=sk-ant-...
export HYPERION_AI_MODEL=claude-sonnet-4-5   # optional, this is the default
echo '{"session":1,"prompt":"say hello three different ways"}' | hyperion-ai-agent
```

In normal operation Hyperion launches and manages this process for you.

## Configuration

| Env var               | Required | Default               | Meaning                          |
|-----------------------|----------|-----------------------|----------------------------------|
| `ANTHROPIC_API_KEY`   | yes¹     | —                     | Anthropic API key                |
| `HYPERION_AI_MODEL`   | no       | `claude-sonnet-4-5`   | Model id passed to the provider  |
| `HYPERION_AI_OFFLINE` | no       | unset                 | If set, skip the LLM and echo    |
| `RUST_LOG`            | no       | `warn`                | stderr log filter                |

¹ unless `HYPERION_AI_OFFLINE` is set.

## How Hyperion finds & launches this binary

`events/bedwars/src/ai.rs` reads `BEDWARS_AI_AGENT_BIN` for the path
(default `~/.cargo/target/debug/hyperion-ai-agent`), spawns it once at boot,
and inherits the server's environment — so set `ANTHROPIC_API_KEY` before
launching bedwars.

When bedwars runs **inside Docker**, this binary must exist *in the container*
and `ANTHROPIC_API_KEY` must be set there — mount the binary and set both env
vars on the `bedwars` service in `docker-compose.override.yml`.

## Resilience

- **Session eviction**: when a player disconnects, Hyperion sends an
  `{"type":"evict","session":N}` line; the sidecar drops that session's
  conversation history and Hyperion frees the session id.
- **Auto-restart**: a supervisor keeps the sidecar alive. If it crashes,
  Hyperion respawns it (1s backoff, gives up after 5 consecutive failures).
  The `session <-> connection` map persists across restarts so reply routing
  keeps working; the restarted process starts with empty per-session history.

## Notes / TODO

- After an auto-restart the sidecar's in-memory conversation history is gone
  (a fresh process). The next turn per player simply seeds a new transcript.
- `fill` is volume-capped (16,384 blocks) on the Hyperion side to bound both
  abuse and the per-block update broadcast.
