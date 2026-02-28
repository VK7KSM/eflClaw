# ZeroClaw & OpenClaw 研究文档

> 本文档记录了两个项目的架构分析，供后续二次开发参考。
> 最后更新：2026-02-27

---

## 一、两个项目的关系

| 维度 | OpenClaw | ZeroClaw |
|------|----------|----------|
| 语言 | TypeScript + Swift + Python | Rust |
| 定位 | 本地优先个人 AI 助手网关 | 高性能自主 Agent 运行时 |
| 性能目标 | Node.js 生态 | <5MB RAM，<10ms 启动 |
| 渠道支持 | 40+ 渠道（完整生态） | 20+ 渠道（聚焦核心） |
| 工具支持 | 45+ 工具（含 Canvas、Browser） | 聚焦核心工具集 |
| 配置 | Zod schema + TOML | TOML + Rust struct |
| 应用形态 | macOS/iOS/Android 原生 App | CLI + Daemon |
| 设计关系 | 参考原型 | Rust 高性能重实现 |

**结论**：ZeroClaw 借鉴了 OpenClaw 的核心设计理念（HEARTBEAT_OK token、Channel trait、Tool 系统、Active Hours 等），并做了大量精简和安全强化。遇到新需求时应优先参考 OpenClaw 对应模块的实现逻辑。

---

## 二、OpenClaw 文件目录（关键路径）

根目录：`C:\Dev\openclaw`

```
src/
├── agents/                         # Agent 核心逻辑
│   ├── tools/                      # 45+ 工具实现
│   │   ├── telegram-actions.ts     # Telegram 发送工具
│   │   ├── tts-tool.ts             # TTS 工具
│   │   └── message-tool.ts         # 消息发送工具
│   └── date-time.ts                # 用户时区解析
├── channels/
│   ├── plugins/
│   │   └── types.ts                # ChannelPlugin 通用接口定义
│   └── chat-type.ts                # ChatType 枚举 (direct/channel/group)
├── config/
│   ├── config.ts                   # OpenClawConfig 顶层配置
│   ├── types.agent-defaults.ts     # AgentDefaultsConfig (含 heartbeat.activeHours)
│   └── sessions.ts                 # SessionEntry 会话记录
├── infra/                          # 基础设施层（关键参考）
│   ├── heartbeat-active-hours.ts   # ★ 活跃时间判断（可配置时区+分钟精度）
│   ├── heartbeat-runner.ts         # ★ Heartbeat 主循环（含 HEARTBEAT_OK 抑制）
│   ├── heartbeat-events.ts         # Heartbeat 事件类型
│   ├── heartbeat-visibility.ts     # Heartbeat 消息可见性控制
│   ├── heartbeat-wake.ts           # Heartbeat 唤醒逻辑
│   ├── channel-activity.ts         # 渠道活跃状态追踪
│   └── outbound/
│       ├── targets.ts              # ★ 渠道路由抽象（含 "last" 机制）
│       ├── deliver.ts              # 出站消息投递
│       └── channel-resolution.ts  # Channel 插件解析
├── telegram/
│   └── targets.ts                  # Telegram 目标解析（:topic: 语法）
├── discord/
│   └── targets.ts                  # Discord 目标解析
├── slack/
│   └── targets.ts                  # Slack 目标解析
├── auto-reply/
│   └── tokens.ts                   # HEARTBEAT_OK token 定义
└── routing/
    └── session-key.ts              # AccountId 归一化
```

### OpenClaw Heartbeat 关键机制

**活跃时间** (`src/infra/heartbeat-active-hours.ts`):
- 配置字段：`heartbeat.activeHours.start`、`heartbeat.activeHours.end`（HH:MM 格式）
- 支持时区：`timezone = "user"` / `"local"` / 任意 IANA 字符串（如 `"Asia/Shanghai"`）
- 分钟级精度，支持跨午夜范围（如 22:00-08:00）
- 未配置时返回 `true`（全天活跃）

**渠道路由** (`src/infra/outbound/targets.ts`):
- `target = "last"` → 路由到用户最后活跃的渠道（Session 记录 `lastChannel` / `lastTo`）
- `target = "none"` → 不投递，仅执行
- `target = "telegram"` / `"discord"` 等 → 显式指定渠道
- `resolveSessionDeliveryTarget()` 解析投递目标，`resolveOutboundTarget()` 委托给 Channel 插件
- `turnSourceChannel` 防止多渠道并发时的路由竞争（issue #24152）

---

## 三、ZeroClaw 文件目录（完整结构）

根目录：`C:\Dev\zeroclaw`

```
src/
├── agent/
│   ├── agent.rs                    # Agent 顶层入口
│   ├── loop_.rs                    # Agent 主循环（run_tool_call_loop 在此）
│   ├── prompt.rs                   # Prompt 构建
│   ├── dispatcher.rs               # 任务分发
│   └── classifier.rs               # 消息分类
├── channels/                       # 渠道实现
│   ├── mod.rs                      # ★ 渠道总管（process_channel_message、渠道路由）
│   ├── traits.rs                   # Channel trait 定义
│   ├── telegram.rs                 # Telegram channel（241 symbols）
│   ├── email_channel.rs            # Email channel（含 IMAP IDLE monitor）
│   ├── discord.rs                  # Discord channel
│   ├── slack.rs                    # Slack channel
│   ├── signal.rs                   # Signal channel
│   ├── matrix.rs                   # Matrix channel
│   ├── whatsapp.rs                 # WhatsApp channel
│   ├── lark.rs                     # Lark/Feishu channel
│   ├── dingtalk.rs                 # DingTalk channel
│   ├── irc.rs                      # IRC channel
│   ├── imessage.rs                 # iMessage channel
│   ├── nostr.rs                    # Nostr channel
│   ├── qq.rs                       # QQ channel
│   ├── mqtt.rs                     # MQTT channel
│   ├── clawdtalk.rs                # ClawdTalk channel（自有协议）
│   ├── cli.rs                      # CLI channel
│   ├── chat_log.rs                 # [二开] 聊天记录 JSON 持久化
│   ├── chat_index.rs               # [二开] SQLite FTS5 索引
│   ├── chat_summarizer.rs          # [二开] 自动总结 worker
│   └── tts.rs                      # [二开] Edge TTS 合成
├── config/
│   ├── schema.rs                   # ★ 配置 schema（公共 API，499 symbols）
│   └── mod.rs                      # 配置加载/合并
├── daemon/
│   └── mod.rs                      # ★ Daemon + Heartbeat Worker
├── tools/
│   ├── mod.rs                      # 工具注册表
│   ├── traits.rs                   # Tool trait 定义
│   ├── shell.rs                    # Shell 执行工具
│   ├── file_read/write/edit.rs     # 文件操作工具
│   ├── web_fetch.rs                # HTTP 抓取工具
│   ├── web_search_tool.rs          # 网页搜索工具
│   ├── browser.rs                  # 浏览器控制工具
│   ├── memory_store/recall/forget  # 记忆工具
│   ├── cron_add/list/update/...    # Cron 工具（6个）
│   ├── pushover.rs                 # Pushover 推送工具
│   ├── search_chat_log.rs          # [二开] 聊天记录搜索
│   ├── send_email.rs               # [二开] 发送邮件工具
│   ├── send_telegram.rs            # [二开] 发送 Telegram 工具
│   └── send_voice.rs               # [二开] 发送语音工具
├── providers/
│   ├── traits.rs                   # Provider trait
│   ├── anthropic.rs                # Anthropic/Claude
│   ├── openai.rs                   # OpenAI
│   ├── gemini.rs                   # Google Gemini
│   ├── bedrock.rs                  # AWS Bedrock
│   ├── ollama.rs                   # Ollama（本地）
│   ├── openrouter.rs               # OpenRouter
│   ├── compatible.rs               # OpenAI-compatible
│   ├── reliable.rs                 # 故障转移包装器
│   └── mod.rs                      # Provider 工厂
├── heartbeat/
│   ├── engine.rs                   # ⚠️ 孤儿文件：HeartbeatEngine 核心方法仅测试用，生产代码不调用
│   └── mod.rs                      # 模块导出
├── cron/
│   ├── scheduler.rs                # ★ Cron 调度器（含 deliver_announcement）
│   ├── store.rs                    # SQLite 持久化
│   ├── schedule.rs                 # Cron 表达式解析
│   └── types.rs                    # Cron 类型定义
├── memory/
│   ├── traits.rs                   # Memory trait
│   ├── markdown.rs                 # Markdown 记忆后端
│   ├── sqlite.rs                   # SQLite 记忆后端
│   ├── postgres.rs                 # PostgreSQL 记忆后端
│   ├── qdrant.rs                   # Qdrant 向量后端
│   └── embeddings.rs               # 嵌入向量计算
├── security/
│   ├── policy.rs                   # 安全策略
│   ├── pairing.rs                  # 设备配对
│   ├── secrets.rs                  # 密钥存储
│   └── ...                         # 其他安全模块
├── gateway/
│   ├── mod.rs                      # Gateway 入口
│   ├── api.rs                      # REST API
│   ├── ws.rs                       # WebSocket
│   └── sse.rs                      # Server-Sent Events
├── runtime/
│   ├── traits.rs                   # RuntimeAdapter trait
│   ├── native.rs                   # 原生运行时
│   ├── docker.rs                   # Docker 运行时
│   └── wasm.rs                     # WASM 运行时
└── peripherals/
    ├── traits.rs                   # Peripheral trait
    ├── rpi.rs                      # Raspberry Pi GPIO
    ├── serial.rs                   # 串口通信
    └── ...                         # Arduino/Nucleo 固件工具

firmware/                           # 嵌入式固件
├── zeroclaw-esp32/                 # ESP32 固件
├── zeroclaw-esp32-ui/              # ESP32 UI 固件
├── zeroclaw-nucleo/                # STM32 Nucleo 固件
└── zeroclaw-uno-q-bridge/          # Arduino UNO Q 桥接（Python）

web/                                # Web UI
└── src/
    ├── components/                 # React 组件
    └── App.tsx                     # 主应用入口

资料/                               # 本地配置文件（不提交到仓库）
└── config.toml                     # 实际运行的配置文件
```

---

## 四、当前二次开发状态（已完成，未提交）

### 4.1 聊天记录持久化 + 索引（Phase 1-2，2026-02-26）
- `src/channels/chat_log.rs` — JSON 日志（按用户/日期），8 个单测
- `src/channels/chat_index.rs` — SQLite FTS5 索引，8 个单测
- `src/tools/search_chat_log.rs` — Agent 搜索工具（owner 权限控制）
- `src/channels/mod.rs` — 集成：消息持久化 + 启动恢复 + owner 跨用户摘要注入

### 4.2 聊天记录自动总结（Phase 3，2026-02-26）
- `src/channels/chat_summarizer.rs` — Heartbeat 触发，hash 变更检测
- `summary_model` 字段移至 Config 顶层（不配置时 fallback 到 default_model）

### 4.3 Heartbeat 重构（2026-02-26）
- 改为"整体 HEARTBEAT.md → 单次 Agent turn"模式
- HEARTBEAT_OK token 抑制推送（对齐 OpenClaw 设计）
- 23:00-07:00 本地时间 sleep（**活跃时间逻辑仍有问题，见第五节**）
- `parse_tasks()` / `HeartbeatEngine` 保留供测试使用（**注意：engine.rs 是孤儿文件，见 CLAUDE.md §15.1**）

### 4.4 TTS + 语音发送（2026-02-26）
- `src/channels/tts.rs` — Edge TTS 合成（msedge-tts crate，无需 API key）
- `src/tools/send_voice.rs` — Agent 主动语音发送工具（先发语音后发文本）

### 4.5 Email Monitor → Telegram 通知（2026-02-25/26）
- `src/tools/send_email.rs` — SendEmailTool（SMTP，安全控制）
- `src/tools/send_telegram.rs` — SendTelegramTool（Bot API）
- Email channel：自发邮件过滤 + digest prompt + tool 排除机制
- **`notify_channel`/`notify_to` 已在 `资料/config.toml` 第 191-192 行正确配置** ✅

### 4.6 编译状态
`cargo check` — 零错误零警告 ✅

---

## 五、待改进：活跃时间逻辑（Active Hours）

### 5.1 现状问题

当前代码位于 `src/daemon/mod.rs:189-194`：

```rust
let local_hour = chrono::Local::now().hour();
if local_hour >= 23 || local_hour < 7 {
    // skip
}
```

**问题列表：**

| # | 问题 | 影响 |
|---|------|------|
| 1 | 时间区间硬编码（23:00-07:00） | 无法根据用户习惯调整，修改需重编译 |
| 2 | 仅小时精度（`< 7` 而非 `< 6:30`） | CLAUDE.md 描述的 06:30 与实际代码不符 |
| 3 | 无时区配置 | 用 `chrono::Local`（OS 时区），无法指定用户所在时区 |
| 4 | `HeartbeatConfig` schema 无对应字段 | 用户无法在 config.toml 中配置 |

### 5.2 OpenClaw 参考实现

`src/infra/heartbeat-active-hours.ts:isWithinActiveHours()`：
- 配置：`heartbeat.activeHours = { start: "08:00", end: "23:00", timezone: "Asia/Shanghai" }`
- 时区支持：`"user"`（用户配置时区）/ `"local"`（OS 时区）/ 任意 IANA 字符串
- 分钟精度：将时间转为"总分钟数"比较
- 跨午夜支持：`endMin < startMin` 时使用 OR 逻辑

### 5.3 修改方案

**Step 1 — 扩展 `HeartbeatConfig`（`src/config/schema.rs`）**

新增字段：
```toml
[heartbeat]
enabled = true
interval_minutes = 30
active_hours_start = "08:00"   # HH:MM，可选
active_hours_end = "23:00"     # HH:MM，可选，支持跨午夜
active_hours_timezone = "local" # "local" / "UTC" / IANA 时区字符串
```

对应 Rust 结构体：
```rust
pub struct HeartbeatConfig {
    // ...现有字段...
    #[serde(default)]
    pub active_hours_start: Option<String>,  // "HH:MM"
    #[serde(default)]
    pub active_hours_end: Option<String>,    // "HH:MM"
    #[serde(default)]
    pub active_hours_timezone: Option<String>, // default: "local"
}
```

**Step 2 — 实现 `is_within_active_hours(config: &HeartbeatConfig) -> bool`（`src/heartbeat/` 新增）**

逻辑：
1. 如果 `active_hours_start` 或 `active_hours_end` 未配置 → 返回 `true`（全天活跃）
2. 解析时区（`local` → `chrono::Local`，其他 → `chrono_tz::Tz::from_str()`）
3. 将当前时间转换为目标时区，取 `hour * 60 + minute`
4. 解析 start/end 为总分钟数
5. 判断：若 `end > start` → 范围内返回 `true`；若 `end < start`（跨午夜）→ 范围内返回 `true`

**Step 3 — 替换 daemon 中的硬编码逻辑**

将 `src/daemon/mod.rs:189-194` 的硬编码检查替换为调用新函数：
```rust
if !crate::heartbeat::is_within_active_hours(&config.heartbeat) {
    tracing::debug!("Heartbeat skipped: outside active hours");
    continue;
}
```

**依赖**：`chrono-tz` crate（解析 IANA 时区），当前 `Cargo.toml` 中需确认是否已有。

---

## 六、待改进：渠道路由抽象（Channel Routing）

### 6.1 现状问题

Heartbeat 消息投递目前通过两个函数实现：
- `heartbeat_delivery_target()`（`src/daemon/mod.rs:322`）
- `deliver_announcement()`（`src/cron/scheduler.rs:302`）

**问题列表：**

| # | 问题 | 位置 | 影响 |
|---|------|------|------|
| 1 | 无 `"last"` 路由能力 | `daemon/mod.rs` | 无法路由到用户最后活跃的渠道 |
| 2 | 手动维护渠道白名单 | `daemon/mod.rs:347-379` + `scheduler.rs:308-380` | 每新增渠道需修改两处 match 语句 |
| 3 | 绕过 Channel trait | `scheduler.rs` | 直接 `new()` 实例化渠道，不走统一接口 |
| 4 | 覆盖渠道不完整 | `scheduler.rs:308` | 只支持 telegram/discord/slack/mattermost；Signal/Matrix/WhatsApp/Lark 等无法收到 heartbeat |
| 5 | 无降级/备用策略 | — | 主渠道不可用时直接报错，无自动降级 |

### 6.2 OpenClaw 参考实现

`src/infra/outbound/targets.ts:resolveHeartbeatDeliveryTarget()`：
- `target = "last"` → 从 Session 记录读取 `lastChannel` + `lastTo`，自动路由到最后活跃渠道
- `target = "none"` → 不投递
- 路由解析委托给 `resolveOutboundChannelPlugin()` → 各渠道实现各自的 `outbound.resolveTarget()`
- Session `lastChannel` 由每次入站消息自动更新（`channel-activity.ts`）
- `turnSourceChannel` 机制防止并发路由竞争

### 6.3 修改方案

#### 方案 A：最小改动（推荐，符合 YAGNI）

不引入 `"last"` 机制，只修复已知问题：

1. **合并两个 match 为一个辅助函数**（DRY），位置：`src/cron/scheduler.rs`
2. **通过 Channel trait 投递**，而非直接实例化

```rust
// 伪代码：用 Channel trait 统一投递
async fn deliver_via_channel(config: &Config, channel_name: &str, target: &str, msg: &str) -> Result<()> {
    let channel = crate::channels::create_channel_by_name(config, channel_name)?;
    channel.send(&SendMessage::new(msg, target)).await
}
```

前提：`src/channels/mod.rs` 需要有 `create_channel_by_name()` 工厂函数（检查是否已有类似的）。

3. **`HeartbeatConfig` 增加更多渠道支持**：只需在工厂函数注册，不改 daemon 逻辑。

#### 方案 B：完整 `"last"` 机制（长期目标，复杂度较高）

需要：
1. **Session 层**：记录每个用户的 `(last_channel, last_to)` 并持久化（如存入 SQLite）
2. **渠道活跃事件**：每次入站消息更新 session 的 `last_channel`
3. **Heartbeat 路由**：读取 session 的 `last_channel`，失败则降级到显式配置的 `target`

**建议**：先实施方案 A，方案 B 作为后续迭代。方案 A 解决了 80% 的问题（渠道不完整、代码重复），方案 B 解决了用户体验问题（自动路由）。

---

## 七、其他参考文件

### OpenClaw 其他值得参考的模块

| 文件 | 内容 | ZeroClaw 对应 |
|------|------|--------------|
| `src/infra/heartbeat-runner.ts` | Heartbeat 完整实现（ghost reminder、ackMaxChars） | `src/daemon/mod.rs` |
| `src/infra/outbound/deliver.ts` | 统一出站投递逻辑 | `src/cron/scheduler.rs:deliver_announcement` |
| `src/agents/tools/telegram-actions.ts` | Telegram 工具完整实现 | `src/tools/send_telegram.rs` |
| `src/infra/channel-activity.ts` | 渠道活跃状态追踪 | 暂无对应 |
| `src/auto-reply/tokens.ts` | Token 常量定义（HEARTBEAT_OK 等） | 硬编码在 daemon |
