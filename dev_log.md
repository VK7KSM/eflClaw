# ZeroClaw 开发日志

---

## 2026-02-28 — Worker 迭代溢出丢失结果修复 + Gemini 时间幻觉修复

**涉及文件**：`src/agent/loop_.rs`、`src/config/schema.rs`、`src/tools/get_time.rs`（新建）、`src/tools/mod.rs`、`src/channels/mod.rs`、`src/cron/scheduler.rs`、`src/daemon/mod.rs`、`src/main.rs`

### 问题链

1. Cron job 报 `Agent exceeded maximum tool iterations (50)` 后**所有已完成的工作丢失**
2. `homework/` 目录下没有预期的新闻文件
3. 每次 cron job 重新抓取相同内容 → 推送重复新闻
4. Gemini 不知道准确时间

根因：`run_tool_call_loop` 达到 `max_iterations` 后 `bail!` 返回错误，丢弃所有已完成的工具调用结果（file_write、send_telegram 等副作用已执行但被视为失败），主 agent 重试消耗更多迭代。

### 改动 1：run_tool_call_loop 迭代用完时优雅降级（核心修复）

**文件**：`src/agent/loop_.rs`

- 循环外新增 `last_response_text` 变量，每轮跟踪最后非空 LLM 回复
- 循环耗尽时：返回 `Ok(last_response_text + 截断提示)` 而非 `bail!`
- 保留 `tool_loop_exhausted` 追踪事件
- 效果：Worker 已执行的副作用（文件写入、消息发送）不会被视为失败

### 改动 2：Cron/Heartbeat 独立迭代上限

**文件**：`src/config/schema.rs`、`src/agent/loop_.rs`、`src/cron/scheduler.rs`、`src/daemon/mod.rs`、`src/main.rs`

- `SchedulerConfig` 新增 `max_tool_iterations: usize`（默认 25）
- `HeartbeatConfig` 新增 `max_tool_iterations: usize`（默认 25）
- `agent::run()` 签名新增 `max_tool_iterations_override: Option<usize>`
- Cron 调用传 `Some(config.scheduler.max_tool_iterations)`
- Heartbeat 调用传 `Some(config.heartbeat.max_tool_iterations)`
- CLI/main 调用传 `None`（使用 `config.agent.max_tool_iterations`）

### 改动 3：get_current_time 工具

**文件**：`src/tools/get_time.rs`（新建）、`src/tools/mod.rs`

- `GetCurrentTimeTool`：返回精确系统时间、日期、时区、Unix 时间戳、星期、主机名、操作系统
- 无条件注册（所有模型都需要）
- 含 2 个单元测试

### 改动 4：动态时间注入到 channel system prompt

**文件**：`src/channels/mod.rs`

- `build_channel_system_prompt()` 末尾追加 `## Current Date & Time` 段
- 每次构建 system prompt 时刷新时间，覆盖 daemon 启动时的过时时间
- Gemini 直接从 system prompt 读取，或调用 `get_current_time` 工具

### 验证

- `cargo build --release` — 零错误零警告 ✓

---

## 2026-02-28 — 上下文管理改进（启动恢复限制 + 摘要注入 + 溢出自动重试 + Gemini 降级修复）

**涉及文件**：`src/channels/mod.rs`、`src/providers/gemini.rs`

### 改动 1a：启动恢复消息数量限制

**文件**：`src/channels/mod.rs` ~行 3390

- `load_all_today_messages()` → `load_recent_messages(STARTUP_RESTORE_RECENT_TURNS=8)`
- 原来启动时加载全部当日消息（可能 100+ 条），远超 `MAX_CHANNEL_HISTORY=50` 限制
- 现在只加载最近 8 条（约 4 轮对话），保证即时连续性而不污染上下文
- 按用户遍历 `list_chat_users()`，每个用户独立加载

### 改动 1b：当前用户历史摘要注入

**文件**：`src/channels/mod.rs` ~行 1684 后

- 在跨用户摘要注入代码后，新增当前用户的 7 天历史摘要注入到 system prompt
- 复用已有的 `chat_index.get_user_summaries()` 方法
- 格式：`- {日期} ({消息数}条消息): {摘要} [话题: {话题}]`
- 所有用户均可看到自己的摘要（不限 owner）
- 提示用户可用 `search_chat_log` 工具搜索更多细节

### 改动 2：上下文溢出自动恢复

**文件**：`src/channels/mod.rs` ~行 2069

- 原来：检测到 context overflow → compact → 告诉用户"请重新发送"
- 现在：检测到 context overflow → 自动重试（最多 2 次）
  - 第 1 次：`compact_sender_history()` 保留近期消息后重试
  - 第 2 次：`clear_sender_history()` 清空历史 + 仅保留当前消息后重试
  - 重试时 system prompt 仍包含摘要，agent 不会完全失忆
- 重试前向用户发送 "⚠️ 上下文过载，正在压缩后重试..." 通知
- 两次重试均失败：清空历史 + 告知用户重新发送
- 保存 `system_prompt_for_retry` 供重试时重建 history 使用

### 改动 3：Gemini 降级文本格式修复

**文件**：`src/providers/gemini.rs` ~行 1433

- 原来：旧历史中每个 tool call 生成一行 `[Used tool: xxx]`（N 行重复刷屏）
- 现在：整个降级块只生成一行文本
  - 有 `content` 文本：使用原始 content
  - 无 `content`：使用 `"(Continued from previous tool interaction)"`
- 大幅减少降级历史占用的 context tokens

### 改动 4：`/new` 命令提示更新

**文件**：`src/channels/mod.rs` ~行 1065

- 原来：`"Conversation history cleared. Starting fresh."`
- 现在：`"对话历史已清空。系统摘要保留，我仍记得近期对话概况。发消息开始新对话。"`
- 让用户知道 `/new` 后 agent 不会完全失忆（system prompt 中仍有摘要）

### 验证

- `cargo build --release` — 零错误零警告 ✓

---

## 2026-02-28 — 上游同步分析（Comparison.md）

**涉及文件**：`Comparison.md`（新增），`C:\Dev\zeroclaw_original`（克隆上游）

**操作**：
- 克隆上游 `https://github.com/zeroclaw-labs/zeroclaw.git` 到 `C:\Dev\zeroclaw_original`
- 分叉点：`d352449`（v0.1.7 release），上游 HEAD：`1a0bb175`
- 上游自分叉点新增 **452 commits**，变更 **692 个文件**

**分析要点**：
- `src/agent/loop_.rs` 上游已重构为子模块目录（`loop_/context.rs` 等），我们仍是单文件，**高风险冲突**
- `src/config/schema.rs`、`src/tools/mod.rs`、`src/channels/mod.rs` 双方均有大量改动，**高风险冲突**
- 上游新增了 Plugin 系统、Goals 引擎、MCP 服务器、Sub-Agent 协调、WASM Skill、SOP 系统、Android 客户端等大型功能
- 我们独有功能（chat_log、chat_index、TTS、send_telegram/email/voice 等）上游均不存在，合并时需保留
- 建议采用专题 cherry-pick 而非整体 rebase，详见 `Comparison.md`

---

## 2026-02-28 — 第二轮 Bug 修复：处理已有脏历史数据（Anthropic 空文本块 + Gemini thought_signature）

**涉及文件**：`src/agent/loop_.rs`、`src/providers/anthropic.rs`、`src/providers/gemini.rs`

**问题**：上一轮 Bug 修复（A/B/C）只处理了"新产生的数据流"，但 **已有脏历史数据** 仍导致两个 400 错误：
1. **Anthropic**：空 assistant 消息（思考模型 thinking-only 响应）在 `convert_messages()` 的 assistant/tool 分支产生空文本块
2. **Gemini**：修复前保存的旧历史中 tool_calls JSON 没有 `thought_signature` 字段，Gemini 思考模型拒绝

### Fix 1：源头防御 — `src/agent/loop_.rs:~2398`
- 在 `history.push(ChatMessage::assistant(...))` 前检查 `response_text.trim().is_empty()`
- 空则用 `"(thinking)"` 占位文本替代，避免空 assistant 消息持久化到历史
- **效果**：从源头阻断新的空 assistant 消息产生

### Fix 2：Anthropic — `src/providers/anthropic.rs` convert_messages()
- **assistant 分支**：当 `parse_assistant_tool_call_message()` 返回 None 且内容为空时，用 `"(thinking)"` 占位文本替代
- **tool 分支**：当 `parse_tool_result_message()` 返回 None 且内容为空时，用 `"(empty tool result)"` 占位文本替代
- **关键决策**：不用 `continue` 跳过！跳过会打破角色交替，导致 "roles must alternate" 400 错误
- **效果**：已有脏历史中的空 assistant/tool 消息不再触发 "text content blocks must be non-empty"

### Fix 3：Gemini — `src/providers/gemini.rs` chat() 历史重建
- **assistant 分支**：检测 tool_calls 是否全部有 `thought_signature`
  - 无签名（旧历史）：整个 assistant 消息降级为纯文本摘要（`[Used tool: xxx]`），对应 tool call ID 标记为 `__degraded__`
  - 有签名（新历史）：走原有正常路径
- **tool 分支**：跳过 `tool_name == "__degraded__"` 的 tool 结果（Gemini 支持连续同 role Contents，不会破坏角色交替）
- **效果**：旧历史不再触发 "thought_signature missing"；新对话正常使用 functionCall

### 与上一轮修复的差异
| 上次 | 本次 |
|------|------|
| 只修了 user 消息分支 | assistant + tool 分支全部处理 |
| 用 `continue` 跳过空消息 | 改用占位文本保持角色交替 |
| 只考虑新数据流 | 同时处理旧历史脏数据 |
| Gemini 旧历史未考虑 | 降级为文本摘要 |

**验证**：`cargo build --release` 零错误零警告 ✓

---

## 2026-02-28 — Gemini Vision 支持 + 历史图片 Marker 中毒修复 + 友好降级

**文件**：`src/multimodal.rs`、`src/agent/loop_.rs`、`src/providers/gemini.rs`

**问题**：
1. Gemini `vision: false` 硬编码，且 `Part` 无 `inlineData` 字段，含图消息报 capability error
2. 重启后发纯文字也报 vision 错误（历史 `[IMAGE:]` marker 被全量扫描）
3. 非 vision 模型收到图片时原始错误直接发给用户，用户体验差

**修改**：

1. **`src/multimodal.rs`**
   - 新增 `strip_history_image_markers()`：只保留最后一条 user 消息的图片，历史图片全部清除（避免每轮重传）
   - 新增 `strip_all_image_markers_with_note()`：清除全部图片，当前消息含图时追加中文友好提示，供非 vision 模型优雅降级

2. **`src/agent/loop_.rs`**
   - 删除 `ProviderCapabilityError` vision 错误抛出（及无用 import）
   - 改为条件 strip 策略：vision 模型 → `strip_history_image_markers`；非 vision 模型 → `strip_all_image_markers_with_note`
   - 非 vision 模型遇图时仅打终端 `WARN` 日志，不对用户暴露技术错误

3. **`src/providers/gemini.rs`**
   - 新增 `InlineData` 结构体（`mimeType` + `data`）
   - `Part` 新增 `inline_data: Option<InlineData>` 字段（`#[serde(rename = "inlineData")]`）
   - `Part::text()` 构造函数加入 `inline_data: None`
   - 新增 `GeminiProvider::parse_data_uri()` 辅助方法（解析 `data:<mime>;base64,<data>`）
   - `chat()` 方法 `"user"` 分支：调用 `parse_image_markers` 提取图片，转为 `inlineData` Part
   - `capabilities()` 改为 `vision: true`
   - 修复测试代码中不完整的 `Part { text: ... }` 字面量，全部改为 `Part::text(...)` 构造函数

**效果**：
- Gemini + 含图消息 → Agent 正常识别图片内容
- 非 vision 模型 + 含图消息 → Agent 自然语言回复"无法识别图片"，无报错
- 重启后发文字（历史含图 marker）→ 不报错正常回复

**验证**：`cargo build --release` 零错误零警告 ✓

---

## 2026-02-28 — 重试 Backoff 上限可配置化（Gemini 503 修复）

**文件**：`src/config/schema.rs`、`src/providers/reliable.rs`、`src/providers/mod.rs`

**问题**：Gemini 503 "high demand" 恢复需要数分钟，但 backoff 上限硬编码为 10s，三次重试
（总耗时约 1.5s）全部命中同一容量瓶颈期后放弃。用户手动等几分钟重试才成功，
是因为瓶颈已过，而非 agent 有任何容错逻辑在生效。

**修改**：

1. **`src/config/schema.rs`**
   - `ReliabilityConfig` 新增 `provider_max_backoff_ms` 字段（默认 60_000ms）。
   - `default_provider_retries()` 从 2 → **5**（共 6 次尝试）。
   - `default_provider_backoff_ms()` 从 500ms → **1000ms**。
   - `Default::default()` 加入 `provider_max_backoff_ms` 初始化。

2. **`src/providers/reliable.rs`**
   - `ReliableProvider` 结构体新增 `max_backoff_ms: u64` 字段。
   - `new()` 签名新增 `max_backoff_ms: u64` 参数；构造时保证 `>= base_backoff_ms`。
   - 4 处 `(backoff_ms.saturating_mul(2)).min(10_000)` 全部替换为 `.min(self.max_backoff_ms)`。

3. **`src/providers/mod.rs`**
   - 工厂调用 `ReliableProvider::new()` 新增第四参数 `reliability.provider_max_backoff_ms`。

**效果（默认配置 retries=5, base=1s, max=60s）**：
等待序列：1s → 2s → 4s → 8s → 16s → 共 ~31 秒重试窗口。

**推荐用户配置**（`资料/config.toml`）：
```toml
[reliability]
provider_retries = 10
provider_backoff_ms = 2000
provider_max_backoff_ms = 120000
```
→ 11 次尝试，总窗口约 10 分钟。

**验证**：`cargo build --release` 零错误零警告 ✓

---

## 2026-02-28 — Gemini API Key 安全修复（key 泄漏 → Telegram）

**文件**：`src/providers/gemini.rs`

**问题**：API key 嵌在 URL Query String（`?key=PLAINTEXT`），reqwest 网络错误时完整 URL 出现在错误消息中，agent loop 将其原样转发给用户，导致 key 明文出现在 Telegram 聊天记录里。

**修改**：

1. `build_generate_content_url()`（第 864-868 行）：移除 `?key=` 拼接，URL 不再携带 key。
2. `build_generate_content_request()`（第 988-993 行）：`_ =>` 分支改为通过 `x-goog-api-key` header 传递 key（Google 官方支持的方式）。
3. `warmup()`（第 1519-1528 行）：models endpoint 同样改为通过 header 传 key，不再拼进 URL。
4. 枚举注释（第 42-46 行）：更新为 `x-goog-api-key header`，与实现一致。

**验证**：`cargo build` 零错误零警告。

**用户须知**：前次泄漏的 key 需在 Google AI Studio 手动撤销并换新 key。

---

## 2026-02-28 — 三个 Bug 修复：Gemini thought_signature + Anthropic 空内容块 + Telegram 空消息

**涉及文件**：`src/providers/traits.rs`、`src/providers/gemini.rs`、`src/providers/anthropic.rs`、`src/providers/ollama.rs`、`src/providers/openai.rs`、`src/providers/openrouter.rs`、`src/providers/bedrock.rs`、`src/providers/compatible.rs`、`src/providers/copilot.rs`、`src/providers/reliable.rs`（测试）、`src/agent/loop_.rs`、`src/multimodal.rs`、`src/channels/mod.rs`

### Bug A：Gemini 思考模型多轮工具调用 400（thought_signature 丢失）

**根本原因**：Gemini 思考模型响应包含 `thought_signature` 字段，但 `extract_tool_calls()` 未捕获，`build_native_assistant_history()` 也未序列化，导致第二轮 `chat()` 历史重建时缺少 thought Part，Gemini API 返回 400。

**修改**：
1. `src/providers/traits.rs` — `ToolCall` 新增 `thought_signature: Option<String>` 字段（`serde default + skip_if_none`）
2. `src/providers/gemini.rs`：
   - 请求侧 `Part` 结构体新增 `thought: Option<bool>` 和 `thought_signature: Option<String>` 字段
   - `Part::text()` 构造函数加入两个新字段的 `None` 初始化
   - 3 处 `Part { ... }` 字面量（inlineData、functionCall、functionResponse）补加两个新字段
   - `extract_tool_calls()` 新增 `pending_thought_sig` 变量，遍历时从 thought Part 捕获签名，`function_call` Part 出现时取走并存入 `ToolCall`
   - `chat()` 历史重建循环：遍历 `tool_calls` 时，若 `thought_signature` 存在，先插入 `thought: Some(true)` Part 再插入 functionCall Part
3. `src/agent/loop_.rs` — `build_native_assistant_history()` 序列化时若 `tc.thought_signature` 存在则写入 JSON
4. 全部 9 个其他 provider 的 `ProviderToolCall` / `ToolCall` 构造处加 `thought_signature: None`

### Bug B：Anthropic 400（空 text content block）

**根本原因**：历史中只含图片的 user 消息（如 `[IMAGE:/tmp/x.png]`），经 `strip_history_image_markers()` 或 `strip_all_image_markers_with_note()` 后 content 变为 `""`，Anthropic provider fallback 将空字符串塞入 text block，API 拒绝。

**修改**：
1. `src/multimodal.rs:strip_history_image_markers()` — 历史图片消息 strip 后若 `cleaned.trim().is_empty()` 且有 refs，改用占位符 `"（此消息包含图片）"` 而非空字符串
2. `src/multimodal.rs:strip_all_image_markers_with_note()` — 同上，非最后 user 消息补同样的占位符
3. `src/providers/anthropic.rs` — fallback 分支加非空守卫：`msg.content.trim().is_empty()` 时用占位符；blocks 仍为空则 `continue` 跳过整条消息

### Bug C：Telegram 400（空消息体）

**根本原因**：工具调用后 LLM 只返回思考内容，`run_tool_call_loop` 返回 `Ok("")`，channels 层直接将空字符串发送给 Telegram Bot API → 400。

**修改**：
1. `src/channels/mod.rs` — 发送前检查 `delivered_response.trim().is_empty()`，若空则仅打 debug 日志，跳过发送
2. `src/agent/loop_.rs:run_tool_call_loop` — 最终 `return Ok(display_text)` 之前，若 `display_text.trim().is_empty()` 则打 warn 日志

**验证**：`cargo build --release` 零错误零警告 ✓

---


### 概述

修复 Gemini API 在工具调用时返回 400 错误的问题。根本原因：`GeminiProvider` 直接将 `t.parameters`（原始 JSON Schema）传给 Gemini API，但 Gemini 不支持多项标准 JSON Schema 格式，如：
- `"type": ["string", "null"]` — 必须是单个字符串
- `"additionalProperties": false` — 不支持
- `"oneOf": [{"type":"string"},{"type":"null"}]` — 不支持

### 修改文件

#### `src/providers/gemini.rs`（唯一修改文件）

**`chat()` 方法内部（约第 1463 行）**：
- `parameters: t.parameters.clone()` → `parameters: crate::tools::SchemaCleanr::clean_for_gemini(t.parameters.clone())`

**`convert_tools()` trait override（约第 1258 行）**：
- `"parameters": t.parameters` → `"parameters": crate::tools::SchemaCleanr::clean_for_gemini(t.parameters.clone())`

### 关键细节

- `SchemaCleanr::clean_for_gemini()` 已在 `src/tools/schema.rs` 中实现，专门处理上述所有不兼容问题
- Anthropic/Claude 模型完全不受影响（各自独立代码路径）
- 编译结果：`cargo build --release` 零错误零警告 ✓

---

## 2026-02-27 — Gemini 原生函数调用（functionDeclarations API）

### 概述

修复 Gemini 作为主模型时工具完全无法调用的问题。根本原因：`GeminiProvider` 未声明 `native_tool_calling: true`，导致 agent loop 传 `request_tools = None` 给 provider；且 `Part`/`ResponsePart` 结构体不支持 `functionCall`/`functionResponse` 格式。

### 修改文件

#### `src/providers/gemini.rs`（唯一修改文件）

**新增请求侧结构体：**
- `FunctionDeclaration` — 单个函数声明（name/description/parameters）
- `GeminiTool` — 包含 `functionDeclarations` 数组
- `ToolConfig` / `FunctionCallingConfig` — mode = "AUTO"
- `RequestFunctionCall` — 发送给 Gemini 的函数调用 Part
- `FunctionResponse` — 工具结果回传 Part

**修改 `Part` struct：**
- 从 `text: String` 改为可选字段 `text/function_call/function_response`
- 新增 `Part::text()` 便利构造方法，保持所有现有调用点简洁

**修改 `ResponsePart` struct：**
- 新增 `function_call: Option<FunctionCallResponse>` 字段
- 新增 `thought_signature: Option<String>`（Gemini 2.5+/3.x，Phase 1 仅捕获，暂不回传）

**新增 `FunctionCallResponse` struct（Deserialize）**

**更新 `GenerateContentRequest`：**
- 新增 `tools: Option<Vec<GeminiTool>>` 和 `tool_config: Option<ToolConfig>` 字段

**更新 `InternalGenerateContentRequest`：**
- 新增 `tools: Option<Vec<GeminiTool>>` 字段，透传给 cloudcode-pa OAuth 路径

**更新 `build_generate_content_request`：**
- 将 `request.tools` 透传到 `InternalGenerateContentRequest.tools`

**替换 `CandidateContent::effective_text()` → `extract_tool_calls()`：**
- 同时提取文本和函数调用，返回 `(Option<String>, Vec<ToolCall>)`
- tool_call id 用 `uuid::Uuid::new_v4()` 生成（Gemini 响应不含 id）

**更新 `send_generate_content()` 签名：**
- 新增 `tools: Option<Vec<GeminiTool>>` 参数
- 返回类型改为 `(Option<String>, Vec<ToolCall>, Option<TokenUsage>)`
- 构建请求时自动添加 `tool_config` 当 tools 非空

**更新 `chat_with_system()` 和 `chat_with_history()`：**
- 使用新 `Part::text()` 替换直接构造
- 调用 `send_generate_content(..., None)` 传 tools = None

**重写 `chat()` override：**
- "assistant" role：尝试解析 native tool-call history JSON (`{"tool_calls": [...], "content": "..."}`），提取 functionCall parts，同时记录 id→name 映射
- "tool" role：解析 `{"tool_call_id": ..., "content": ...}`，查映射获取 tool_name，构建 `functionResponse` Content（role = "user"，符合 Gemini API 要求）
- 将 `request.tools` 转换为 `Vec<GeminiTool>` 传入 `send_generate_content`

**新增 `capabilities()` 实现：**
- 返回 `native_tool_calling: true`，触发 agent loop 走原生工具路径

**新增 `convert_tools()` 实现：**
- 将 `ToolSpec` slice 转换为 `ToolsPayload::Gemini { function_declarations }`

### 设计说明
- `thoughtSignature`（Gemini 2.5+/3.x）：Phase 1 仅反序列化，不回传。影响：多轮工具调用推理连续性稍差，但工具调用本身正常工作。
- `functionResponse` role = "user"：符合 Gemini API 规范。
- tool_id 从 uuid v4 生成，与 agent loop 期望格式一致。

### 编译
- `cargo build --release` — 成功，零错误零警告

---

## 2026-02-27 — CronJob `delegate_to` 字段实现

### 概述

新增 `delegate_to: Option<String>` 字段，在数据库层面绑定 cron job 与 worker sub-agent。调度器检测到该字段后，自动将 prompt 包装为显式 `delegate(...)` 调用指令，强制主 agent 执行委派。

### 修改文件

#### `src/cron/types.rs`
- `CronJob` struct 中 `model` 字段后新增 `delegate_to: Option<String>`（含注释说明用途）
- `CronJobPatch` struct 末尾新增 `delegate_to: Option<String>`（None=不修改，Some=更新）

#### `src/cron/store.rs`
- `add_agent_job()` 签名新增最后一个参数 `delegate_to: Option<String>`
- INSERT 语句添加 `delegate_to` 列（?12 参数）
- `list_jobs`、`get_job`、`due_jobs` 的 SELECT 语句添加 `delegate_to` 列（索引 17）
- `map_cron_job_row()` 添加 `delegate_to: row.get(17)?`
- `update_job()` 中添加 `if let Some(delegate_to) = patch.delegate_to { job.delegate_to = Some(delegate_to); }` 处理
- UPDATE SET 语句添加 `delegate_to = ?13`，WHERE id = ?14（原来 ?13 移到 ?14）
- `with_connection()` 末尾添加 `add_column_if_missing(&conn, "delegate_to", "TEXT")?;` 迁移

#### `src/cron/scheduler.rs`
- `run_agent_job()` 中 prompt 构建逻辑：检测 `job.delegate_to`，若有则包装为 `Use the delegate tool now: delegate(agent="...", prompt="...")`，prompt 内部做 `\` 和 `"` 转义
- 测试 `test_job()` 添加 `delegate_to: None`
- 测试中 5 处 `cron::add_agent_job(...)` 调用末尾添加 `None` 参数

#### `src/tools/cron_add.rs`
- `parameters_schema()` 添加 `delegate_to` 参数（type: string|null，附描述）
- `execute()` 中 Agent 分支解析 `delegate_to` 并传入 `cron::add_agent_job()`

#### `src/tools/cron_update.rs`
- 工具描述更新，提及 `delegate_to`
- `CronJobPatch` 已包含 `delegate_to` 字段，patch 反序列化自动支持

### 设计原则
- `delegate_to` 存的是 config 中的 agent 名称，不绑定具体模型
- Scheduler 不直接实例化 DelegateTool（因为没有 runtime/memory 依赖），而是通过包装 prompt 指令实现委派

### 编译
- `cargo build --release` — 成功，零错误零警告

---

## 2026-02-27 — Heartbeat 活跃时间可配置化

### 概述

将 heartbeat 活跃时间从硬编码 `hour >= 23 || hour < 7` 改为 config.toml 可配置的 `HH:MM` 格式，支持分钟精度和跨午夜区间。

### 修改文件

#### `src/config/schema.rs`
- `HeartbeatConfig` 新增 `active_hours_start: String`（默认 "06:30"）和 `active_hours_end: String`（默认 "23:00"）
- 新增 `parse_hhmm()` 函数：解析 "HH:MM" 为午夜起的总分钟数
- 新增 `is_within_active_hours()` 函数：判断当前时间是否在窗口内（支持跨午夜）

#### `src/config/mod.rs`
- 导出 `parse_hhmm` 和 `is_within_active_hours`

#### `src/daemon/mod.rs`
- 替换硬编码 `local_hour >= 23 || local_hour < 7` 为 `config.heartbeat.active_hours_start/end` 读取
- 日志输出包含当前时间和配置的窗口范围

### 配置改动

#### `资料/config.toml`
- `[heartbeat]` 移除 `timezone`，新增 `active_hours_start = "06:30"` 和 `active_hours_end = "23:00"`

### 编译

- `cargo build --release` — 成功

---

## 2026-02-27 — Delegate 不生效排查 + AGENTS.md 修复

### 问题

部署新 binary、config.toml、HEARTBEAT.md 后，cron job 仍用 sonnet 直接抓 RSS，没有 delegate 给 news_fetcher。

### 根因

`load_openclaw_bootstrap_files()` (channels/mod.rs:2280) 只加载 5 个文件到 system prompt：

```rust
let bootstrap_files = ["AGENTS.md", "SOUL.md", "TOOLS.md", "IDENTITY.md", "USER.md"];
```

**HEARTBEAT.md 不在列表中**。HEARTBEAT.md 仅在 `daemon/mod.rs` heartbeat worker 中作为 user message 发送。cron job 是独立 session，其 prompt 由 `cron_add` 时的 agent 自己编写——而那个 agent 的 system prompt 里没有 HEARTBEAT.md 的 delegate 指令，所以自然不知道要用 delegate。

### 修复

在 `资料/AGENTS.md` 中新增 "Worker 委派规则" section：
- 明确列出必须委派的任务类型（RSS/新闻 → news_fetcher）
- 给出 delegate 调用示例
- 禁止自己用 http_request 抓 RSS

AGENTS.md 已在 bootstrap 文件列表中，所有 session（Telegram、cron、heartbeat）都能看到。

### 部署

复制 `资料/AGENTS.md` 到 workspace 后重启 daemon。旧 cron job 需删掉重建。

---

## 2026-02-27 — 架构债务文档化

### 概述

纯文档变更，无代码改动。将本次静态分析发现的三处架构问题记录进 CLAUDE.md §15，并同步更新 Research.md 中 engine.rs 的描述。

### 修改文件

#### `CLAUDE.md` — 新增 §15 架构债务记录
- **§15.1 孤儿文件**：`src/heartbeat/engine.rs` 中 `HeartbeatEngine` 核心方法（`run()`、`tick()` 等）在生产代码中完全未被调用，仅测试使用。唯一生产用途：`ensure_heartbeat_file()`（daemon/mod.rs:22）。重构时可将该函数移到 daemon 内然后删除整个 `src/heartbeat/` 目录。
- **§15.2 活跃时间硬编码**：~~`daemon/mod.rs:188-194` 的 `< 7` 与注释 "06:30" 不一致，且无法通过 config.toml 配置。~~ **(✅ 已于今日后续提交中修复，详情见上方的 "Heartbeat 活跃时间可配置化" 日志)**
- **§15.3 渠道路由割裂**：Heartbeat/Cron 投递（`deliver_announcement`）与普通消息（`channels_by_name.get()`）是两套独立代码，仅支持 4 个渠道，绕过 Channel trait。

#### `Research.md`
- 文件目录中 `engine.rs` 描述更新为 `⚠️ 孤儿文件：HeartbeatEngine 核心方法仅测试用，生产代码不调用`
- §4.3 Heartbeat 重构条目末尾添加孤儿文件注记，交叉引用 CLAUDE.md §15.1

### 验证

无需编译，纯文档变更。

---

## 2026-02-27 — Worker Agent 基础设施 + 新闻管道重设计

### 概述

解决 Cron 新闻任务直接在 session 内抓取多个 RSS 源导致上下文爆掉的问题。引入 Haiku delegate worker + tool result 截断双重防护。

### 问题根因

Cron 新闻任务直接用主模型在隔离 session 里连续调 `http_request` 抓 5-10 个 RSS 源 → tool 输出全量写入 session history → 超 200K tokens → 爆掉。

### 新架构

```
Cron 触发主 agent → delegate("news_fetcher") → Haiku 子 agent (独立 history)
  → 抓取 RSS → 写本地文件 → 去重 → 推送 Telegram → 返回报告
→ 主 agent 读报告 → 一句话评价 → send_telegram
```

### 代码改动

#### `src/config/schema.rs`
- `DelegateAgentConfig` 新增 `system_prompt_file: Option<String>` 字段
- 支持从外部 MD 文件加载 worker 指令（TOML 里不用写长 prompt）

#### `src/tools/delegate.rs`
- 新增 `workspace_dir: Option<PathBuf>` 字段
- 新增 `resolve_system_prompt()` 方法：优先从文件加载，失败 fallback 到内联
- `execute()` 和 `execute_agentic()` 两处使用点更新

#### `src/tools/mod.rs`
- 传 `workspace_dir` 给 DelegateTool（从 `root_config.workspace_dir`）

#### `src/tools/model_routing_config.rs`
- `upsert_agent` 支持 `system_prompt_file` 参数
- `snapshot()` 输出包含 `system_prompt_file`
- 参数 schema 新增 `system_prompt_file` 描述

#### `src/agent/loop_.rs`
- 新增 `MAX_TOOL_RESULT_IN_HISTORY_CHARS = 8000` 常量
- tool result 写入 history 时截断超限输出（兜底防护）
- LLM 当前 iteration 仍看到完整输出，截断只影响后续 iteration 的 history 回顾

### 配置改动

#### `资料/config.toml`
- 新增 `[agents.news_fetcher]`：Haiku 模型、file-based system_prompt、4 个受限 tools

#### `资料/workers/news_fetcher.md` [新文件]
- 新闻采集工人工作手册：抓取流程、封禁源处理（3 次失败自动封禁）、推送格式、返回报告格式

#### `资料/HEARTBEAT.md`
- 6 个新闻时段全部改为 delegate 模式
- 每个时段明确写出 delegate 指令和主 agent 后续动作

### 封禁源处理

- 失败 1-2 次 → 状态「观察中」，下次仍尝试
- 失败 >= 3 次 → 状态「已封禁」，不再抓取
- 主 agent 收到报告后通知用户更换新闻源

### 本地文件结构

```
D:\ZeroClaw_Workspace\
├── workers/
│   └── news_fetcher.md          # 工人工作手册
└── homework/news/
    ├── YYYY-MM-DD-{时段名}.md   # 当天抓取内容
    ├── last_push_{时段名}.md    # 去重用
    └── ban_list.md              # 封禁源记录
```

### 编译结果

- `cargo build --release` — 成功（6m29s）
- 修复了 7 个文件的 test DelegateAgentConfig 初始化 + 2 处 Config test 缺失字段

### 待办（P2）

- [ ] `channels/mod.rs` context overflow 优雅恢复（自动 LLM 摘要 + 重试）
- [ ] 预防性 compact（history 超阈值时主动压缩）

---

## 2026-02-27 — Research.md 创建 + CLAUDE.md 更新

### 概述

写入研究文档，更新工程协议，无代码改动。

### 新增文件

#### `Research.md` — ZeroClaw + OpenClaw 架构研究文档
- 两个项目的关系对比表
- OpenClaw 完整关键文件目录（`C:\Dev\openclaw\src\`）
- ZeroClaw 完整文件目录（含二开新增模块标注）
- 当前二次开发状态总结（Phase 1-5）
- **活跃时间（Active Hours）问题分析 + 改造方案**（见第五节）
- **渠道路由（Channel Routing）问题分析 + 改造方案**（见第六节）

### 修改文件

#### `CLAUDE.md`
- **§0 新增规则 4**：要求每次开始工作前阅读 `Research.md`
- **§14.3 Bug 状态更新**：`notify_channel`/`notify_to` 已确认修复，删除错误描述，添加已修复标记

### 分析结论（文档化到 Research.md 第五、六节）

#### 活跃时间逻辑问题
- 时间区间硬编码在 `src/daemon/mod.rs:190`（23:00-07:00）
- 仅小时精度，无时区配置，`HeartbeatConfig` 无对应字段
- 改造方案：扩展 schema 增加 `active_hours_start/end/timezone`，提取 `is_within_active_hours()` 函数

#### 渠道路由问题
- 无 `"last"` 路由（无法自动路由到用户最后活跃渠道）
- 两处独立 match 维护渠道白名单（`daemon/mod.rs` + `scheduler.rs`）
- `deliver_announcement` 只支持 4 个渠道（telegram/discord/slack/mattermost），绕过 Channel trait
- 改造方案 A（最小改动）：统一投递函数 + 通过 Channel trait 投递
- 改造方案 B（长期）：实现 Session 层 + `"last"` 路由机制

---

## 2026-02-26 — 聊天记录自动总结（Phase 3）

### 概述

实现了聊天记录的自动总结触发流程。heartbeat 每小时扫描 JSON 日志文件，通过 hash 检测变更，调用轻量 LLM 模型生成摘要并写入 SQLite 索引。

### 新增文件

#### `src/channels/chat_summarizer.rs` — 自动总结 worker
- `summarize_chat_logs(&Config)` — 主入口，扫描所有日志文件
- 复用 `create_resilient_provider_with_options()` 构建 provider（支持所有格式）
- `file_content_hash()` + SQLite `source_hash` 变更检测，跳过未变更文件
- `provider.simple_chat()` 调用轻量模型生成"摘要/话题"格式输出
- `parse_summary_response()` 解析 LLM 返回
- 含 4 个单元测试

### 修改文件

#### `src/config/schema.rs` — summary_model 移至 Config 顶层
- `summary_model: Option<String>` 从 `ChatLogConfig` 移到 `Config`
- 不配置时 fallback 到 `default_model`

#### `src/onboard/wizard.rs` — 两处构造器 + `summary_model: None`
#### `src/channels/chat_log.rs` — 新增 `list_log_files()` + `LogFileEntry`
#### `src/channels/mod.rs` — 注册 `pub mod chat_summarizer`
#### `src/daemon/mod.rs` — heartbeat 循环末尾调用 `summarize_chat_logs()`

### 配置示例 (config.toml)

```toml
default_model = "claude-sonnet-4-6"
# 不配置则用 default_model
summary_model = "claude-haiku-4-5-20251001"

[chat_log]
enabled = true
owner = "e1vix"
```

### 编译结果

- `cargo build --release` — 零错误零警告
- `cargo check` — 零输出

---

## 2026-02-26 — 聊天记录持久化 + 索引搜索 + 跨用户上下文（Phase 1-2）

### 概述

实现了 Telegram 聊天记录的完整持久化和索引系统。支持按用户名+日期的 JSON 日志文件、SQLite FTS5 全文索引、owner 权限控制的搜索工具，以及跨用户对话摘要注入。

### 新增文件

#### `src/channels/chat_log.rs` — JSON 日志持久化模块
- 按 `{username}_{YYYY-MM-DD}.json` 格式存储每日聊天记录
- 支持文本/语音/图片三种消息类型
- `append_turn()` 追加写入、`load_recent_messages()` 加载最近记录
- `load_all_today_messages()` 启动时恢复当日对话到内存
- 含 8 个单元测试

#### `src/channels/chat_index.rs` — SQLite 索引模块
- 独立 `chat_summaries` 表 + `chat_summaries_fts` FTS5 虚拟表
- `upsert_summary()` 幂等写入、`search_fts()` 全文搜索
- `get_recent_cross_user_summaries()` 排除自己的摘要查询
- `watchdog_check()` 监控数据库大小（>100K 行 / >200MB 告警）
- `source_hash` 变更检测避免重复索引
- 含 8 个单元测试

#### `src/tools/search_chat_log.rs` — SearchChatLogTool
- Agent 可用的聊天记录搜索工具
- **三层安全控制**：Tool 层权限检查 + 注入层 owner 限定 + 日志访问层隔离
- 同时搜索 JSON 原始消息和 SQLite 摘要索引
- 含 4 个单元测试

### 修改文件

#### `src/config/schema.rs` — ChatLogConfig 配置
- 新增 `ChatLogConfig` 结构体（enabled, owner）
- 集成到 `Config` 结构体和 `Config::default()`

#### `src/channels/mod.rs` — 集成入口
- 用户消息持久化（自动检测语音/图片）
- 助手消息持久化
- 启动时加载当日聊天记录
- **跨用户摘要注入**：仅 owner 在 system prompt 中看到其他用户的对话摘要

#### `src/tools/mod.rs` — 工具注册
- 有条件注册 SearchChatLogTool（chat_log.enabled 时）

#### `src/config/mod.rs` — 导出 ChatLogConfig
#### `src/onboard/wizard.rs` — 两处 Config 构造新增 chat_log 字段
#### `src/peripherals/mod.rs` — 清理 unused import 警告
#### `src/channels/mod.rs` — 清理 unused ClawdTalkConfig re-export

---

## 2026-02-26 — Heartbeat 重构 + SendVoiceTool + EmailConfig 修复

### 概述

将 Heartbeat 系统从"逐行解析 HEARTBEAT.md 执行 N 次 agent turn"改造为对齐 OpenClaw 设计的"整体 prompt + HEARTBEAT_OK 抑制"模式。同时新增 SendVoiceTool，修复 EmailConfig 测试编译错误。

### 修改文件

#### `src/daemon/mod.rs` — Heartbeat Worker 重写

- **删除**：`parse_tasks()` 逐行提取 `- ` 行的调用逻辑
- **删除**：`HeartbeatEngine` 初始化（observer、engine 构建）
- **新增**：读取整个 HEARTBEAT.md 内容作为一个 prompt 发给 Agent
- **新增**：`contains_heartbeat_ok()` — Agent 回复含 `HEARTBEAT_OK`（开头/结尾）时跳过推送
- **新增**：`is_heartbeat_content_empty()` — 只有标题/空行时跳过 API 调用
- **新增**：activeHours — 23:00-06:30 本地时间跳过 heartbeat
- **新增**：prompt 包含 Cron 同步指示（Agent 用 cron_list/cron_add/cron_update 自动同步）
- **保留**：`heartbeat_tasks_for_tick()` 作为 legacy helper（测试使用）
- **效果**：从 30+ 次 agent turn 减为 1 次；无事时静默不推送

#### `src/heartbeat/engine.rs` — 恢复到备份状态

- 恢复到 20260225 备份版本（305 行）
- 移除了时间槽解析、`HeartbeatState`、`heartbeat_state.json` 持久化等代码
- `parse_tasks()` 和 `HeartbeatEngine` 保留原样用于测试

#### `src/config/schema.rs` — 移除 timezone 字段

- `HeartbeatConfig` 移除 `timezone: String` 字段
- 移除 `default_heartbeat_timezone()` 函数
- `Default` impl 恢复到备份状态

#### `src/tools/send_voice.rs` [新文件]

- 实现 `SendVoiceTool`：Agent 主动合成语音并发送到 Telegram
- 使用 Microsoft Edge TTS (`msedge-tts`) 合成
- 先发语音消息，再发原文文本
- 包含安全检查（`can_act`、`record_action`、rate limiting）

#### `src/tools/mod.rs` — 注册 SendVoiceTool

- 添加 `send_voice` 模块声明
- 在 `all_tools_with_runtime()` 中条件注册（TTS 启用且 Telegram 配置时）

#### `src/channels/mod.rs` — 移除自动 TTS

- 删除了之前自动附加 TTS 到所有 Telegram 回复的逻辑
- TTS 现在完全由 Agent 通过 `SendVoiceTool` 主动控制

#### `src/channels/email_channel.rs` — 测试修复

- 3 处 `EmailConfig` 测试初始化添加 `..Default::default()` 适配新增字段

#### `src/gateway/api.rs` — 测试修复

- 2 处 `EmailConfig` 测试初始化添加 `..Default::default()` 适配新增字段

#### `资料/config.toml` — 配置更新

- `[tts]` 启用 TTS（`enabled = true`），设置 `bot_token`，`reply_to_user = true`
- `[heartbeat]` 移除 `timezone` 配置行

#### `资料/TOOLS.md` — 工具文档

- 新增 `send_voice`、`send_telegram`、`cron_add` 工具说明

### 设计决策

1. **Heartbeat 定位**：定期唤醒 Agent 做状态检查 + Cron 同步。不再负责具体任务执行。
2. **HEARTBEAT.md 是唯一源文件**：RSS 源清单 + 时间表都在此文件。Agent 在 heartbeat turn 中读取后自动同步到 Cron job。
3. **Cron 负责精确执行**：SQLite 持久化 + `next_run` 字段确保不重复不遗漏。
4. **HEARTBEAT_OK 抑制**：对齐 OpenClaw 设计，Agent 回复含此 token 时不推送消息。

### 测试结果

- `cargo test heartbeat --lib` — 29 passed
- `cargo test send_voice --lib` — 6 passed
- `cargo build --release` — 成功
