# 1000-Player Single-Server Load Test, Cost Model, and Telemetry Plan

> Scope: hyperion (Rust + Bevy ECS Minecraft engine), this fork with the `events/bedwars`
> plugins (AI assistant, weather, NPC) and the world-persistence overlay.
> Status: planning. No code changes are proposed here — this is an execution plan.

## 0. Executive summary

The upstream engine is not the bottleneck at 1000 players. Published benchmark:
**1000 players = 0.40 ms tick** on a 14-core 2023 MacBook Pro Max, render distance 32
(4225 chunks), measured with bots via `just bots {N}`. The bulk of per-player work is in
the **horizontally scalable proxy layer** (regional multicast), and the benchmark used
plain bots — **no AI, no weather, no persistence**.

This fork adds four new load/cost centers that the upstream benchmark does **not** cover:

1. **LLM calls** — the dominant variable OpEx. `/ai` and NPC interactions go through the
   iacoder agent (vendored in `../iacoder`) via the out-of-process sidecar
   `crates/hyperion-ai-agent`. Each interaction can fan out up to `MAX_TURNS = 8`
   agent rounds (`crates/hyperion-ai-agent/src/main.rs:44`), each round a full model call.
2. **Proxy egress bandwidth** — trends toward N² entity updates when players cluster.
3. **Block-broadcast bursts** — `events/bedwars/src/plugin/weather.rs` pulses damage around
   **every player anchor** and broadcasts a `BlockUpdateS2c` per destroyed block; AI `fill`
   is capped at 16384 blocks (`events/bedwars/src/ai.rs:97`), each block its own broadcast.
4. **Persistence overlay** — `crates/hyperion/src/simulation/blocks/persist.rs`:
   `save_edits()` does a **synchronous, full-overlay serialize + fsync on the game thread**
   (`crates/hyperion/src/simulation/blocks/mod.rs:131-146`). A WAL is a noted TODO; today a
   large overlay can hitch the tick.

The strategic point of this document: AI spend is unavoidable if `/ai` is a product
feature, so **every dollar spent must also produce agent-eval data**. Telemetry (Section 3)
turns "cost" into a labeled dataset that drives iacoder's success-rate up over time.
Without telemetry, the money is pure cost.

---

## 1. Load-test plan (layered)

Each layer is tested in isolation first, then combined. Tools are the ones already in-repo:
`tools/rust-mc-bot` (driven by `just bots {ip} {count}` — `justfile:102`), the `/weather` and
`/ai` commands, and Tracy tracing (`just run` builds with `-t`). Run the server with
`just bedwars-full` (release) for any numbers that will be quoted.

### Common harness

- Hardware baseline: match the published benchmark class (14-core, 2023 MBP Max) for the
  engine layer so results are comparable; for proxy/egress use a representative production
  proxy node.
- File descriptors: `ulimit -Sn 32768` (the justfile already does this for `bots` and `proxy`).
- Instrument with Tracy (already wired). Capture: tick time, per-system time, egress bytes.
- Always record the git commit and the launch command in the result row.

### Layer A — Engine tick (regression vs. upstream baseline)

- **Goal:** confirm this fork still holds ~0.4 ms tick at 1000 idle/roaming bots, i.e. the
  new plugins don't regress the engine when idle.
- **Inject:** `just bots 127.0.0.1:25565 1000` against `just bedwars-full`.
- **Measure:** mean / p99 tick time (ms), per-system time for `weather_tick`, `npc_follow`,
  `npc_interactions`, `drain_ai_actions`, core ingress/egress systems.
- **Pass/fail:** mean tick ≤ 0.6 ms, p99 ≤ 2 ms at 1000 bots with all plugins loaded but
  idle (weather calm, no `/ai`). A regression beyond this means a plugin system is doing
  per-tick work it shouldn't.

### Layer B — Proxy egress (the real scaling axis)

- **Goal:** characterize egress as players cluster (the N² entity-update regime called out
  in the README).
- **Inject:** 1000 bots, then vary spatial distribution: (i) spread across the map,
  (ii) all bots within one render-distance cluster (worst case for multicast fan-out).
- **Measure:** egress GB/hour per proxy, CPU per proxy, p99 packet latency, dropped/coalesced
  updates. Sweep proxy count (1 → N) to find players-per-proxy ceiling.
- **Pass/fail:** a single proxy sustains its target player share with p99 egress latency
  < 50 ms (one tick) under the clustered worst case; egress/player stays bounded (does not
  grow super-linearly past the multicast cutoff). Record the GB/player-hour figure — it
  feeds the cost model (Section 2).

### Layer C — Sidecar LLM throughput (`/ai` and NPC)

- **Goal:** find the sidecar's concurrent-turn ceiling and end-to-end `/ai` latency, and
  validate backpressure behavior.
- **Inject:** drive `/ai` load two ways:
  - **Plumbing/throughput (free):** `HYPERION_AI_OFFLINE=1` makes the sidecar echo without
    calling the model (`crates/hyperion-ai-agent/src/main.rs:443`). Use this to load-test the
    stdin/stdout bridge, the `flume` channels, the per-session map, and `drain_ai_actions`
    at high request rates **with zero token spend**.
  - **Real model (metered):** a small set of scripted prompts spanning intents (chat,
    `place_block`, `fill`, multi-step builds) issued at a controlled requests/sec. Because
    bots don't send chat/commands by default, drive `/ai` either via a scripted client or a
    temporary debug command that calls `AiBridge::ask` (test-only; do not ship).
- **Measure:** per-turn rounds used (0–8), tokens in/out per interaction, wall-clock latency
  to first `say` and to completion, sidecar CPU/RAM, and how the **unbounded** `flume`
  command channel behaves under burst (`build_bridge` uses `flume::unbounded`,
  `events/bedwars/src/ai.rs:449-450` — watch for memory growth, since there is no explicit
  cap on queued turns).
- **Pass/fail:** define a target concurrent-interaction rate (e.g. 1–3% of 1000 players
  active per minute — see Section 2). At that rate: p95 time-to-first-token < 3 s, no
  unbounded queue growth, sidecar restarts cleanly (the supervisor in `ai.rs:323` should
  recover without dropping session routing).

### Layer D — Block-broadcast bursts (weather + AI fill)

- **Goal:** quantify the broadcast storm from weather damage and large AI fills, since each
  changed block is an individual `BlockUpdateS2c` broadcast to a region.
- **Inject:**
  - Weather: `/weather` storm/quake at intensity 12 with 1000 anchored bots. Damage scales
    as `hits = 1 + intensity/2` **per player anchor per pulse**
    (`weather.rs:166-193`), with pulses as frequent as every 5 ticks at max intensity
    (`weather.rs:136`). At 1000 anchors that is up to ~7000 block-broadcasts every 5 ticks.
  - AI fill: trigger `fill` at the 16384-block cap and measure the placement + per-block
    broadcast loop in `fill_region` (`ai.rs:566-617`).
- **Measure:** egress spike (bytes & packets) per pulse, tick-time impact of the
  set_block + broadcast loops, client-visible lag.
- **Pass/fail:** worst-case storm pulse keeps tick under the Layer A p99 bound and does not
  saturate a proxy. If it does, the action item is to **batch/coalesce** block updates
  (multi-block section packets) and/or cap damage broadcasts per pulse — note these as
  findings, do not implement here.

### Layer E — Persistence flush (tick hitch)

- **Goal:** measure the game-thread stall from `save_edits()` as the edit overlay grows.
- **Inject:** accumulate a large overlay (many AI `fill`s and persistent weather damage with
  `persist_damage = true`), then trigger the snapshot timer / shutdown save. The overlay is
  serialized whole each time (`persist.rs:119-134`) and written with `sync_all` + `rename` +
  dir fsync synchronously on the game thread (`mod.rs:143`).
- **Measure:** serialize time vs. overlay size (records), fsync/rename time, and the tick gap
  during the flush. Sweep overlay sizes (10K, 100K, 1M edits).
- **Pass/fail:** a single flush must not stall the tick beyond the Layer A p99 bound. The
  expected failure mode (already flagged in code comments) is large overlays hitching the
  tick — the remediation is the WAL + off-thread/incremental write noted as TODO in
  `persist.rs`. Report the overlay size at which the stall crosses the threshold.

### Combined soak

Run A+B+C+D+E together: 1000 bots, periodic storms, a steady `/ai` interaction rate, and
persistence enabled, for a multi-hour soak. Watch for memory growth (session map, flume
queues, edit overlay), proxy egress drift, and tick p99 over time.

---

## 2. Cost model

### 2.1 Parametric formula

Monthly cost is the sum of LLM, egress, and machine costs:

```
Cost_month = Cost_llm + Cost_egress + Cost_machine

Cost_llm    = P · A · D · R · (T_in · price_in + T_out · price_out)
Cost_egress = E_player · P · H · price_egress_per_GB
Cost_machine= n_game · price_game + n_proxy · price_proxy
```

Where:

| Symbol | Meaning | Notes / source |
|---|---|---|
| `P` | concurrent players | 1000 (target) |
| `A` | AI interactions per player per month | the big lever; modeled in 3 tiers below |
| `D` | model **rounds** amplification per interaction | up to `MAX_TURNS = 8` (`main.rs:44`); typical < 8 |
| `R` | rounds-to-calls factor | 1 model call per round; ≈ `D` |
| `T_in` / `T_out` | input / output tokens per call | includes growing per-session history (multi-turn) |
| `price_in` / `price_out` | model token prices ($/token) | provider/model dependent |
| `E_player` | egress GB per player per hour | **measure in Layer B** |
| `H` | played hours per month (aggregate) | `≈ P · hours_per_player_month` |
| `n_game`, `n_proxy` | server / proxy node counts | engine is 1 node; proxies scale out |

Key amplification to keep visible: **per interaction, token cost ≈ `D` model calls**, and
because the sidecar keeps **per-session history** (`main.rs:57-59`, `prior_history` in
`main.rs:347-365`), `T_in` grows turn over turn within a session. Long conversations are
super-linear in input tokens unless `history_budget` is set (currently `None`,
`main.rs:364`).

### 2.2 Worked estimate (1000 players) — ESTIMATE, see assumptions

These are order-of-magnitude figures to size the problem, **not** a quote. Replace the
bracketed assumptions with measured/contract numbers before budgeting.

**Assumptions (state them, then plug in real values):**
- `D` (rounds) ≈ 3 average (cap 8). Builds hit the cap; plain chat is 1–2.
- Tokens per call: `T_in ≈ 1500` (system prompt + short history + position context),
  `T_out ≈ 200`. System prompt is fixed (`main.rs:46-55`); history grows, so treat 1500 as
  a session average.
- So tokens per **interaction** ≈ `D · (T_in + T_out)` ≈ 3 · 1700 ≈ **5100 tokens**
  (~4500 in / ~600 out).
- Model price (illustrative, mid-tier "sonnet"-class): `price_in ≈ $3 / 1M`,
  `price_out ≈ $15 / 1M`. → per interaction ≈ `4500·$3/1M + 600·$15/1M` ≈
  `$0.0135 + $0.009` ≈ **$0.022 / interaction**.

**Three AI-interaction tiers** (`A` = interactions/player/month; assume ~30 active days):

| Tier | Per player / day | `A` (per player / month) | Total interactions/month (P=1000) | Tokens/month | **Est. LLM $/month** |
|---|---|---|---|---|---|
| Conservative | ~0.5 | 15 | 15,000 | ~76M | **~$330** |
| Medium | 2 | 60 | 60,000 | ~306M | **~$1,320** |
| Aggressive | 10 | 300 | 300,000 | ~1.53B | **~$6,600** |

Round to ranges: **conservative ≈ $0.2–0.5K, medium ≈ $1–2K, aggressive ≈ $5–10K per
month**, dominated by the `D` multiplier and history growth. Prompt caching of the fixed
system prompt and a `history_budget` cap would each shave input tokens materially.

**Egress (illustrative):** with `E_player` measured in Layer B (say 0.5 GB/player-hour as a
placeholder) and ~30 player-hours/month each: `1000 · 30 · 0.5 GB = 15,000 GB`. At a
$0.01–0.05/GB cloud rate → **$150–750/month**. Replace with the measured `E_player`.

**Machine:** the engine is 1 node (benchmark shows headroom at 1000); proxies scale out per
Layer B's players-per-proxy ceiling. A handful of proxy nodes + 1 game node is plausibly
**low hundreds of $/month** on commodity cloud.

### 2.3 Conclusion: tokens dominate

Even at the **conservative** tier, LLM spend is in the same ballpark as servers; at the
**aggressive** tier it is roughly **10–50×** the machine cost and outpaces egress too. AI
interaction rate (`A`) and round amplification (`D`) — not player count or CPU — are the
budget drivers. Controlling cost = controlling `A` (product gating) and `D`/`T_in`
(turn caps, prompt caching, history budget), **not** buying smaller servers.

---

## 3. Telemetry design (the point of this plan)

**Thesis: if we're paying for LLM calls, every call must also produce labeled agent-eval
data.** Otherwise the spend is a sunk cost. The telemetry below converts each `/ai`
interaction into a row that feeds iacoder's eval/iteration loop (success rate, self-correction
rate, regression tracking).

### 3.1 What to record per interaction

One structured record per AI interaction (one `/ai` or one NPC interact), keyed by
`session` + a generated `interaction_id`:

| Field | Why it matters for eval |
|---|---|
| `interaction_id`, `session`, `ts`, `source` (`/ai` vs `npc`) | join key, provenance |
| `prompt_raw` and `prompt_with_context` | the position-injected prompt actually sent (`main.rs:335-345`) |
| `intent_class` | label: chat / place / fill / multi-step-build / clear / other (classify from prompt + tool calls) |
| `tool_call_sequence` | ordered list of `say` / `place_block` / `fill` with args — the agent's actual plan |
| `world_diff` | before/after block states for affected positions (see 3.3) |
| `intent_satisfied` | did the action achieve what the player asked (see 3.4) |
| `rounds_used` / `MAX_TURNS` | did it hit the 8-round cap (truncated = likely failure) |
| `tokens_in`, `tokens_out`, `cost_est` | per-call and per-interaction; ties telemetry to Section 2 |
| `latency_ms` | time-to-first-`say` and time-to-complete |
| `error` | agent run error (`main.rs:378-387`) / unknown block / fill-too-large / out-of-bounds |
| `caps_hit` | fill-volume cap (`ai.rs:581`), turn cap, broadcast-range skip |

### 3.2 Where it lands (sinks)

- **Primary sink = sidecar stderr, structured.** The sidecar already routes all diagnostics
  to stderr and keeps **stdout reserved for the action stream** (`main.rs:6`, `main.rs:120-131`,
  and the `tracing_subscriber` stderr writer at `main.rs:520-527`). So emit telemetry as
  structured `tracing` events (JSON via `tracing_subscriber`'s JSON formatter) on stderr —
  **no protocol change**, no risk of polluting the action channel. The sidecar is the right
  place because it alone sees the full tool-call sequence, rounds, tokens, and timing.
- **Hyperion side complements it:** `ai.rs` already logs caps and errors
  (`fill_region` reports placed/skipped; `resolve_block` reports unknown blocks). Emit those
  as structured events too, tagged with `interaction_id`, so the world-side outcome
  (what actually got placed) joins to the sidecar-side plan.
- **Durable sink:** ship sidecar stderr JSON to a log pipeline / object store (or a separate
  file sink) keyed by `interaction_id`. This is the agent-eval dataset. Keep it out of the
  hot path — it's already off the game thread (sidecar process).

### 3.3 World before/after diff

The agent currently builds **open-loop**: there is no `get_block` tool, so the agent can't
read the world (noted in the brief). The diff therefore has to be captured **Hyperion-side**,
where `set_block` returns the old state:

- `Blocks::set_block` / `set_block_inner` already return the **old** `BlockState`
  (`mod.rs:450-507`); `place_block`/`fill_region` in `ai.rs` currently discard it. The
  telemetry hook records `(pos, old_state, new_state)` per applied action, tagged with the
  `interaction_id`, to produce the world diff.
- This diff is also the data that would later justify adding a read-back `get_block` tool:
  it shows how often the open-loop agent places into already-occupied space, builds inside
  the player, or fills mostly-air, which are exactly the closed-loop failures a read tool fixes.

### 3.4 Did it achieve the player's intent?

Two complementary signals, neither requiring a human in the loop for the bulk of rows:

- **Heuristic / automatic:** derive a label from the record — did it error, hit the turn cap,
  produce zero block changes for a build intent, place mostly into non-air, or skip most of a
  fill (`skipped` count)? These give a cheap, high-volume "likely failed" signal.
- **LLM-judge / sampled human:** periodically replay `prompt + tool_call_sequence + world_diff`
  through a judge model (or human spot-check) to label `intent_satisfied` on a sample. This is
  the gold label that calibrates the heuristics.

### 3.5 How it feeds back into iacoder

The dataset directly produces the metrics iacoder iterates on:

- **Success rate** = fraction of interactions with `intent_satisfied = true`, sliced by
  `intent_class`. This is the headline eval metric; track per model/prompt version.
- **Self-correction rate** = of interactions that errored or mis-stepped mid-sequence,
  how often the agent recovered within its `rounds_used` (visible in `tool_call_sequence`).
  Measures agent robustness, not just one-shot accuracy.
- **Cap-truncation rate** = how often `rounds_used == MAX_TURNS`; high values argue for a
  higher cap, better planning, or the `fill`-first guidance already in the system prompt.
- **Cost-per-success** = `cost_est / success` — the single number that unifies Section 2 and
  Section 3. Optimizing prompts/tools to lower this is the whole feedback loop.
- **Regression set:** failed interactions become eval fixtures (`prompt + expected outcome`)
  in iacoder's harness, so future agent/prompt/model changes are checked against real
  in-game failures. Candidate new tools (notably `get_block`) are justified by the diff data
  in 3.3.

**Bottom line:** the telemetry turns spend into a self-improving loop — every paid
interaction lowers future cost-per-success. No telemetry, and the money is just cost.

---

## Appendix — concrete pointers (for whoever implements this)

- Round cap / token amplifier: `crates/hyperion-ai-agent/src/main.rs:44` (`MAX_TURNS = 8`).
- History grows per session (unbounded budget): `main.rs:57-59`, `main.rs:347-365`.
- Offline mode for free plumbing load tests: `main.rs:443-464` (`HYPERION_AI_OFFLINE`).
- stderr is the safe telemetry channel; stdout is sacred: `main.rs:6`, `main.rs:120-131`,
  `main.rs:520-527`.
- Unbounded request queue (watch under burst): `events/bedwars/src/ai.rs:449-450`.
- Fill cap + per-block broadcast: `ai.rs:97`, `ai.rs:566-617`.
- `set_block` returns old state (free world-diff source): `crates/hyperion/src/simulation/blocks/mod.rs:450-507`.
- Weather damage scales per-anchor per-pulse: `events/bedwars/src/plugin/weather.rs:136`, `166-193`.
- Synchronous on-thread persistence flush: `crates/hyperion/src/simulation/blocks/mod.rs:131-146`;
  full serialize each time: `persist.rs:119-134`; WAL TODO: `persist.rs:13-15`.
- Load-test driver: `just bots {ip} {count}` (`justfile:102`), release server `just bedwars-full`.
