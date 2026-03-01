<p align="center">
  <img src="zeroclaw.png" alt="elfClaw" width="200" />
</p>

<h1 align="center">elfClaw 🦀</h1>

<p align="center">
  <strong>ZeroClaw 的大胆分叉——为真实使用场景而重建。</strong><br>
  持久对话记忆 · 主动语音通知 · 模块化架构 · 纯 Rust 生产级 AI Agent 运行时
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <a href="https://github.com/VK7KSM/eflClaw">GitHub</a> ·
  <a href="#-新功能">新功能</a> ·
  <a href="#-为什么要分叉架构问题">为什么要分叉</a> ·
  <a href="#-路线图与上游同步">路线图</a>
</p>

---

## 目录

- [elfClaw 是什么？](#-elfclaw-是什么)
- [为什么要分叉——架构问题](#-为什么要分叉架构问题)
- [新功能](#-新功能)
  - [聊天记录持久化与记忆](#1-聊天记录持久化与记忆)
  - [对话历史全文搜索](#2-对话历史全文搜索)
  - [文字转语音与语音消息](#3-文字转语音与语音消息)
  - [主动通知——Telegram 与 Email](#4-主动通知telegram-与-email)
  - [Email 监控 → Telegram 摘要](#5-email-监控--telegram-摘要)
  - [可配置活跃时间段](#6-可配置活跃时间段)
  - [统一渠道投递](#7-统一渠道投递)
  - [Agent Loop 模块化](#8-agent-loop-模块化)
- [已集成的上游功能](#-已集成的上游功能)
- [路线图与上游同步](#-路线图与上游同步)
- [致谢](#-致谢)
- [快速开始](#-快速开始)

---

## 🦅 elfClaw 是什么？

**elfClaw** 是 [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) 的一个分叉项目。ZeroClaw 是 ZeroClaw Labs 社区开发的 Rust 原生自主 AI Agent 运行时。我们分叉它，是因为我们需要上游项目尚未优先实现的功能——最关键的是**持久对话历史**、**主动通知**以及一个**可持续演进的代码架构**，而不是一个随时可能倒塌的屎山。

elfClaw 包含 ZeroClaw 的所有功能——多 Provider LLM 支持、多渠道消息（Telegram、Discord、Slack、Matrix、Email、IRC、WhatsApp 等）、工具执行、定时心跳、硬件外设、MCP 客户端、子 Agent 协调——并在此之上增加了让它真正可以作为日常工具使用的一层完善。

我们持续跟踪 ZeroClaw 上游并合并有价值的更新。当我们的二次开发功能成熟后，计划将其作为 PR 提交回上游。

---

## 🏗 为什么要分叉——架构问题

ZeroClaw 在技术上令人印象深刻：编译后二进制文件不到 5MB，启动时间不足 10ms，覆盖了极其广泛的集成。我们深深尊重这个团队的工作。

但诚实的工程需要诚实的评估。截至 2026 年初，ZeroClaw 的代码库存在**结构性债务**，使得贡献和维护越来越痛苦：

### 上帝模块（God Module）问题

| 文件 | 行数（上游） | 问题 |
|------|------------|------|
| `src/config/schema.rs` | **11,061 行** | 单体配置巨无霸。数百个结构体，没有 derive 宏简化。每个 `Default` 实现全部手写。添加一个配置键需要在 3 处以上修改。 |
| `src/channels/mod.rs` | **10,645 行** | 典型的上帝模块。消息处理、历史管理、System Prompt 构建、Channel 初始化、Tool dispatch 全部塞在一个文件里。 |
| `src/onboard/wizard.rs` | **8,061 行** | 交互式 setup、快速 setup、Provider 模型列表、Scaffold 全混在一起，毫无关注点分离。 |
| `src/agent/loop_.rs` | **5,153 行** | 整个 Agent 执行循环——上下文构建、历史压缩、工具解析、并行派发——全在一个文件里。 |

> **合计：** 四个文件贡献约 **34,920 行**，占整个代码库近 20%。

### 重复代码与维护黑洞

上游内部代码审查记录了这些问题：

- **24 个测试**需要手写完整的 `ChannelRuntimeContext { ... }` 初始化，每新增一个字段就要改 24 处
- `Config` 构造出现在 3 个地方（`Config::default()`、Wizard 交互式、Wizard 快速 setup），字段增减需要同步 3 处
- 大量 `match channel_name { "telegram" => ..., "discord" => ..., "slack" => ... }` 硬编码重复判断

### 结果：功能"存在"但残废

ZeroClaw 的功能广度具有欺骗性。很多功能只是**骨架级实现**——能编译、有文档，但无法真实使用：

- **没有持久聊天记录。** 每次对话从零开始。Agent 对昨天说过什么一无所知。
- **没有自动对话总结。** 随着历史窗口填满，长会话会默默丢失上下文。
- **没有主动通知能力。** Agent 能收消息，但无法主动联系你。
- **Email 监控是摆设。** 可以拉取邮件，但没有机制将其通知给你（比如发到 Telegram）。
- **活跃时间段硬编码。** 心跳调度器在固定的编译时间唤醒，无法配置。
- **渠道投递硬编码。** Cron 和心跳通知只支持 4 个渠道（telegram/discord/slack/mattermost），通过硬编码 `match` 块绕过 `Channel` trait。

上游项目的内部代码质量评估总结：

> *"功能齐全但架构需要重构，不算屎山但是已经在屎山的路上了——再不拆文件就变成了。"*

### elfClaw 的做法

elfClaw 采取**针对性修复**——我们不为重写而重写，只修复那些实际阻碍功能开发的部分：

- 渠道投递统一通过 `deliver_to_channel()` 走 `Channel` trait——这是我们自己修复的架构债务：我们 fork 的基础版本中，`daemon/mod.rs` 和 `cron/scheduler.rs` 各自有一套硬编码 `match channel_name` 块，完全绕过了 `Channel` trait
- Agent Loop 子模块结构来自上游 ZeroClaw 较新版本的提交，我们将其集成并在上面叠加了 elfClaw 特有的定制逻辑
- 新功能以**专注、可测试的模块**形式添加（每个 200~450 行），每个模块包含覆盖正常路径、错误路径和边界条件的单元测试

---

## ✨ 新功能

### 1. 聊天记录持久化与记忆

**ZeroClaw 最大的缺失功能。**

elfClaw 自动将每次对话持久化到磁盘，并在 SQLite FTS5 中建立索引以供全文搜索。Agent 在会话之间不会丢失上下文。

**文件：** `src/channels/chat_log.rs`（449 行）、`src/channels/chat_index.rs`（439 行）

```toml
# config.toml — 无需任何配置，开箱即用
# 日志存储路径：~/.zeroclaw/chat_log/<渠道>/<用户>/<YYYY-MM-DD>.json
# 索引存储路径：~/.zeroclaw/chat_index.db
```

**功能详情：**
- 将所有消息（用户 + Agent）以带时间戳和元数据的结构化 JSON 持久化
- 维护 SQLite FTS5 索引，支持即时全文搜索
- 会话恢复时自动加载最近的对话历史
- Owner 账户可获得跨用户摘要注入（可了解所有用户的对话模式）
- 16 个单元测试，覆盖持久化、索引、恢复和访问控制

**对比 ZeroClaw 上游：** 完全不存在。每次会话都是无状态的。

---

### 2. 对话历史全文搜索

Agent 可以将自己的对话历史作为工具进行搜索。

**文件：** `src/tools/search_chat_log.rs`（245 行）

**使用方式——直接向 Agent 提问：**
```
"在我的对话历史中搜索关于树莓派项目的内容"
"上周我们讨论了哪些关于 API 密钥的内容？"
"找出所有我询问过部署流程的对话"
```

**工具参数：**
```json
{
  "query": "树莓派 GPIO 配置",
  "channel": "telegram",
  "user_id": "495916105",
  "limit": 20
}
```

**访问控制：** 只有 `owner` 角色用户可以跨用户搜索，普通用户只能搜索自己的历史。

**对比 ZeroClaw 上游：** 完全不存在。

---

### 3. 文字转语音与语音消息

elfClaw 能够合成语音并发送语音消息——**无需任何 API Key**。

**文件：** `src/channels/tts.rs`（195 行）、`src/tools/send_voice.rs`（223 行）

使用 **Microsoft Edge Read Aloud API**（完全免费，无需认证）。Agent 根据上下文自主决定何时发送语音。

**使用方式：**
```
"把今天的摘要以语音消息的形式发给我"
"用语音告诉我这段代码的分析结果"
```

**配置（可选）：**
```toml
[tts]
voice = "zh-CN-XiaoxiaoNeural"
rate = "+0%"
pitch = "+0Hz"
```

**对比 ZeroClaw 上游：** 完全不存在。

---

### 4. 主动通知——Telegram 与 Email

**文件：** `src/tools/send_telegram.rs`（214 行）、`src/tools/send_email.rs`（239 行）

```json
// send_telegram
{ "chat_id": "495916105", "message": "定时任务已完成，共处理 3 个项目。" }

// send_email
{ "to": "user@example.com", "subject": "elfClaw 每日摘要", "body": "..." }
```

**对比 ZeroClaw 上游：** 完全不存在。

---

### 5. Email 监控 → Telegram 摘要

```toml
[channels.email]
mode = "monitor"
imap_host = "imap.gmail.com"
imap_port = 993
username = "你的邮箱@gmail.com"
password = "应用专用密码"
from_address = "你的邮箱@gmail.com"
notify_channel = "telegram"
notify_to = "你的 TELEGRAM CHAT ID"
```

工作流：IMAP IDLE 监听 → Agent 分类分析 → 推送到 Telegram → 由用户决定后续操作 → Agent 绝不主动发邮件。

**对比 ZeroClaw 上游：** Email 监控功能存在，但没有跨渠道通知能力。

---

### 6. 可配置活跃时间段

```toml
[heartbeat]
active_hours_start = "08:00"
active_hours_end = "23:00"
timezone = "Asia/Shanghai"
```

支持跨午夜时间段。**对比 ZeroClaw 上游：** 硬编码，无法配置。

---

### 7. 统一渠道投递

所有通告路径（心跳、Cron 任务、定时通知）现在通过单一的 `deliver_to_channel()` 函数走 `Channel` trait 统一投递，适用于全部已配置渠道。

**对用户意味着什么：** 心跳通告和 Cron 通知适用于所有已配置的渠道，不再受限于固定的几个。

**对开发者意味着什么：** 新增渠道后自动对所有投递路径生效，无需维护多处重复的 match 块。

**背景说明：** 我们 fork 起点的代码中，`daemon/mod.rs` 和 `cron/scheduler.rs` 各自维护着独立的 `match channel_name { "telegram" => ..., "discord" => ..., "slack" => ..., "mattermost" => ... }` 硬编码块，完全绕过了 `Channel` trait。这是我们继承的架构债务，我们自己修了它，不是上游的问题。

**对比 ZeroClaw 上游：** 仅支持 4 个硬编码渠道，完全绕过 `Channel` trait。

---

### 8. Agent Loop 子模块结构

elfClaw 集成了上游 ZeroClaw 较新版本中引入的子模块架构，并在其基础上叠加了 elfClaw 特有的定制逻辑。

原本 5,810 行的单体 `src/agent/loop_.rs` 现在组织为：

| 模块 | 行数 | 职责 |
|------|------|------|
| `loop_/context.rs` | 81 | 上下文构建 |
| `loop_/history.rs` | 106 | 历史压缩与管理 |
| `loop_/execution.rs` | 166 | 工具执行与派发 |
| `loop_/parsing.rs` | ~1,540 | 所有工具调用解析 |
| `loop_.rs`（核心） | ~3,970 | 主循环逻辑 |

**在上游结构之上，elfClaw 特有的添加：**
- `DEFAULT_MAX_TOOL_ITERATIONS = 10`（上游为 20）——更紧密的循环控制，减少失控工具链
- CJK 延迟动作检测——识别中日韩文本中的延迟意图表达
- URL 安全策略：纯 URL 绝不被静默转换为 `curl` shell 命令（上游曾有自动转换行为，我们作为安全修复将其移除）

---

## 📦 已集成的上游功能

| 功能 | 状态 |
|------|------|
| 审批系统 | ✅ 已集成 |
| 研究阶段 | ✅ 已集成 |
| OTP + RBAC | ✅ 已集成 |
| 紧急停止 | ✅ 已集成 |
| 查询分类器 | ✅ 已集成 |
| apply_patch 工具 | ✅ 已集成 |
| 子 Agent 协调 | ✅ 已集成 |
| 目标引擎 | ✅ 已集成 |
| MCP 客户端套件 | ✅ 已集成 |
| Agent IPC 工具 | ✅ 已集成 |
| OpenAI/OpenClaw 兼容网关 | ✅ 已集成 |
| WebSocket 网关 | ✅ 已集成 |
| 插件系统 | ✅ 已集成 |
| 技能系统 + SkillForge | ✅ 已集成 |
| IRC 渠道 | ✅ 已集成 |
| Android 客户端 + FFI | ✅ 已集成 |
| Web 前端 | ✅ 已集成 |
| process / url_validation / task_plan / subagent 工具 | ✅ 已集成 |

---

## 🗺 路线图与上游同步

### 计划向上游提交的功能

- 聊天历史 + 索引模块
- 对话自动总结
- TTS + 语音发送工具
- Email 监控 → 跨渠道通知
- 活跃时间段配置
- 统一渠道投递路径（`deliver_to_channel()`）

### 未来工作

- [ ] `channels/mod.rs` 拆分（10,645 行 → 4 个模块）
- [ ] `config/schema.rs` derive 宏改造
- [ ] 混合内存（SQLite + Qdrant）
- [ ] 内存卫生与自动归档
- [ ] Perplexity 事实核验

---

## 🙏 致谢

**没有 ZeroClaw，就没有 elfClaw。**

深深感谢 [ZeroClaw Labs](https://github.com/zeroclaw-labs/zeroclaw) 团队——[theonlyhennygod](https://github.com/theonlyhennygod) 及全体贡献者——从零开始构建了这个极其强大的 Rust Agent 运行时。本文档中的架构批评是以诚实工程的精神提出的，而非贬低。ZeroClaw 提供的基础——trait 架构、Provider 抽象、工具系统、安全模型——是出色的，elfClaw 直接建立在其上。

同样感谢 **[OpenClaw](https://github.com/openclaw) 团队**，感谢他们对本地优先个人 AI 助手的愿景，以及对我们设计理念的启发。

---

## 🚀 快速开始

```bash
git clone https://github.com/VK7KSM/eflClaw.git
cd eflClaw
cargo build --release
./target/release/zeroclaw setup
./target/release/zeroclaw daemon
```

> 可执行文件名为 `zeroclaw`——内部模块和命令名称与上游保持一致，保证完全兼容性。

---

## 许可证

MIT OR Apache-2.0 · elfClaw 是 ZeroClaw 的分叉项目，原始版权归属 ZeroClaw Labs 贡献者。
