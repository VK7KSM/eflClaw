<p align="center">
  <img src="zeroclaw.png" alt="elfClaw" width="200" />
</p>

<h1 align="center">elfClaw 🦀</h1>

<p align="center">
  <strong>A bold fork of ZeroClaw — rebuilt for real-world use.</strong><br>
  Persistent memory. Proactive voice. Modular architecture. Production-ready AI agent runtime in pure Rust.
</p>

<p align="center">
  <a href="README.zh-CN.md">中文文档</a> ·
  <a href="https://github.com/VK7KSM/eflClaw">GitHub</a> ·
  <a href="#-new-features">New Features</a> ·
  <a href="#-why-we-forked">Why We Forked</a> ·
  <a href="#-roadmap--upstream-sync">Roadmap</a>
</p>

---

## Table of Contents

- [What is elfClaw?](#-what-is-elfclaw)
- [Why We Forked — The Architecture Problem](#-why-we-forked--the-architecture-problem)
- [New Features](#-new-features)
  - [Chat History & Memory](#1-chat-history--memory)
  - [Full-Text Search](#2-full-text-search-over-conversation-history)
  - [Text-to-Speech & Voice Messages](#3-text-to-speech--voice-messages)
  - [Proactive Notifications](#4-proactive-notifications-telegram--email)
  - [Email Monitor → Telegram](#5-email-monitor--telegram-digest)
  - [Configurable Active Hours](#6-configurable-active-hours)
  - [Unified Channel Delivery](#7-unified-channel-delivery)
  - [Agent Loop Modularization](#8-agent-loop-modularization)
- [Upstream Features We've Integrated](#-upstream-features-weve-integrated)
- [Roadmap & Upstream Sync](#-roadmap--upstream-sync)
- [Acknowledgements](#-acknowledgements)
- [Quick Start](#-quick-start)

---

## 🦅 What is elfClaw?

**elfClaw** is a fork of [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw), a Rust-native autonomous AI agent runtime developed by the ZeroClaw Labs community. We forked it because we needed features that the upstream project hasn't prioritized — most critically, **persistent conversation history**, **proactive notifications**, and a **sustainable codebase architecture** that won't collapse under its own weight.

elfClaw ships everything ZeroClaw has — multi-provider LLM support, multi-channel messaging (Telegram, Discord, Slack, Matrix, Email, IRC, WhatsApp, and more), tool execution, heartbeat scheduling, hardware peripherals, MCP client, sub-agent coordination — and adds a layer of polish that makes it actually usable as a daily driver.

We track ZeroClaw upstream continuously and merge valuable updates. When our secondary-development features are mature, we plan to submit them back as PRs.

---

## 🏗 Why We Forked — The Architecture Problem

ZeroClaw is technically impressive. It compiles to a sub-5MB binary, starts in under 10ms, and covers an extraordinary breadth of integrations. We respect the team's work deeply.

But honest engineering requires honest assessment. As of early 2026, ZeroClaw's codebase has a **structural debt problem** that makes contribution and maintenance increasingly painful:

### The God Module Problem

| File | Lines (upstream) | Problem |
|------|-----------------|---------|
| `src/config/schema.rs` | **11,061** | Monolithic config blob. Hundreds of structs with no derive-macro simplification. Every `Default` impl hand-written. Adding a single config key requires changes in 3+ places. |
| `src/channels/mod.rs` | **10,645** | Classic God Module. Message handling, history management, system prompt construction, channel initialization, and tool dispatch all crammed into a single file. |
| `src/onboard/wizard.rs` | **8,061** | Interactive setup, quick setup, provider model listing, and scaffolding all mixed together with no separation of concerns. |
| `src/agent/loop_.rs` | **5,153** | The entire agent execution loop — context building, history compaction, tool parsing, parallel dispatch — in one file. |

> **Total:** Four files account for **~34,920 lines**, nearly 20% of the entire codebase.

### The Consequence: Features That "Exist" But Don't Work

ZeroClaw's breadth is deceptive. Many features are **implemented at the skeleton level** — they compile, they're documented, but they don't work for real use:

- **No persistent chat history.** Every conversation starts from zero. The agent has no memory of what you said yesterday.
- **No automatic conversation summarization.** Long sessions silently lose context as the history window fills up.
- **No proactive notifications.** The agent can receive messages but cannot reach out on its own volition.
- **Email monitor is passive.** It fetches emails but has no way to notify you via another channel (e.g., Telegram).
- **Active hours are hardcoded.** The heartbeat scheduler wakes at fixed compile-time hours with no user configuration.
- **Channel delivery is hardcoded.** Cron jobs and heartbeat announcements only support 4 channels via a hardcoded `match` block, bypassing the `Channel` trait entirely.

The project's own analysis (pre-fork) summarized it well:

> *"功能齐全但架构需要重构，不算屎山但是已经在屎山的路上了——再不拆文件就变成了。"*
> — internal code review note
>
> *(Feature-complete but architecture needs refactoring. Not a garbage fire yet, but heading there — one more big file and it will be.)*

### What elfClaw Does Differently

elfClaw applies **surgical fixes** — we don't rewrite for the sake of rewriting. We fix the parts that actively block feature development:

- Channel delivery unified via `deliver_to_channel()` — our own fix to an architectural debt we inherited from the fork base: hardcoded 4-channel `match` blocks in `daemon/mod.rs` and `cron/scheduler.rs` that bypassed the `Channel` trait entirely
- Agent loop submodule structure integrated from upstream's newer commits, with elfClaw-specific customizations layered on top
- New features added as **focused, testable modules** (200–450 lines each), each with unit tests covering happy path, error path, and edge cases

---

## ✨ New Features

### 1. Chat History & Memory

**The single biggest missing feature in ZeroClaw.**

elfClaw automatically persists every conversation to disk and indexes it in SQLite FTS5 for full-text search. The agent never loses context between sessions.

**Files:** `src/channels/chat_log.rs` (449 lines), `src/channels/chat_index.rs` (439 lines)

```toml
# config.toml — no configuration needed, works out of the box
# Logs stored at: ~/.zeroclaw/chat_log/<channel>/<user>/<YYYY-MM-DD>.json
# Index stored at: ~/.zeroclaw/chat_index.db
```

**What it does:**
- Persists all messages (user + agent) as structured JSON with timestamps and metadata
- Maintains a SQLite FTS5 index for instant full-text search
- Automatically restores recent conversation history when a session resumes
- Owner accounts get cross-user summary injection (see conversation patterns across all users)
- 16 unit tests covering persistence, indexing, recovery, and access control

**vs ZeroClaw upstream:** Not present. Zero chat persistence. Every session is stateless.

---

### 2. Full-Text Search over Conversation History

The agent can search its own conversation history as a tool.

**File:** `src/tools/search_chat_log.rs` (245 lines)

**Usage — ask the agent directly:**
```
"Search my conversation history for mentions of the Raspberry Pi project"
"What did we discuss about the API keys last week?"
"Find all conversations where I asked about deployment"
```

**Tool parameters:**
```json
{
  "query": "raspberry pi GPIO setup",
  "channel": "telegram",          // optional: filter by channel
  "user_id": "495916105",         // optional: filter by user
  "limit": 20                     // optional: max results (default 20)
}
```

**Access control:** Only `owner`-role users can search across all users. Regular users can only search their own history.

**vs ZeroClaw upstream:** Not present.

---

### 3. Text-to-Speech & Voice Messages

elfClaw can synthesize speech and send voice messages — **no API key required**.

**Files:** `src/channels/tts.rs` (195 lines), `src/tools/send_voice.rs` (223 lines)

Uses the **Microsoft Edge Read Aloud API** (free, no authentication). The agent decides when to send voice vs text based on context.

**Usage — ask the agent to use voice:**
```
"Read me the summary as a voice message"
"Send me a voice note with today's briefing"
```

**Config (optional):**
```toml
[tts]
voice = "zh-CN-XiaoxiaoNeural"   # default voice
rate = "+0%"                      # speaking rate
pitch = "+0Hz"                    # pitch adjustment
```

**Supported voices:** All Microsoft Edge neural voices (100+ languages). Notable voices:
- `zh-CN-XiaoxiaoNeural` — Chinese Mandarin (female)
- `en-US-AriaNeural` — English US (female)
- `en-GB-SoniaNeural` — English UK (female)
- `ja-JP-NanamiNeural` — Japanese (female)

**vs ZeroClaw upstream:** Not present.

---

### 4. Proactive Notifications — Telegram & Email

Two new tools let the agent reach out without waiting for user input.

**Files:** `src/tools/send_telegram.rs` (214 lines), `src/tools/send_email.rs` (239 lines)

**`send_telegram` tool:**
```json
{
  "chat_id": "495916105",
  "message": "Your scheduled task completed successfully. 3 items processed."
}
```

**`send_email` tool:**
```json
{
  "to": "user@example.com",
  "subject": "elfClaw Daily Digest",
  "body": "Here's your summary for March 1st..."
}
```

**Security controls on `send_email`:**
- Cannot send to external addresses not in allowlist (configurable)
- Automatically excluded from email digest processing loops (prevents reply storms)
- Rate-limited to prevent abuse

**vs ZeroClaw upstream:** Not present.

---

### 5. Email Monitor → Telegram Digest

elfClaw monitors your inbox and intelligently digests incoming emails to your Telegram.

**How it works:**
1. IMAP IDLE watches for new emails
2. Agent analyzes and classifies (important / newsletter / junk)
3. Agent sends a formatted summary to your Telegram
4. **You decide** what to do — ignore, reply, forward
5. Agent **never sends email autonomously** unless you explicitly ask

**Config:**
```toml
[channels.email]
mode = "monitor"
imap_host = "imap.gmail.com"
imap_port = 993
username = "you@gmail.com"
password = "your-app-password"
from_address = "you@gmail.com"
notify_channel = "telegram"
notify_to = "YOUR_TELEGRAM_CHAT_ID"
```

**vs ZeroClaw upstream:** The email monitor exists upstream but has no cross-channel notification capability — it fetches emails and does nothing useful with them.

---

### 6. Configurable Active Hours

elfClaw's heartbeat scheduler respects your time zone and schedule.

**Config:**
```toml
[heartbeat]
active_hours_start = "08:00"   # don't wake before 8am
active_hours_end = "23:00"     # sleep after 11pm
timezone = "Asia/Shanghai"     # your local timezone
```

Supports **cross-midnight ranges** (e.g., `22:00`–`06:00` for night-shift workers).

**vs ZeroClaw upstream:** Hardcoded to specific compile-time values. No user configuration.

---

### 7. Unified Channel Delivery

All announcement paths (heartbeat, cron jobs, scheduled tasks) now route through the `Channel` trait via a single `deliver_to_channel()` function.

**What this means for users:** Heartbeat announcements and cron job notifications work on **all configured channels** — not just Telegram, Discord, Slack, and Mattermost.

**What this means for developers:** Adding a new channel automatically makes it available for all delivery paths. No more editing multiple hardcoded match blocks.

**Background:** The fork base we started from had `daemon/mod.rs` and `cron/scheduler.rs` each containing their own hardcoded `match channel_name { "telegram" => ..., "discord" => ..., "slack" => ..., "mattermost" => ... }` blocks that bypassed the `Channel` trait entirely. This was our own architectural debt to fix, not something to blame on upstream.

---

### 8. Agent Loop Submodule Structure

elfClaw integrates the submodule architecture that ZeroClaw upstream introduced in their newer commits, adapted to our codebase and extended with elfClaw-specific logic.

The 5,810-line monolithic `src/agent/loop_.rs` is now organized as:

| Module | Lines | Responsibility |
|--------|-------|---------------|
| `loop_/context.rs` | 81 | Context building from memory and hardware RAG |
| `loop_/history.rs` | 106 | History compaction, trimming, auto-compact |
| `loop_/execution.rs` | 166 | Tool execution (parallel + sequential), cancellation |
| `loop_/parsing.rs` | ~1,540 | All tool call parsing (XML, JSON, GLM, MiniMax, Perl-style) |
| `loop_.rs` (core) | ~3,970 | Main loop logic, deferred action detection |

**elfClaw-specific additions on top of the upstream structure:**
- `DEFAULT_MAX_TOOL_ITERATIONS = 10` (upstream: 20) — tighter loop control to reduce runaway tool chains
- CJK deferred action detection — recognizes deferred intent in Chinese/Japanese/Korean text
- URL-safe policy: plain URLs are **never** silently converted to `curl` shell commands (upstream had an auto-convert behavior that we removed as a security fix)

---

## 📦 Upstream Features We've Integrated

elfClaw tracks ZeroClaw upstream continuously. The following upstream features have been integrated and tested:

| Feature | Status | Notes |
|---------|--------|-------|
| Approval system (`src/approval/`) | ✅ Integrated | Interactive tool authorization before execution |
| Research phase (`src/agent/research.rs`) | ✅ Integrated | Pre-answer search phase for factual queries |
| OTP + RBAC (`src/security/`) | ✅ Integrated | TOTP authentication + role-based access control |
| Emergency stop (`src/security/estop.rs`) | ✅ Integrated | One-command halt of all agent activity |
| Query classifier | ✅ Integrated | Auto-route queries to appropriate model tier |
| `apply_patch` tool | ✅ Integrated | Agent self-repair via unified diff |
| Sub-agent coordination (`src/coordination/`) | ✅ Integrated | Message bus + worker communication |
| Goal engine (`src/goals/`) | ✅ Integrated | Long-horizon task planning and tracking |
| MCP client suite (4 files) | ✅ Integrated | Model Context Protocol support |
| Agents IPC tools | ✅ Integrated | SQLite-based cross-process agent communication |
| OpenAI-compatible gateway | ✅ Integrated | `/v1/chat/completions` compatibility layer |
| OpenClaw-compatible gateway | ✅ Integrated | `/api/chat` endpoint for OpenClaw migration |
| WebSocket gateway | ✅ Integrated | Full-duplex streaming connection |
| Plugin system (`src/plugins/`) | ✅ Integrated | External plugin discovery and loading |
| Skills system + SkillForge | ✅ Integrated | Skill templates and forge marketplace |
| IRC channel | ✅ Integrated | IRC protocol with TLS + SASL |
| Android client + FFI bridge | ✅ Integrated | Kotlin/Jetpack Compose + UniFFI bindings |
| Web frontend | ✅ Integrated | React + Vite documentation navigator |
| `process` tool | ✅ Integrated | Background process management |
| `url_validation` tool | ✅ Integrated | URL safety checks with CIDR blocking |
| `task_plan` tool | ✅ Integrated | Session-scoped task list for the agent |
| `subagent_*` tools (4 tools) | ✅ Integrated | Dynamic sub-agent spawning and management |

**Skipped (with reasons):**
- SOP system — not needed for our use case
- WASM tool sandbox — requires `RuntimeAdapter::as_any()` trait change, deferred
- Quota adapter — depends on non-existent `quota_types` crate, deferred

---

## 🗺 Roadmap & Upstream Sync

### Our Commitment to Upstream Compatibility

We track ZeroClaw upstream on branch `dev/upstream-fixes` and merge valuable changes continuously. We believe in giving back to the community that made this project possible.

**Planned upstream contributions:**
- Chat history + indexing module (chat_log.rs, chat_index.rs, chat_index_tool.rs)
- Conversation summarizer (chat_summarizer.rs)
- TTS integration and voice sending tools
- Email monitor → cross-channel notification bridge
- Active hours configuration
- Unified channel delivery path (`deliver_to_channel()`)

We will submit these as PRs once the implementation is stable and well-tested.

### What We Won't Merge Back

Features that depend on our specific deployment configuration (personal Telegram chat IDs, email addresses) will be generalized before submission.

### Future Work

- [ ] Architecture refactoring: split `channels/mod.rs` into `message_handler.rs`, `history.rs`, `prompt_builder.rs`, `channel_init.rs`
- [ ] Architecture refactoring: split `config/schema.rs` with derive macros and unified `Config::default()`
- [ ] Hybrid memory: SQLite + Qdrant vector search
- [ ] Memory hygiene: automatic archival of old conversations
- [ ] Cron consolidation: merge overlapping cron jobs
- [ ] Perplexity fact-checking integration
- [ ] Syscall anomaly detection

---

## 🙏 Acknowledgements

**elfClaw would not exist without ZeroClaw.**

Deep thanks to the [ZeroClaw Labs](https://github.com/zeroclaw-labs/zeroclaw) team — [theonlyhennygod](https://github.com/theonlyhennygod) and contributors — for building an extraordinarily capable Rust agent runtime from scratch. The architectural criticism in this document is offered in the spirit of honest engineering, not dismissal. The foundation ZeroClaw provides — the trait architecture, the provider abstraction, the tool system, the security model — is excellent, and elfClaw builds directly on it.

Thanks also to the **[OpenClaw](https://github.com/openclaw) team** for their vision of a local-first personal AI assistant and for inspiring parts of our design philosophy. The concept of treating the AI agent as a persistent local runtime — not a stateless API call — is something both projects share.

---

## 🚀 Quick Start

### Prerequisites

- Rust 1.87+
- An API key for at least one supported provider (OpenAI, Anthropic, Gemini, etc.)

### Install

```bash
git clone https://github.com/VK7KSM/eflClaw.git
cd eflClaw
cargo build --release
./target/release/zeroclaw setup
```

> The binary is named `zeroclaw` — this is intentional. Internal module and command names remain unchanged from upstream for full compatibility.

### Configure

```bash
# Run the interactive setup wizard
./target/release/zeroclaw setup

# Or manually edit the config
nano ~/.zeroclaw/config.toml
```

### Run

```bash
# Interactive CLI chat
./target/release/zeroclaw chat

# Daemon mode (Telegram, Discord, etc.)
./target/release/zeroclaw daemon

# With specific config
./target/release/zeroclaw --config /path/to/config.toml daemon
```

### Docker

```bash
docker compose up -d
```

---

## License

MIT OR Apache-2.0

elfClaw is a fork of ZeroClaw. Original copyright belongs to ZeroClaw Labs contributors.
