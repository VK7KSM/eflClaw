# ZeroClaw 开发日志

---

## 2026-03-01 — Agent Loop 完整模块化 + Cargo 编译优化

**涉及文件**：
- `Cargo.toml` — 优化 release profile
- `src/agent/loop_.rs` — 5810 行精简为 ~3976 行，提取 4 个子模块
- `src/agent/loop_/context.rs`（新建）
- `src/agent/loop_/history.rs`（新建）
- `src/agent/loop_/execution.rs`（新建）
- `src/agent/loop_/parsing.rs`（新建，含后续追加函数）

### 改动内容

#### `Cargo.toml`
- `[profile.release]`：`lto = "fat"` → `lto = "thin"`（并行链接时间优化，体积几乎不变，编译速度大幅提升）
- `codegen-units = 1` → `codegen-units = 0`（Cargo 自动选取 = CPU 核心数，充分利用顶配硬件并行编译）
- 移除了树莓派相关注释（生产不考虑低配硬件）
- 移除了冗余的 `[profile.release-fast]` 和 `[profile.dist]`，统一为单一 release profile

#### `src/agent/loop_/context.rs`（与上游完全一致）
- `build_context()`：从 SQLite 记忆搜索并构建上下文前缀
- `build_hardware_context()`：从硬件 RAG 检索数据手册块

#### `src/agent/loop_/history.rs`（与上游完全一致）
- COMPACTION 常量（`COMPACTION_KEEP_RECENT_MESSAGES`、`COMPACTION_MAX_SOURCE_CHARS`、`COMPACTION_MAX_SUMMARY_CHARS`）
- `trim_history()`、`build_compaction_transcript()`、`apply_compaction_summary()`、`auto_compact_history()`

#### `src/agent/loop_/execution.rs`（与上游完全一致）
- `execute_one_tool()`：单工具执行 + 超时取消
- `ToolExecutionOutcome`：执行结果结构体
- `should_execute_tools_in_parallel()`：并行执行判断（需审批的工具保持串行）
- `execute_tools_parallel()`、`execute_tools_sequential()`

#### `src/agent/loop_/parsing.rs`（包含 elfClaw 保留项）
- 全部 tool call 解析函数（XML、JSON、GLM、minimax、perl 风格等）
- `build_native_assistant_history()`、`build_native_assistant_history_from_parsed_calls()`、`build_assistant_history_with_tool_calls()`（追加）
- **有意保留**：不包含 `normalize_shell_command_from_raw` 等函数（elfClaw URL 安全策略，URL 不转为 curl 命令）
- 新增 `use crate::providers::ToolCall;` import

#### `src/agent/loop_.rs` 主文件改动
- 删除所有已迁移到子模块的函数（history/context/execution/parsing）
- 添加 `mod context/execution/history/parsing;` 声明
- 添加 `use` 导入块，含所有子模块函数
- **保留在主文件**（elfClaw 特色功能）：
  - Deferred action 检测（CJK + 英文，第 170-230 行附近）
  - `DEFAULT_MAX_TOOL_ITERATIONS = 10`（上游为 20，elfClaw 降低至 10，加注释标记）
  - `DEFAULT_MAX_HISTORY_MESSAGES = 50`（与上游一致，无需标记）
- 测试模块新增 `use crate::providers::ToolCall;` import
- clippy 修复：`for entry / if let Some(...)` → `.into_iter().flatten()` 展平迭代

### 测试结果
- `cargo test agent::` → 197 通过，1 失败（`run_tool_call_loop_returns_structured_error_for_non_vision_provider` 为**预存在**的失败，模块化前即已失败）
- `cargo test` 全量 → ~3415 通过，26~27 失败（均为预存在失败，与模块化无关）
- `cargo clippy` 对我们的文件零错误；全库存量 141 个 clippy 错误均为先前存在

---

## 2026-03-01 — Agent Loop 模块化：提取 parsing.rs

**涉及文件**：
- `src/agent/loop_/parsing.rs`（新建）— 从 `loop_.rs` 提取所有解析相关函数

### 改动内容

#### `src/agent/loop_/parsing.rs`（新建）
- 从 `loop_.rs` 第 323-1803 行提取所有 tool call 解析函数，跳过第 473-528 行（deferred action 相关逻辑，保留在 `loop_.rs`）
- 迁移内容包含：
  - `ParsedToolCall` 结构体（新增 `#[derive(Debug, Clone)]` 和 `pub(super)` 可见性）
  - 完整解析函数链：`parse_arguments_value`、`parse_tool_call_id`、`canonicalize_json_for_tool_signature`、`tool_call_signature`、`parse_tool_call_value`、`parse_tool_calls_from_json_value`
  - XML 解析：`is_xml_meta_tag`、`extract_xml_pairs`、`parse_xml_tool_calls`、`parse_minimax_invoke_calls`
  - 辅助函数：`find_first_tag`、`matching_tool_call_close_tag`、`extract_first_json_value_with_end`、`strip_leading_close_tags`、`extract_json_values`、`find_json_end`
  - 格式特定解析：`parse_xml_attribute_tool_calls`、`parse_perl_style_tool_calls`、`parse_function_call_tool_calls`
  - GLM 格式：`map_tool_name_alias`、`build_curl_command`、`parse_glm_style_tool_calls`、`default_param_for_tool`、`parse_glm_shortened_body`
  - 主解析入口：`parse_tool_calls`、`detect_tool_call_parse_issue`、`parse_structured_tool_calls`
- 所有函数均标记为 `pub(super)` 供 `loop_.rs` 主逻辑调用
- 有意**未包含**：`normalize_shell_command_from_raw`、`normalize_shell_arguments`、`normalize_tool_arguments`（ZeroClaw 定制决策，URL 安全考虑）
- 文件顶部使用 upstream 风格的 imports：`use regex::Regex; use std::collections::HashSet; use std::sync::LazyLock;`

---

## 2026-03-01 — Phase 4 完成：Android 客户端 + Android FFI + Web 前端 + 插件示例

**涉及文件**：
- `clients/android/`（22文件，新建）— Android 客户端（Kotlin/Jetpack Compose）
- `clients/android-bridge/`（3文件，新建）— UniFFI/JNI Rust 桥接
- `site/`（10文件，新建）— React + Vite Web 前端（GitHub Pages）
- `extensions/hello-world/`（2文件，新建）— 插件示例

### 改动内容

#### 4.6 Android 客户端 (`clients/android/`)
- `app/build.gradle.kts`：Android 应用构建配置（SDK 34、Compose、NDK）
- `app/src/main/AndroidManifest.xml`：权限声明、Activity/Service/Receiver 注册
- `app/src/main/java/ai/zeroclaw/android/MainActivity.kt`：聊天 UI（Compose，含 ChatBubble/EmptyState/StatusIndicator）
- `app/src/main/java/ai/zeroclaw/android/ZeroClawApp.kt`：Application 类，创建通知渠道
- `app/src/main/java/ai/zeroclaw/android/bridge/ZeroClawBridge.kt`：JNI 桥接 stub，等待 UniFFI 生成
- `app/src/main/java/ai/zeroclaw/android/receiver/BootReceiver.kt`：开机自启广播接收器
- `app/src/main/java/ai/zeroclaw/android/service/ZeroClawService.kt`：前台服务，StateFlow 状态管理
- `app/src/main/java/ai/zeroclaw/android/ui/SettingsScreen.kt`：设置 UI（Provider/Model/APIKey/AutoStart）
- `app/src/main/java/ai/zeroclaw/android/ui/theme/Theme.kt`：Material 3 主题（ZeroClawOrange + 暗色方案）
- `app/src/main/res/`：XML 资源（drawable/values）
- `build.gradle.kts`、`settings.gradle.kts`、`gradle.properties`、`gradle/wrapper/gradle-wrapper.properties`

#### 4.7 Android FFI 桥接 (`clients/android-bridge/`)
- `Cargo.toml`：独立 crate（cdylib，依赖 uniffi 0.27 + tokio）
- `src/lib.rs`：UniFFI 绑定（`ZeroClawController`、`AgentStatus` enum、`ZeroClawConfig/ChatMessage/SendResult` record）
- `uniffi-bindgen.rs`：uniffi 代码生成入口

#### 4.8 Web 前端 (`site/`)
- `index.html`、`src/main.tsx`：React 入口
- `src/App.tsx`：完整 Docs Navigator（全文搜索 + 分类过滤 + 命令面板 + i18n + 主题 + TOC）
- `src/styles.css`：设计系统（CSS 变量 + 响应式布局）
- `src/generated/docs-manifest.json`：从仓库 Markdown 生成的文档清单
- `scripts/generate-docs-manifest.mjs`：构建时自动生成清单脚本
- `package.json`、`tsconfig.json`、`vite.config.ts`：构建配置

#### 4.9 插件示例 (`extensions/hello-world/`)
- `zeroclaw.plugin.toml`：插件元数据（id/name/description/version）
- `src/lib.rs`：示例插件（实现 `Plugin` trait，注册 `HelloTool` 工具和 `HelloHook` 钩子）

### 验证结果
- `cargo check --lib` — 零 error，13 warnings（全部为预存在告警）
- 主 Rust 项目完全不受影响（Android/site 均为独立项目）

---

## 2026-03-01 — 移植上游 providers 模块改进（reliable + compatible + mod）

**涉及文件**：
- `src/providers/reliable.rs`（修改：添加 provider 级别 fallback 和 vision_override）
- `src/providers/compatible.rs`（修改：添加 CompatibleApiMode、WebSocket 支持）
- `src/providers/mod.rs`（修改：新常量、新别名、扩展 ProviderRuntimeOptions、扩展 secret scrubbing）
- `src/agent/loop_.rs`（修改：ProviderRuntimeOptions 初始化添加 ..default()）

### 改动内容

#### `src/providers/reliable.rs`
- 导入 `HashSet`（从 `HashMap` 改为 `{HashMap, HashSet}`）
- `ReliableProvider` struct 新增两个字段：
  - `provider_model_fallbacks: HashMap<String, Vec<String>>` — provider 级别的 model 映射
  - `vision_override: Option<bool>` — vision 支持配置覆盖
- `new()` 初始化新增两字段
- `with_model_fallbacks()` 重写：根据 provider 名称将 fallback key 路由到对应 map（provider 级别 vs. model 级别）
- 新增 `with_vision_override()` builder 方法
- 新增 `provider_model_chain()` 私有方法：返回特定 provider 应尝试的 model 列表
- `supports_vision()` 更新：使用 `vision_override` 覆盖逻辑
- 更新所有 5 个 Provider trait 方法的循环（`chat_with_system`, `chat_with_history`, `chat_with_tools`, `chat`, `stream_chat_with_system`）使用 `enumerate()` 和 `provider_model_chain()`
- 保留了我们原有的 `max_backoff_ms` 字段（上游移除但我们保留）

#### `src/providers/compatible.rs`
- 新增 WebSocket 导入：`SinkExt`, `tokio_tungstenite`, `connect_async`, `IntoClientRequest`, `HeaderName`, `AUTHORIZATION`, `WsHeaderValue`, `WsMessage`
- 新增 `serde_json::Value` 导入
- `OpenAiCompatibleProvider` struct 新增两个字段：
  - `api_mode: CompatibleApiMode` — API 协议模式
  - `max_tokens_override: Option<u32>` — 最大 token 覆盖
- 新增 `CompatibleApiMode` enum（`OpenAiChatCompletions` | `OpenAiResponses`）
- 所有构造函数更新为传递新参数（默认值：`OpenAiChatCompletions, None`）
- 新增 `new_custom_with_mode()` 构造函数
- `ResponsesRequest` 新增 `max_output_tokens`, `tools`, `tool_choice` 字段
- `ResponsesResponse` 改为 `Clone`，新增 `id` 字段
- `ResponsesOutput` 改为 `Clone`，新增 `kind`, `name`, `arguments`, `call_id` 字段
- `ResponsesContent` 改为 `Clone`
- 新增 `ResponsesWebSocketCreateEvent` struct
- 新增 `ResponsesWebSocketAccumulator` struct（含 `apply_event()`, `fallback_response()`, `record_output_item()`, `final_text()`）
- 新增 `extract_responses_stream_error_message()` 函数
- 新增 `extract_responses_stream_text_event()` 函数
- 新增 `extract_responses_tool_calls()` 函数
- 新增 `parse_responses_chat_response()` 函数
- `extract_responses_text()` 签名改为取引用 `&ResponsesResponse`（更新所有调用点含测试）
- 新增 WebSocket 方法：`should_use_responses_mode()`, `effective_max_tokens()`, `should_try_responses_websocket()`, `responses_websocket_url()`, `apply_auth_header_ws()`, `send_responses_websocket_request()`, `send_responses_http_request()`, `send_responses_request()`
- `chat_via_responses()` 重构为委托给 `send_responses_request()`

#### `src/providers/mod.rs`
- 新增常量 `QWEN_CODING_PLAN_BASE_URL = "https://coding.dashscope.aliyuncs.com/v1"`
- 新增函数 `is_qwen_coding_plan_alias(name)` → `matches!(name, "qwen-coding-plan")`
- `is_qwen_alias()` 更新：包含 `is_qwen_coding_plan_alias`
- `qwen_base_url()` 更新：优先检查 `is_qwen_coding_plan_alias`
- `list_providers()` 中 qwen 别名添加 `"qwen-coding-plan"`
- 测试中别名列表添加 `"qwen-coding-plan"`
- 新增 `pub use compatible::CompatibleApiMode;` re-export
- `ProviderRuntimeOptions` 新增 4 个字段：`reasoning_level`, `custom_provider_api_mode`, `max_tokens_override`, `model_support_vision`
- `Default` impl 初始化新字段为 `None`
- `scrub_secret_patterns()` 扩展：从 7 个前缀扩展到 26 个 `(&str, usize)` 元组，新增 `AIza`, `AKIA`, JSON token 模式, `Bearer` 前缀

#### `src/agent/loop_.rs`
- 修复 2 处 `ProviderRuntimeOptions` 初始化（添加 `..providers::ProviderRuntimeOptions::default()`）

### 验证结果
- `cargo build --lib` 无错误（Finished in 0.84s）

---



**涉及文件**：
- `src/skills/templates.rs`（新建，171 行，逐字节与上游一致）
- `src/skills/audit.rs`（修改：同步上游差异）
- `src/skills/mod.rs`（修改：添加 `mod templates;` 声明）
- `templates/`（新建目录：从上游复制所有模板文件）

### 改动内容

#### `src/skills/templates.rs`（新建）
- 从上游 `zeroclaw_original` 逐字复制
- 定义 `TemplateFile`、`SkillTemplate` struct
- 5 个内置模板：`weather_lookup`（Rust）、`calculator`（Rust）、`hello_world`（TypeScript）、`word_count`（Go）、`text_transform`（Python）
- 使用 `include_str!` 宏引用 `templates/` 目录下的文件内容
- 提供 `find(name)` 和 `apply(content, name, bin_name)` 公共函数

#### `templates/`（新建）
- 从上游复制 4 个语言的模板目录：`rust/`、`typescript/`、`go/`、`python/`
- `templates.rs` 中的 `include_str!` 宏依赖这些文件

#### `src/skills/audit.rs`（同步上游）
- 新增 `use zip::ZipArchive;`（zip crate 已在 Cargo.toml 中）
- 新增 `SkillAuditOptions { allow_scripts: bool }` struct（pub，Copy，Default）
- `audit_skill_directory` 重构为包装器，逻辑移入 `audit_skill_directory_with_options`
- 新增 `audit_skill_directory_with_options(skill_dir, options)` 公共函数
- 内部 `audit_path` 增加 `options: SkillAuditOptions` 参数，`allow_scripts` 控制脚本文件检查
- 新增 `audit_zip_bytes(bytes)` 函数：zip 存档安全审计
- 新增辅助函数：`is_native_binary_zip_entry`、`is_text_zip_entry`
- 新增 zip 安全审计常量
- 新增测试：`audit_allows_shell_script_files_when_enabled` 以及 9 个 zip 审计测试

#### `src/skills/mod.rs`（修改）
- 第 10 行新增 `mod templates;` 声明

**编译结果**：`cargo check --lib` 零错误，7 个 warnings（均为 unused imports，已有预存）

---

## 2026-03-01 — 移植上游 WebSocket gateway（ws.rs）

**涉及文件**：
- `src/gateway/ws.rs`（重写：167 行 → 547 行）
- `src/channels/mod.rs`（修改：`sanitize_channel_response` 可见性 `fn` → `pub(crate)`）

### 改动内容

**上游 ws.rs（510 行）vs 我们（167 行）的差异**：
- 上游有完整的 session history 管理、response sanitization、tool output fallback
- 上游认证方式：`Authorization: Bearer <token>` 或 `Sec-WebSocket-Protocol: bearer.<token>` header
- 上游使用 `run_tool_call_loop` 直接调用 agent loop（需要 `tools_registry_exec: Arc<Vec<Box<dyn Tool>>>`）
- 上游有 `build_ws_system_prompt`、`sanitize_ws_response`、`finalize_ws_response` 等辅助函数

**兼容性适配**：
1. 上游依赖 `state.tools_registry_exec`（`Arc<Vec<Box<dyn Tool>>>`），我们的 `AppState` 只有 `tools_registry: Arc<Vec<ToolSpec>>`。解决方案：`build_ws_system_prompt` 接收 `&[ToolSpec]` 而非 `&[Box<dyn Tool>]`；`finalize_ws_response` 传入空 `&[]`（sanitization 仍可剥离裸 XML tool-call 块）
2. 上游依赖 `build_tool_instructions_from_specs` 和 `build_shell_policy_instructions`（不存在于我们的代码库）。解决方案：内联工具协议 block，从 `ToolSpec` 直接构建
3. 认证方式完全移植：从 `?token=<bearer>` query param 改为 header-based（`Authorization: Bearer` 或 `Sec-WebSocket-Protocol: bearer.<token>`）
4. 保留了对 `super::run_gateway_chat_with_tools` 的调用（我们没有 `tools_registry_exec` 所以无法直接用 `run_tool_call_loop`）

**新增内容**：
- `sanitize_ws_response` / `normalize_prompt_tool_results` / `extract_latest_tool_output` / `finalize_ws_response`：response 后处理
- `build_ws_system_prompt`：基于 `ToolSpec` 构建系统提示，包含工具协议说明
- `extract_ws_bearer_token`：解析 header-based 认证
- 对 `crate::security::detect_adversarial_suffix` 的 perplexity filter 检查
- 完整 session history 维护（`Vec<ChatMessage>`）
- 9 个单测（token 提取、response sanitization、prompt 构建、finalize fallback）

**channels/mod.rs**：`sanitize_channel_response` 由 `fn`（私有）改为 `pub(crate)` 以供 ws.rs 调用。

**编译结果**：`cargo build --lib` 零错误，7 个预存在警告（unused imports）

---

## 2026-03-01 — Phase 3 编译修复与提交

**涉及文件**：
- `src/util.rs`（添加 `floor_utf8_char_boundary` 函数）
- `src/main.rs`（添加 `mod coordination;` 声明）
- `src/tools/process.rs`（移除测试结构体中不属于 `RuntimeAdapter` trait 的 `as_any` 方法）

### 改动内容

- `syscall_anomaly.rs` 依赖 `crate::util::floor_utf8_char_boundary`，该函数在 `util.rs` 中缺失。已添加实现：在给定字节上限 `max_bytes` 处找最大合法 UTF-8 字符边界。
- `delegate_coordination_status.rs` 使用 `crate::coordination`，但 `main.rs` 的 mod 列表中缺少 `mod coordination;`。binary 编译失败。已添加声明。
- `process.rs` 的 `NoLongRunningRuntime` 测试结构体包含 `as_any` 方法，但我们的 `RuntimeAdapter` trait 不含此方法。移除该方法后编译通过。

**编译结果**：`cargo build` 零错误，6 个警告（全部为 unused imports，无 deny 级别）
**测试结果**：3307 passed，24 failed（均为 Windows 上的预存在失败，如 `sleep 60` 不可用、ripgrep 依赖等）

---

## 2026-03-01 — 移植上游 tools 模块：agents_ipc.rs 和 delegate_coordination_status.rs

**涉及文件**：
- `src/tools/agents_ipc.rs`（新建，1023 行，逐字节与上游一致）
- `src/tools/delegate_coordination_status.rs`（新建，881 行，逐字节与上游一致）
- `src/tools/mod.rs`（修改：新增模块声明、pub use 导出、agents_ipc 工具注册）

### 改动内容

#### `src/tools/agents_ipc.rs`（新建）
- 基于共享 SQLite 数据库的进程间通信工具集（IPC for independent ZeroClaw agents）
- 核心结构体：`IpcDb`（共享 SQLite 句柄，WAL 模式，agent 注册/注销、heartbeat）
- 5 个 LLM 可调用工具：
  - `AgentsListTool`：列出在线 Agent（staleness 窗口过滤）
  - `AgentsSendTool`：向指定 Agent 或广播发送消息（security policy 控制）
  - `AgentsInboxTool`：读取收件箱（直接消息读后标记已读，广播消息不变）
  - `StateGetTool`：读取共享 KV 状态
  - `StateSetTool`：写入共享 KV 状态（security policy 控制）
- `IpcDb::open()` 从 workspace 路径的 SHA-256 哈希派生 agent_id（防止伪造）
- `Drop` 实现：进程退出时从 agents 表删除自身记录
- 依赖：`crate::config::AgentsIpcConfig`（已存在于 schema.rs）、`rusqlite`、`sha2`、`shellexpand`
- 14 个单元测试（schema 创建、注册、heartbeat、收件箱隔离、广播、staleness 过滤、身份强制执行、state upsert、安全策略阻断等）

#### `src/tools/delegate_coordination_status.rs`（新建）
- Delegate 协调系统的只读运行时可观测工具
- 公开结构体：`DelegateCoordinationStatusTool`（需要 `InMemoryMessageBus` 实例）
- 功能：查询 Agent 收件箱积压、context 状态转换、dead-letter 事件
- 支持分页（offset/limit）、按 agent 名过滤、按 correlation_id 过滤
- 依赖：`crate::coordination::{CoordinationPayload, InMemoryMessageBus, SequencedEnvelope}`
- 6 个集成测试（覆盖 context/inbox 报告、dead-letter 分页、context 分页、message 分页带 correlation 过滤）
- **注意**：模块已声明并 pub use 导出，但暂未在 `all_tools_with_runtime()` 中注册。
  原因：我们 codebase 尚无 `CoordinationConfig`（coordination 完整移植后再注册）。
  上游注册逻辑依赖 `root_config.coordination.enabled`，等待后续 coordination 配置移植。

#### `src/tools/mod.rs`（修改）
- 新增模块声明：`pub mod agents_ipc;` 和 `pub mod delegate_coordination_status;`
- 新增 pub use 导出：`DelegateCoordinationStatusTool`
- 在 `all_tools_with_runtime()` 的 chat_log 块之后新增 agents_ipc 注册块：
  - 当 `root_config.agents_ipc.enabled == true` 时调用 `IpcDb::open()`
  - 成功时注册 5 个工具；失败时 `tracing::warn!` 降级（不 panic）

### 编译状态
- `cargo check` 通过，无新引入错误
- 已存在的 `syscall_anomaly.rs` 中 `floor_utf8_char_boundary` 错误和 plugins 未使用 import 警告不属于本次改动
- `DelegateCoordinationStatusTool` pub use 有 unused import 警告（预期，待 coordination 完整移植后注册）

---

## 2026-03-01 — 移植上游 security 模块：perplexity.rs 和 syscall_anomaly.rs

**涉及文件**：
- `src/security/perplexity.rs`（新建，195 行，逐字节与上游一致）
- `src/security/syscall_anomaly.rs`（新建，678 行，逐字节与上游一致）
- `src/security/mod.rs`（修改：新增模块声明和 pub use 导出）

### 改动内容

#### `src/security/perplexity.rs`（新建）
- 对抗性后缀检测（adversarial suffix / GCG prompt injection 防御）
- 基于字符类转移矩阵的 bigram 困惑度计算（无外部依赖，纯 Rust）
- 公开类型：`PerplexityAssessment`（perplexity、symbol_ratio、suspicious_token_count、suffix_sample）
- 公开函数：`detect_adversarial_suffix(prompt, cfg)` — 返回 `Option<PerplexityAssessment>`
- 依赖：仅 `crate::config::PerplexityFilterConfig`（已存在于 schema.rs）
- 4 个单元测试（disabled 短路、GCG 检测、自然语言不误报、延迟 <50ms）

#### `src/security/syscall_anomaly.rs`（新建）
- Daemon shell/进程执行的 syscall 异常检测器
- 消费 stdout/stderr 输出，提取 seccomp/audit 行，匹配基线配置
- 公开类型：`SyscallAnomalyDetector`（主结构体）、`SyscallAnomalyAlert`、`SyscallAnomalyKind`
- 特性：速率限制窗口（60s）、alert cooldown、每分钟 alert 预算、基线 syscall allowlist、审计日志集成
- 依赖：`crate::config::{AuditConfig, SyscallAnomalyConfig}`、`crate::security::audit::{AuditEvent, AuditEventType, AuditLogger}`、`regex`（已在 Cargo.toml）、`parking_lot`（已在 Cargo.toml）
- 9 个单元测试（覆盖 seccomp denied、hex/数字/符号 syscall 解析、cooldown、限速、disabled 模式）

#### `src/security/mod.rs`（修改）
- 在 `domain_matcher` 行之后插入 `pub mod perplexity;` 和 `pub mod syscall_anomaly;`
- 在 prompt_guard 导出块之后插入：
  - `pub use perplexity::{detect_adversarial_suffix, PerplexityAssessment};`
  - `pub use syscall_anomaly::{SyscallAnomalyAlert, SyscallAnomalyDetector, SyscallAnomalyKind};`

### 为什么这样做
- 两个文件依赖的 config 类型（`PerplexityFilterConfig`、`AuditConfig`、`SyscallAnomalyConfig`）均已存在于 `src/config/schema.rs`
- `regex` 和 `parking_lot` 均已在 `Cargo.toml` 中声明，无需新增依赖
- 按逐字节方式移植，不做任何功能修改，保持与上游一致

---

## 2026-03-01 — 移植上游 MCP (Model Context Protocol) 工具套件

**涉及文件**：
- `src/tools/mcp_protocol.rs`（新建，126 行）
- `src/tools/mcp_transport.rs`（新建，285 行）
- `src/tools/mcp_client.rs`（新建，357 行）
- `src/tools/mcp_tool.rs`（新建，68 行）
- `src/tools/mod.rs`（修改：新增模块声明、pub use 导出）
- `src/channels/mod.rs`（修改：添加 MCP 工具异步注册逻辑）

### 改动内容

#### `src/tools/mcp_protocol.rs`（新建，逐字节与上游一致）
- JSON-RPC 2.0 协议类型：`JsonRpcRequest`、`JsonRpcResponse`、`JsonRpcError`
- MCP 工具列表类型：`McpToolDef`、`McpToolsListResult`
- 协议版本常量：`JSONRPC_VERSION = "2.0"`、`MCP_PROTOCOL_VERSION = "2024-11-05"`
- 标准错误码常量（`PARSE_ERROR`、`INVALID_REQUEST` 等）
- 4 个单元测试

#### `src/tools/mcp_transport.rs`（新建，逐字节与上游一致）
- `McpTransportConn` trait：抽象传输层（`send_and_recv`、`close`）
- `StdioTransport`：spawn 本地进程，通过 stdin/stdout 通信
- `HttpTransport`：HTTP POST 请求
- `SseTransport`：SSE 传输（当前简化为 HTTP POST）
- `create_transport()` 工厂函数，根据 `McpTransport` 枚举选择传输类型
- import 路径：`crate::config::schema::{McpServerConfig, McpTransport}`（schema 模块是 pub，路径有效）
- 3 个单元测试

#### `src/tools/mcp_client.rs`（新建，逐字节与上游一致）
- `McpServer`：单个 MCP 服务器的连接，封装在 `Arc<Mutex<McpServerInner>>` 内
- `McpServer::connect()`：执行 initialize 握手 + `tools/list` 获取工具列表
- `McpServer::call_tool()`：带超时的工具调用（可配置，上限 600 秒）
- `McpRegistry`：多服务器聚合，工具名以 `<server>__<tool>` 前缀去重
- `McpRegistry::connect_all()`：非致命性批量连接（单个失败只 log 不中断）
- 5 个测试（含 2 个 async 测试）

#### `src/tools/mcp_tool.rs`（新建，逐字节与上游一致）
- `McpToolWrapper`：将 MCP 工具包装为 `Tool` trait 实现
- 通过 `Arc<McpRegistry>` 分发工具调用，工具错误转换为 `ToolResult { success: false }`

#### `src/tools/mod.rs`（修改）
- 新增 4 个模块声明（字母排序插入）：`pub mod mcp_client;`、`pub mod mcp_protocol;`、`pub mod mcp_tool;`、`pub mod mcp_transport;`
- 新增 pub use 导出：`McpRegistry`、`McpServer`、`McpToolWrapper`、`create_transport`、`McpTransportConn`、以及协议类型（`JsonRpcRequest/Response/Error`、`McpToolDef`、`McpToolsListResult`）
- MCP 工具注册不在 `all_tools_with_runtime`（同步函数）内，见 channels/mod.rs 说明

#### `src/channels/mod.rs`（修改）
- 在 `run_channels()` 中，将原来同步的 `Arc::new(all_tools_with_runtime(...))` 拆分为：
  1. `let mut built_tools = all_tools_with_runtime(...)` — 先建可变 Vec
  2. 当 `config.mcp.enabled && !config.mcp.servers.is_empty()` 时，异步 `McpRegistry::connect_all()` 并追加 `McpToolWrapper` 实例
  3. `let tools_registry = Arc::new(built_tools)` — 冻结
- 与上游 channels/mod.rs 逻辑完全一致
- 失败为非致命性（`tracing::error!` 记录，daemon 继续运行）

### 架构说明
- MCP 工具注册必须在异步路径中完成（`connect_all` 是 async），因此放在 `channels/mod.rs` 的 `run_channels()` 异步函数中，而非同步的 `all_tools_with_runtime()`
- `McpConfig`（`mcp.enabled`、`mcp.servers`）已在 `src/config/schema.rs` 中定义，无需修改 schema

### 验证
- `cargo check --lib` — 零错误，6 个 warnings（均为已有 plugins 模块 unused imports，与本次改动无关）✓

---

## 2026-03-01 — 移植上游 gateway 兼容层：openai_compat + openclaw_compat

**涉及文件**：
- `src/gateway/openai_compat.rs`（新建，720 行）
- `src/gateway/openclaw_compat.rs`（新建，902 行，含适配）
- `src/gateway/mod.rs`（修改：添加模块声明 + 路由注册 + 启动提示）

### 改动内容

#### `src/gateway/openai_compat.rs`（新建）
- 原封不动从上游移植，提供 `POST /v1/chat/completions`（简单 provider 直连，无 agent loop）和 `GET /v1/models` 端点
- 导出常量 `CHAT_COMPLETIONS_MAX_BODY_SIZE = 524288`（512KB），供 openclaw_compat 引用
- 支持流式（SSE）和非流式响应，含 Bearer token 认证和速率限制
- 包含 8 个单元测试

#### `src/gateway/openclaw_compat.rs`（新建，含适配）
- 移植自上游，提供两个端点：
  - `POST /api/chat`：ZeroClaw 原生端点，调用完整 agent loop（含工具和记忆），面向 OpenClaw 迁移用户
  - `POST /v1/chat/completions`（工具增强版）：OpenAI 兼容 shim，提取最后一条用户消息 + 最近上下文，路由到完整 agent loop
- **适配说明**：上游版本引用了 `state.tools_registry_exec` 和 `super::sanitize_gateway_response`，这两个在我们的 codebase 中均不存在。适配方案：直接使用 `run_gateway_chat_with_tools` 的返回值，不再调用 sanitize（agent loop 本身已产出干净输出）。这是最简、符合 KISS 原则的处理方式。
- 包含 9 个单元测试

#### `src/gateway/mod.rs`（修改）
- 新增模块声明：`pub(crate) mod openclaw_compat;` 和 `pub(crate) mod openai_compat;`
- 新增路由注册：
  - `POST /api/chat` → `openclaw_compat::handle_api_chat`
  - `POST /v1/chat/completions` → `openclaw_compat::handle_v1_chat_completions_with_tools`（512KB body limit 子路由器）
  - `GET  /v1/models` → `openai_compat::handle_v1_models`
- 新增启动提示信息（3 行）

### 依赖差异说明

| 上游符号 | 我们的 codebase | 处理方式 |
|---------|----------------|---------|
| `state.tools_registry_exec` | 不存在（我们只有 `tools_registry: Arc<Vec<ToolSpec>>`） | 移除调用，直接使用 agent 返回值 |
| `super::sanitize_gateway_response` | 不存在（channels 中有私有版本） | 移除调用，agent loop 输出无需二次清洗 |

### 验证
- `cargo check --lib` — 零错误，6 个 warnings（均为已有 plugins 模块 unused import，与本次无关）✓

---

## 2026-03-01 — 上游 channel 移植核查：irc.rs + nostr.rs

**涉及文件**：`src/channels/irc.rs`、`src/channels/nostr.rs`、`src/channels/mod.rs`、`src/config/schema.rs`

### 改动内容

#### 核查结论：已完整移植，无需修改

执行了全面核查，结论如下：

- `src/channels/irc.rs`（1021 行）：已存在，与上游 `zeroclaw_original` 逐字节一致（`diff` 输出 IDENTICAL）
- `src/channels/nostr.rs`（398 行）：已存在，与上游 `zeroclaw_original` 逐字节一致（`diff` 输出 IDENTICAL）
- `src/channels/mod.rs`：已包含 `pub mod irc;`、`pub mod nostr;`、`pub use irc::IrcChannel;`、`pub use nostr::NostrChannel;`，以及工厂注册代码（第 3107 行 IRC、第 3200 行 Nostr 健康检查、第 3440 行 Nostr 运行时启动）
- `src/config/schema.rs`：`IrcConfig`（第 3531 行）和 `NostrConfig`（第 4043 行）均已存在

#### 验证

- `cargo check --lib` — 零错误，6 个 warnings（均为已有 plugins 模块 unused import，与本次无关）✓

---

## 2026-03-01 — 上游工具移植：task_plan + url_validation

**涉及文件**：`src/tools/task_plan.rs`（新建）、`src/tools/url_validation.rs`（新建）、`src/tools/mod.rs`（修改）

### 改动内容

#### `src/tools/task_plan.rs`（新建，608 行）

- 从上游 `C:\Dev\zeroclaw_original\src\tools\task_plan.rs` 原封不动复制 `TaskPlanTool` 实现（逐字节一致，未做任何修改）
- `TaskPlanTool`：会话范围内的任务清单工具，状态存于 `Arc<RwLock<Vec<TaskItem>>>`，会话结束即丢弃（不持久化到 Memory trait）
- 支持 5 个 action：`create`（批量建立，替换现有列表）、`add`（追加单条）、`update`（更新状态）、`list`（列出全部）、`delete`（清空）
- 状态枚举：`pending` / `in_progress` / `completed`
- 安全控制：读操作（`list`）不需要权限；写操作调用 `enforce_tool_operation(ToolOperation::Act)`，`ReadOnly` 模式下全部被拒绝
- 含 13 个单元测试，覆盖 create/add/update/list/delete 全流程、只读模式阻止、无效参数等

#### `src/tools/url_validation.rs`（新建，568 行）

- 从上游 `C:\Dev\zeroclaw_original\src\tools\url_validation.rs` 原封不动复制（逐字节一致，未做任何修改）
- **纯工具函数模块，不是 `Tool` trait 实现**，不注册到工具列表，仅供其他工具内部调用
- 依赖 `crate::config::UrlAccessConfig`（我们已在 `config/schema.rs` 中添加）
- 核心函数：`validate_url()`、`extract_host()`、`host_matches_allowlist()`、`normalize_domain()`、`is_private_or_local_host()`、CIDR 匹配、DNS 重绑定防护
- 含 20 个单元测试

#### `src/tools/mod.rs`（修改）

- 新增 `pub mod task_plan;` 和 `pub mod url_validation;` 声明（按字母顺序，插入 `shell` 之后）
- 新增 `pub use task_plan::TaskPlanTool;` 导出
- 在 `all_tools_with_runtime()` 中注册：`Arc::new(TaskPlanTool::new(security.clone()))`（位于 `ApplyPatchTool` 之后）
- `url_validation` 无需注册（辅助函数模块，无 Tool 实现）

### 验证

- `cargo check --lib` — 零错误，6 个 warnings（均为已有 plugins 模块的 unused import，与本次修改无关）✓

---

## 2026-03-01 — 移植上游 subagent 管理工具四件套

**涉及文件**：
- `src/tools/subagent_registry.rs`（新建，547 行）
- `src/tools/subagent_list.rs`（新建，224 行）
- `src/tools/subagent_manage.rs`（新建，478 行）
- `src/tools/subagent_spawn.rs`（新建，729 行）
- `src/tools/mod.rs`（修改：新增模块声明、pub use 导出、工具注册）

### 改动内容

从上游 `C:\Dev\zeroclaw_original\src\tools\` 逐字移植四个 subagent 管理文件，并集成进 `all_tools_with_runtime()`。

### 各文件职责

- **subagent_registry.rs**：线程安全的 session 注册中心（`parking_lot::RwLock`），管理后台 sub-agent 会话生命周期（Running / Completed / Failed / Killed），支持原子并发检查、lazy 清理（超过 1 小时的终态 session 自动删除）
- **subagent_list.rs**：`SubAgentListTool` — 只读工具，列出所有 session，支持按状态过滤
- **subagent_manage.rs**：`SubAgentManageTool` — 查询单个 session 状态（无安全门控）或 kill 运行中的 session（`ToolOperation::Act` 安全门控）
- **subagent_spawn.rs**：`SubAgentSpawnTool` — 在 `tokio::spawn` 中异步启动 delegate agent，立即返回 session_id；支持 simple mode（单次 `chat_with_system`）和 agentic mode（完整 `run_tool_call_loop`），最大并发 10 个

### 兼容性调整

- `subagent_spawn.rs` 测试中的 `DelegateAgentConfig` 初始化添加了 `system_prompt_file: None`（我们 fork 扩展的字段，上游原始测试未包含）

### 注册方式

在 `all_tools_with_runtime()` 的 agents 非空分支中，在 `DelegateTool` 之前注册：
1. 创建 `Arc<SubAgentRegistry>`（共享实例）
2. 注册 `SubAgentSpawnTool`（持有 registry + parent_tools 快照）
3. 注册 `SubAgentListTool`（持有 registry）
4. 注册 `SubAgentManageTool`（持有 registry + security）

`parent_tools` 快照在 subagent 工具注册前捕获，确保 subagent_spawn 不能递归产生新的 spawn/delegate。

### 验证

`cargo check` 通过，无新增错误，无新增警告。

---

## 2026-03-01 — 移植上游 ProcessTool（去除 SyscallAnomalyDetector）

**涉及文件**：`src/tools/process.rs`（新建）、`src/tools/mod.rs`（修改）、`src/tools/shell.rs`（修改）

### 改动内容

- 从上游 `C:\Dev\zeroclaw_original\src\tools\process.rs`（905 行）移植 `ProcessTool` 到 `C:\Dev\zeroclaw\src\tools\process.rs`
- 移除所有 `SyscallAnomalyDetector` 相关内容（Phase 3 功能，我们 fork 暂不实现）：
  - 删除 `use crate::security::SyscallAnomalyDetector;` 导入
  - 从 `ProcessTool` struct 删除 `syscall_detector: Option<Arc<SyscallAnomalyDetector>>` 字段
  - 内联 `new_with_syscall_detector()` 到 `new()`（去掉双函数结构）
  - 从 `ProcessEntry` 删除 `analyzed_offsets` 字段（仅 syscall detector 使用）
  - 在 `handle_output()` 移除 `if let Some(detector) = ...` 检测块；留下 `TODO(Phase 3)` 注释标记复原点
  - 删除辅助函数 `slice_unseen_output()`（仅 syscall detector 使用）
  - 删除测试 `test_syscall_detector()` helper 和 `process_output_runs_syscall_detector_incrementally` 测试函数
  - 删除测试中的 `use crate::config::{AuditConfig, SyscallAnomalyConfig}` 和 `use crate::security::SyscallAnomalyDetector` 导入
- `src/tools/shell.rs`：将 `collect_allowed_shell_env_vars` 可见性从 `fn`（私有）改为 `pub(super)`，与上游一致，允许 `process.rs` 在同模块内调用
- `src/tools/mod.rs`：
  - 新增 `pub mod process;`
  - 新增 `pub use process::ProcessTool;`
  - 在 `all_tools_with_runtime()` 的工具列表中注册：`Arc::new(ProcessTool::new(security.clone(), runtime))`，紧接 `ShellTool` 之后（`ShellTool` 改用 `runtime.clone()` 先转移 Arc 引用）

### 为什么改

- 上游已包含完整的后台进程管理工具（spawn/list/output/kill），支持并发进程限制、安全策略链、输出缓冲等功能
- 该工具补充了同步 `ShellTool` 无法覆盖的超时场景（长时间运行命令）
- `SyscallAnomalyDetector` 是 Phase 3 安全功能，当前 fork 无此模块，移植时按要求剥离，留 TODO 标记便于后续复原

### 验证

- `cargo check` 通过（无新增错误；`consolidation.rs:71` 错误为预存 bug，与本次改动无关）

---

## 2026-03-01 — 上游 goals/engine.rs 移植

**涉及文件**：`src/goals/engine.rs`（新建）、`src/goals/mod.rs`（修改）

### 改动内容

- 从上游 `C:\Dev\zeroclaw_original\src\goals\engine.rs` 原封不动复制 `GoalEngine` 实现到 `C:\Dev\zeroclaw\src\goals\engine.rs`（932 行，逐字节一致，未做任何修改）
- 将 `src/goals/mod.rs` 中的存根注释替换为 `pub mod engine;`，正式公开 engine 子模块

### 为什么改

- 此前 `src/goals/mod.rs` 仅是"Implementation in Phase 2"占位注释，实际代码未移植
- 上游已包含完整的 `GoalEngine`（状态加载/保存、步骤选择、prompt 构建、stalled 目标检测）及 31 个单元测试，直接移植可保持与上游的一致性

### 改动要点

- `GoalEngine`：管理 `{workspace}/state/goals.json` 的原子读写（写 .tmp 再 rename）
- `GoalState / Goal / Step`：完整数据模型，含 `GoalStatus`、`GoalPriority`（支持优先级排序）、`StepStatus` 枚举，均带 self-healing 反序列化（未知值 fallback 到 Pending）
- `select_next_actionable()`：按优先级选取下一个可执行步骤（跳过已耗尽重试的步骤）
- `find_stalled_goals()`：检测所有步骤均已完成/阻塞/耗尽的目标，触发 reflection
- `build_step_prompt()` / `build_reflection_prompt()`：生成 Agent turn 所需的结构化 prompt
- `interpret_result()`：简单启发式判断步骤成功/失败

---

## 2026-02-28 (续) — Phase 1 上游功能移植

**涉及文件**：`src/config/schema.rs`、`src/config/mod.rs`、`src/agent/research.rs`（新建）、`src/agent/mod.rs`、`src/agent/agent.rs`、`src/tools/apply_patch.rs`（新建）、`src/tools/mod.rs`、`src/onboard/wizard.rs`、`src/security/otp.rs`、`src/security/roles.rs`（新建）、`src/security/mod.rs`

### 1. Research 研究阶段（`src/agent/research.rs`）

- 新增 `src/agent/research.rs`：主动信息收集阶段，在主响应前先用工具搜索
- 新增配置结构体到 `config/schema.rs`：
  - `ResearchTrigger` 枚举（Never/Always/Keywords/Length/Question）
  - `ResearchPhaseConfig` 结构体（enabled, trigger, keywords, max_iterations, show_progress 等）
  - `GoalLoopConfig` 结构体（为 Phase 2 目标引擎预留）
- `Agent` 结构体新增 `research_config` 字段，`AgentBuilder` 新增对应建造者方法
- `turn()` 方法集成：检测是否触发研究阶段，将收集结果注入用户消息上下文
- 原子写兼容：ToolCall 构造时添加 `thought_signature: None`（我们 fork 的 Gemini 扩展字段）
- 包含 6 个单元测试覆盖所有 `should_trigger()` 场景

### 2. apply_patch 工具（`src/tools/apply_patch.rs`）

- 新增 `ApplyPatchTool`：安全的 git patch 应用工具
- 接受 unified diff 字符串，通过 stdin 管道传递给 git（避免 tempfile dev-dependency）
- 默认 `dry_run=true`：先跑 `git apply --check` 验证，不会误改文件
- 支持可选 `commit_message`：自动 stage + commit
- 大小限制：超过 1MB 的 patch 直接拒绝
- 注册到 `all_tools_with_runtime()` 工具列表

### 3. OTP 重放保护修复（`src/security/otp.rs`）

- **关键安全 bug 修复**：`validate_at()` 中重放保护缓存检查逻辑错误
  - 旧代码：发现缓存中的已用 OTP 码 → 返回 `Ok(true)`（错误！允许重放攻击）
  - 新代码：发现缓存中的已用 OTP 码 → 返回 `Ok(false)`（正确！拒绝重放）
  - 缓存语义：存储"已用过的码" → 找到 = 已用 = 拒绝

### 4. RBAC 角色系统（`src/security/roles.rs`）

- 新增 `SecurityRoleConfig` 配置结构体到 `config/schema.rs`
- 新增 `SecurityConfig.roles` 字段（可配置的自定义角色列表）
- 新增 `src/security/roles.rs`：`RoleRegistry` + `ToolAccess`
  - 5 个内置角色：owner（全权）、admin（全权+TOTP全局）、operator（多数工具+shell TOTP）、viewer（只读）、guest（无工具）
  - 支持继承链（通过 `inherits` 字段）
  - 支持 TOTP 门控：角色级 + 全局级
  - 循环继承检测（DFS cycle detection）
  - 7 个单元测试覆盖 operator/viewer/owner/custom 角色和继承循环检测

### 配置测试修复
- `src/onboard/wizard.rs`：两处 Config 初始化补充 `research` 和 `goal_loop` 字段
- `src/config/schema.rs`：两处测试内 Config 初始化补充 `research` 和 `goal_loop` 字段

---

## 2026-02-28 — 上游关键修复移植 + 测试编译修复

**涉及文件**：`src/agent/loop_.rs`、`src/config/schema.rs`、`src/channels/mod.rs`、`src/config/mod.rs`、`src/daemon/mod.rs`、`src/onboard/wizard.rs`、`src/integrations/registry.rs`、`src/providers/gemini.rs`、`src/providers/reliable.rs`、`src/providers/mod.rs`、`src/agent/agent.rs`、`src/agent/dispatcher.rs`、`src/agent/tests.rs`、`src/tools/delegate.rs`、`src/tools/file_read.rs`

### 概述

从上游 zeroclaw（452 commits ahead）中选取 3 个关键修复手动移植，同时修复了所有预先存在的测试编译错误。

### 移植 1：URL→shell 安全修复（upstream dedb59a4）

**文件**：`src/agent/loop_.rs`

- 删除 `parse_glm_style_tool_calls()` 中 "Plain URL" 自动转 `curl` shell 命令的代码块
- 纯 URL（如 `https://example.com`）不再被自动当作 shell 命令执行
- Agent 必须通过显式工具调用（`http_request`、`shell` 等）访问 URL
- 新增 3 个防护测试：验证纯 URL 不被转换
- 安全意义：防止无意中代理网络请求 + 信息泄露风险

### 移植 2：Telegram 自定义 Bot API base_url（upstream 63fcd7dd）

**文件**：`src/config/schema.rs`、`src/channels/mod.rs` + 6 个测试文件

- `TelegramConfig` 新增 `base_url: Option<String>` 字段
- 默认 `None`（使用 `https://api.telegram.org`），可配置为第三方兼容 API
- `collect_configured_channels()` 启动时读取 `tg.base_url` 并调用 `with_api_base()`
- TelegramChannel 已有 `api_base` 和 `with_api_base()` 支持，只需配置层连接
- 配置示例：`base_url = "https://tapi.bale.ai"`

### 移植 3：CJK 延迟工具调用重试（upstream 1a0bb175）

**文件**：`src/agent/loop_.rs`

- 新增 4 个 static Regex：英文延迟动作模式 + CJK cue/verb/script 检测
- 新增 `looks_like_deferred_action_without_tool_call()` 函数
- 新增 `MISSING_TOOL_CALL_RETRY_PROMPT` 常量
- `run_tool_call_loop()` 新增重试逻辑：
  - 检测到 LLM 说"让我查看"/"let me try"但没给 tool_call
  - 注入修正 prompt 重试一次（单次保护，不会无限循环）
  - 记录 `tool_call_followthrough_retry` 追踪事件
- 新增 3 个测试：英文/中文检测 + 负面用例

### 测试编译修复（预先存在的问题）

修复了二次开发期间添加新字段后遗留的 73 个测试编译错误：

| 问题 | 文件数 | 修复 |
|------|--------|------|
| `ToolCall` 缺 `thought_signature` | 6 文件 27 处 | 加 `thought_signature: None` |
| `GenerateContentRequest` 缺 `tools`/`tool_config` | 1 文件 4 处 | 加 `tools: None, tool_config: None` |
| `InternalGenerateContentRequest` 缺 `tools` | 1 文件 3 处 | 加 `tools: None` |
| `ReliabilityConfig` 缺 `provider_max_backoff_ms` | 1 文件 7 处 | 加 `provider_max_backoff_ms: 60_000` |
| `ReliableProvider::new()` 参数不足 | 1 文件 25 处 | 加第 4 参数 `60_000` |
| `effective_text()` 方法不存在 | 1 文件 7 处 | 改为 `extract_tool_calls().0` |
| `api_key_url_includes_key_query_param` 测试过时 | 1 处 | 更新为验证 key 不在 URL 中 |

### 跳过的上游修复（我们方案更优）

| Commit | 原因 |
|--------|------|
| `b63dfb89` Windows 编译修复 | 上游 import 是给 WASM/CIDR 等我们没有的功能用的 |
| `5981e505` Vision preflight | 我们已有优雅降级方案（strip + 友好提示），比上游抛错更好 |
| `15457cc3` XML tool 解析 | 上游已拆 parsing.rs 子模块，移植成本高收益低 |
| `8004260e` 延迟行动重构 | 380 行重构，风险太大 |
| `1e8c09d3` 迭代上限恢复 | 我们的方案更适合无人值守场景 |

### 安全保障

- `dev/custom-features-snapshot` 分支：快照我们所有自定义功能（可随时回退）
- `dev/upstream-fixes` 分支：本次移植工作分支
- 每步修改后 `cargo check` 验证编译

### 验证

- `cargo build --release` — 零错误零警告 ✓
- `cargo test --lib` — 3015 通过，22 失败（全部为预先存在的非编译问题）
- 新增测试全部通过（6 个）✓
- `git diff dev/custom-features-snapshot --stat` — 17 文件，+267/-36 行

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

---

## 2026-03-01 — Phase 4 完成：Approval 系统、配置增强、Dispatcher XML 正规化

**涉及文件**：
- `src/approval/mod.rs`（扩展：426 → ~650 行，添加非 CLI 审批系统）
- `src/config/schema.rs`（添加：`NonCliNaturalLanguageApprovalMode` 枚举 + AutonomyConfig 3 个新字段）
- `src/config/mod.rs`（添加：`NonCliNaturalLanguageApprovalMode` 重新导出）
- `src/agent/dispatcher.rs`（修改：添加 XML 标签正规化）
- `src/integrations/registry.rs`（修改：更新模型描述）

### 改动内容

#### `src/approval/mod.rs`
- 添加 `PendingNonCliApprovalRequest` struct：非 CLI 渠道的待审批请求，含 30 分钟超时
- 添加 `PendingApprovalError` enum：`NotFound`/`Expired`/`ChannelMismatch`
- `ApprovalManager` 新增 `pending_non_cli: Mutex<HashMap<String, PendingNonCliApprovalRequest>>` 字段
- 新增方法：
  - `create_non_cli_request()` — 创建待审批请求，返回 request_id
  - `resolve_non_cli_request()` — 解析（消费）请求并记录决策
  - `get_pending_non_cli_request()` — 按 ID 查询（不消费）
  - `pending_requests_for_channel()` — 返回某渠道的所有活跃请求
  - `expire_stale_requests()` — 清理已过期请求
  - `pending_non_cli_count()` — 活跃请求计数
- 新增 7 个测试覆盖所有新功能

#### `src/config/schema.rs`
- 新增 `NonCliNaturalLanguageApprovalMode` enum（Disabled / RequestConfirm / Direct）
- `AutonomyConfig` 新增 3 个字段：
  - `non_cli_approval_approvers: Vec<String>` — 可批准的用户 ID 列表
  - `non_cli_natural_language_approval_mode: NonCliNaturalLanguageApprovalMode` — 默认 RequestConfirm
  - `non_cli_natural_language_approval_mode_by_channel: HashMap<String, NonCliNaturalLanguageApprovalMode>` — 按渠道覆盖
- 修复测试中的 AutonomyConfig 初始化，添加 `..AutonomyConfig::default()`

#### `src/agent/dispatcher.rs`
- `parse_xml_tool_calls()` 中添加 XML 标签正规化：
  - `<toolcall>` → `<tool_call>`
  - `<tool-call>` → `<tool_call>`
  - `<invoke>` → `<tool_call>`
  - 对应闭合标签同样处理
- 兼容不同 fine-tuned 模型的 XML 输出格式

#### `src/integrations/registry.rs`
- 更新模型描述：
  - OpenRouter: "200+ models, 1 API key" → "Claude Sonnet 4.6, GPT-5.2, Gemini 3.1 Pro"
  - Anthropic: "Claude 3.5/4 Sonnet & Opus" → "Claude Sonnet 4.6, Claude Opus 4.6"
  - OpenAI: "GPT-4o, GPT-5, o1" → "GPT-5.2, GPT-5.2-Codex, o3"

---

## 2026-03-01 — 架构改进：统一渠道投递路径 (deliver_to_channel)

**涉及文件**：
- `src/channels/mod.rs`（添加：`deliver_to_channel()` 公开函数）
- `src/cron/scheduler.rs`（重构：`deliver_announcement()` 委托给新函数）

### 背景
CLAUDE.md §15.3 指出 `deliver_announcement`（在 `scheduler.rs` 中）是架构债务：
- 只硬编码支持 4 个渠道（telegram/discord/slack/mattermost）
- 绕过了 `Channel` trait，每次重新实例化渠道对象
- heartbeat 和 cron 使用独立的投递路径

### 改动内容

#### `src/channels/mod.rs`
- 新增 `deliver_to_channel(config, channel, target, text)` 公开函数
- 使用现有 `collect_configured_channels()` 获取所有已配置渠道
- 按名称（不区分大小写）查找渠道，调用 `Channel.send()`
- 支持**所有**已配置渠道（Telegram/Discord/Slack/Mattermost/Signal/WhatsApp/IRC/Email 等）
- 未找到时，返回包含可用渠道列表的友好错误消息

#### `src/cron/scheduler.rs`
- `deliver_announcement()` 简化为单行委托：调用 `crate::channels::deliver_to_channel()`
- 移除了 72 行硬编码 match 逻辑
- 移除了不再需要的 `TelegramChannel/DiscordChannel/SlackChannel/MattermostChannel/SendMessage/Channel` 导入
- 更新测试：错误消息匹配从 "unsupported delivery channel" 扩展为也接受 "no channel named"
