# Minecraft AI 插件市场调研、竞品拆解与功能优先级

> 调研日期：2026-05。数据来源：各产品商品页（Modrinth / Polymart / SpigotMC）、
> 公开评论、GitHub、网络搜索摘要。**局限**：最深的抱怨池在各产品的
> **Discord**（如 CreatureChat 的 GitHub issues 为空，支持全导流 Discord）和
> **BuiltByBit 登录墙**后，未能直接抓取；以下为足够清晰但非全样本的信号。

---

## 1. 结论速览

- **需求已验证**：CreatureChat（AI 对话+行为生物）Modrinth ~155K + CurseForge ~168K ≈ **32 万下载**。"让 MC 世界活起来的 AI"是真实、规模化的需求。
- **红海**：对话 NPC（依赖 Citizens、纯聊天），几十个产品、大量免费、同质化。**不要进。**
- **全行业核心痛点（铁律）**：① 建造质量差/通用/重复（开发者自己都承认）；② "装了不工作"——几乎都来自 LLM 配置门槛。
- **闭环不再是空白**：DeepSeek 版 **AIBuilder** 已做"计划→校验/修复→`/feedback`→`/next`续建+AI bot NPC"。但它是**单次规划**，不是真·边建边看的循环——真正的空白在这里 + 质量 + "一键撤销"。

---

## 2. 竞品格局

| 类别 | 代表 | 形态 | 收费 | 成熟度 |
|---|---|---|---|---|
| 对话 NPC（最拥挤） | LLMCraft、AI-NPC、GPT Talk、CraftGPT、iNPC、Friend-GPT、Chat-With-NPC | 多依赖 Citizens，纯聊天 | 大量免费 + 少数 premium | 红海 |
| **AI 建造**（最相关） | **AIBuilder(DeepSeek)**、BuilderAI(BBB)、AIBuilds(Polymart)、BuildAI(开源)、AI Builder | NL/图片→结构 | 免费~付费/按 build | 不成熟、质量差 |
| AI 客服/助手 | ServerAssistantAI、NSR-AI、ChayulaAI | RAG + 多 provider | freemium，最商业化 | 较成熟、运营工具 |
| **会行为的 NPC**（最接近我们） | **CreatureChat** | 聊天+Follow/Flee/Attack/Protect+记忆+好感度 | BYO-key/本地/卖token | 品类标杆 ~32 万下载 |

### 通用事实
- **BYO-key 是默认**：几乎都让服主自带 key，反复强调"每次几分钱"。验证了"零边际成本"打法是市场惯例。
- **分层范式**（抄 CreatureChat）：① 免费本地 Ollama（难）② BYO-key（中）③ 官方 Token Shop 转卖 token（最简单，给非技术服主）。
- **授权**：Polymart/BuiltByBit 自带授权系统 gate `.jar`；标准 ToS = 单服、禁转售、不退款。
- **盗版猖獗**：BlackSpigot/spigotunlocked 等泄露站常驻搜索结果。

---

## 3. 痛点清单（附证据）

| # | 抱怨 | 证据 |
|---|---|---|
| 1 | 建造质量差/通用/重复 | BuildAI 自认"AI 还造不出好东西"；AIBuilds 自认"远低于展示图、要反复试错"；BuilderAI 评测"房子偏通用、用久重复" |
| 2 | "装了不工作" = 配置门槛 | 失败几乎都来自 LLM 配置：API key、OpenAI 要绑卡、本地模型要强 GPU、模型选错 |
| 3 | 付费+第三方站+还要自带 key | BuilderAI 真实差评："要钱、第三方站、还得自带 key → 不推荐" |
| 4 | 模型强弱直接决定好坏，服主想用强模型 | AI Builder 唯一评论："让它能用 ChatGPT，上 4o 能比现在[Gemini]好 10 倍" |
| 5 | 实体/摆放卡服 | villager AI 本就吃性能、堆实体叠加；BuilderAI 把"不卡服"当卖点 = 痛点 |
| 6 | 给 AI 世界写权限的信任/防破坏 | Mindcraft 安全警告；AI 乱建/griefing 风险 |

---

## 4. 竞品深拆：AIBuilder（DeepSeek 版，最接近"闭环 agent"）

**不开源**（未找到公开仓库），但官方文档已说清机制。

**工作流**：选区域(stick) → `/aibuild <prompt>` 把 prompt + 区域尺寸(+可选已有方块) 发给 DeepSeek → AI 返回 JSON 计划(方块+相对坐标) → 插件**校验/修复**计划 → 逐块摆放 → `/next` 续建、`/feedback <text>` 加进历史。还有 `/aibot` NPC 用你的选区建造、survival 扣材料、streaming 输出。

**它强在哪（值得抄的工程细节）**：streaming、`/next` 续建、survival 扣材料、AI bot NPC、JSON 校验修复。

**它的结构性弱点（= 你的机会）**：
1. **单次规划，不是真闭环**。一个 prompt → 一整张 JSON → 摆放。所谓"feedback"是**人在回路里重新提示**，不是 agent 自己读世界、发现建歪了再改。
2. **只做语法校验**。"validation/repair"是修畸形 JSON / 非法方块名 / 越界，**不是语义/美学纠错**（不会"看一眼觉得屋顶没封住→补上"）。
3. **裸方块列表 → 必然通用/方块感**。没有施工原语/模板，逃不出痛点 1。
4. **锁死单一模型（DeepSeek）**。正撞痛点 4——服主想上 GPT-4o/Claude 而不能。
5. **无撤销**。文档无 undo/回滚；建坏了只能手动清。
6. **强制手动选区**。建造被框在一个手选盒子里，摩擦高。

---

## 5. 功能优先级（痛点 → 功能，尽量复用既有资产）

### P0（决定生死）
1. **质量楔子 = 强模型 + 真闭环 + 施工原语**（痛点 1+4）
   - iacoder 多 provider，**让服主轻松上强模型**（直击痛点 4，AIBuilder 做不到）。
   - **真·observe-act 循环**：用 `get_block`/`scan_region` 边建边读、对照实际结果迭代——超过 AIBuilder 的单次规划。
   - 给 agent **施工原语/模板**（墙/屋顶/楼梯/对称）去组合，而非逐块裸建——跳出"通用重复"。
2. **零摩擦上手 + 离线自检**（痛点 2）
   - 引导式填 key、清晰报错、合理默认、连接健康检查。**复用已有 `HYPERION_AI_OFFLINE`**：先离线跑通管道再填 key。

### P1（口碑与留存）
3. **撤销/回滚 AI 建造**——隐藏优势（痛点 6）
   - 复用**持久化 overlay + transient/persistent 双写**，"撤销上一次 AI 建造/一键复原"几乎免费。**市面没人强调**，却是服主敢把世界交给 AI 的前提。
4. **摆放节流/批处理 + 防破坏护栏**（痛点 5+6）：异步批量摆放、体积上限（`fill` 已有 16384 cap）、区域限制、权限组、操作审计。
5. **成本透明 + 降摩擦计费**（痛点 3）：游戏内显示 token 花销、慷慨免费档、给非技术服主的托管 token 懒人档（抄 CreatureChat Token Shop，带毛利再上）。

### P2（NPC 线增值）
6. **持久人格 + 记忆**：iacoder session 历史已支持多轮 → 每个 NPC 稳定人格 + 长期记忆（CreatureChat 的核心卖点）。
7. **会行动的 NPC**：不只聊天，能真执行（建造/带路/取物）——对话红海里唯一值得碰的角度。

---

## 6. 一句话校准

全行业铁律没变：**质量差、配置烦**。但"闭环修复"已被 AIBuilder 占坑——护城河要往更深推：
**强模型 + 真·边建边看的循环 + 施工原语 + 别人没做的"一键撤销"**（持久化架构白送的优势）。先打 P0。

## 来源
- CreatureChat: https://modrinth.com/mod/creaturechat ；https://github.com/CreatureChat/creature-chat
- AIBuilder (DeepSeek): https://modrinth.com/plugin/ai-builder ；https://www.curseforge.com/minecraft/bukkit-plugins/ai-builder
- BuildAI (开源): https://modrinth.com/plugin/buildai ；https://github.com/KEL0002/BuildAI
- AIBuilds: https://polymart.org/product/4497/aibuilds
- BuilderAI: https://builtbybit.com/resources/builderai-minecraft-builder.96180/
- ServerAssistantAI: https://builtbybit.com/resources/serverassistantai.43148/
- 同类开源参考: https://github.com/dimitarbez/ai-build
