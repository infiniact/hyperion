# AI Sidecar 池化 + 外置 Session 状态架构方案

## 1. 现状与问题定位

### 当前数据流（单进程）
```
玩家 /ai  ──► AiCommand.execute ──► cmd_tx (flume) ──► writer ──► child.stdin (JSON 行)
                                                                       │ iacoder Runtime.run()
                                                                       │ Sessions: Arc<Mutex<HashMap<u64, Arc<Vec<Message>>>>>  (进程内)
玩家聊天 ◄── drain_ai_actions (Bevy tick) ◄── action_rx ◄── reader ◄── child.stdout (JSON 行)
```

### 三个根本问题
| 问题 | 现状代码位置 | 后果 |
|---|---|---|
| **SPOF** | `ai.rs` `supervise()` 单 child + `main.rs` `Sessions` 进程内 | sidecar 崩溃/滚动重启 → 全部玩家多轮历史丢失（`supervise` 注释明确承认 "new process starts with empty history"） |
| **吞吐瓶颈** | `ai.rs` 单条 stdin 管道 + `main.rs` 每 turn `tokio::spawn` | 无法横向扩；1000 人单服并发 LLM 请求受单进程出口 + provider 限速制约，无背压 |
| **协议手抄** | `ai.rs` `OutMsg`/`AgentAction` vs `main.rs` `InMsg`/`ActionKind`/`OutLine` | 两份枚举手工保持同步，新增 action 易漂移 |

### 关键技术事实（决定方案可行性）
- **`iacoder_core::Message` 与 `Content` 已实现 `Serialize`/`Deserialize`**（`provider/mod.rs:59`，`content.rs:17`）。整段 transcript 可直接 JSON 序列化进外置 KV——这是外置 session 状态的地基，无需自造 wire 格式。
- **session 已是可序列化值** `Arc<Vec<Message>>`，worker 处理一轮只需：取历史 → `Runtime::run` → 写回历史。本质上已是无状态计算，状态只是恰好放在了进程内 HashMap。
- **provider 启动构建一次**（`resolve_backend`），是 worker 冷启动成本，池化后由 worker 常驻摊销。
- **工具链边界必须保留**：hyperion 钉 `nightly-2025-02-22`，iacoder 要 stable（`Cargo.toml` workspace `exclude`，Dockerfile `sidecar-build` 独立 stable stage）。进程/二进制边界不可拆。

## 2. 目标架构

```
                          ┌─────────────────────── Hyperion (bedwars, nightly) ───────────────────────┐
   玩家 /ai ──► AiCommand ─┤ AiBridge (dispatcher)                                                      │
                          │   • SessionMap: ConnectionId ↔ SessionId (仍进程内, 仅做路由)               │
                          │   • TurnQueue (有界 flume, 背压)                                            │
                          │   • WorkerPool 客户端: N 条连接 + 负载均衡 + 健康检查                        │
                          └───────────────┬───────────────────────────────────────────────────────────┘
                                          │  hyperion-ai-wire (共享 crate: Turn/Evict/Action/Ack)
                          ┌───────────────┴───────────────┐  Unix socket (本地) / TCP (跨机)
                          ▼                ▼                ▼
                    ┌──────────┐     ┌──────────┐     ┌──────────┐   无状态 worker (stable 工具链)
                    │ worker-0 │     │ worker-1 │ ... │ worker-N │   • 不持有 session 历史
                    │ iacoder  │     │ iacoder  │     │ iacoder  │   • 每 turn: GET 历史 → run → SET 历史
                    │ Runtime  │     │ Runtime  │     │ Runtime  │   • provider 常驻
                    └────┬─────┘     └────┬─────┘     └────┬─────┘
                         └────────────────┴────────────────┘
                                          ▼
                              ┌───────────────────────┐
                              │  Session Store (Redis) │  key: ai:sess:{server}:{session}
                              │  • transcript (JSON)   │  • per-session 锁 (串行化)
                              │  • TTL 驱逐             │  • prior_tool_meta
                              └───────────────────────┘
```

核心转变：**session 历史从 worker 进程内 HashMap 搬到 Redis；worker 变成无状态、可水平扩、可滚动重启的纯计算单元。** dispatcher 把一轮请求路由到任意空闲 worker，worker 从 Redis 拉对应 session 历史续接。

## 3. 详细设计

### 3.1 共享协议 crate `hyperion-ai-wire`

新建 `crates/hyperion-ai-wire`，**两侧唯一依赖**，消掉手抄。它必须能被 hyperion（nightly）和 hyperion-ai-agent（stable）**双工具链编译**，故保持零重依赖（仅 serde/serde_json），**不依赖 iacoder**（否则把 stable-only 依赖拖进 nightly 侧）。

```rust
// 单一来源:替代 ai.rs::OutMsg / main.rs::InMsg
#[serde(tag="type", rename_all="snake_case")]
pub enum Request {
    Turn { session: SessionId, request_id: Uuid, prompt: String, pos: Option<[f32;3]> },
    Evict { session: SessionId },
    Health,                      // 健康检查 ping
}
// 替代 ai.rs::AgentAction / main.rs::ActionKind+OutLine
#[serde(tag="type", rename_all="snake_case")]
pub enum Response {
    Action { session: SessionId, request_id: Uuid, kind: ActionKind },
    Done   { session: SessionId, request_id: Uuid },        // 该 turn 完成 → dispatcher 释放 slot
    Error  { session: SessionId, request_id: Uuid, msg: String },
    Pong,
}
pub enum ActionKind { Say{..}, PlaceBlock{..}, Fill{..} }  // 此处定义,两侧 import
pub type SessionId = u64;
```
注意：`SessionId` 改为全局唯一（见 3.3 key 设计），`request_id` 新增——多 worker 下需要它把异步回复关联回正确的 turn 并支持超时。

- hyperion 侧：`ai.rs` 删除 `OutMsg`/`InAction`/`AgentAction`/`ActionKind`，改 `use hyperion_ai_wire::*`。
- worker 侧：`main.rs` 删除 `InMsg`/`ActionKind`/`OutLine`，改 import；`emit()` 发 `Response::Action`，turn 末尾发 `Response::Done`。

### 3.2 传输层升级：取舍与推荐

| 方案 | 多 worker 扇出 | 跨机 | 实现成本 | 背压 | 推荐 |
|---|---|---|---|---|---|
| stdin/stdout（现状） | ✗ 单管道 | ✗ | 极低 | flume 队列 | **保留为本地 dev 默认** |
| **Unix socket** | ✓ 多连接 | ✗ 仅本机 | 低（tokio `UnixListener`，行 JSON 不变） | 自然 per-conn | **本地/单机池化推荐** |
| **TCP（行 JSON）** | ✓ | ✓ | 低（同上换 `TcpListener`） | per-conn | **跨机扩展推荐** |
| gRPC | ✓ | ✓ | 高（tonic + proto，nightly 侧 tonic 编译风险） | 流控内建 | 否（过重，且威胁工具链边界） |
| 消息队列（NATS/Redis Stream） | ✓ 解耦 | ✓ | 中 | broker 托管 | 否（为 1000 人单服引入额外 broker 运维，收益不抵） |

**推荐：同一行分隔 JSON 帧协议，传输后端做成可插拔 enum，本地 stdio / Unix socket，跨机 TCP。** 理由：
1. 帧格式不变（还是换行 JSON），`hyperion-ai-wire` 的 `Request`/`Response` 直接复用，迁移面最小。
2. 避免 gRPC/tonic 在 nightly-2025-02-22 上的编译/proc-macro 风险，守住工具链边界这条硬约束。
3. 1000 人单服 I/O bound、瓶颈在 provider 限速而非本地传输，Unix/TCP 行 JSON 足够，不需要 broker。

dispatcher 与 worker 间：worker 启动监听 socket（或 dispatcher 主动 spawn 一组 worker 进程并各连一条 Unix socket）。**推荐 dispatcher 拉起 worker 进程**（沿用现有 `spawn_child` 心智模型 + supervisor），仅把"一条管道"换成"一组 socket 连接"。

### 3.3 Session 状态模型

- **存哪**：Redis（单实例即可满足 1000 人；transcript 几 KB~几十 KB/session）。抽象成 `trait SessionStore`，默认 `RedisSessionStore`，dev 提供 `InMemorySessionStore`（进程内，等价现状，无外部依赖）。
- **Key 设计**：`ai:sess:{server_id}:{session_id}`，value = `serde_json::to_string(&Vec<Message>)`（利用 iacoder `Message: Serialize`）。附带 `ai:meta:{...}` 存 `prior_tool_meta`。`session_id` 由 dispatcher 全局分配（现 `SessionMap.next` 改为带 server 前缀的全局唯一，避免多服/重启碰撞）。
- **并发同 session 串行化（防两轮并发写乱历史）**：这是池化引入的**头号正确性风险**。现状单进程靠"同 session 历史在同一 HashMap 槽 + 顺序 spawn"侥幸不乱，但多 worker 下两轮可能落到不同 worker。方案：
  - **dispatcher 层 per-session 排队**：同一 `session_id` 的 turn 串行下发，前一轮 `Done`/超时前不发下一轮（玩家几乎不会在 AI 回复前再发 `/ai`，代价极小）。这是首选——简单、零额外往返。
  - **Store 层乐观锁兜底**：`SET` 时带版本号（Redis `WATCH`/Lua CAS），写回冲突则丢弃旧轮。防御 dispatcher 重启后的竞态。
- **过期与驱逐**：Redis key 设 TTL（如 30 min 无活动）替代现 `Evict` 的强依赖；`on_player_disconnect` 仍发 `Evict` 做即时清理（`DEL`），但即使漏发也由 TTL 兜底——比现状更健壮。
- **重启恢复**：worker 滚动重启 → 历史在 Redis，**新 worker 接任意 session 续接，上下文零丢失**（直接解掉现状 SPOF）。dispatcher 重启 → `SessionMap`（路由）丢失，但下次玩家 `/ai` 重新分配 session_id 即可；若要连 dispatcher 重启都不丢路由，可把 `ConnectionId↔SessionId` 也入 Redis（可选，优先级低，因 ConnectionId 本身随重连失效）。

### 3.4 背压与限流

- **队列满**：`TurnQueue` 改有界 `flume::bounded(N)`（现为 `unbounded`）。`try_send` 失败已有友好提示（"AI assistant is busy"）——逻辑复用，只改容量。
- **provider 限速**：worker 侧已可借 iacoder 的 `RetryPolicy`/`FailoverReason`（`provider/failover.rs`）。dispatcher 维护**全局并发 LLM 请求上限**（信号量），因瓶颈是 provider RPM/TPM 而非本地 CPU——按"并发 LLM 请求数"计正好对应一个全局 semaphore。
- **超时**：每 turn 带 `request_id` + deadline；dispatcher 超时未收到 `Done` → 给玩家报错、释放 slot、标记 worker 可疑。利用 `RunRequest.cancel: CancellationToken` 让 worker 端可取消。
- **健康检查 + 负载均衡**：dispatcher 周期发 `Request::Health` 期待 `Pong`；失联 worker 移出轮转并由 supervisor 重启。负载均衡用**最少在途请求（least-outstanding）**而非轮询，匹配 turn 时长方差大的特性。

## 4. 分阶段迁移路径（本地 dev 体验不退化）

每阶段独立可上线、可回滚。`SessionStore` 与传输后端均做成 trait/enum，**dev 默认仍是进程内 + stdio**。

### 阶段 0 — 共享协议 crate（纯重构，零行为变化）
- 新增 `crates/hyperion-ai-wire`（零重依赖，双工具链可编译）。
- 改动点：`events/bedwars/src/ai.rs`（删手抄枚举，import wire）；`crates/hyperion-ai-agent/src/main.rs`（同上）；`crates/hyperion-ai-agent/Cargo.toml` + `events/bedwars/Cargo.toml` 加依赖；workspace `Cargo.toml` members 加新 crate；Dockerfile `sidecar-build` stage 需 COPY 进 wire crate 源码。
- 风险：Dockerfile 路径布局——wire crate 要同时出现在 nightly 构建上下文和 stable `sidecar-build` 上下文。低风险但需同步改 `COPY`。

### 阶段 1 — 外置 session 状态（单 worker 不变，先解 SPOF）
- worker 侧：`main.rs` 的 `Sessions` 类型替换为 `Arc<dyn SessionStore>`；`handle_turn` 的"snapshot history / insert final_history"改成 `store.get`/`store.put`（序列化 `Vec<Message>`）。`Evict` 改 `store.del`。
- 新增 `crates/hyperion-ai-agent/src/store.rs`：`SessionStore` trait + `InMemorySessionStore`（默认，等价现状）+ `RedisSessionStore`（env `HYPERION_AI_REDIS_URL` 存在时启用）。
- 风险：transcript 序列化体积；Redis 不可用时须 fallback 到 in-memory（dev）。**此阶段单 worker，串行化问题尚不暴露，但先把 CAS 版本号写进 store 接口。**

### 阶段 2 — 传输层抽象 + Unix socket（仍单 worker，验证扇出帧）
- `ai.rs`：把 `run_session`/`write_msg`/`reader_loop` 抽象到 `Transport` enum（`Stdio` | `UnixSocket`），dev 默认 `Stdio`。引入 `request_id`、`Done`/超时。
- worker `main.rs`：`serve` 增加监听 socket 的入口（env 选择 stdio vs socket）。
- 风险：连接生命周期/重连；保持 stdio 路径行为完全不变以护住 dev。

### 阶段 3 — Worker 池 + dispatcher 负载均衡 + 背压（完成横向扩）
- `ai.rs` 的 `AiBridge` 升级为 dispatcher：管理 `Vec<WorkerHandle>`、least-outstanding 调度、per-session 串行队列、全局 LLM 并发 semaphore、健康检查、有界队列。`supervise` 从"单 child"泛化为"worker 池 supervisor"（池大小 env `HYPERION_AI_WORKERS`，dev=1）。
- 部署：docker-compose 把 worker 拆成可 `--scale` 的服务（TCP 传输），或单容器内多进程（Unix socket）。
- 风险：并发同 session 写乱（靠 3.3 串行化 + CAS 防）；调度公平性；worker 崩溃中途 turn 的清理（超时 + request_id 兜底）。

### 阶段 4 — 跨机扩展（可选，按规模触发）
- 传输切 TCP；Redis 已是共享态，worker 可跨机。dispatcher 仍单点（它只做路由/调度，无状态历史，崩溃不丢上下文）。
- 风险：网络分区下的健康检查抖动；Redis 成为新的（但成熟的）共享依赖，需主从/持久化策略。

## 5. 关键决策取舍表

| 决策点 | 选择 | 备选 | 取舍理由 |
|---|---|---|---|
| 状态后端 | Redis（trait 抽象） | Postgres / 自建 | KV + TTL + 原子 CAS 天然契合 transcript；运维轻；dev 可 in-memory 回退 |
| 序列化格式 | iacoder `Message` 的 serde JSON | 自定义 wire 历史格式 | `Message`/`Content` 已 `Serialize`，零额外维护；避免协议二次手抄 |
| 传输 | 行 JSON over stdio/Unix/TCP（可插拔） | gRPC / NATS | 复用现有帧；守住 nightly 工具链边界；1000 人单服无需 broker |
| 协议位置 | 独立零依赖 `hyperion-ai-wire` crate | 继续两侧手抄 / 放进 iacoder | 双工具链可编译；单一来源；不把 stable-only 依赖拖进 nightly 侧 |
| 串行化 | dispatcher per-session 队列 + store CAS 兜底 | 仅靠 store 锁 / 不处理 | 避免每轮额外往返；玩家行为下队列几乎不阻塞；CAS 防 dispatcher 重启竞态 |
| 限流口径 | 全局 LLM 并发 semaphore | per-worker 限流 | 瓶颈是 provider RPM/TPM（I/O bound），全局口径才对得上真实约束 |
| Worker 拉起方 | dispatcher spawn + supervise | 独立编排（k8s/compose） | 沿用现有 supervisor 心智；dev 单进程体验不退化；阶段 4 再交给编排 |
| dispatcher 单点 | 接受（无状态路由） | dispatcher 也做 HA | 历史已外置，dispatcher 崩溃不丢上下文，重分配 session 即可；HA 收益不抵复杂度 |

## 6. 风险汇总
1. **并发同 session 写乱历史**（池化头号正确性风险）→ dispatcher 串行 + CAS。
2. **Dockerfile 双上下文** wire crate 同步 COPY → 阶段 0 即验证 CI 两个构建 stage。
3. **Redis 可用性** → in-memory fallback（dev） + TTL 兜底 + 阶段 4 加持久化。
4. **transcript 体积膨胀** → 复用 iacoder `HistoryBudget::MaxMessages` 截断（`RunRequest.history_budget`，现传 `None`）。
5. **工具链边界破坏** → wire crate 零重依赖、不引 iacoder、不引 tonic；阶段 0 即在两条工具链下编译验证。

## 7. 实施关键文件
- `events/bedwars/src/ai.rs`（dispatcher / 传输 / 调度 / 背压）
- `crates/hyperion-ai-agent/src/main.rs`（worker 无状态化 / SessionStore / socket serve）
- `Cargo.toml`（新增 hyperion-ai-wire member；sidecar 仍 exclude）
- `Dockerfile`（sidecar-build stage 双工具链 + wire crate COPY + worker 扇出）
- `../iacoder/crates/iacoder-core/src/provider/mod.rs`（Message 的 Serialize 契约——外置状态的地基）
