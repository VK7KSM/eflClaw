# ZeroClaw 上游对比分析报告

**生成时间**：2026-02-28
**我们的分叉点**：`d352449` — `ci(release): pin setup-ndk action to commit sha (#1600)`（对应 v0.1.7 release）
**上游当前 HEAD**：`1a0bb175` — `fix(agent): retry deferred tool follow-through in CJK contexts`
**上游自分叉点新增提交数**：**452 commits**
**上游变更文件总数**：**692 个文件**

---

## 一、上游新增功能概述

自 v0.1.7 以来，上游进行了大量功能扩展，主要方向如下：

### 1.1 核心架构重构
- **`src/agent/loop_.rs` 拆分**：重构为子模块目录 `src/agent/loop_/`（context.rs、execution.rs、history.rs、parsing.rs），提升可维护性。我们的 `loop_.rs` 是单文件版本。
- **新增 `src/agent/agent.rs`、`dispatcher.rs`、`classifier.rs`、`prompt.rs`**：进一步解耦 Agent 内部责任

### 1.2 全新子系统（我们没有）
| 子系统 | 路径 | 功能描述 |
|--------|------|----------|
| Plugin 系统 | `src/plugins/` | WASM 插件加载与注册 |
| MCP 服务器 | `src/tools/mcp_client.rs` 等 | 外部 MCP server 接入 |
| Goals 引擎 | `src/goals/` | 长期目标追踪与规划 |
| Sub-Agent 协调 | `src/coordination/` + `subagent_spawn.rs` | 多 Agent 协调消息总线 |
| WASM Skill 引擎 | `src/skills/` + `src/skillforge/` | 可安装技能包 |
| SOP 系统 | `src/sop/` | 标准操作流程定义与执行 |
| Android 客户端 | `clients/android/` + `clients/android-bridge/` | UniFFI 桥接原生 Android App |
| Approval 系统 | `src/approval/` | 一次性 all-tools 授权流程 |
| IPC 工具 | `src/tools/agents_ipc.rs` | 进程间通信工具 |
| 进程管理工具 | `src/tools/process.rs` | 后台进程 spawn/list/output/kill |
| 任务计划工具 | `src/tools/task_plan.rs` | 会话级多步任务追踪 |
| WASM 沙箱 | `src/runtime/` 扩展 | 可配置 WASM 安全运行时 |
| 集成注册表 | `src/integrations/` | 第三方集成统一注册 |

### 1.3 新增工具（`src/tools/`）
上游新增（我们没有）：
- `apply_patch.rs` — 应用 unified diff 补丁
- `docx_read.rs` — 读取 Word 文档
- `task_plan.rs` — 多步任务规划
- `subagent_spawn.rs / list / manage / registry` — Sub-Agent 管理
- `mcp_client.rs / mcp_tool.rs / mcp_transport.rs / mcp_protocol.rs` — MCP 客户端
- `wasm_module.rs / wasm_tool.rs` — WASM 工具
- `url_validation.rs` — 统一 URL 验证
- `schema.rs` — 工具 schema 辅助
- `sop_*.rs`（advance/approve/execute/list/status）— SOP 工具套件
- `delegate_coordination_status.rs` — 委派协调状态

### 1.4 新增 Channel 支持
- **WhatsApp heartbeat/cron 投递**：`feat(whatsapp): support heartbeat and cron delivery for whatsapp_web`
- **Slack Socket Mode**：`feat(slack): add socket mode listener fallback`
- **Lark/Feishu cron 投递**：`feat(cron): add lark and feishu delivery targets`
- **Telegram 自定义 Bot API base_url**：`feat(telegram): support custom Bot API base_url`
- 新增 channels：`clawdtalk.rs`、`imessage.rs`、`irc.rs`、`mqtt.rs`、`nostr.rs`（我们没有）

### 1.5 Provider 增强
- **Reasoning Level Override**：每 Provider 可独立配置推理强度
- **Vision 支持增强**：`model_support_vision` 配置项，可按模型覆盖 vision 能力
- **Anthropic 视觉修复**：`fix(agent): avoid anthropic vision preflight false negatives`（与我们自己的 Gemini 视觉修复并行）
- **Gemini OAuth 自动刷新**：`feat(providers): auto-refresh expired Gemini OAuth tokens in warmup`
- **新增 Provider 文件**：`openai_codex.rs`、`quota_adapter.rs`、`quota_cli.rs`、`router.rs`
- **Qwen 编码端点**：`feat(provider): add qwen-coding-plan endpoint alias`
- GitHub Copilot 集成到 onboard wizard

### 1.6 安全增强
- **Role Policy + OTP**：`feat(security): add role-policy and otp challenge foundations`，默认启用 OTP
- **URL CIDR/Domain 白名单**：`feat(security): unify URL validation with configurable CIDR/domain allowlist`
- **Perplexity 对抗后缀过滤**：`feat(security): add opt-in perplexity adversarial suffix filter`
- **Syscall 异常检测**：`feat(security): add and harden syscall anomaly detection`
- **秘钥生命周期加固**：`hardening: tighten gateway auth and secret lifecycle handling`

### 1.7 Gateway 扩展
- **流式响应**：`feat(gateway): add streaming mode for webhook responses`
- **OpenClaw 兼容层**：`feat(gateway): add OpenClaw migration compat layer with /api/chat and tools-enabled /v1/chat/completions`
- **Node Control 脚手架 API**：`feat(gateway): add experimental node-control scaffold API`

### 1.8 内存后端扩展
- **SQLite + Qdrant 混合后端**：`feat(memory): add sqlite+qdrant hybrid backend`（`src/memory/hybrid.rs`）
- **PostgreSQL 后端修复**：恢复 tokio-postgres-rustls 依赖

### 1.9 重要 Bug 修复（影响我们的代码）
| Commit | 描述 | 与我们的关联 |
|--------|------|-------------|
| `1a0bb175` | fix(agent): retry deferred tool follow-through in CJK contexts | **高度相关**，我们的 loop_.rs 有大量中文相关修改 |
| `8004260e` | fix(agent): retry deferred-action replies missing tool calls | Agent loop 修复 |
| `5981e505` | fix(agent): avoid anthropic vision preflight false negatives | **与我们的 Anthropic 修复并行** |
| `b63dfb89` | fix(config): resolve windows compile blockers | **Windows 开发环境直接受益** |
| `7f3b7302` | fix(config): resolve env credential reporting and safer compaction default | config 相关，我们改了 config |
| `dedb59a4` | fix(agent): stop converting plain URLs into shell calls | loop_.rs 安全修复 |
| `15457cc3` | fix(agent): parse direct XML tool tags in web chat | loop_.rs 解析修复 |

---

## 二、冲突风险文件清单（上游和我们都改过）

### 🔴 高风险（变更量大，逻辑深度重叠）

| 文件 | 上游提交数 | 我们的修改内容 | 冲突原因 |
|------|-----------|----------------|----------|
| `src/agent/loop_.rs` | **35+** | Anthropic/Gemini 脏历史修复、Gemini vision 支持 | 上游已重构为子模块目录，我们仍是单文件 |
| `src/channels/mod.rs` | **20+** | 聊天记录持久化、email digest、tool 排除机制 | 上游大量新功能注入 |
| `src/config/schema.rs` | **50+** | HeartbeatConfig active_hours、TTS 配置 | 上游新增大量配置项（OTP、MCP、Skills、Plugin 等） |
| `src/config/mod.rs` | **30+** | 配置加载改动 | 上游重构 re-export 结构多次 |
| `src/tools/mod.rs` | **25+** | 注册 send_email/telegram/voice/search_chat_log | 上游注册了大量新工具 |
| `src/providers/anthropic.rs` | **10+** | 空文本块修复、历史脏数据处理 | 上游同期也在修 Anthropic vision |
| `src/providers/gemini.rs` | **8+** | thought_signature 降级、vision 支持、OAuth token | 上游同期修 Gemini OAuth 刷新 |
| `src/daemon/mod.rs` | **15+** | active_hours 可配置化、heartbeat 重构 | 上游修改了 cron 投递和 heartbeat 路由 |

### 🟡 中风险（有交叉但范围较小）

| 文件 | 上游提交数 | 我们的修改内容 |
|------|-----------|----------------|
| `Cargo.toml` | **30+** | 添加 msedge-tts、lettre、sqlx 等依赖 |
| `src/providers/mod.rs` | **15+** | Provider 注册可能有差异 |
| `src/providers/openai.rs` | **8+** | 可能有 reasoning level 相关 |
| `src/providers/openrouter.rs` | **5+** | 配置更新 |
| `src/providers/reliable.rs` | **5+** | 模型回退映射更新 |
| `src/providers/traits.rs` | **10+** | 新增 reasoning_level、capabilities() 等接口 |
| `src/cron/scheduler.rs` | **10+** | Lark/Feishu/WhatsApp 投递目标 |
| `src/gateway/api.rs` | **8+** | streaming mode、安全加固 |
| `src/multimodal.rs` | **5+** | Vision 支持修复（我们也修了）|
| `src/onboard/wizard.rs` | **5+** | 上游加了 Copilot、我们可能改了提示 |
| `src/peripherals/mod.rs` | **3+** | hardware 条件编译变化 |
| `src/doctor/mod.rs` | **3+** | 诊断命令扩展 |
| `src/tools/cron_add.rs` | **5+** | Cron 投递目标扩展 |
| `src/tools/delegate.rs` | **5+** | 协调功能扩展 |
| `src/tools/model_routing_config.rs` | **3+** | 路由配置更新 |
| `CLAUDE.md` | **3+** | 上游也在更新 CLAUDE.md |

### 🟢 低风险（我们改了，上游改的部分很小）

| 文件 | 说明 |
|------|------|
| `src/channels/email_channel.rs` | 上游没有大改这个文件，我们的修改是新增功能（self-email filter、digest prompt）|
| `src/providers/bedrock.rs` | 上游小改，我们小改，交叉少 |
| `src/providers/compatible.rs` | 上游小改，兼容 API 相关 |
| `src/providers/copilot.rs` | 上游可能改了 Copilot onboard，我们是否改了？ |
| `src/cron/store.rs` | 上游小改 |
| `src/cron/types.rs` | 上游小改 |

---

## 三、安全可合并文件清单（上游改了，我们没动）

这些文件可以**直接从上游 cherry-pick 或 merge**，无冲突风险：

### 全新文件（我们完全没有）
```
src/plugins/         — 整个插件系统
src/goals/           — Goals 引擎
src/coordination/    — Sub-Agent 协调总线
src/skills/          — WASM Skill 引擎
src/skillforge/      — Skill 注册管理
src/sop/             — SOP 系统
src/approval/        — 审批流系统
src/integrations/    — 第三方集成注册表
clients/android/     — Android 客户端
clients/android-bridge/ — UniFFI 桥接
src/tools/apply_patch.rs
src/tools/docx_read.rs
src/tools/task_plan.rs
src/tools/subagent_*.rs
src/tools/mcp_*.rs
src/tools/wasm_*.rs
src/tools/url_validation.rs
src/tools/schema.rs
src/tools/sop_*.rs
src/tools/delegate_coordination_status.rs
src/tools/process.rs
src/gateway/openai_compat.rs
src/gateway/openclaw_compat.rs
src/gateway/ws.rs
src/memory/hybrid.rs
src/memory/hygiene.rs
src/providers/openai_codex.rs
src/providers/quota_adapter.rs
src/providers/quota_cli.rs
src/providers/router.rs
src/agent/agent.rs
src/agent/dispatcher.rs
src/agent/classifier.rs
src/agent/prompt.rs
src/agent/loop_/context.rs
src/agent/loop_/execution.rs
src/agent/loop_/history.rs
src/agent/loop_/parsing.rs
src/channels/clawdtalk.rs
src/channels/imessage.rs
src/channels/irc.rs
src/channels/mqtt.rs
src/channels/nostr.rs
src/channels/whatsapp_storage.rs
```

### 存在但我们未改的文件（可安全更新）
```
src/channels/telegram.rs      — Telegram Bot API base_url 支持
src/channels/discord.rs       — Discord 改进
src/channels/slack.rs         — Socket Mode
src/channels/lark.rs          — 富文本、mention_only
src/channels/whatsapp.rs / whatsapp_web.rs  — heartbeat/cron
src/channels/qq.rs            — sandbox mode
src/channels/signal.rs
src/channels/matrix.rs
src/channels/mattermost.rs
src/channels/wati.rs
src/channels/traits.rs        — 新增 trait 方法
src/channels/transcription.rs — config-level api_key
src/memory/backend.rs / mod.rs / postgres.rs / cli.rs
src/gateway/mod.rs
src/lib.rs / src/main.rs      — 新模块导出
src/agent/mod.rs
src/agent/research.rs
src/identity.rs
.github/workflows/**          — CI/CD 全面重构
docs/**                       — 文档更新
```

---

## 四、我们独有的功能（上游没有）

这些功能在上游不存在，必须在合并时**保留**：

| 文件/功能 | 描述 |
|-----------|------|
| `src/channels/chat_log.rs` | JSON 格式聊天记录持久化 |
| `src/channels/chat_index.rs` | SQLite FTS5 全文检索索引 |
| `src/channels/chat_summarizer.rs` | Heartbeat 触发的聊天自动总结 |
| `src/channels/tts.rs` | Edge TTS 语音合成（无需 API Key） |
| `src/tools/search_chat_log.rs` | Agent 搜索聊天记录工具 |
| `src/tools/send_email.rs` | SendEmailTool（SMTP） |
| `src/tools/send_telegram.rs` | SendTelegramTool（Bot API） |
| `src/tools/send_voice.rs` | Agent 主动发送语音工具 |
| `src/channels/email_channel.rs` 的 monitor 逻辑 | 自发邮件过滤、digest prompt、notify_channel/to 路由 |
| `src/channels/mod.rs` 中聊天记录集成 | 消息持久化、启动恢复、owner 跨用户摘要 |
| `src/daemon/mod.rs` 中 active_hours 配置化 | HeartbeatConfig active_hours_start/end |
| `src/config/schema.rs` 中 HeartbeatConfig 扩展 | active_hours_start/end 字段 |

---

## 五、推荐同步工作流

### 5.1 设置 upstream remote（一次性操作）

```bash
# 在我们的工作目录
cd /c/Dev/zeroclaw
git remote add upstream https://github.com/zeroclaw-labs/zeroclaw.git
git fetch upstream
```

### 5.2 日常同步策略：专题 cherry-pick

**不建议直接 merge 或 rebase**——上游改动了 452 个提交，直接合并冲突量极大。

推荐按功能专题 cherry-pick：

```bash
# 示例：cherry-pick Windows compile 修复（低冲突风险）
git cherry-pick b63dfb89  # fix(config): resolve windows compile blockers

# 示例：cherry-pick CJK 工具调用修复（需解决 loop_.rs 冲突）
git cherry-pick 1a0bb175  # fix(agent): retry deferred tool follow-through in CJK contexts
```

### 5.3 大版本同步策略（推荐 3 步走）

**第一步：同步"安全可合并"的全新模块**

```bash
# 把上游全新文件复制进来（无冲突）
git checkout upstream/main -- src/plugins/
git checkout upstream/main -- src/goals/
git checkout upstream/main -- src/coordination/
git checkout upstream/main -- src/skills/
git checkout upstream/main -- src/sop/
git checkout upstream/main -- src/approval/
git checkout upstream/main -- clients/android/
git checkout upstream/main -- clients/android-bridge/
# 等等...
```

**第二步：逐文件对比合并高风险文件**

使用 `git diff upstream/main -- <file>` 查看上游对该文件的全量变更，
然后手动将上游新功能移植进我们修改过的版本：

```bash
# 查看上游对 loop_.rs 的变更
git diff d3524494 upstream/main -- src/agent/loop_.rs > /tmp/upstream_loop_diff.patch

# 查看上游对 config/schema.rs 的变更
git diff d3524494 upstream/main -- src/config/schema.rs > /tmp/upstream_schema_diff.patch
```

重点需要手动合并的文件（按优先级排序）：
1. `src/config/schema.rs` — 先同步上游新配置项，再加回我们的字段
2. `src/tools/mod.rs` — 先同步上游新工具注册，再加回我们的工具
3. `src/providers/traits.rs` — 同步新 trait 方法，确保我们的 Provider 实现兼容
4. `src/agent/loop_.rs` — 最复杂，建议分段 cherry-pick 而不是整体 diff

**第三步：更新 Cargo.toml**

```bash
# 查看上游新增依赖
git diff d3524494 upstream/main -- Cargo.toml
# 手动将上游新依赖添加到我们的 Cargo.toml，保留我们的 msedge-tts、lettre、sqlx 等
```

### 5.4 长期建议：建立 upstream tracking 分支

```bash
# 创建一个专门跟踪上游的分支
git checkout -b upstream-tracking
git reset --hard upstream/main

# 我们的功能在 main 分支开发
# 定期将 upstream-tracking 更新，然后专题 cherry-pick 到 main
git checkout main
git cherry-pick <commit>...
```

### 5.5 持续监控上游更新

建议每周执行：

```bash
cd /c/Dev/zeroclaw_original
git pull origin main  # 更新上游镜像

# 查看新增提交
git log --oneline <上次同步commit>..HEAD

# 查看新增的与我们重叠的文件
git diff --name-only <上次同步commit>..HEAD | grep -E "^src/(agent|channels|config|tools|providers|daemon)"
```

---

## 六、优先级建议

### 近期可直接合并（低风险、高价值）

1. **`fix(config): resolve windows compile blockers`** (`b63dfb89`) — Windows 开发友好
2. **`feat(telegram): support custom Bot API base_url`** (`63fcd7dd`) — 增强 Telegram 配置灵活性
3. **`feat(tools): add docx_read tool`** (`df6f7455`) — 独立新工具，无冲突
4. **`feat(tools): add apply_patch tool`** (`fbb3c6ae`) — 独立新工具，无冲突
5. 上游新增的全部独立 Channel 文件（clawdtalk、irc、mqtt、nostr 等）

### 中期规划（需解决冲突）

1. **`src/providers/traits.rs` 同步** — reasoning_level 接口，影响所有 provider
2. **`src/config/schema.rs` 同步** — 引入 MCP、Plugin、OTP 等新配置
3. **`src/tools/mod.rs` 同步** — 引入新工具注册（MCP、SOP、Sub-Agent 等）

### 长期规划（高复杂度）

1. **`src/agent/loop_.rs` 与上游子模块对齐** — 考虑将我们的修改迁移到 loop_/ 子模块结构
2. **Plugin/Skill 系统集成** — 可将我们的 TTS、ChatLog 功能包装为插件
3. **Sub-Agent 协调** — 与我们的 delegate 工具整合

---

*报告基于上游 commit `1a0bb175`（2026-02-27），分叉点 `d352449`（v0.1.7）。*
