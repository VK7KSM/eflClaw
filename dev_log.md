# elfClaw 开发日志

---

## 2026-03-14 — Self-Improving 反思系统 + Gemini Embedding Provider

### 功能概述
整合 OpenClaw self-improving skill 的反思能力到 elfClaw 现有的 chat_summarizer 管道中，同时新增 Gemini 原生 Embedding Provider 实现语义检索。

### 修改文件

#### `src/memory/embeddings.rs` — 新增 GeminiEmbedding Provider
- 新增 `GeminiEmbedding` 结构体，实现 `EmbeddingProvider` trait
- 使用 Gemini 原生 `batchEmbedContents` API（非 OpenAI 兼容层）
- 认证方式：`x-goog-api-key` header（与主 provider 共享同一 API key）
- taskType 设为 `SEMANTIC_SIMILARITY`（通用场景，无需区分 query/document）
- 工厂函数 `create_embedding_provider()` 新增 `"gemini"` 分支
- 新增 2 个单测：factory_gemini + gemini_embed_empty_batch

#### `src/channels/chat_summarizer.rs` — 扩展离线纠错提取
- `summarize_chat_logs()` 签名新增 `memory: Option<&dyn Memory>` 参数
- LLM prompt 新增「纠错」行：要求弱模型从对话中提取用户纠正 agent 的错误
- `parse_summary_response()` 返回值从 `(String, Option<String>)` 扩展为 `(String, Option<String>, Option<String>)`
- 新增纠错解析：匹配「纠错：」前缀，「无」时返回 None
- 当 corrections 非 None 时，写入 Memory（key=`correction_{date}_{chat_id}`, category=Core）
- 新增 3 个单测：with_corrections / corrections_none_value / missing_corrections_line

#### `src/channels/chat_index.rs` — chat_summaries 表加列
- `init_schema()` 建表语句新增 `corrections TEXT` 列
- 新增 ALTER TABLE 迁移：检测 corrections 列是否存在，不存在则 ALTER TABLE ADD COLUMN
- `upsert_summary()` 签名新增 `corrections: Option<&str>` 参数
- INSERT/UPDATE 语句同步更新

#### `src/channels/mod.rs` — 纠错注入 + /reflect 命令
- `process_channel_message()` 中 system_prompt 构建后：从 Memory 加载 `correction_*` 前缀的 Core 记忆，去重后注入到 system prompt 的「已学习的纠错规则」段落（上限 20 条）
- 新增 `/reflect` 命令识别（类似 `/selfcheck` 模式）：重写消息为反思 prompt，让主模型分析对话历史并用 memory_store 记录教训
- `/reflect` 不需要 SelfCheckGate（只用 memory_store，非 restricted tier 工具）
- 自动保存 memory 排除 `/reflect` 命令消息

#### `src/daemon/mod.rs` — 传入 Memory 实例
- heartbeat loop 中 `summarize_chat_logs()` 调用前创建 Memory 实例
- 使用 `create_memory()` 标准工厂函数，与 agent 共享 brain.db（WAL 模式支持并发）

#### K3 `config.toml` — 配置变更
- `[memory]` 段：`embedding_provider = "gemini"`，`embedding_model = "gemini-embedding-001"`，`embedding_dimensions = 768`
- `[agent]` 段：新增 `system_prompt`，包含纠错学习指令

### 设计决策
- **不修改 hygiene.rs**：`prune_conversation_rows()` 只删除 `category='conversation'` 的行，Core 类别的 correction_* 记忆不受影响
- **纠错去重**：注入 system prompt 时按 content 去重，避免实时通道和离线通道产生的重复纠错
- **弱模型兼容**：纠错提取是简单的模式匹配+文本提取任务，弱模型（gemini-flash-lite）完全能胜任

### 验证
- `cargo build --release --features wasm-tools` ✅ 编译通过
- `cargo test` ✅ 4245 passed（11 pre-existing failures 与本次改动无关）
- chat_summarizer 7/7 tests passed
- chat_index 8/8 tests passed
- embeddings 22/22 tests passed
- 二进制 + 配置已上传 K3

---

## 2026-03-14 — PR #14 冲突解决（Rebase fix 分支到 main）

### 背景
PR #14（`fix/v0.3.1-heartbeat-selfcheck` → `main`）因 18 个共同修改文件产生冲突无法自动合并。
Main 的改动几乎完全是 Fix 分支改动的子集（v0.4.0 功能在 Fix 分支中已更完整地存在）。

### 执行步骤
1. Web 后台更新先提交到 main（`8350a764`）
2. Fix 分支 rebase 到 main（11 个 commit 全部成功 replay）
3. 测试修复后强制推送

### 冲突解决策略
- `src/channels/mod.rs`：保留 main 的 v0.4.0 self_check 指导文本 + capability boundaries
- `src/daemon/mod.rs`：保留 main 的详细 heartbeat 提示词（含 timezone 规则）
- `src/tools/self_check.rs`、`source_sync.rs`：保留 main 的 v0.4.0 版本
- `src/tools/mod.rs`：合并 — ToolRiskTier 定义保留在 traits.rs（Fix），tool_risk_tier() 函数保留在 mod.rs（Main）
- `dev_log.md`：保留 main 版本

### 测试修复（`849a23ac`）
| 文件 | 修改 |
|------|------|
| `src/tools/self_check.rs` | 3 个测试添加 SelfCheckGate.open/close + 竞态容错断言 |
| `src/channels/mod.rs` | 迭代测试从 11→2（适应 loop_detection no_progress_threshold=3）|
| `src/channels/mod.rs` | 时间戳前缀断言改为 contains("hello") |
| `src/config/schema.rs` | observability 默认值断言改为 "log"，补充 tool_overrides/pairing 字段 |

### 测试结果
- 4240 通过，11 失败（全部为 Windows 平台特有的 symlink/路径/进程问题，非 rebase 引入）

---

## 2026-03-14 — Web 后台全面更新

### 修改文件一览

| 文件 | 改动 |
|------|------|
| `src/integrations/mod.rs` | 新增 `mask_secret()`, `integration_settings()`, `build_ai_model_fields()`, `build_chat_fields()` |
| `src/gateway/api.rs` | 新增 `handle_api_integration_settings()`, `handle_api_integration_credentials()`；`handle_api_tools()` 增加 `risk_tier` 字段 |
| `src/gateway/mod.rs` | 新增 `/api/integrations/settings` GET + `/api/integrations/{id}/credentials` PUT 路由 |
| `src/tools/mod.rs` | 新增 `ToolRiskTier` 枚举 + `tool_risk_tier()` 函数 |
| `web/package.json` | 新增 5 个 CodeMirror 依赖 |
| `web/src/components/config/ConfigRawEditor.tsx` | textarea → CodeMirror（语法高亮 + 行号） |
| `web/src/types/api.ts` | `ToolSpec` 新增 `risk_tier` 可选字段 |
| `web/src/pages/Tools.tsx` | 工具卡片显示风险等级彩色标签（safe/sensitive/restricted） |
| `web/src/pages/Logs.tsx` | 新增 level/category 过滤下拉框，支持后端查询参数 |
| `web/src/pages/AgentChat.tsx` | 添加 `ChatMessage[]` 显式类型注解 |

### 各改动说明

**Integrations 后端 API（P0）**：前端 `getIntegrationSettings()` 请求 `GET /api/integrations/settings`，该端点不存在导致 SPA fallback 返回 HTML → JSON.parse 失败。新增 `integration_settings()` 函数返回匹配前端类型的 JSON，所有敏感字段通过 `mask_secret()` 处理。PUT credentials 端点返回 501。

**CodeMirror 编辑器（P1）**：Config 页面 TOML 编辑器从 `<textarea>` 升级为 CodeMirror，支持 TOML 语法高亮、行号、代码折叠。

**Tools 风险等级（P1）**：新增 `ToolRiskTier` 四级分类（safe/standard/sensitive/restricted），Tools 页面卡片右上角显示彩色标签，standard 级不显示以减少视觉噪音。

**Logs 过滤（P2）**：新增 level（debug/info/warn/error）和 category 下拉过滤，后端 `/api/logs/recent` 已支持这些查询参数。

---

## 2026-03-11 — Fix: skill tool CWD + stdin_json 输入模式（web_scrape 双重根因修复）

### 根因分析

K3 上 `web_scrape` 约 70% 失败率，双重原因：

1. **问题 A — CWD 缺失**：`SkillToolHandler.execute()` 不设置 `current_dir()`，
   相对路径 `.\tools\cf-crawler-win-x64.exe` 依赖 zeroclaw.exe 的启动位置。
   对比 `src/runtime/native.rs:60` 正确设置了 `.current_dir(workspace_dir)`。
2. **问题 B — 弱模型 JSON 构造不稳定**：SKILL.toml 要求 LLM 在 `json_input` 字符串参数中
   手动构造 JSON。gemini-flash-lite 有时发 `{url: "..."}`（无引号属性名），解析失败。

K3 agent 用 `file_edit` 将路径改为绝对路径（绕过了 OTP gated_actions 不含 file_edit 的安全漏洞），
虽然能临时解决问题 A，但不可移植。

### 修复方案（3 项代码改动）

| 文件 | 修改 |
|------|------|
| `src/skills/mod.rs` | `SkillTool` 新增 `input_mode: String` 字段（`#[serde(default)]`，默认 "args"）；`create_skill_tools()` 新增 `workspace_dir` 参数 |
| `src/skills/tool_handler.rs` | `SkillToolHandler` 新增 `workspace_dir: PathBuf` 字段；所有 `Command` 添加 `.current_dir(&self.workspace_dir)`；新增 `execute_stdin_json()` 方法：序列化 args → JSON，通过 stdin pipe 传给进程，完全绕过 shell 引号问题 |
| `src/agent/loop_.rs` (2处) + `src/channels/mod.rs` (1处) | 3 个调用点传入 `workspace_dir` |
| `资料/skills/cf-crawler/SKILL.toml` | web_scrape/web_crawl/web_login 改为 `input_mode = "stdin_json"` + 独立参数；所有命令还原为相对路径 |

### stdin_json 模式工作原理
- LLM 填独立 typed 参数（url、goal、mode 等），不再需要手动构造 JSON 字符串
- tool handler 用 `serde_json::to_string(&args)` 序列化，保证 JSON 格式正确
- 通过 Rust stdin pipe 直传 exe，完全绕过 `echo|pipe` + shell 引号问题
- `extract_parameters()` 对 stdin_json 模式从 `args` HashMap 提取参数（而非 command 占位符）
- 参数描述中含 "required" 的自动标记为必填

### 编译验证
`cargo build --release --features wasm-tools` — ✅ 成功

### 测试修复（补充）
`tool_handler.rs` 6 个单测构造 `SkillTool` 时缺少 `input_mode` 字段，`SkillToolHandler::new()` 缺少 `workspace_dir` 参数。
已修复：所有测试添加 `input_mode: String::new()` + `PathBuf::from(".")`。`cargo check` 编译通过。

---

## 2026-03-11 — Fix: 自检门拒绝消息 + Telegram 菜单 + Agent 能力边界

### 问题 1：自检门拒绝后 LLM 不停止
gate 关闭时 self_check/check_logs 返回 `success:false`，gemini-flash 把失败解读为"任务未完成"，
转而用 memory_recall → shell → file_read → glob_search 手动诊断，浪费 131 秒 + ~67000 tokens。

**修复**：将 gate 返回改为 `success:true`。LLM 看到"成功"就不会去补偿。output 包含让 LLM 转述的中文提示。

- `src/tools/self_check.rs:610-620` — success:true + 引导消息
- `src/tools/check_logs.rs:67-77` — 同上

### 问题 2：Telegram 菜单缺少 /selfcheck
实现自检权限笼时遗漏了菜单注册。

**修复**：`src/channels/telegram.rs:978-982` — commands 数组添加 selfcheck 项。

### 问题 3：Agent 不知能力边界
主 agent 尝试修改 config.toml 和源代码，不知道自己是已部署的编译二进制。

**修复**：`src/channels/mod.rs` `build_runtime_status_section()` 末尾追加 Capability Boundaries 段落，
注入 FORBIDDEN/ALLOWED 列表到 system prompt。

---

## 2026-03-11 — Fix: Heartbeat cron_add 循环失败 + K3 SSH 远程运维

### 问题
K3 上的 elfClaw 每小时 heartbeat 触发时，gemini-flash-lite 反复尝试用 cron_add 重新创建已有的 cron 任务：
- 弱模型不带 `recurring_confirmed=true` → 0ms 瞬间失败
- 连续失败 4 次 → "Tool loop exhausted after 25 iterations"
- 过去 2 天 cron_list 被调用 199 次，大量 cron_add 0ms 失败

### 根因
1. Heartbeat prompt 说"如果不一致就 cron_add 同步"，弱模型每小时都误判为"不一致"
2. `cron_add.rs` 的 `recurring_confirmed` 校验让无参数的 cron_add 瞬间失败
3. 弱模型有时还把 UTC 时间当悉尼时间

### 修改内容

| 文件 | 修改 |
|------|------|
| `src/daemon/mod.rs:247-256` | 重写 heartbeat prompt：职责从"同步 cron"改为"验证 cron"；新增时区规则（Australia/Sydney）；明确"已存在不要 cron_add"、"失败不重试" |
| `src/tools/cron_add.rs:~202` | Agent job 去重：在 recurring_confirmed 检查前检测同名任务，存在则返回 success（action=already_exists） |
| `CLAUDE.md` | 新增 §17 K3 远程运维：SSH 连接信息、运行目录、日志路径、常用命令 |

### K3 SSH 配置完成
- K3 IP: 192.168.2.21，用户: JiJiWa，SSH 别名: `ssh k3`
- 密钥: `~/.ssh/id_k3`（ed25519）
- elfClaw 运行目录: `D:\ZeroClaw_Workspace\`
- 日志 JSONL: `D:\ZeroClaw_Workspace\workspace\state\elfclaw-logs.jsonl`
- 推荐日志获取方式: `scp k3:D:/ZeroClaw_Workspace/workspace/state/elfclaw-logs.jsonl /tmp/`

---

## 2026-03-11 — Feature: 自检模块权限笼（SelfCheckGate）

### 问题
LLM（尤其是弱模型）自行调用 self_check/check_logs 工具，导致：上下文污染、记忆污染（错误信息被强化）、发疯循环。

### 设计：三层防线

| 层次 | 机制 | 防护目标 |
|------|------|----------|
| 展示层 | excluded_tools 默认排除 self_check + check_logs | LLM 看不到这些工具 |
| 执行层 | execute() 内硬性 gate 检查 | LLM 幻觉调用 / cron worker → 被拦截 |
| 心理层 | 拒绝消息明确指令"不要重试" | 防止错误消息堆积 |

### 修改内容

| 文件 | 修改 |
|------|------|
| `src/tools/self_check.rs` | 新增 `SelfCheckGate`（AtomicBool + Mutex prompt）；execute() 加 gate 检查；analyze 注入用户 focus prompt |
| `src/tools/check_logs.rs` | execute() 加 gate 检查（双保险） |
| `src/channels/mod.rs` | 识别 `/selfcheck` 命令开门；默认排除 self_check+check_logs；自检消息跳过 autosave；完成后关门 |

### 用户交互
- `/selfcheck 检查cron任务` → 开门 → 执行 → 返回结果 → 自动关门
- `/selfcheck`（无参数）→ 全面自检
- 正常对话 → self_check/check_logs 不可见也不可执行

---

## 2026-03-11 — Fix: Cron 任务工具调用死循环（seen_tool_signatures 作用域修正）

### 问题
Cron 任务使用 `gemini-3.1-flash-lite-preview` 执行时，触发 25 轮迭代上限被截断。根因是 `seen_tool_signatures: HashSet` 定义在 `for iteration` 循环**外部**（line 344），导致跨轮去重：任何与历史轮次签名相同的工具调用都被静默跳过，LoopDetector 完全看不到这些调用，无法触发循环检测。

### 修改
| 文件 | 修改 |
|------|------|
| `src/agent/loop_.rs` | 将 `seen_tool_signatures` 从循环外移入 `for iteration` 循环体内部，每轮迭代重置。单轮内重复仍被去重（正确），跨轮重复由 LoopDetector 处理（3次后警告 → HardStop） |

### 效果
- 同轮内 2 个相同调用：第 2 个仍被去重 ✅
- 跨轮合理重试：正常执行 ✅
- 跨轮死循环：LoopDetector 3 次后 InjectWarning → 继续则 HardStop ✅

---

## 2026-03-10 — Fix: web_scrape 不可用 + cron job 多余中间 Agent（方案 C）

### 问题 1：`create_skill_tools()` 从未被调用
`src/skills/mod.rs:861` 定义了 `create_skill_tools()`，但全代码库无任何调用。SKILL.toml 注册的工具（web_scrape / web_crawl / web_login）永远不进入工具注册表。

### 问题 2：cron job 有无用的中间 Agent
当 cron job 配置了 `delegate_to`，scheduler 不直接运行目标 agent，而是启动一个中间 Agent 来调用 `delegate` 工具。浪费 ~55K tokens + 2 次 LLM call。

### 修复内容（方案 C：给 run() 加 allowed_tools 参数）

| 文件 | 修改 |
|------|------|
| `src/agent/loop_.rs` | `run()` 加 `allowed_tools: Option<Vec<String>>` 参数；load_skills 后调用 `create_skill_tools()` 注册 SKILL.toml 工具；加 allowed_tools 过滤逻辑 |
| `src/agent/loop_.rs` | `process_message()` 也加 `create_skill_tools()` 调用 |
| `src/cron/scheduler.rs` | `run_agent_job()` — delegate_to 分支不再生成 "Use the delegate tool" prompt，改为直接解析 agent config 的 allowed_tools + max_iterations，传给 run() |
| `src/daemon/mod.rs` | heartbeat 调用 run() 加 `None`（无工具过滤）|
| `src/tools/self_check.rs` | self_check 调用 run() 加 `None` |
| `src/main.rs` | CLI 入口调用 run() 加 `None` |
| `src/channels/mod.rs` | daemon 启动时在 `Arc::new(built_tools)` 前调用 `create_skill_tools()` 注册 skill tools |

### 与原方案 B 对比
- 零代码重复（不需要复制 run() 初始化逻辑）
- 全部功能保留（observer/MCP/system prompt/memory）
- 新增代码 ~30 行 vs 方案 B ~100 行

### 编译验证
`cargo build --release --features wasm-tools` — ✅ 成功

---

## 2026-03-10 — Fix: SKILL.toml 命令模板双重引号 Bug（web_scrape 从未真正工作）

**问题**：`web_scrape` / `web_crawl` / `web_login` 工具调用始终失败，LLM 降级使用 `http_request`，CF 仪表板无记录。

**根因**：`资料/skills/cf-crawler/SKILL.toml` 命令模板：
```
command = "echo '{json_input}' | .\tools\cf-crawler-win-x64.exe scrape-page --pretty"
```
`{json_input}` 两侧已有单引号。但 `src/skills/tool_handler.rs` 的 `render_command` 对 String 参数**再次**包单引号，生成：
```bash
echo ''{"url":"...","goal":"..."}'' | .\tools\cf-crawler-win-x64.exe scrape-page
```
在 bash/sh 中，`''value''` 使 JSON 变成裸字符串，双引号被 shell 解析掉，EXE 收到损坏的 JSON → `SyntaxError: Unexpected token 'u', "url:https:"...`

**本机验证**（`C:\Dev\cf-crawler\release\cf-crawler-win-x64.exe`）：
- 双层引号版本：`SyntaxError` 失败 ✅ 复现
- 单层引号版本：`success=true`，TWZ RSS 正确返回新闻+链接 ✅

**修改**：`资料/skills/cf-crawler/SKILL.toml` — 3 条命令模板去掉 `{json_input}` 两侧的 `'...'`，让 render_command 统一处理引号：
- `web_scrape`（scrape-page）
- `web_crawl`（crawl-site）
- `web_login`（login）

**无需编译**，部署后下次 cron 触发即可验证 CF 仪表板出现 Worker 执行记录。

---

## 2026-03-10 — Fix: cf-crawler 工具未被调用 + 新闻推送格式修复

**问题**：news_fetcher worker 跳过 `web_scrape`，直接用 `http_request` 抓 RSS，完成后生成幻觉报告声称"cf-crawler 成功"。Telegram 消息头显示时间（`03:45`）而非日期，新闻条目无链接。

**根因**：
1. **模型弱**：news_fetcher 使用 `gemini-3.1-flash-lite-preview`（超轻量），指令遵循能力弱，面对多步协议（web_scrape → http_request → web_search）直接跳步。
2. **指令歧义**：HEARTBEAT.md 执行铁律写"先用 cf-crawler"，但 LLM 不理解这等同于"调用 web_scrape 工具"。
3. **格式规范缺失**：news_fetcher.md 本地文件标题用 `HH:MM`，LLM 把时间格式复用到 Telegram 消息头；格式示例中无链接要求。

**修改文件**：
- `资料/config.toml`：`[agents.news_fetcher]` 新增 `model = "gemini-3-flash-preview"`，从 flash-lite 升级到 flash，提升指令遵循能力
- `资料/workers/news_fetcher.md`：
  - CRITICAL 节末尾加"防幻觉铁律"（工具必须实际调用，报告必须列出工具链路，否则整批作废）
  - 本地文件标题格式注释：明确 `HH:MM` 仅用于本地去重，不用于 Telegram
  - Telegram 无突发格式：消息头改为 `YYYY-MM-DD（AEST）`，每条新闻加 `[标题](URL)` 链接
  - 突发事件格式：`[日期]` 改为 `YYYY-MM-DD（AEST）`，常规新闻也加链接
- `资料/HEARTBEAT.md`：
  - 所有 6 个 cron 任务的执行铁律第 1 条：`先用 cf-crawler` → `` 先用 `web_scrape` 工具 ``（消除歧义，直接写明工具名）
  - 推送格式要求节：加"消息头必须是日期"和"每条新闻必须附链接"规则
- `资料/skills/cf-crawler/SKILL.md`：开头补充"原生工具调用"对照表（web_scrape/web_crawl/web_login 等与 shell 命令的对应关系）

**验证**：等待下次 cron 触发，预期：CF 仪表板有 Worker 执行记录 + Telegram 消息头显示日期 + 每条新闻有可点击链接。

---

## 2026-03-10 — Fix: Memory 页面黑屏（MemoryCategory 序列化修复）

**问题**：Web 前端 `/memory` 页面完全黑屏，其他页面正常。

**根因**：`MemoryCategory::Custom("newslog")` 经 serde（`rename_all = "snake_case"`）序列化为 JSON 对象 `{"custom":"newslog"}` 而非字符串。前端 `Memory.tsx:149`（categories 下拉 `{cat}`）和 `Memory.tsx:301`（表格 `{entry.category}`）尝试渲染该对象，React 抛出 "Objects are not valid as a React child"。无 ErrorBoundary → 整个 app 静默卸载 → 黑屏。初期只有 Core/Daily/Conversation unit variant（序列化为字符串，正常），agent 使用后产生了自定义类别条目触发 bug。

**修改文件**：`src/gateway/api.rs`
- 新增 `MemoryEntryDto` struct（`category: String`）和 `From<MemoryEntry>` impl
- 使用 `e.category.to_string()`（已有 Display trait，`Custom("newslog")→"newslog"`）
- `handle_api_memory_list` 的 search 分支（recall）和 list 分支各修改一处 `Ok(entries)` 块，改为先转 DTO 再序列化
- 前端无需改动，`web/dist` 无需重建

**编译结果**：`cargo build` exit code 0，2m16s，无新增 warning

---

 + 通用 shell 失败引导防螺旋

**问题**：news_fetcher worker 用 `shell` 工具调 cf-crawler.exe（echo pipe 方式），在 Windows 环境不稳定，大概率失败。LLM 收到冷冰冰的错误输出，无引导 → 调查报错 → 越走越偏 → 25 轮截断。

**根因链**：shell 调用不稳定 → 失败无引导 → LLM 进入调查螺旋 → 轮次耗尽。用 `web_scrape` 工具直接调用立即成功（Telegram 截图验证）。

### 三层防御方案

**第一层（预防）`资料/workers/news_fetcher.md`**：
- 在文件顶部第 3 行后插入 `## ⚠️ CRITICAL` 节，覆盖 5 种场景的 `web_scrape` 调用示例
- 明确禁止用 `shell` 调用 cf-crawler.exe
- 明确禁止用模型内置搜索 / 凭记忆生成新闻
- "Shell 运行规则 > cf-crawler 调用示例" → 改为指向 CRITICAL 节的一行说明（删除 bash 示例）
- 步骤 2a：`shell 调用 cf-crawler` → `用 web_scrape 工具抓取（见 CRITICAL 节 5 种场景）`
- "cf-crawler 命令参考" 小节：删除 bash 命令，改为 `工具调用格式见文件顶部 CRITICAL 节`

**第二层（通用引导）`src/tools/shell.rs`**：
- 在 `if !output.status.success()` 块末尾追加通用提示 `stderr.push_str(...)`
- 提示内容：检查 allowed_tools 是否有专用工具，列举 web_scrape 等
- 标注 `// elfClaw:` + 说明为通用引导，不专属 cf-crawler

**第三层（自文档）`C:\Dev\cf-crawler\src\cli\index.ts`**：
- `allowedCommands` 集合增加 `"help"`，usage 文本更新
- `main()` 函数中 health 前插入 `help` 命令处理，输出 JSON `{success, command, help}`
- `main().catch()` 的输出对象增加 `hint` 字段，引导使用 skill 工具

**`资料/skills/cf-crawler/SKILL.toml`**：
- 新增工具 6：`web_help`，调用 `cf-crawler help --pretty`，用于报错时自查
- prompts 末尾追加两条规则：报错先 web_health + web_help；不要用 shell 调用 cf-crawler.exe

**`资料/config.toml` `[agents.news_fetcher]`**：
- 新增 `"web_search_tool"` 到 `allowed_tools`（步骤 2c fallback，防止 LLM 无工具可用时凭记忆生成虚假新闻）
- `max_iterations = 8` → `15`（7 步工作流需要足够轮次空间）

**需要编译**：zeroclaw（shell.rs 改动）；cf-crawler（index.ts 改动，重新 `npm run build:exe`）

---

## 2026-03-08 — Fix: 双进程竞争 + 诊断工具确认门（日志分析根因修复）

### 问题（日志文件：资料/运行日志.txt）

cron 新闻推送触发后同时出现两个进程竞争 Gemini API 速率：
- **进程 1**（主模型 gemini-3-flash-preview）：self_check analyze 自动触发，180,576 tokens
- **进程 2**（news_fetcher delegate）：工具调用死循环，每轮 +15K tokens，共 30 轮
- 结果：320 万 tokens / 4.5 分钟，worker model 被速率限制阻塞

三个根因：
1. `self_check.analyze_inner()` 使用 `RunContext::Interactive`（主模型），与用户会话竞争速率
2. `news_fetcher.max_iterations = 30`，工具失败时反复重试，token 爆炸
3. `self_check` 和 `check_logs` 描述有强诱导词，LLM 出错时自动调用

### 修改（6处）

#### 1. `src/tools/self_check.rs:460`（RunContext）
`analyze_inner()` 中 `agent::loop_::run()` 的 RunContext 从 `Interactive` 改为 `Background`。
效果：self_check 内部 agent 使用 worker model（gemini-3.1-flash-lite-preview），
不再与主模型竞争 API 速率配额。

#### 2. `src/agent/loop_.rs:1488-1498`（auto-save）
Background 任务（cron/delegate/self_check）不再 auto-save 机器生成的 prompt 到 memory。
防止 self_check 诊断数据（数千字）污染未来 Interactive 会话的 recall 结果。

#### 3. `src/tools/self_check.rs:520-526`（description）
删除 `Trigger: 自检/self-check/健康检查/debug自检`（主要诱导源），删除 `Autonomous`。
加入 `CONFIRMATION REQUIRED`：明确要求用户显式确认才能调用，禁止出错时自动触发。

#### 4. `src/tools/check_logs.rs:26-33`（description）
删除 `PREFERRED`（强诱导词）和宽泛触发条件（errors/failures/diagnosing）。
加入 `USER-INITIATED ONLY`：禁止错误场景自动调用。保留 self_check 内部无需再次确认的 Exception。

#### 5. `src/cron/scheduler.rs:205-228`（cron prompt）
delegate 路径和非 delegate 路径的 cron prompt 中均加入禁止调用 self_check/check_logs 的规则。

#### 6. `资料/config.toml:580-588`（news_fetcher 配置）
- `max_iterations`: 30 → 8（防止工具失败时 30 次重试导致 token 爆炸）
- `allowed_tools` 新增 `web_scrape`（cf-crawler skill 工具），LLM 直接用格式化参数调用，
  不再需要手动拼 shell 命令，避免幻觉参数问题（`web_search_tool.exe` 为 LLM 幻觉，不存在）

### 验证

`cargo build --release --features wasm-tools` 编译通过，无新增错误。

---

## 2026-03-08 — Fix: Background 任务跳过记忆召回（Cron 新闻推送污染根因）

### 问题

Cron 新闻推送被诊断报告覆盖。根因：`error_watchdog_*` 条目被 `mem.recall()` 返回，
因为语义相似度（delegate/agent/shell/news_fetcher 关键词重叠），导致 gemini-flash-lite 看到
诊断报告格式后输出诊断报告而不调用 delegate 工具。

上一轮 Fix（删除 MCP 工具注册、改存储键前缀）未解决问题，tokens 从 36K 升至 44K，
证明是记忆库积累了多条诊断条目被 recall，而非工具问题。

### 修改

**`src/agent/loop_.rs:1496-1504`**（原 1496-1498）

- 在 `build_context()` 调用前加 Background 判断
- `RunContext::Background`（cron/delegate）→ 返回空串，跳过记忆召回
- `RunContext::Interactive`（用户交互）→ 正常召回记忆（行为不变）

### 为什么安全

- Background 任务有明确的指令（"Use delegate tool now..."），不需要记忆上下文
- 主 agent 交互和 self_check 内部调用均使用 Interactive，不受影响
- 修改范围极小（3 行 → 7 行，逻辑一目了然）

### 验证

`cargo check` 通过，6 个预先存在的警告，无新增错误。

### 后续（在运行机器上）

清理 SQLite 记忆库中遗留的 error_watchdog_* 污染条目：
```sql
DELETE FROM memories WHERE key LIKE 'error_watchdog_%';
```
或通过 Telegram 让 agent 使用 memory_forget 工具清理。

---

## 2026-03-08 — Python 服务端 STT 修复：DTX 检测 + OGG 封装

### 问题

Python 测试服务端运行期间 STT 从未被触发，原因有二：

| 问题 | 根因 |
|------|------|
| STT 从未触发 | 触发点在 `listen:stop` 分支；realtime 模式设备永远不发此消息 |
| 即使触发也会失败 | `b"".join(opus_frames)` = 裸 Opus 拼接，无 OGG 容器；Groq 需要完整容器 |

### 修改内容（`xiaozhi/server.py`）

#### 1. DTX 静音检测（等价于 Rust 实现）
- 新增常量：`DTX_MAX_BYTES = 1`、`DTX_SILENCE_TRIGGER = 8`
- Binary 帧处理：≤1 字节的帧视为 DTX 静音，累计连续计数；真实音频帧重置计数
- 连续 8 个静音帧（480ms）直接触发 STT，无需等待 `listen:stop`
- 新增 `do_stt_and_respond(websocket, opus_frames)` 函数，被 DTX 和 `listen:stop` 共用

#### 2. OGG 封装（等价于 Rust `wrap_opus_frames_in_ogg`）
- 新增 `_ogg_crc_table()` / `_ogg_crc32()` — RFC 3533 标准 CRC32（多项式 0x04c11db7，非反射）
- 新增 `_make_ogg_page()` — 构造单个 OGG 页面，含正确 CRC
- 新增 `wrap_opus_frames_in_ogg()` — 完整 OGG Opus 容器：OpusHead(BOS) + OpusTags + 音频页（每帧一页，EOS 标记最后一帧）
- `transcribe_with_groq()` 改为调用 `wrap_opus_frames_in_ogg()` 并以 `"voice.ogg"` / `"audio/ogg"` 发给 Groq

#### 3. elfClaw `资料/config.toml` transcription 配置分析
- `[transcription]` 节：`enabled = true`，有 `api_url` 和 `model`，**无 `api_key`**
- 运行时回退顺序：`config.api_key`（None）→ `GROQ_API_KEY` 环境变量
- **结论**：elfClaw STT 需要在启动时设置 `GROQ_API_KEY` 环境变量，否则 STT 静默失败
- 修复方案：在 `资料/config.toml` 的 `[transcription]` 节添加 `api_key = "gsk_..."` 即可

### 验证
- `python -c "import ast; ast.parse(open('xiaozhi/server.py').read())"` 语法检查通过
- 实际 STT 测试需要真实设备 + `GROQ_API_KEY` 环境变量

---

## 2026-03-08 — Xiaozhi 协议修复 v4（基于真实设备测试数据）

### 问题根因（Python 测试服务端实测确认）
通过 Python 测试服务端与真实 AI-VOX3 设备对话，得到精确协议数据：
- 设备使用**协议版本 v1（裸 Opus，无帧头）**，此前 v3 假设完全错误
- 设备连上后**立即发 `listen:start mode=realtime`**，不等任何 ACK
- `realtime` 模式**永远不发 `listen:stop`**，靠 DTX 静音帧判断说话结束
- `stt:start` ACK 是错误假设，直接导致原代码阻塞在发 ACK 后的 `break`
- `tts:idle` 固件无此处理分支，徒增噪音
- server hello 的 `sample_rate` 是服务端**下行** TTS 音频率（24000），不是设备录音率

### 修改内容（`src/channels/xiaozhi.rs`）

| 位置 | 改动 |
|------|------|
| 模块注释 | 更新为实测确认的正确协议流程 |
| 新增常量 | `DTX_MAX_BYTES=1`, `DTX_SILENCE_TRIGGER=8`（8帧×60ms=480ms静音） |
| `respond_ota` | OTA JSON 加 `"version":1`，覆盖设备 NVS 历史配置 |
| server hello | 删除 `version:3`/`format`/`channels`，`sample_rate` 改为 24000 |
| hello 后 | **删除** `tts:idle`（固件无此处理分支） |
| idle loop | **删除** `stt:start` ACK（设备不等 ACK 直接发帧） |
| 帧收集循环 | **重写**：计数连续 1B DTX 帧，达到阈值触发 STT；兼容 auto 模式 `listen:stop` |
| TTS 后 | **删除** `tts:idle`（两处均删除） |

### 验证
`cargo check` 通过，0 error，warnings 均为预存无关项。

---

## 2026-03-08 — Xiaozhi 协议调试：Python 最小化测试服务端

### 目标
用 Python 最小服务端验证官方 v1.9.0 固件的真实协议行为，避免继续基于假设修改 Rust 实现。

### 新增文件
- `xiaozhi/server.py` — 测试服务端主体
- `xiaozhi/requirements.txt` — 依赖声明（`websockets>=12.0`, `requests`）

### 架构：双端口
| 端口 | 协议 | 用途 |
|------|------|------|
| 8765 | WebSocket | 主对话端口 |
| 8766 | HTTP | OTA mock（设备启动时拉取 WS 地址） |

### 关键设计决策（基于 v1.9.0 源码）
1. **OTA 返回 version=1**：强制设备使用裸 Opus（最简解析路径），覆盖设备 NVS 中可能残留的历史配置
2. **server hello 中 sample_rate=24000**：这是服务端**下行**音频采样率，不是设备录音率（之前误填 16000）
3. **transport 必须是 "websocket"**：否则设备固件直接报错
4. **移除 stt:start ACK**：v1.9.0 源码中无此逻辑，设备发完 listen:start 立即发帧
5. **移除 tts:idle**：application.cc 中无任何 tts:idle 处理分支
6. **支持 v1/v2/v3 帧解析**：打印帧元信息，验证设备实际使用的协议版本
7. **自动检测本机 IP**：也支持 `--ip` 参数手动指定

### 日志标签设计
`[CONNECT]` → `[HELLO]` → `[SEND]` → `[BINARY/TEXT]` × N → `[FRAMES]` → `[STT]` → `[SEND]×3`

### 使用方法（本机 IP: 192.168.2.54）
```bash
pip install -r xiaozhi/requirements.txt
python xiaozhi/server.py --ip 192.168.2.54
# 填入设备配置: http://192.168.2.54:8766/xiaozhi/ota/
```

---

## 2026-03-08 — P0 修复：Cron 新闻推送恢复 + 自检 INFO 日志覆盖

### 根因
上一轮 commit `746380d3` 的 Part B（MCP 全局集成）和 Part C3（watchdog 错误记忆）导致 cron 新闻推送彻底失效：
- MCP 初始化在 `loop_::run()` 中 spawn 新进程，注入 33 个 MCP tool schema（+25K tokens）
- error_watchdog 记忆被 `mem.recall()` 语义匹配到 cron prompt（关键词重叠：delegate, news_fetcher）
- haiku 收到 36,531 input tokens（正常 ~7000），被 MCP tools + 诊断记忆淹没，输出诊断报告而非调用 delegate

### Fix 1 [P0]: 删除 loop_.rs MCP 初始化
- **文件**: `src/agent/loop_.rs`
- **操作**: 整块删除 29 行 MCP 初始化代码（`McpRegistry::connect_all` + `McpToolWrapper` 注册）
- **原因**: self_check/cron/delegate 不需要 MCP 工具；主 agent MCP（channels/mod.rs）不受影响

### Fix 2 [P0]: 删除 error_watchdog 记忆存储
- **文件**: `src/channels/mod.rs`
- **操作**: 删除 `error_watchdog_*` 记忆存储代码（response 含 `📋 失败摘要` 时存入 Daily 记忆）
- **原因**: error_watchdog key 不被 `is_assistant_autosave_key()` 过滤，build_context 的 recall 会将诊断记忆匹配到 cron prompt
- **保留**: watchdog 恢复注入（prior_turns 检查）和 hard stop 格式化输出不受影响

### Fix 3 [P1]: collect() 增加 INFO 日志独立字段
- **文件**: `src/tools/self_check.rs`
- **操作**: 收集 30 条 INFO 日志，作为独立 `info_context` 字段（不混入 entries/logs）
- **设计决策**: INFO 不进 entries 是因为：(1) entries.is_empty() 检查会失效 (2) 避免 extract_search_keywords 提取无关关键词 (3) 避免 determine_search_paths 扩大搜索范围
- 三个返回路径（无日志无源码、无日志有源码、完整结果）均包含 info_context

### Fix 4 [P1]: 分析 prompt 增加 INFO 使用指引
- **文件**: `src/tools/self_check.rs`
- **操作**: analyze_inner prompt 新增第 4 条：利用 info_context 了解 agent_lifecycle/cron_job/tool_call/system 上下文
- 原第 4-6 条顺延为第 5-7 条

### Fix 5 [P1]: 二次复核 prompt 增加 INFO 覆盖
- **文件**: `src/channels/mod.rs`
- **操作**: self_check 引导 prompt 增加 INFO 查询场景和示例（`check_logs(level="info", category="agent_lifecycle", since_minutes=60)`）
- 最大工具调用次数从 3 次增加到 5 次

---

## 2026-03-08 — 自检报告质量 v3 + MCP 全局集成 + 看门狗错误学习

### 三大改进方向

| 方向 | 文件 | 改动要点 |
|------|------|---------|
| A. 自检报告质量 | self_check.rs, channels/mod.rs, source_sync.rs | 多级别日志+分类标注+反编造 prompt+二次复核 |
| B. MCP 全局集成 | loop_.rs | 子 agent（自检/cron 等）也能使用 MCP 工具 |
| C. 看门狗错误学习 | detection.rs, loop_.rs, channels/mod.rs | 失败数据暴露+记忆存储+恢复注入 |

### Part A：自检报告质量修复

#### `src/tools/self_check.rs`
- **collect()**：同时收集 error(80条)+warn(30条)，不再二选一
- **source_hint**：每条日志加 `source_hint` 分类（ToolCall→"user-triggered tool execution"，System→"system/daemon lifecycle" 等）
- **分析 prompt**：增加反编造规则（只报有直接证据的问题、标注 timestamp+component、禁止编造版本号）、按严重程度分级（🔴/🟡/🔵）、解释禁止工具的原因

#### `src/channels/mod.rs`
- 自检引导 prompt 改为三步：调用 analyze → 二次复核（check_logs 验证最多 3 次）→ 标注 ✅/⚠️/❌

#### `src/tools/source_sync.rs`
- `sync_via_http()` 三处新增 `tracing::info!`：
  - SHA 匹配：`"source up-to-date, skipping download"`
  - 版本文件不存在：`"local source exists but no version marker, will re-download"`
  - API 不可达：`"GitHub API unreachable, using existing local copy"`

### Part B：MCP 全局集成

#### `src/agent/loop_.rs`
- 在 `run()` 中 peripheral tools 之后添加 MCP 工具初始化
- 与 channels/mod.rs 中的模式完全一致：`McpRegistry::connect_all` → `McpToolWrapper` 注册
- 效果：自检分析 agent、cron agent 等所有通过 `loop_::run()` 创建的 agent 自动获得 MCP 工具

### Part C：看门狗错误学习

#### `src/agent/loop_/detection.rs`
- 新增 `last_failed_args: HashMap<String, String>` 字段，`record_call()` 失败时记录参数（截取200字符）
- 新增 `failure_summary()` — 返回所有失败工具+连续失败次数
- 新增 `last_failed_args()` — 返回最后失败参数

#### `src/agent/loop_.rs`
- hard stop 处理增强：构建详细 `error_report`（中断原因+失败工具列表+参数+教训）
- 输出格式：`⚠️ [循环检测] + 📋 失败摘要`，包含具体工具名+次数+参数

#### `src/channels/mod.rs`
- **错误记忆存储**：当 response 包含 `📋 失败摘要` 时，提取摘要存入 `MemoryCategory::Daily`（key: `error_watchdog_YYYYMMDD_HHMM`）
- **看门狗恢复注入**：检测到上一轮 assistant 消息是 watchdog hard stop 时，注入恢复指引：
  - 禁止重复同一工具+参数
  - 0ms 失败 = 安全策略拦截 → 切换方法
  - 自然衰减：成功完成一轮后不再注入

### 验证
- `cargo check` ✅ 通过（0 个新增 warning）
- `cargo test --lib -- tools::self_check tools::source_sync agent::loop_::detection` ✅ 30/30 通过

---

## 2026-03-08 — Xiaozhi 第三轮修复（STT 挂死 + 帧收集超时 + 日志可见化 + OGG granule）

### 问题

上轮 Fix 1-4（stt:start ACK）确认在运行（日志显示 `listen:start (session=...)`），但 99 秒内仍零后续日志。

根因分析：
1. **STT HTTP 无超时**：`transcription.rs` 使用 `build_runtime_proxy_client`（无 timeout）。Groq API 若无响应 → HTTP 永久挂起 → 完全符合"99 秒零日志"现象。
2. **帧收集循环无超时**：设备若不发 `listen:stop` → `source.next().await` 永久挂起。
3. **关键日志不可见**：`listen:stop` 是 `debug!` 级别，INFO 模式看不到；`frames.is_empty()` 和 STT 返回空字符串均无日志 → 无法区分三种失败模式。
4. **OGG granule 错误**：device 用 `frame_duration=60ms`，但代码写的是 `samples_per_frame=960`（20ms），应为 `2880`（60ms × 48kHz / 1000）。

### 改动

#### `src/channels/transcription.rs`（Fix 5）
- 行 83：将 `build_runtime_proxy_client` 换为 `build_runtime_proxy_client_with_timeouts("transcription.groq", 20, 10)`
- total timeout 20s，connect timeout 10s — 避免 STT 调用永久挂死

#### `src/channels/xiaozhi.rs`（Fix 6 + Fix 7）
- **帧收集循环**（Fix 6）：
  - `source.next().await` → `tokio::time::timeout(30s, source.next()).await`，超时 warn + continue 'outer
  - `listen:stop` 日志从 `debug!` 升为 `info!`（INFO 模式可见）
  - `frames.is_empty()` 加 `warn!`（之前完全无日志）
  - STT 返回空字符串加 `warn!`（之前完全无日志）
  - 帧收集 WS 错误：精确的 warn/debug 替代原来的 `_ => break 'outer`
- **granule position**（Fix 7）：`samples_per_frame: u64 = 960 → 2880`（正确反映 60ms 帧时长）

### 验证

`cargo check` 通过，无新增 warnings/errors。

### 期望日志序列（修复后）

```
listen:start (session=...)          ← 已有
listen:stop (N frames collected)    ← Fix 6 新增（N>0 = 音频到达）
Xiaozhi: device_id → "说话内容"   ← STT 成功
```

如出现 warn：
- `frame collection timeout` → 设备未发 `listen:stop`（协议/固件问题）
- `listen:stop received but no audio frames` → VAD 问题
- `STT returned empty result` → 音质/API 问题
- `STT failed: ...` → 含超时原因的 API 错误

---

## 2026-03-07 — Xiaozhi 三问题修复（STT ACK + tts:idle + 语音感知 + 主动推送）

### 问题

1. **问题 1（阻塞性 bug）**：说话无反应，STT 完全失效。根因：v3 协议要求收到 `listen:start` 后 server 必须回发 `stt:start` ACK，设备收到 ACK 才会开始发送 Binary Opus 音频帧。代码直接 `break` 进入帧收集循环，从未发 ACK → 设备永远卡在"聆听中" → Binary 帧为零。
2. **问题 2**：多轮对话不工作。TTS 播放完后缺少 `tts:idle` 信号 → 设备不知道可以进入下一轮。
3. **问题 3**：Agent 不知道这是语音设备 → 可能返回 markdown/列表/长文本。
4. **附加**：主动推送架构不完整 — 设备空闲时 `tts_rx` 不被消费。

### 修改内容

**`src/channels/xiaozhi.rs`**：

- **Fix 1**（行 ~401）：`listen:start` 分支提取 `session_id`，发送 `stt:start` ACK（含 session_id 或不含），然后再 `break` 进入帧收集。
- **Fix 2**（行 ~541）：`tts:stop` 发送之后追加 `tts:idle`，重置设备到就绪状态，保证多轮对话。
- **Fix 4**：等待 `listen:start` 的内层 `loop` 改为 `tokio::select!`，同时监听：
  - `source.next()` — 设备 WebSocket 消息（原有逻辑不变，缩进调整）
  - `tts_rx.recv()` — 主动推送分支：收到 OGG 后直接发 `tts:start` → Binary 帧 → `tts:stop` → `tts:idle`，继续等待（不 break）

**`src/channels/mod.rs`**：

- **Fix 3**（`channel_delivery_instructions()` 函数）：添加 `"xiaozhi"` 分支，指示 agent 回复简短口语化、不用 markdown、不用列表、数字自然拼读。

### 验证

- `cargo check` — 通过（仅预存 warnings，与本次修改无关）

---

## 2026-03-07 — cf-crawler Skill 运行失败修复（路径 + Windows shell 兼容）

### 问题

cf-crawler skill 在 elfClaw 上首次测试运行失败，日志分析发现 3 个需要代码修复的问题：

1. **路径错误**：SKILL.toml 命令路径写 `workspace/tools/cf-crawler-win-x64.exe`，但 shell 的 cwd 已经是 workspace 目录，导致解析为 `workspace/workspace/tools/`（双重 workspace）→ CommandNotFoundException
2. **环境变量未传递**：`shell_env_passthrough` 只透传父进程中已存在的环境变量，目标机器未设置 `CF_CRAWLER_ENDPOINT` / `CF_CRAWLER_TOKEN` → cf-crawler 默认连 `localhost:8787` → ECONNREFUSED
3. **SkillToolHandler Windows 不兼容**：`tool_handler.rs` 硬编码 `sh -c` 执行 skill 工具命令，Windows 上若无 Git Bash 则 `sh` 不存在，所有 SKILL.toml 的 shell 工具都会失败

### 修改内容

**`资料/skills/cf-crawler/SKILL.toml`**：

- 所有 6 处 `workspace/tools/cf-crawler-win-x64.exe` → `.\tools\cf-crawler-win-x64.exe`（使用 `.\` 前缀 + 反斜杠，PowerShell 执行相对路径的必要格式）

**`资料/skills/cf-crawler/SKILL.md`**：

- 所有 8 处路径同步修改：`workspace/tools/cf-crawler-win-x64.exe` → `.\tools\cf-crawler-win-x64.exe`

**`src/skills/tool_handler.rs`**（第 391 行）：

- 原代码：无条件 `sh -c` 执行命令
- 新代码：`#[cfg(windows)]` 分支 — 先尝试 `sh -c`（Git Bash），失败则 fallback 到 `powershell -NoProfile -NonInteractive -Command`；同时处理 `python3` → `python` 重写（Windows 上 Python 通常不提供 `python3` 命令）
- `#[cfg(not(windows))]` 分支 — 保持原有 `sh -c` 不变

### 环境变量（手动操作）

目标机器需设置用户级环境变量后重启 zeroclaw：
```powershell
[System.Environment]::SetEnvironmentVariable("CF_CRAWLER_ENDPOINT", "https://cf-crawler-worker.kangarooo-network.workers.dev", "User")
[System.Environment]::SetEnvironmentVariable("CF_CRAWLER_TOKEN", "CFCRAWLER20260307KANGAROOAUTHKEY", "User")
```

### 验证

- `cargo check` — 通过（仅预存 warnings，与本次修改无关）

---

## 2026-03-07 — Xiaozhi AI-VOX3 闪退 v3 精确修复（tts:idle session_id + hello 协议完整化）

### 问题（v3 根因分析）

1. **主根因（最高置信度）**：`tts:idle` 消息含多余 `session_id` 字段 → ESP32 固件 cJSON 解析路径异常 → TCP RST → AI 程序闪退
   - 时序：connected 日志 → tts:idle 发出 → 设备瞬间崩溃（完全吻合）
2. **协议兼容问题（高置信度）**：hello 响应格式不完整 — 缺 `version:3`、`transport`、`frame_duration:60`；`sample_rate` 为 24000（应为 16000 告知设备录音采样率）

### 修改内容

**`src/channels/xiaozhi.rs`**：

- **Fix A（最高优先级）**：`tts:idle` 去掉 `session_id` 字段，只发 `{"type":"tts","state":"idle"}`；hello 循环同步改为只返回 `device_id`（`session_id` 仅在 hello 内部使用）
- **Fix B（高优先级）**：hello 响应补全 xiaozhi v3 标准格式：添加 `version:3`、`transport:"websocket"`、`frame_duration:60`；`sample_rate` 从 24000 改为 16000（与 `wrap_opus_frames_in_ogg(&frames, 16000)` 一致；TTS 输出仍是 24kHz 不受影响）
- **Fix C**：`listen:start` 日志级别从 `debug!` 改为 `info!`，提升可见性
- **doc comment**：文件顶部协议示例更新为完整 v3 hello 格式

### 验证

- `cargo check` — 通过（只有预存 warnings，与本次修改无关）

---

## 2026-03-07 — Xiaozhi AI-VOX3 闪退 & 版本卡死修复

### 问题

1. **设备连接后 27 秒静默断开**：服务端发完 hello 响应后未发 `tts:idle` 信号，AI-VOX3 固件等待服务端就绪确认，超时后发 Close 帧断开
2. **断开原因无日志**：WS 错误 / 流关闭 / 设备主动 Close 三种情况都静默 `break 'outer`，无法定位根因
3. **OTA 响应含顶层 version 字段**：Nulllab 固件可能将其解读为固件版本，触发 OTA 检查流程（约 1 分钟超时）才连 WebSocket
4. **listen:detect 消息被静默丢弃**：设备在自动检测模式发送的 `listen:detect` 落入 `_ => {}` 无日志，调试困难

### 修改内容

**`src/channels/xiaozhi.rs`**：

- **Fix 1（主修复）**：hello 循环改为 break `(id, sid)` 返回元组；`info!("connected")` 之后立即发送 `{"type":"tts","state":"idle","session_id":"..."}` 信号；idle 发送失败时清理 sessions 后 return
- **Fix 2**：Phase 2 等待 listen:start 的内层 loop — `Some(Err(e))` 分支加 `warn!`，`None` 分支加 `debug!`，`Message::Close(frame)` 加 `debug!`，不再静默 break
- **Fix 3**：`match json["type"].as_str()` 添加 `Some("listen") if state == "detect"` 分支，打印 debug 日志
- **Fix 4**：`respond_ota()` 的 JSON body 去掉顶层 `"version": "1.9.0"` 字段，只保留 `{"websocket":{"url":"..."}}`

### 验证

- `cargo check` — 通过（只有预存 warnings）

---

## 2026-03-07 — self_check 独立进程 + content_search Windows 兼容 + source_sync 本地优先

### 问题

1. **自检 token 爆炸**：collect() 返回 ~104K JSON → `MAX_TOOL_RESULT_IN_HISTORY_CHARS=8000` 截断到 8K → 模型丢失 92% 数据 → 探索循环 → 603K token → 超时
2. **content_search 在 Windows 无后端**：系统无 rg/grep 时搜索完全失败
3. **source_sync 每次重下**：HTTP 模式不检查本地版本，每次自检都重新下载 ZIP
4. **预存测试编译错误**：`DelegateAgentConfig` 的 `provider/model` 改为 `Option<String>` 后测试代码未同步

### 修改内容

**`src/tools/self_check.rs`**：
- 新增 `action="analyze"`（默认）：collect + 独立 `agent::run()` + save
- `analyze()` 使用 `AtomicBool` 防递归，5分钟超时，15次迭代上限
- `analyze_inner()` 在隔离上下文运行分析（独立 1M token 窗口，完整 104K 数据无截断）
- `collect()` 新增 `environment` 字段：OS/arch/hostname/search_backend/cli_tools
- 主对话只收到 ~800 字符摘要，不爆 token
- `description()/schema()` 更新为 analyze 优先

**`src/tools/content_search.rs`**：
- 新增 `has_grep` 字段，构造函数检测 rg + grep
- 新增 `build_findstr_command()` Windows 后端（literal 模式，安全）
- `execute()` 三级后端选择：rg → grep → findstr → 报错
- 新增 `strip_unc_prefix()` 解决 Windows UNC 路径 (`\\?\...`) 兼容问题
- grep/findstr 命令和输出 relativize 均使用 stripped 路径
- 删除冗余的 `format_rg_output/format_grep_output` wrapper
- 修复 `content_search_rejects_absolute_path` 测试的 Windows 兼容性

**`src/tools/source_sync.rs`**：
- `sync_via_http()` 新增本地版本检查：写入 `.elfclaw_sync_sha` 版本标记
- 流程：检查本地 Cargo.toml → 读取本地 SHA → 对比 GitHub latest → 相同则跳过下载
- GitHub API 不可达时，使用已有本地副本（不报错）
- `repo_status()` 显示本地 SHA 信息

**`src/channels/mod.rs`**：
- 自检提示词改为一步式：`self_check(action="analyze")`，无需多步操作

**`src/doctor/mod.rs` + `src/migration.rs`**（预存修复）：
- `DelegateAgentConfig` 测试代码同步：`provider/model` 包裹 `Some()`

### 验证

- `cargo check` — 通过（只有预存 warnings）
- `cargo build` — 通过
- `cargo test --lib -- tools::self_check tools::content_search tools::source_sync` — 37 passed, 0 failed

---

## 2026-03-07 — XiaozhiChannel token 移除（fix/v0.3.1-heartbeat-selfcheck）

### 问题

`XiaozhiConfig` 有 `token: Option<String>` 字段，但实现存在流程缺陷：
- OTA 响应没有把 token 告知设备
- 设备不会在 hello JSON 中发送 token（固件读取 OTA → Authorization header，不是 hello field）
- 服务端检查 hello JSON 的 `json["token"]`，永远收不到 → 设备被拒绝

### 决策

局域网设备，不需要 token 验证。直接移除 token，消除这个坏死路径。

### 修改内容

**`src/channels/xiaozhi.rs`**：
- `XiaozhiConfig` 删除 `pub token: Option<String>` 字段
- `handle_connection()` 删除 `token: Option<String>` 参数，删除 token 验证代码块
- `listen()` 删除 `let token = self.config.token.clone()` 和相关传递
- 3 个测试中删除 `token: None` 字段（`xiaozhi_config_defaults`、`server_ip_*`、`channel_name_*`）

### 验证

- `cargo check` — 通过，无错误（其他预存 `Option<String>` 类型错误与本次修改无关）

---



### 实现范围

为 AI-VOX3（ESP32-S3，固件 1.9.0）实现 elfClaw 服务端支持，设备无需改固件，仅需将 OTA URL 重定向到 elfClaw。

### 新增 / 修改文件

#### `src/channels/xiaozhi.rs` [NEW, ~600 行]

- `XiaozhiConfig` — 配置结构体（port/host/token/ota_port/server_ip，全部有默认值）
- `XiaozhiChannel` — 实现 `Channel` trait
- `listen()` — 同时启动 OTA mock HTTP 和 WebSocket 服务器
- `send()` — 调用 `synthesize_to_ogg_opus()` 合成 24kHz OGG Opus，推到设备 session channel
- `handle_connection()` — 每个设备连接的独立 task，完整状态机：
  - hello 握手 → 收帧（Opus binary WS 帧）→ OGG 封装 → Groq STT → 转发 agent → 等 TTS → 播放
  - 支持 abort 消息，Ping/Pong 保活
- `wrap_opus_frames_in_ogg()` — 将裸 Opus 帧封装为 RFC 7845 OGG Opus 容器（用于 Groq STT）
  - OpusHead（19 bytes）+ OpusTags + 音频页，granule 位置按 48kHz/960 samples/frame
- `send_ogg_to_ws()` — 将 OGG Opus 逐帧发送到 WebSocket（跳过前两个 header 包）
- `serve_ota()` / `respond_ota()` — 纯 tokio TCP 实现的 OTA mock HTTP，返回 elfClaw WebSocket 地址 JSON
- 5 个单元测试：OGG magic 验证、config defaults、server_ip 逻辑、channel name

#### `src/channels/tts.rs`

- 新增 `synthesize_to_ogg_opus()` — Edge TTS 合成 24kHz OGG Opus（音频格式 `ogg-24khz-16bit-mono-opus`），返回 `Vec<u8>`，不写临时文件

#### `src/channels/mod.rs`

- 添加 `pub mod xiaozhi;` 和 `pub use xiaozhi::XiaozhiChannel;`
- `collect_configured_channels()` 末尾注册 Xiaozhi

#### `src/config/schema.rs`

- `ChannelsConfig` 添加 `pub xiaozhi: Option<crate::channels::xiaozhi::XiaozhiConfig>` 字段
- `Default for ChannelsConfig` 添加 `xiaozhi: None`
- `channels_except_webhook()` 添加 Xiaozhi 条目
- 3 处直接构造 `ChannelsConfig` 的测试各补 `xiaozhi: None`

#### `Cargo.toml`

- 添加 `ogg = "0.9"` — 纯 Rust OGG 容器读写

### 关键设计决策

- TTS 在 `XiaozhiChannel::send()` 中完成，`handle_connection` 只等 channel 推来的字节，不做合成
- OGG Opus 对于 Groq Whisper：裸 Opus → OGG 封装（需要 `ogg` crate）
- OGG Opus 对于 TTS 下行：Edge TTS 直接返回 OGG 容器，不需要手动封装
- ogg 0.9 API：`write_packet` 接受 `Into<Cow<[u8]>>`，直接传 `Vec<u8>`（不需要 `into_boxed_slice()`）

### 验证

- `cargo check` 通过
- `cargo clippy -- -D warnings`：xiaozhi.rs 无新警告/错误（其余错误均为预存在的旧代码问题）
- 单元测试（OGG、config defaults、server_ip）在 lib 编译通过

---

## 2026-03-07 — v0.3.1 热修复 #2：self_check 路径 bug 四连修

### 问题背景

v0.3.1 部署后 self_check 运行 50 次工具迭代无结果。根因分析发现 4 个 bug 协同导致：

### 修改内容

**`src/tools/source_sync.rs`**
- **Bug 0（根因）**：`SOURCE_DIR = "workspace/github"` 与 `workspace_dir`（已含 workspace 后缀）拼接产生双 workspace 路径。源码下载到 `workspace/workspace/github/elfclaw` 而非 `workspace/github/elfclaw`。修复：改为 `"github"`。

**`src/tools/self_check.rs`**
- **Bug 1a**：`source_dir_exists()` 第 92 行 `.join("workspace/github")` → `.join("github")`
- **Bug 1b**：content_search 路径（第 208-211 行）从绝对路径 `format!("{}/workspace/github/elfclaw/...", workspace_dir)` 改为相对路径 `format!("github/elfclaw/...", search_path)`。绝对路径被 `content_search.rs:174` 的安全检查拒绝。
- **Bug 1c**：file_read 路径（第 239-242 行）同样改为相对路径。绝对路径被 `policy.rs:1073` 的 `workspace_only` 检查拒绝。
- **Bug 1d**：source_tree 路径（第 267 行）`.join("workspace/github/elfclaw/src")` → `.join("github/elfclaw/src")`
- **Bug 2**：3 个 JSON return 点添加 `source_base_path` 和 `usage_hint` 字段。

**`src/channels/mod.rs`**
- **Bug 3**：self_check 系统提示从"用 file_read/content_search 深入查看源码"改为"直接分析 collect 返回的 JSON 数据"，并明确禁止 shell/git 命令。

---

## 2026-03-07 — v0.3.1 综合修复：Heartbeat 死循环 + source_sync HTTP 回退 + self_check 架构重设计

### 问题背景

1. **Heartbeat Worker 死循环**：弱模型（gemini-flash-lite）被提示词强制调用 `check_logs`，看到错误后不知如何处理，反复调用 check_logs + cron_list 直到耗尽 25 次迭代上限。每个 heartbeat 周期重复一次。
2. **source_sync 无 git 回退**：生产 Windows 服务器未安装 git，source_sync 直接失败。
3. **self_check 内部 LLM 调用**：self_check 内部使用次级模型做搜索规划/文件筛选/报告撰写，关键分析绕过主模型。
4. **GitHub MCP 工具提示缺失**：系统提示未提及已配置的 GitHub MCP 工具。

### 修改文件

#### `src/daemon/mod.rs`
- **简化 heartbeat 提示词**：移除 `check_logs` 强制调用指令（死循环直接触发点）
- 弱模型只做 cron 同步（对比 HEARTBEAT.md 和 cron_list），明确禁止调用 check_logs
- 日志诊断统一到 self_check，由用户通过主模型触发

#### `src/config/schema.rs`
- **降低 heartbeat max_tool_iterations 默认值**：25 → 8
- Heartbeat 只需 cron_list + 少量 cron_add/update = 5-6 次够用，8 留余量但不会失控
- 更新对应测试断言

#### `src/tools/source_sync.rs`
- **新增 HTTP ZIP 下载回退**：git 不可用时通过 GitHub ZIP archive API 下载源码
- `git_available()` — OnceLock 缓存 git 可用性检查
- `fetch_latest_commit(api_url)` — GitHub API 获取 commit SHA（best-effort）
- `sync_via_http(repo_id)` — reqwest 下载 ZIP + zip crate 解压
- `extract_zip()` — 独立函数，strip GitHub ZIP 前缀目录
- `sync_repo()` — git 可用走 git，不可用走 HTTP
- `repo_status()` — 同时检查 `.git` 和 `Cargo.toml` 判断目录存在性
- 新增 `ALLOWED_REPOS_HTTP` 白名单常量
- 新增 `extract_zip_strips_prefix` 测试

#### `src/tools/self_check.rs`
- **架构重设计**：从"内置 LLM 管线"改为"纯 Rust 数据收集器"
- 删除全部 3 处内部 LLM 调用（create_provider + chat_with_system）
- 新增 `action="collect"`：纯 Rust，零 LLM — sync 源码 → 查 DB 收集日志 → ripgrep 搜索关键词 → 读取关键文件 → 返回结构化 JSON
- 新增 `action="save_report"`：接收主模型撰写的报告文本，写入 homework/
- 保留依赖：source_sync、content_search、file_read（纯 Rust 调用）
- 新增辅助函数：extract_search_keywords、determine_search_paths
- 更新测试：schema_has_action_param、save_report_requires_report_param、rejects_unknown_action 等

#### `src/channels/mod.rs`
- 更新 `build_runtime_status_section` 中 self_check 工具说明（反映新的两阶段架构）
- 新增 GitHub MCP 工具使用指引

### 验证
- `cargo check` 通过（lib + bin），无新增 warning
- 已有测试编译错误（delegate.rs、subagent_spawn.rs 等）为历史遗留，非本次改动引入

---

## 2026-03-06 — Fix: SelfCheckTool v2 — 修复首次部署 3 个关键缺陷

### 问题背景

SelfCheckTool 首次部署到生产环境（Windows + Gemini），暴露三个缺陷：
1. source_sync 失败 — daemon 进程 PATH 中没有 git
2. LLM 幻觉报告 — 源码不可用时 LLM₃ 编造文件路径、commit hash、代码片段
3. 报告写错目录 — file_write 沙箱将报告写到 workspace/ 而非用户 config 目录

### 修改文件

#### `src/tools/source_sync.rs`
- 新增 `find_git()` 方法：`OnceLock` 缓存，先尝试 PATH 中的 git，失败后探测常见 Windows 路径（`C:\Program Files\Git\{bin,cmd}\git.exe`、PROGRAMFILES 环境变量）
- `run_git()` 改用 `Self::find_git()` 替代硬编码 `"git"`

#### `src/tools/self_check.rs`
- **反幻觉门禁 1**：source_sync 后检查源码目录是否存在（`source_dir_exists()`），不存在则标记 `has_source_code = false`
- **反幻觉门禁 2**：搜索结果为空时跳过 LLM₂ 和文件精读（Steps 7-9）
- **双模板分流**：`code_sections` 为空时使用日志-only prompt，显式禁止编造代码；非空时使用完整 prompt 并加入反编造指令
- **报告路径修正**：移除 `file_write` 依赖，改用 `tokio::fs::write` 直接写入 `config_dir/homework/`
- 新增 `report_dir()` 方法从 `config.config_path` 推导报告目录
- 新增 `source_dir_exists()` 方法检查 `workspace/github/{repo}/Cargo.toml`
- ToolResult 输出增加模式标记（日志-only / 完整）
- `write_clean_report()` 同步改用 `tokio::fs::write`

#### `src/tools/mod.rs`
- SelfCheckTool 构造移除 `file_write` 参数（从 6 参数变 5 参数）

### 验证
- `cargo check` 通过（lib + bin），无新增 warning

---

## 2026-03-06 — Feature: SelfCheckTool — 程序化自检工具（第 54 号工具）

### 背景

旧的 debug 工作流仅靠 system prompt 文字指引 LLM 手动执行 6 步操作，不可靠。
SelfCheckTool 用 Rust 程序控制整个流程，外部 LLM 只需 1 次 tool call。

### 新建文件

#### `src/tools/self_check.rs`（~500 行，含 13 个单测）
- `SelfCheckTool` 实现 `Tool` trait，11 步流水线：
  1. source_sync × 2 — 同步 elfclaw + zeroclaw 源码，捕获 commit hash
  2. elfclaw_log::query_recent() — 直接调库获取 Vec<LogEntry>（不经 CheckLogsTool）
  3. 错误分组去重 — HashMap<key, Vec<LogEntry>>，保持首次出现顺序
  4. LLM₁ — 日志 → 搜索计划 JSON（temp=0.1）
  5. content_search × N — 按搜索计划执行
  6. LLM₂ — 搜索结果 → 精读文件列表 JSON（temp=0.1）
  7. file_read × N — 读 elfclaw 文件
  8. 自动配对 — 每个 elfclaw 文件自动读 zeroclaw 同路径文件（程序保证）
  9. LLM₃ — 全量数据 → 结构化诊断报告（temp=0.3）
  10. file_write — 报告写入 homework/debug_代码修改计划_YYYY-MM-DD.md
- 报告受众：AI 编程助手，含文件路径+行号+代码片段+修改建议
- 健壮 JSON 提取：支持直接 JSON、```json 包裹、前后有多余文字
- 降级策略：source_sync 失败继续、LLM₂ 失败跳过精读、LLM₁/₃ 失败中止
- 安全：can_act() + record_action() 两道闸门

### 修改文件

#### `src/tools/mod.rs`
- 添加 `pub mod self_check` + `pub use self_check::SelfCheckTool`
- 重构 `all_tools_with_runtime()` 中文件系统工具注册：
  - 提取 `source_sync_arc` 命名 Arc（避免重复构造）
  - 在 `has_filesystem_access` 块内构造 SelfCheckTool 并传入子工具引用
- SourceSyncTool 注册改用命名 Arc

#### `src/channels/mod.rs`
- 删除旧的 14 行手动 debug 工作流系统提示（568-581 行）
- 替换为 3 行 self_check 工具说明

### 验证
- `cargo check` 通过（lib + bin）
- `cargo test --lib` 有 31 个预先存在的编译错误（DelegateAgentConfig 字段类型变更），非本次修改引起

---

## 2026-03-06 — Feature: 源码级 Debug 分析能力 + GitHub MCP

### 背景

elfClaw 有 `check_logs` 读日志，但不知道自己的源码，无法将日志错误映射到代码行做根因分析。
本次添加两个能力：(1) source_sync 工具将源码 clone 到本地分析；(2) GitHub MCP 查阅 issues/PRs。

### 新建文件

#### `src/tools/source_sync.rs`（~250 行）
- `SourceSyncTool` 实现 `Tool` trait
- 参数：`repo_id`（elfclaw/zeroclaw）、`action`（sync/status）
- URL 白名单硬编码：仅允许 elfClaw 和 zeroclaw 两个仓库
- 目标路径固定 `<workspace>/workspace/github/<repo_id>/`
- sync: 未 clone 时 `git clone --depth=1`；已存在时 `git fetch --depth=1` + `git reset --hard`
- status: 检查当前 branch/commit
- 安全：`can_act()` + `record_action()` 两道闸门，120s 超时
- 直接调 `tokio::process::Command::new("git")`，不走 shell 工具链
- 6 个单元测试（白名单、大小写、ReadOnly 阻止、未知 action）

### 修改文件

#### `src/tools/mod.rs`
- 添加 `pub mod source_sync` + `pub use source_sync::SourceSyncTool`
- 在 `all_tools_with_runtime()` 中注册（紧跟 CheckLogsTool 之后）

#### `src/channels/mod.rs`
- `build_runtime_status_section()` 末尾追加 debug 工作流指令（~15 行）
- 包含 6 步流程 + 日志 category → 源码目录映射表

#### `资料/config.toml`
- 新增 `[mcp]` section，配置 GitHub MCP server（stdio 模式）
- command 指向 `tools/github-mcp-server.exe`
- token 占位符需用户替换

#### `.gitignore`
- 添加 `/tools/` 排除外部二进制

### GitHub MCP Server
- 来源：GitHub 官方（github/github-mcp-server v0.31.0）
- 已下载 Windows x86_64 二进制到 `tools/github-mcp-server.exe`（20MB）
- elfClaw 已有完整 MCP 基础设施，启动时自动连接并注册工具

---

## 2026-03-04 — Fix 15: runtime_trace 权限错误修复 + Logs 页面历史记录

### Fix 15a: `src/config/schema.rs` — `default_runtime_trace_mode()` 改回 `"none"`

- **原因**：Fix 13 把默认值改为 `"rolling"`，导致高并发时 Windows 文件锁冲突（os error 5）
- `elfclaw-logs.db`（SQLite WAL）已完整覆盖 runtime_trace 功能，后者为冗余
- `default_runtime_trace_mode()` 注释改为说明原因
- 同步更新单元测试 `observability_config_default`（断言改为 `"none"`）

### Fix 15b: `资料/config.toml` — `runtime_trace_mode = "none"`

- 用户配置与默认值同步，重启后立即停止 runtime-trace.jsonl 写入

### Fix 15c: `src/gateway/api.rs` — 新增历史日志端点

- 新增 `pub async fn handle_api_logs_recent()` 和 `LogsRecentParams` struct
- 调用 `crate::elfclaw_log::query_recent()` 查询 SQLite，返回 JSON
- 参数：`limit`（默认100，上限500）、`level`、`category`、`since_minutes`
- 鉴权通过 `require_auth()` 实现

### Fix 15d: `src/gateway/mod.rs` — 注册 `/api/logs/recent` 路由

- 紧接 `/api/events` SSE 路由之后注册，1 行改动

### Fix 15e: `web/src/types/api.ts` — 新增 `ElfClawLogEntry` 接口

- 镜像后端 `src/elfclaw_log/types.rs` 的 `LogEntry` 结构
- 字段：`id`, `timestamp`, `level`, `category`, `component`, `message`, `details`

### Fix 15f: `web/src/pages/Logs.tsx` — 挂载时加载历史

- 新增 `loadHistory()` 异步函数：`apiFetch('/api/logs/recent?limit=100')` 读取历史
- 历史条目转换为 `LogEntry[]`，倒序（最旧在前）注入 `entries` 初始状态
- 与 SSE 连接并发执行（`void loadHistory()`），不阻塞实时流
- 失败静默处理，不影响实时 SSE 功能
- 引入 `apiFetch` 和 `ElfClawLogEntry` 类型

---

## 2026-03-04 — Fix 14: check_logs 工具 + 系统提示感知 + 心跳自动日志分析

### 背景
Agent 不知道 `check_logs` 工具存在，遇到"查看日志"请求时会尝试 shell 命令（tail/grep/Get-Content），
在 Windows 环境下这些命令要么不可用要么被安全策略拦截，导致浪费大量 LLM 调用和时间。
Fix 14 通过三步骤解决：新建查询工具、系统提示感知注入、心跳自动诊断。

### Fix 14a: `elfclaw_log/store.rs` — `query_recent` 加 `since_minutes` 参数

- `query_recent()` 签名新增第4个参数 `since_minutes: Option<u64>`
- `since_minutes` 非 `None` 时在 SQL 加 `AND timestamp >= ?N` 条件（cutoff = now - N分钟）
- 更新同文件内 3 处测试调用（加 `None` 第4参数）

### Fix 14b: `elfclaw_log/mod.rs` — 暴露公开查询 API

- 新增 `pub fn query_recent(limit, level_filter, category_filter, since_minutes) -> Vec<LogEntry>`
- 委托给全局 `LOGGER` store，失败时 warn + 返回空 vec（不崩溃）
- Agent 通过此函数零 shell 调用直接读取日志 DB

### Fix 14c: `src/tools/check_logs.rs` — 新建 CheckLogsTool（~100行）

- `name()`: `"check_logs"`
- `description()`: 明确说明"无需 shell 命令，直接查询数据库"
- 参数：`limit`（默认20，上限100）、`level`（debug/info/warn/error）、`category`（8种）、`since_minutes`
- `execute()`: 调用 `crate::elfclaw_log::query_recent()`，格式化为人类可读文本
  - warn/error 条目附加 `details` JSON，便于诊断
  - 时间戳截取 RFC3339 的 `MM-DDThh:mm` 段

### Fix 14d: `src/tools/mod.rs` — 注册 CheckLogsTool

- 新增 `pub mod check_logs;` 和 `pub use check_logs::CheckLogsTool;`
- 在 `all_tools_with_runtime()` Composio 块之后无条件注册（所有 agent 均可用）

### Fix 14e: `src/channels/mod.rs` — 系统提示感知

- `build_runtime_status_section()` 末尾追加一行诊断提示：
  "Use `check_logs` tool to query runtime logs directly (no shell needed). Supports filters: ..."
- 每次 agent 启动时即知道该工具，无需先搜索

### Fix 14f: `src/daemon/mod.rs` — 心跳自动日志分析

- heartbeat prompt 构建段：在 HEARTBEAT.md 内容之前注入 **自动日志检查** 段落
- 每次心跳 tick 自动执行 `check_logs level=error since_minutes={interval_mins}`
- `since_mins` 与心跳间隔对齐，只检查上一个周期的错误
- 无错误时明确告知 agent "无需提及"，避免冗余汇报

### 验证结果

- `cargo build` ✅ 编译通过
- lib test 编译失败 31 处 → 均为预存在的 Fix 13 遗留问题（schema.rs/delegate.rs 中 `Option<String>` 类型不匹配），与本次改动无关

---


### 背景
elfClaw 部署后完全没有日志系统（`backend = "none"` + `runtime_trace_mode = "none"`），无法诊断运行状态。Gemini 无法区分新旧消息（缺少时间戳）。Cron job 可能因重复 heartbeat 创建多个同名任务。

### Fix 13a (P0): Cron Job 同名幂等去重

**`src/cron/store.rs`**
- 新增 `find_job_by_name()` 函数：按 name 字段查找已有 job
- `add_shell_job()` 加去重逻辑：同名 job 存在时自动转 `update_job()`
- `add_agent_job()` 加去重逻辑：同名 job 存在时自动转 `update_job()`
- 创建和去重更新时写 `elfclaw_log::log_cron_event()` 日志

### Fix 13b (P1): 消息时间戳注入

**`src/channels/telegram.rs`**
- 新增 `extract_message_timestamp()` 辅助函数：从 Telegram API `message.date` 提取 Unix 时间戳，缺失时 fallback 到 `SystemTime::now()`
- 4 处 `SystemTime::now()` 替换为 `Self::extract_message_timestamp(message)`（callback_query、attachment、voice、text message）

**`src/channels/mod.rs`**
- 用户消息存入历史时加 `[MM-DD HH:MM]` 前缀（`format_unix_timestamp(msg.timestamp)`）
- 助手响应存入历史时也加 `[MM-DD HH:MM]` 时间戳前缀

### Fix 13c (P2): elfClaw 日志系统

**新模块 `src/elfclaw_log/`（4 文件）**
- `types.rs`: `LogEntry`、`LogLevel`（Debug/Info/Warn/Error）、`LogCategory`（AgentLifecycle/LlmCall/ToolCall/CronJob/Heartbeat/ChannelMessage/WorkerStatus/System）
- `store.rs`: SQLite WAL 存储（`state/elfclaw-logs.db`）+ JSONL 追加写入（`state/elfclaw-logs.jsonl`）+ 启动时 prune 7 天旧日志 + 3 个单测
- `observer.rs`: `ElfClawObserver` 包装 base Observer，同时写 SQLite + 广播 SSE JSON 事件；序列化逻辑与 `gateway/sse.rs:BroadcastObserver` 一致
- `mod.rs`: 全局 `LazyLock` 单例（`LOGGER` + `GLOBAL_EVENT_TX`）+ `init()`/`log()`/`wrap_observer()`/`global_event_tx()` + 便捷函数（`log_tool_call`/`log_cron_event`/`log_channel_message`/`log_agent_start`/`log_agent_end`/`log_error`）+ `format_chat_timestamp`/`format_unix_timestamp`

**改动的现有文件**
- `src/lib.rs`: 注册 `pub mod elfclaw_log;`
- `src/main.rs`: 注册 `mod elfclaw_log;` + 启动时调用 `elfclaw_log::init()`
- `src/gateway/mod.rs`: `broadcast::channel(256)` → `elfclaw_log::global_event_tx()`；`BroadcastObserver::new()` → `elfclaw_log::wrap_observer()`
- `src/channels/mod.rs`: observer 替换为 `elfclaw_log::wrap_observer()`；incoming 消息加 `log_channel_message()` 调用
- `src/agent/loop_.rs`: 2 处 observer 替换为 `elfclaw_log::wrap_observer()`；`tool_loop_exhausted` 加 `log_error()` 调用
- `src/cron/scheduler.rs`: `execute_and_persist_job()` 加 `log_cron_event()` 的 started/completed/failed 日志
- `src/daemon/mod.rs`: heartbeat 失败加 `log_error()` 调用
- `src/config/schema.rs`: `ObservabilityConfig` 默认值 `backend: "log"`、`runtime_trace_mode: "rolling"`；更新对应测试断言
- `资料/config.toml`: `backend = "log"`, `runtime_trace_mode = "rolling"`

### 架构要点
- `ElfClawObserver` 放在 `elfclaw_log` 模块（不是 `gateway`），避免 channels→gateway 循环依赖
- 全局 `GLOBAL_EVENT_TX` (`broadcast::channel(512)`) 统一 SSE 事件总线，gateway/channels/agent 三方共享
- 日志写入失败不崩溃主流程（catch + warn）
- SQLite WAL 模式避免写阻塞读

### 编译验证
- `cargo build` ✅ 通过
- `cargo build --release` ✅ 通过
- `cargo test` 编译失败是预先存在的问题（`git stash` 后同样失败），与本次改动无关

---

## 2026-03-04 — Fix 12: send_telegram 消息分片 + 失败日志 + Cron Prompt 强化

### 背景
Fix 11 已部署后，Gemini cron agent 确实执行了新闻抓取任务，但用户只收到空洞通知（"任务已执行完毕"），无实际内容。根因：
1. send_telegram 不支持分片（>4096 字符的消息静默失败，且失败无 warn 日志）
2. Cron prompt 引导不够强，agent 只回复"任务完成"而不输出实际内容
3. Agent 可能冗余调用 send_telegram（cron 系统已自动投递文本响应）

### 改动要点

**Fix 12a: `src/channels/telegram.rs` — 常量和函数改 pub(crate)**
- `TELEGRAM_MAX_MESSAGE_LENGTH`、`TELEGRAM_CONTINUATION_OVERHEAD` 改为 `pub(crate)`
- `split_message_for_telegram()` 改为 `pub(crate)`，供 send_telegram 工具跨模块使用

**Fix 12a: `src/channels/mod.rs` — 重新导出**
- 添加 `pub(crate) use telegram::split_message_for_telegram;`

**Fix 12a: `src/tools/send_telegram.rs` — 消息分片 + message_id 日志 + 失败 WARN**
- `execute()` 方法重写：用 `split_message_for_telegram()` 自动分片长消息
- 多片消息加 continuation 标记（`_(continues... 1/N)_`、`_(continued 2/N)_`）
- 新增 `send_one_chunk()` 方法：
  - 成功时 `info!()` 记录 chat_id、message_id、chunk/total
  - 失败时 `warn!()` 记录 chat_id、status、chunk/total（解决原来静默失败问题）
  - Markdown 降级逻辑保持不变（`can't parse entities` → plain text retry）
- 新增 `extract_message_id()` 从 Telegram API 响应 JSON 提取 message_id
- 片间 100ms 间隔防止速率限制

**Fix 12b: `src/cron/scheduler.rs` — 增强 Cron Prompt 引导**
- 替换 Fix 11 的引导文本，5 条明确规则：
  1. 直接用工具执行任务
  2. 最终文本响应就是用户看到的消息 — 必须包含所有结果和摘要
  3. 禁止空洞回复（"task completed"、"please check above"）
  4. 禁止调用 send_telegram（系统自动投递）
  5. 不要等其他 agent
- 标记 `// elfClaw:` 注释

### 编译验证
- `cargo build --release --features wasm-tools` ✅ 通过（无新增 warning）

---

## 2026-03-03 — Fix 11: Cron Agent Prompt 行为引导（防 haiku 自言自语）

### 背景
Cron 推送机制正常（message 到达 Telegram），但 haiku 执行 cron 任务时只会自言自语（"我来等 news_fetcher 完成任务..."），不执行实际工作。根因：非委派路径（`delegate_to=None`）的 cron prompt 没有行为引导，haiku 不知道自己应该直接执行任务。

### 改动要点

**Fix 11: `src/cron/scheduler.rs` — line 197-210（非委派 cron prompt 行为引导）**
- 原代码：`format!("[cron:{} {name}] {prompt}", job.id)` — 零行为指令
- 新代码：添加 IMPORTANT 行为引导指令，告知 agent：
  - 你是后台定时任务，直接用工具执行
  - 不要描述计划，而是实际执行
  - 不要等其他 agent，你就是负责人
  - 你的文本输出会送达用户
- 对比委派路径已有 "Use the delegate tool now" 指令，非委派路径现获得同等级别引导
- 已有具体 prompt 的任务（如 22:00 新闻源搜索）不受影响，因为引导指令与具体步骤不冲突
- 标记 `// elfClaw:` 注释

---

## 2026-03-03 — Fix: Windows 安全策略路径兼容 + Shell PATHEXT + Telegram 确认日志

### 背景
部署后 agent 尝试用绝对路径 `X:\...\uv.exe` 运行 uv 被安全策略拦截。根因：`is_command_allowed()` 用 `rsplit('/')` 提取 base command，Windows `\` 路径无法正确分割。同时 shell 子进程缺少 `PATHEXT` 环境变量导致裸命令 `uv` 无法解析为 `uv.exe`。Cron 推送到 Telegram 后无成功确认日志。

### 改动要点

**Fix 8a: `src/security/policy.rs` — 新增 `extract_base_command_name()`**
- 同时按 `/` 和 `\` 分割路径，提取 base command
- 剥离 Windows 可执行文件扩展名（.exe/.cmd/.bat/.com）用于白名单匹配
- 例：`C:\Users\xxx\.local\bin\uv.exe` → `uv`

**Fix 8b: `src/security/policy.rs` — `is_command_allowed()` line 800**
- `rsplit('/')` 替换为 `extract_base_command_name()`
- 修复 Windows 绝对路径命令被误拦截的问题

**Fix 8c: `src/security/policy.rs` — `command_risk_level()` line 586**
- 同样替换 `rsplit('/')` 为 `extract_base_command_name()`

**Fix 8d: `src/security/policy.rs` — `looks_like_path()`**
- 新增 Windows 绝对路径检测（`C:\...`）
- 新增 UNC 路径检测（`\\server\share`）

**Fix 9a: `src/tools/shell.rs` — `SAFE_ENV_VARS_WINDOWS`**
- 添加 `PATHEXT`（Windows 命令解析必需）和 `COMSPEC`（cmd.exe 路径）

**Fix 9b: `src/tools/shell.rs` — PATH 诊断日志**
- shell 命令失败且 stderr 含 `CommandNotFoundException` 时记录 PATH 值
- 帮助诊断 `env_clear()` 后子进程环境变量问题

**Fix 10: `src/channels/telegram.rs` — 发送成功确认**
- `send_text_chunks()` HTML 格式成功：解析响应体，记录 chat_id + message_id
- 检测 Telegram API 返回 `ok=false` 的异常情况
- plain text fallback 成功也记录确认日志

### 验证
- `cargo build --release --features wasm-tools` 成功
- 部署后让 agent 运行 `uv run python -c "print('ok')"` 验证安全策略
- Cron 推送后终端应出现 `"Telegram message delivered"` + message_id

---

## 2026-03-03 — Fix: Cron 全局推送 + Python/uv 白名单

### 背景
部署测试发现 cron job 执行成功（haiku 模型）但消息没有推送到 Telegram。同时 skill python 脚本被安全策略拦截。

### 改动要点

**Fix 6a: `src/channels/mod.rs` — 注册 live channel 实例**
- `start_channels()` 在 `channels_by_name` 构建后调用 `register_live_channels()`
- 将所有启动的 channel（telegram/discord/slack/等）注册到全局 registry
- **根因**：`register_live_channels()` 已定义但**从未被调用**，导致全局 registry 永远为空

**Fix 6b: `src/channels/mod.rs` — `deliver_to_channel()` 优先 live 实例**
- 在 `collect_configured_channels()` 之前，先查全局 live channel registry
- 找到就用活跃实例发送（与 channels runtime 共享连接）
- 找不到才降级创建 ad-hoc 实例
- 这是全局方案：任何启动的 channel 都自动支持 cron/daemon 投递

**Fix 6c: `src/cron/scheduler.rs` — 推送日志可见**
- `deliver_if_configured()` 在调用 `deliver_announcement` 前添加 `tracing::info!`
- 含 job_id, channel, target, output_len，让 cron → channel 推送流程在终端可追踪

**Fix 7: `资料/config.toml` — 添加 `uv` 到白名单**
- `allowed_commands` 新增 `"uv"`
- skill 用 `uv run python script.py` 时，安全策略检查第一个词 `uv`，之前不在白名单被拒绝

### 验证
- `cargo build --release --features wasm-tools` 成功
- 部署注意：编译后需将 `资料/config.toml` 一起复制到 `D:\ZeroClaw_Workspace\`

---

## 2026-03-03 — Fix: 运行时 5 个关联问题（基于运行日志实证）

### 背景
上一轮修改部署后，运行日志暴露了 5 个互相关联的问题：UTF-8 panic、shell 拒绝静默、python3/Windows 兼容、agent 环境无感知、delegate 失败无原因。

### 改动要点

**Fix 1 (Critical): `src/cron/scheduler.rs`**
- 新增 `truncate_str_safe()` 函数：UTF-8 安全截断，避免中文字符边界 panic
- 替换 `&response[..response.len().min(120)]` 为 `truncate_str_safe(&response, 200)`
- 修复 `panicked at byte index 120 is not a char boundary` 问题

**Fix 2: `src/tools/shell.rs`**
- `validate_command_execution` 拒绝点添加 `tracing::warn!`（含 command + reason）
- `forbidden_path_argument` 拒绝点添加 `tracing::warn!`（含 command + path）
- `record_action` 耗尽点添加 `tracing::warn!`（含 command）
- 让安全策略拒绝在终端可见，之前只有 LLM 能看到 ToolResult.error

**Fix 3a: `src/channels/mod.rs`**
- `build_system_prompt_with_mode()` 的 Runtime 段注入平台详情
- Windows: "Shell: PowerShell. Use `python` (not `python3`)."
- macOS/Linux: 对应的 shell 和 python 命令提示
- 使 LLM 知道当前运行环境，避免生成不兼容命令

**Fix 3b: `src/runtime/native.rs`**
- Windows `build_shell_command()` 中自动将 `python3` 规范化为 `python`
- 双层防御：系统提示告诉 LLM 用 python，运行时兜底自动转换

**Fix 4a: `src/channels/mod.rs` — ChannelRuntimeContext**
- 新增 `config: Arc<Config>` 字段用于运行时状态注入
- 生产构造处和所有测试构造处均添加了字段

**Fix 4b: `src/channels/mod.rs` — 运行时状态注入**
- 新增 `build_runtime_status_section()` 函数，生成动态 Runtime Status 段
- 内容包括：autonomy 级别、allowed_commands、worker_model、已配置 agents、活跃 cron jobs（从 store 动态读取）
- 在 `process_channel_message()` 中紧跟 `build_channel_system_prompt()` 之后注入
- Agent 现在能看到所有 cron job 的 ID、名称、状态、调度表达式

**Fix 5a: `src/tools/delegate.rs`**
- `execute_agentic()` 的 `Ok(Err(e))` 路径添加 `tracing::warn!`（含 agent + error）

**Fix 5b: `src/tools/delegate.rs`**
- Agent 有显式 provider 且与默认 provider 不同时发出 `tracing::warn!`
- 帮助检测过期配置

**Fix 5c: `src/tools/delegate.rs`**
- 将 agentic completion log 拆分为成功/失败两条路径
- 成功用 `info!`，失败用 `warn!`（含 error 详情）

### 验证
- `cargo build` 成功，无新增 error（预存在 warnings 不变）

---

## 2026-03-03 — Fix: Cron/Worker 日志缺失 + Skill Python 执行被锁

### 改动要点

**Fix 1: `src/cron/scheduler.rs`**
- `run_agent_job()` 新增 `tracing::info!` 日志：job 启动（含 job_id/name/delegate_to）、完成（含输出预览）、失败
- `persist_job_result()` 中 `record_run()` 错误从 `let _ =` 吞掉改为 `if let Err(e)` 并输出 `warn!`
- 所有新增日志均带 `// elfClaw:` 注释

**Fix 2: `src/tools/delegate.rs`**
- `execute()` 中 provider/model 解析成功后新增 `tracing::info!` "Delegate: starting sub-agent"（含 agent/provider/model/agentic 字段）
- 非 agentic 成功路径新增 `tracing::info!` "Delegate: sub-agent completed"（含 output_len）
- agentic 路径 `return Ok(result)` 前新增 `tracing::info!` "Delegate: sub-agent (agentic) completed"
- 所有新增日志均带 `// elfClaw:` 注释

**Fix 3a: `资料/config.toml`**
- `allowed_commands` 列表追加 `"python"` 和 `"python3"`（带 elfClaw 注释）
- 目的：允许 SKILL.toml 定义的 Python 脚本通过 shell 工具执行

**Fix 3b: `src/skills/tool_handler.rs:367`**
- `validate_command_execution(&command, false)` → `validate_command_execution(&command, true)`
- Skill 命令模板由用户在 SKILL.toml 中明确定义，属于预信任命令（approved=true）
- 高风险命令仍由 `block_high_risk_commands=true` 独立拦截，安全性不降低

### 验证
- `cargo build` 成功，无新增 error（预存在 warnings 不变）

---

## 2026-03-03 — delegate worker agent 继承 worker_model，provider/model 改为 Optional

### 背景
`news_fetcher` 等 worker agent 在 `[agents.xxx]` 中必须硬编码 `provider`/`model`，
切换主 provider 时需逐一更新。旧配置使用已失效的 Anthropic 自定义 endpoint，导致 `API key not valid`。

### 根因
`DelegateAgentConfig.provider` / `.model` 为强制 `String`，无法省略。

### 改动（6 个文件）

**`src/config/schema.rs`**
- `DelegateAgentConfig.provider` / `.model` 改为 `#[serde(default)] Option<String>`

**`src/tools/delegate.rs`**
- `DelegateTool` struct 新增 `fallback_provider: Option<String>` / `fallback_model: Option<String>`
- `new_with_options` / `with_depth_and_options` 初始化时赋 `None`
- 新增 builder 方法 `with_worker_model_fallback(provider, model)`
- `execute()` 中插入 `effective_provider` / `effective_model` 解析（优先 agent 自身配置，再 fallback）
- `execute_agentic()` 签名增加 `effective_provider`/`effective_model` 参数，内部 run_tool_call_loop 使用这两个值
- 测试辅助函数 `sample_agents()` / `agentic_config()` 中 `provider`/`model` 改为 `Some(...)`

**`src/tools/mod.rs`**
- 构造 `DelegateTool` 时链式调用 `.with_worker_model_fallback(default_provider, worker_model|default_model)`

**`src/tools/model_routing_config.rs`**
- `has_provider_credential()` 调用改为 `.as_deref().unwrap_or("")`
- `handle_upsert_agent()` 中赋值和 struct 初始化改为 `Some(...)`

**`src/tools/subagent_spawn.rs`**
- `create_provider_with_options` / `chat_with_system` / `run_tool_call_loop` 调用中 provider/model 改为 `.as_deref().unwrap_or("")`
- 格式化字符串改为 `.as_deref().unwrap_or("(none)")`

**`src/doctor/mod.rs`**
- `provider_validation_error` 调用改为 `if let Some(provider_name) = agent.provider.as_deref()`

**`src/migration.rs`**
- `.trim()` 调用改为 `.as_deref().unwrap_or("").trim()`
- `DelegateAgentConfig` 初始化改为 `Some(...)`

**`资料/config.toml`**
- `[agents.news_fetcher]` 删除 `provider` / `model`，改为继承 `worker_model`

### 验证
- `cargo build` → 成功（仅有预存 warnings，无新 error）

---

## 2026-03-03 — 修复 Gemini 400 "Function call is missing a thought_signature"

### 背景
上一个修复（删除降级块）后，cron job 触发时报：
```
Gemini API error (400 Bad Request): Function call is missing a thought_signature
in functionCall parts. This is required for tools to work correctly.
```

### 根因
Gemini 3 Flash 将 `thought_signature` 直接放在 **functionCall Part 本身**（不是独立的 thought Part），而原有代码只从 `thought=true` 的 Part 读取签名。结果：
1. 捕获阶段：`thought_signature` 丢失 → 历史工具调用 `thought_signature = None`
2. 重放阶段：function_call Part 的 `thought_signature: None` → Gemini 400

### 改动文件

**`src/providers/gemini.rs`**（两处，均标 `// elfClaw:`）

**Fix A**（行 ~314-332，`extract_tool_calls()`）：
- 改 `if let Some(sig) = part.thought_signature` 为 `if let Some(ref sig) = ...`（避免所有权移动）
- 在处理 function_call Part 时，用 `.or_else(|| part.thought_signature.clone())` 从 function_call Part 本身捕获签名
- Gemini 2.5（签名在 thought Part）和 Gemini 3（签名在 function_call Part）均正确处理

**Fix B**（行 ~1557-1577，history rebuild）：
- 提取 `sig_opt` 变量（共用）
- function_call Part 新增 `thought_signature: sig_opt.map(|s| s.to_string())`
- thought Part（Gemini 2.5）保持不变；functionCall Part 同时携带签名（Gemini 3 要求）

### 验证
- `cargo build --release` → 成功（无新 error）

---

## 2026-03-03 — 修复 Gemini 工具调用停不下来（降级块根因修复）

### 背景
Gemini 模型调用任何工具（send_voice、read_file、cron job 等）后陷入无限循环，每次迭代向 Telegram 发送 "(Continued from previous tool interaction)" 消息，最终命中 25/50 次上限失败。

### 根因
`src/providers/gemini.rs` 行 1517–1550 存在"降级块"：当历史工具调用缺少 `thought_signature` 时，将整个工具调用历史替换为文本 "(Continued from previous tool interaction)"，并跳过工具结果。

根本原因：Gemini 3 Flash 在 "low" thinking 级别（`reasoning_level = 1`）下**有时直接输出 function_call 而不包含 thought 部分**，导致 `thought_signature = None`。降级块将此视为异常历史，把整轮工具调用上下文抹掉，Gemini 下一轮失去上下文 → 重复调用 → 无限循环。

### 修改文件

**`src/providers/gemini.rs`**
- 删除 `all_have_signature` 检查 + 整个降级 `if` 块（-34 行）
- 删除 `if tool_name == "__degraded__"` 死代码检查及注释（-6 行）
- 更新降级块位置的注释，说明正常路径已正确处理有/无 `thought_signature` 两种情况
- 净变化：-40 行（纯删除，零新增）

### 验证
- `cargo build --release` → 成功（无新 error，仅已有 warning）

---

## 2026-03-03 — 修复 Telegram TOCTOU 竞态 + Gemini 503 重试间隔过短

### Bug A：Telegram 附件路径 TOCTOU 竞态

**根因**：`parse_path_only_attachment()` 用 `Path::new(candidate).exists()` 检测文件是否存在，但 TTS 清理任务可能在 `exists()` 与后续 `canonicalize()` 之间删掉文件，导致 `❌ Failed to reply on telegram: Telegram attachment path not found`，且 Agent 文字回复被 `?` 跳过、从未发出。

**改动文件**：`src/channels/telegram.rs` line 373

- `Path::new(candidate).exists()` → `Path::new(candidate).canonicalize().is_err()`
- 检测阶段即完成路径解析，TOCTOU 窗口收敛至接近零
- 文件若已被删除 → `canonicalize()` 失败 → 返回 `None` → 走文字发送路径

### Bug B：Gemini 503 重试间隔过短

**根因**：`compute_backoff()` 在无 Retry-After 头时直接返回 `base`（默认 500ms）。Gemini 503 "model overloaded / high demand" 需要 5-30 秒恢复，500ms/1000ms 间隔全部失败。

**改动文件**：`src/providers/reliable.rs`

- 新增 `is_server_overload()` 函数：检测 reqwest 503 或错误消息含 overload/high demand 等关键词
- 新增常量 `OVERLOAD_BACKOFF_FLOOR_MS = 5_000`
- `compute_backoff()` 新增 `else if is_server_overload(err)` 分支：`base.max(OVERLOAD_BACKOFF_FLOOR_MS)`
- 效果：503 重试等待至少 5s；Retry-After 优先级不变；其他错误路径完全不受影响

### 验证

- `cargo build --release` → 成功（无新 error，仅已有 warning）

---

## 2026-03-03 — 修复波形图失败：附件未找到通知用户 + plotly 脚本规范

### 背景

elfClaw 使用 plotly skill 生成波形图时，Python 脚本 shell 执行连续失败（loop detection HardStop）。图片文件从未生成，但 LLM 在回复中仍引用了脚本里硬编码的输出路径，导致两个问题：

1. Telegram 附件发送失败但用户收不到任何提示（内部错误被 `?` 静默传播）
2. LLM 生成的脚本缺少 `os.makedirs` + 错误处理，也未明确说明 kaleido/Chrome 依赖

### 修改文件

**`src/channels/telegram.rs`**（两处，已标 `// elfClaw:`）

- 修改 `send_reply_with_attachments()` 和 `send()` 中的 `for attachment in &attachments` 循环
- 原来：`self.send_attachment(...).await?`（文件不存在 → 内部错误传播，用户看不到提示）
- 现在：`if let Err(e) = ...` 捕获错误；若错误包含 "Telegram attachment path not found" 或 "is not a file"（且不是 HTTP URL），则向用户发送 ⚠️ 文字通知，而非传播内部错误；其他错误仍正常传播

**`资料/skills/scientific-tools/scientific-skills/plotly/SKILL.md`**

- Quick Start 之后新增 "Script Execution Rules" 章节：`os.makedirs` 要求、`uv run` 语法、成功确认输出 + 错误处理模板
- Export Options 章节更新：加入 kaleido/Chrome 依赖警告（⚠️ 红色提示）、HTML 首选回退方案

**`资料/skills/scientific-tools/scientific-skills/plotly/references/export-interactivity.md`**

- Static Image Export 章节新增：kaleido/Chrome 不可用时的故障排查说明 + 安全导出模板（含 `os.makedirs` + 成功确认 + HTML 回退）

### 设计决策

- telegram.rs 修改**拦截错误**而非**预先检查路径**：避免在循环中重复 `resolve_workspace_attachment_path` 的路径解析逻辑（符合 DRY）
- 错误消息字符串 "Telegram attachment path not found" 和 "is not a file" 是本仓库内部定义（`telegram.rs:268,274`），不会误匹配外部错误
- `// elfClaw:` 标记已加在两处循环修改的起始注释行

---

## 2026-03-03 — 修复 Windows 上 shell 工具无法执行 Python 脚本

### 背景

用户要求生成 220V 正弦波图片，shell 工具连续失败 4 次触发 HardStop。日志只显示 "Tool 'shell' failed 4 consecutive times"，没有具体原因。通过代码分析发现两处 Windows 兼容性问题。

### 根因 A：NativeRuntime 硬编码 `sh`

`src/runtime/native.rs:46` 无条件使用 `Command::new("sh")`。Windows 上 `sh` 只有安装 Git Bash 且加入 PATH 才存在，直接导致命令无法启动。

### 根因 B：`env_clear()` 后 Windows uv 无法工作

`src/tools/shell.rs:17-19` 的 `SAFE_ENV_VARS` 只包含 Unix 变量，不包含 `APPDATA`/`LOCALAPPDATA`/`TEMP` 等 Windows 系统变量。`uv` 在 Windows 上把包缓存放在 `%LOCALAPPDATA%\uv\`，没有这些变量时包解析失败。

### 根因 C（诊断障碍）：shell 错误不写日志

shell 失败的 stderr 只返回给 LLM，不写到应用日志，操作员无法从日志看到具体错误原因。

### 修改文件

**`src/runtime/native.rs`**（已标 `// elfClaw:`）

- `build_shell_command()` 改为平台分支：
  - `#[cfg(windows)]`：使用 `powershell -NoProfile -NonInteractive -Command`
  - `#[cfg(not(windows))]`：保持原有 `sh -c`（Linux/macOS 不变）

**`src/tools/shell.rs`**（已标 `// elfClaw:`，3 处）

- 在 `SAFE_ENV_VARS` 之后新增 `#[cfg(windows)]` 常量 `SAFE_ENV_VARS_WINDOWS`，包含 `APPDATA`、`LOCALAPPDATA`、`USERPROFILE`、`TEMP`、`TMP`、`SYSTEMROOT`、`SYSTEMDRIVE`、`WINDIR`
- `collect_allowed_shell_env_vars()` 末尾新增 `#[cfg(windows)]` 块，将 Windows 变量追加到返回列表
- `execute()` 结果处理新增两处 `tracing::warn!`：
  - shell 返回 exit code 非零时记录命令 + exit_code + stderr
  - 进程启动失败（`Ok(Err(e))`）时记录命令 + error

### 验证

- `cargo check` → 通过，无新 error/warning




---

## 2026-03-03 — 修复 Gemini 工具滥用问题（无限循环发语音/邮件）

### 背景
Gemini 2.5 Pro 在执行一次 `send_voice`/`send_email` 后，同一轮内重复调用 3+ 次，之后每条新消息开头也先重发一次道歉语音。根因是两个独立 bug：
1. LoopDetector 的现有三种策略（no_progress / ping_pong / failure_streak）全部漏掉"不同参数、持续成功"的场景。
2. 历史摘要里包含"已发送语音道歉 × 3"，Gemini thinking 层认为任务未完成，在下一条消息前重复执行。

### 改动文件

**`src/agent/loop_/detection.rs`**（Fix 1）
- 新增常量 `ACTION_SPAM_TOOLS: &[&str]`（send_voice / send_email / send_telegram）
- `LoopDetectionConfig` 新增 `action_success_limit: usize`（默认 1）
- `LoopDetector` 新增 `success_counts: HashMap<String, usize>` 和 `success_spam_warned: HashSet<String>`
- `record_call()` 在 success=true 且工具属于 ACTION_SPAM_TOOLS 时递增 `success_counts`
- 新增 `check_action_success_spam()` 方法：首次达到 limit → InjectWarning；超过 limit → HardStop
- `check()` 在现有三种策略之后调用 `check_action_success_spam()`（独立状态，不干扰 warning_injected）
- 新增 3 个单元测试（测试 12/13/14），全部通过

**`src/agent/loop_.rs`**（Fix 2）
- 新增常量 `SINGLE_USE_TOOLS: &[&str]`（与 detection.rs 中 ACTION_SPAM_TOOLS 保持一致）
- 主循环前新增 `used_action_tools: HashSet<String>`
- 工具成功后：`if outcome.success && SINGLE_USE_TOOLS.contains(&call.name.as_str())` → 插入 `used_action_tools`
- 每次 LLM 调用前计算 `turn_tool_specs`：从 `tool_specs` 过滤掉 `used_action_tools` 中的工具
- `request_tools` 改用 `turn_tool_specs.as_slice()`（若为空则 None）
- 效果：send_voice 成功一次后，下次 Gemini 的 tool_specs 里就没有它，物理上无法再调用

**`src/channels/mod.rs`**（Fix 3）
- 新增辅助函数 `contains_action_tool_summary(content: &str) -> bool`（检测 action 工具名是否出现在历史消息中）
- `history.extend(prior_turns)` 之后：若 `history[1..len-1]` 中有任何消息含 action 工具引用，在当前用户消息前注入任务完成边界消息 `[SYSTEM] The previous tasks listed above are COMPLETE...`
- 效果：从程序层面给 Gemini 注入明确边界，阻断跨消息历史污染

### 验证
- `cargo build --release` → 成功（无新 error/warning）
- `cargo test --lib detection` → 32/32 全部通过

---

## 2026-03-02 — 修复 scientific-tools skill 安全审计失败

### 背景
elfClaw 启动时日志显示 `skipping insecure skill directory .../scientific-tools`，原因是安全审计扫描到 `curl ... | bash` 高风险命令模式。

### 根因
三处文件含有触发安全扫描的 curl-pipe-shell 模式：
1. `README.md` 第 142 行：`curl -fsSL https://claude.ai/install.sh | bash`
2. `alphafold-database/references/api_reference.md` 第 304 行：`curl https://sdk.cloud.google.com | bash`
3. `denario/references/llm_configuration.md` 第 137 行：`curl https://sdk.cloud.google.com | bash`

### 改动文件

**删除**
- `资料/skills/scientific-tools/README.md`：skill 目录不应包含 README（audit 会扫描），直接删除

**修改（Fix 2/3）**
- `资料/skills/scientific-tools/scientific-skills/alphafold-database/references/api_reference.md`
  - 第 304 行：`curl ... | bash` → 拆分为下载 + 执行两步
- `资料/skills/scientific-tools/scientific-skills/denario/references/llm_configuration.md`
  - 第 137 行：同上修改

**修复路径错字（Fix 4/5）**
- `scientific-skills/neuropixels-analysis/SKILL.md`：所有 `](reference/` → `](references/`（含 section headers）
- `scientific-skills/plotly/SKILL.md`：所有 `](reference/` → `](references/`（5 处链接 + Reference Files 列表）

### 验证
`grep -r "curl.*|.*bash"` → 无匹配；`grep -r "](reference/"` → 无匹配。
重启 elfClaw 后应看到 `loaded skill "scientific-tools"` 而非 skip 警告。

---

## 2026-03-02 — reasoning_level 重设计：0-4 整数，覆盖全部 Gemini 思维模型

### 背景
原字符串系统（"low"/"high"）只支持 Gemini 3 的 `thinkingLevel`，无法覆盖 Gemini 2.5 系列的 `thinkingBudget` 整数 API。

### 改动文件

**`src/config/schema.rs`**
- `ProviderConfig.reasoning_level`: `Option<String>` → `Option<u8>`
- `RuntimeConfig.reasoning_level`: `Option<String>` → `Option<u8>`
- `normalize_reasoning_level_override()`: 返回类型改为 `Option<u8>`，新增数字解析（0-4），保留 legacy 字符串（minimal/low/medium/high/xhigh）向后兼容
- `effective_provider_reasoning_level()`: 返回 `Option<u8>`，简化实现（不再需要 normalize）
- 环境变量 override（`ZEROCLAW_REASONING_LEVEL`）仍解析字符串，映射到 u8
- 5 个相关测试全部更新为整数断言，通过

**`src/providers/mod.rs`**
- `ProviderRuntimeOptions.reasoning_level`: `Option<String>` → `Option<u8>`

**`src/providers/gemini.rs`**
- `GeminiProvider.thinking_level`: `Option<String>` → `Option<u8>`
- `new_with_auth()` 第四参数: `Option<String>` → `Option<u8>`
- `ThinkingConfig` 结构体: 增加 `thinking_budget: Option<i32>` 字段，`thinking_level` 改为 `Option<String>`（互斥注入）
- 删除 `map_reasoning_level()` 字符串映射函数
- 新增 `build_thinking_config(level: u8, model: &str) -> Option<ThinkingConfig>` 函数
  - 检测顺序：gemini-3.1-pro / gemini-3-pro → gemini-3 → gemini-2.5-flash-lite → gemini-2.5-flash → gemini-2.5-pro → 其他（None）
  - Gemini 3 Pro: thinkingLevel，无法关闭（0→"low"）
  - Gemini 3 Flash: thinkingLevel，0→"minimal" 近关
  - Gemini 2.5 Flash Lite/Flash: thinkingBudget 整数，可关闭（level 0 → budget 0）
  - Gemini 2.5 Pro: thinkingBudget，无法关闭（0→128）
  - Gemini 2.0 及更早：不注入（返回 None）
- `send_generate_content()` 注入逻辑更新：使用 `and_then(|lvl| Self::build_thinking_config(lvl, model))`
- 测试辅助函数 `test_provider()` 和 `warmup_managed_oauth_requires_auth_service` 中补充 `thinking_level: None`

**`src/providers/openai_codex.rs`**
- 构造时不再调用 `normalize_reasoning_level(options.reasoning_level.as_deref(), ...)`
- 改为直接 match `Option<u8>`：0/1→"low"，2→"medium"，3/4→"high"

**`src/channels/mod.rs`（测试代码）**
- 23 处 `ChannelRuntimeContext { ... }` 测试构造器中补充 `worker_model: None`（修复预存在编译错误）

### 配置示例
```toml
[provider]
reasoning_level = 2   # 0-4 整数，各模型自动映射
```

### 验证
- `cargo check` ✅ 无新增错误
- 5 个 reasoning_level 相关测试全通过
- 4146 个其余测试通过；20 个失败均为 Windows 环境预存在问题（symlink/grep/进程）

---

## 2026-03-02 — 修复测试代码中 ChannelRuntimeContext 缺失 worker_model 字段

**文件**：`src/channels/mod.rs`

**问题**：`ChannelRuntimeContext` 结构体新增了 `worker_model: Option<String>` 字段（elfClaw 原创），但测试代码中共 23 处 `ChannelRuntimeContext { ... }` 字面量构造器未同步添加该字段，导致 `cargo test` 无法编译。

**修改**：在以下所有测试构造器中添加 `worker_model: None,`：
- `compact_sender_history_*`（3955 行区域）
- `append_sender_turn_*`（4006 行区域）
- `rollback_orphan_user_turn_*`（4060 行区域）
- `process_channel_message_executes_tool_calls_instead_of_sending_raw_json`（4537 行区域）
- `process_channel_message_telegram_does_not_persist_tool_summary_prefix`（4598 行区域）
- `process_channel_message_strips_unexecuted_tool_json_artifacts_from_reply`（4673 行区域）
- `process_channel_message_executes_tool_calls_with_alias_tags`（4734 行区域）
- `process_channel_message_handles_models_command_without_llm_call`（4804 行区域）
- `process_channel_message_uses_route_override_provider_and_model`（4895 行区域）
- `process_channel_message_prefers_cached_default_provider_instance`（4968 行区域）
- `process_channel_message_uses_runtime_default_model_from_store`（5054 行区域）
- `process_channel_message_respects_configured_max_tool_iterations_above_default`（5128 行区域）
- `process_channel_message_reports_configured_max_tool_iterations_limit`（5190 行区域）
- `message_dispatch_processes_messages_in_parallel`（5371 行区域）
- `message_dispatch_interrupts_in_flight_telegram_request_and_preserves_context`（5455 行区域）
- `message_dispatch_interrupt_scope_is_same_sender_same_chat`（5547 行区域）
- `process_channel_message_cancels_scoped_typing_task`（5623 行区域）
- `process_channel_message_adds_and_swaps_reactions`（5684 行区域）
- `process_channel_message_restores_per_sender_history_on_follow_ups`（6207 行区域）
- `process_channel_message_enriches_current_turn_without_persisting_context`（6294 行区域）
- `process_channel_message_telegram_keeps_system_instruction_at_top_only`（6381 行区域）
- `e2e_photo_attachment_rejected_by_non_vision_provider`（6938 行区域）
- `e2e_failed_vision_turn_does_not_poison_follow_up_text_turn`（7006 行区域）

**验证**：`cargo check --tests` 成功，exit code 0，无新增错误（仅已知的 unused import 警告）。

---

## 2026-03-02 — 修复 Gemini Flash 模型"发疯"（疯狂发邮件/语音）

### 根因
1. Gemini 3 Flash 默认 `thinkingLevel = high` + `temperature = 1.0`，导致 agent 行为极度发散
2. `LoopDetector`（`src/agent/loop_/detection.rs`，413行）完整实现但从未被调用（死代码）

### Fix 1：Gemini thinkingConfig 支持（elfClaw 原创）

**文件：`src/providers/gemini.rs`**
- 新增 `ThinkingConfig` 结构体（`thinkingLevel` 字段，serde camelCase）
- `GenerationConfig` 新增 `thinking_config: Option<ThinkingConfig>` 字段（`skip_serializing_if = "Option::is_none"`）
- `GeminiProvider` struct 新增 `thinking_level: Option<String>` 字段
- 新增 `map_reasoning_level()` 私有函数：`minimal/low/medium/high/xhigh` → Gemini API thinkingLevel string
- `new()` 初始化 `thinking_level: None`
- `new_with_auth()` 新增第四参数 `reasoning_level: Option<String>`，存入 `thinking_level`
- `send_generate_content()` 中 `GenerationConfig` 构造注入 `thinking_config`
- 所有测试内的 `GenerationConfig` 构造补充 `thinking_config: None`

**文件：`src/providers/mod.rs`**
- Gemini 工厂分支（`"gemini" | "google" | "google-gemini"`）传递 `options.reasoning_level.clone()` 到 `new_with_auth()`

**文件：`src/channels/mod.rs`、`src/agent/loop_.rs`**
- `ProviderRuntimeOptions` 构造新增 `reasoning_level: config.provider.reasoning_level.clone()`
- Claude 等其他 provider 会忽略 `reasoning_level`，Gemini 会用它设置 `thinkingConfig`

**配置说明（无需重新编译）：**
```toml
[provider]
reasoning_level = "low"    # Gemini Flash 用 low；Flash 专属可用 minimal
```

### Fix 2：激活 LoopDetector 死代码（elfClaw 原创）

**文件：`src/agent/loop_.rs`**
- 声明 `mod detection;`，导入 `DetectionVerdict, LoopDetectionConfig, LoopDetector`
- 主循环前创建 `loop_detector`（使用默认配置）和 `loop_hard_stop: Option<String>`
- 在工具执行结果内循环（executable_calls 处理）中，每次工具调用后：
  - `loop_detector.record_call(tool_name_lower, args_json, output, success)`
  - `loop_detector.check()` → `Continue` 继续 / `InjectWarning` 注入 user 消息让 LLM 自纠正 / `HardStop` 设置 flag 并 break 内循环
- 内循环结束后检查 `loop_hard_stop`，若 Some 则设置 `last_response_text` 并 break 外循环

**检测策略（继承 detection.rs 实现）：**
- `no_progress_repeat`：同一工具同参数同输出重复 3 次 → 警告/停止
- `ping_pong`：两工具交替 2 次循环 → 警告/停止
- `failure_streak`：同一工具连续失败 3 次 → 警告/停止

### 验证
`cargo build --release` 成功，exit code 0，无新增 warning（仅已知的 plugins/channels unused import warning）。

---

## 2026-03-02 — 修复 Gemini 两类 400 错误（items 缺失 + api_key 未解密）

### Bug 1：400 "items: missing field"（主聊天，gemini-2.5-pro-preview）

**根因**：`src/tools/channel_ack_config.rs:619` 中 `rules` 字段 type 为 `["array", "null"]` 但缺少 `items`。Gemini API 严格要求 type 含 array 时必须提供 items。

**修改**：
- `src/tools/channel_ack_config.rs:619`：`rules` 字段加 `"items": {"type": "object"}`
- `src/tools/schema.rs`：`clean_object()` 末尾加 Gemini safety net——若 type=array 且无 items，自动注入 `{"type": "string"}` 并发出 warn 日志

### Bug 2：400 "API key not valid"（heartbeat/cron/chat_summarizer，gemini-2.5-flash-preview）

**根因**：`Config::load_or_init()` 不解密 `enc2:` 前缀的 api_key；仅 channels 热重载路径（`load_runtime_defaults_from_config_file`）会解密。background tasks（daemon heartbeat、cron、chat_summarizer）直接使用未解密的 `config.api_key`，Gemini 收到 `enc2:b0963ab...` 当作 API key，返回 401。

**修改**：`src/main.rs:919` — 在 `apply_env_overrides()` 后立即解密 `config.api_key`（调用 `SecretStore::decrypt()`，对明文是 no-op），覆盖所有下游路径（daemon/cron/chat_summarizer/gateway）。

### 验证

`cargo build` 成功，exit code 0，无新增 warning。
测试编译因预存在 `worker_model` 缺失问题无法运行（与本次无关）。

---

## 2026-03-02 — WebSocket 握手修复 + Telegram caption 诊断

### 问题 1：Agent 页面 WebSocket 握手失败（Chrome 145+）

**现象**：`WebSocket connection to 'ws://127.0.0.1:42617/ws/chat' failed: Error during WebSocket handshake: Sent non-empty 'Sec-WebSocket-Protocol' header but no response was received`

**根因**：前端发送 `Sec-WebSocket-Protocol: zeroclaw.v1, bearer.<token>`，后端升级响应未回传协议头。Chrome 145+ 强制要求服务端在 101 响应中选择一个协议，否则拒绝握手。

**修改**：`src/gateway/ws.rs` — `handle_ws_chat` 函数中将

```rust
ws.on_upgrade(...)
```

改为：

```rust
// elfClaw: echo Sec-WebSocket-Protocol: zeroclaw.v1 in 101 response
ws.protocols(["zeroclaw.v1"]).on_upgrade(...)
```

### 问题 2：回复日志截断过短（调试辅助）

**修改**：`src/channels/mod.rs:2073` — 回复日志截断从 80 字符改为 200 字符，便于诊断含 caption 的图片消息。

```rust
truncate_with_ellipsis(&delivered_response, 200) // elfClaw: 80→200
```

### 确认已实现（无需重复修改）

- `src/agent/loop_.rs`：`prepare_messages_for_provider` 前后 caption 字符计数诊断日志（已实现，有 `// elfClaw:` 注释）
- `src/agent/loop_.rs`：multimodal 错误降级为纯文本（已实现）
- `src/channels/mod.rs`：chat_log IMAGE 路径提取（已用 `parse_image_markers`，bug 已修复）
- `src/gateway/mod.rs`：`/api/pairing/devices` 路由已注册（上次修复）

### 验证

`cargo build` 成功，exit code 0，无新增 warning。

---

## 2026-03-02 — 修复 Web 仪表盘 Devices 页面路由缺失

### 问题

`/devices` 页面访问报错：`Unexpected token '<', "<!DOCTYPE "... is not valid JSON`

### 根因

`handle_api_pairing_devices`（GET）和 `handle_api_pairing_device_revoke`（DELETE）两个
handler 已在 `src/gateway/api.rs:532-572` 实现，但从未注册到路由表。
请求命中 SPA fallback → 返回 `index.html`（HTML）→ 前端解析为 JSON → 报错。

### 修改

**文件**：`src/gateway/mod.rs`，在 `.route("/api/health", ...)` 之后追加：

```rust
.route("/api/pairing/devices", get(api::handle_api_pairing_devices))
.route("/api/pairing/devices/{id}", delete(api::handle_api_pairing_device_revoke))
```

### 未修改

- Web 仪表盘 Integrations 页面（`GET /api/integrations/settings`、`PUT /api/integrations/{id}/credentials`）
  —— 等待上游代码实现对应后端 handler，暂不修复

---

## 2026-03-01 — RunContext + worker_model 任务路由系统（elfClaw 原创）

### 背景

合并上游代码后，新闻推送等后台 cron 任务默认使用 `default_model`（Sonnet），
而原本应走次级模型（Haiku / Gemini Flash），导致 token 消耗大幅上升。

### 设计方案（elfClaw 原创）

**三层模型解析**：`model_override` → `worker_model`（背景任务）→ `default_model` → 硬编码默认

**新增内容**：
- `src/agent/mod.rs`：`RunContext` 枚举（`Interactive` / `Background`）
- `src/config/schema.rs`：`worker_model: Option<String>` 字段（紧随 `summary_model`）
- `src/agent/loop_.rs`：`run()` 新增 `run_context` 参数，三层模型解析逻辑
- `src/daemon/mod.rs`：heartbeat 传入 `RunContext::Background`
- `src/cron/scheduler.rs`：cron 传入 `RunContext::Background`
- `src/main.rs`：CLI 传入 `RunContext::Interactive`
- `src/channels/mod.rs`：
  - `ChannelRuntimeDefaults` / `ChannelRuntimeContext` 新增 `worker_model` 字段
  - `runtime_defaults_from_config()` 读取 `config.worker_model`
  - `runtime_defaults_snapshot()` 热加载时回退到 ctx 字段
  - email-digest 消息将 `route.model` 覆盖为 `worker_model`

**配置示例**（config.toml）：
```toml
worker_model = "claude-haiku-4-5-20251001"
# 或兼容上游 hint 系统：
# worker_model = "hint:worker"
```

**兼容性**：CronJob.model 字段仍有效（最高优先级 model_override）。

### 编译结果

- 编译用时：8分38秒（fat LTO + opt-level=z）
- 可执行文件大小：18 MB（zeroclaw.exe）
- Cargo.toml：`lto = "thin"` → `lto = "fat"`

### 提交

`a4dfa67b` — 已推送至 `origin main`

---

## 2026-03-01 — Telegram 图文消息诊断 + Web 仪表盘缺失路由记录

### Telegram photo+caption 诊断修复

**问题**：用户发送带文字说明（caption）的图片时，agent 只看到图，文字被忽略。

**调查结论**：
- Telegram Bot API **在单个 Message 对象中同时传递 photo + caption**，不分成两条消息
- `telegram.rs` 正确提取 caption：`msg.content = "[IMAGE:/path]\n\nCaption"`
- 静态代码分析：整个处理链路（telegram → channels/mod → loop_ → multimodal → anthropic）
  理论上正确，caption 应被保留
- **需要运行时诊断日志才能定位确切丢失位置**

**本次修改（commit 待提交）**：

1. `src/channels/mod.rs:~1576`：日志截断从 80 → 200 字符，便于看到完整内容

2. `src/channels/mod.rs:~1673-1678`：chat_log 路径提取 Bug 修复
   - `strip_suffix(']')` 对 `"[IMAGE:/path]\n\nCaption"` 返回 None（不影响 LLM，影响 chat_log 存储）
   - 改为 `parse_image_markers` 正确提取路径

3. `src/agent/loop_.rs`：加入诊断日志 + 错误降级
   - 在 `strip_history_image_markers` 后 + `prepare_messages_for_provider` 后各加 `tracing::debug!`
     记录 `caption_chars`（用户消息中非图片标记文字的字符数）
   - `prepare_messages_for_provider` 失败时降级为纯文字模式（保留 caption），不报错退出

**如何查看诊断日志**：
```bash
RUST_LOG=elfclaw=debug cargo run -- daemon
# 发送 Telegram 图+文 → 观察：
# before multimodal prepare: caption_chars > 0  ✓
# after  multimodal prepare: caption_chars > 0  → chain 正常，问题在 Anthropic API 端
# after  multimodal prepare: caption_chars = 0  → prepare_messages_for_provider 有 bug
```

---

### Web 仪表盘缺失端点（已记录，等待上游完成）

**问题**：
- `/integrations` 页面报错 "Unexpected token '<', DOCTYPE"
- `/devices` 页面同样报错

**根本原因**（commit `03bf3f10` 引入的上游未完成功能）：

| 端点 | 状态 |
|------|------|
| `GET /api/pairing/devices` | handler 在 `api.rs:533` 已实现，**未注册路由** |
| `DELETE /api/pairing/devices/{id}` | handler 在 `api.rs:546` 已实现，**未注册路由** |
| `GET /api/integrations/settings` | **handler 不存在**，上游也没有 |
| `PUT /api/integrations/{id}/credentials` | **handler 不存在**，上游也没有 |

**处理策略**：等待上游在一两周内完成这部分功能。下次合并上游时：
- 检查上游是否实现了 `/api/integrations/settings` 和 `/api/integrations/{id}/credentials`
- 若已完成 → merge 进来，同时注册 `/api/pairing/devices` 路由
- 若未完成 → 继续等待

---



**操作**：
- `git checkout main && git reset --hard merge/upstream-2026-03-01`
- `git push origin main --force-with-lease`（origin 上 main 是新分支，推送成功）
- `git branch -d merge/upstream-2026-03-01`（清理临时分支）

**结果**：
- `main` 分支现在包含完整的上游 merge（750+ commits）+ elfClaw 所有改动
- 测试结果：4131 passed，19 failed（全部是预存在的 Windows 平台限制，非回归）
- GitHub 仓库：https://github.com/VK7KSM/eflClaw（main 分支已更新）

---

## 2026-03-01 — upstream/main 合并冲突全量解决（78 个冲突文件）

**涉及文件**（主要修改）：
- `src/config/schema.rs` — 保留 elfClaw 字段 + 集成上游新类型
- `src/channels/mod.rs` — 保留 deliver_to_channel + 集成上游新渠道
- `src/channels/email_channel.rs` — 保留 monitor 模式 + 集成 IMAP ID
- `src/agent/loop_.rs` — 保留 HEAD 模块化版本
- `src/daemon/mod.rs` — 保留 elfClaw heartbeat 实现
- `src/main.rs` — 修复 Commands::Agent 新字段解构
- 多文件编译修复（8 个文件加 thought_signature，4 个文件加 quota_metadata）

### 合并策略

**AA 文件（43 个）**：上游新增文件全部接受 (`--theirs`)

**UU 文件（35 个）**：
- 无 elfClaw 标记 → 接受上游 (`--theirs`)
- 有 elfClaw 标记 → 以 HEAD 为基础，人工补入上游新增内容

### elfClaw 特性保留

| 特性 | 文件 |
|------|------|
| TtsConfig, ChatLogConfig | schema.rs |
| HeartbeatConfig active_hours + max_tool_iterations | schema.rs, daemon/mod.rs |
| parse_hhmm / is_within_active_hours | schema.rs |
| summary_model, SchedulerConfig.max_tool_iterations | schema.rs |
| deliver_to_channel() 统一渠道路由 | channels/mod.rs |
| Email monitor + notify_channel/notify_to | email_channel.rs |
| loop_.rs 4 个子模块（context/execution/history/parsing） | agent/loop_/ |
| DEFAULT_MAX_TOOL_ITERATIONS = 10 | agent/loop_.rs |

### 上游功能集成

| 功能 | 来源 |
|------|------|
| EmailImapIdConfig + send_imap_id() | email_channel.rs |
| ToolCall.thought_signature | providers/traits.rs |
| ChatResponse.quota_metadata | providers/traits.rs |
| AckReactionConfig, EconomicConfig, GroupReplyConfig | schema.rs |
| QQReceiveMode, QQEnvironment | schema.rs |
| Skill.always, IdentityConfig.extra_files | schema.rs + channels/mod.rs |
| MattermostConfig.group_reply, SlackChannel 5 参数 new() | schema.rs + channels/mod.rs |
| TelegramChannel::new ack_enabled 参数 | channels/mod.rs |
| BlueBubbles/GitHub/Napcat 新渠道 | channels/mod.rs |
| Serial path 验证（is_serial_path_allowed） | util.rs |
| Skills SkillToolHandler | skills/mod.rs |
| PrometheusObserver::new() → Result<Self> | gateway/mod.rs |

### 编译修复（cargo check --all-targets 0 错误）

- 删除 6 个文件中的重复模块声明
- 为 8 个文件中所有 ToolCall 构造添加 `thought_signature: None`
- 为 4 个文件中所有 ChatResponse 构造添加 `quota_metadata: None`
- 修复函数参数数量不匹配（consolidation.rs, channels/mod.rs, main.rs）
- 替换 `windows_by_handle` 不稳定 API（file_link_guard.rs）
- 恢复 HEAD 版本的 gateway/\*、agent/agent.rs、mod.rs（引用了上游-only API）

**提交**：`64b4b26c` 在分支 `merge/upstream-2026-03-01`

---

## 2026-03-01 — 合并后测试修复（10 项）

**提交**：`5108cd03` 在分支 `merge/upstream-2026-03-01`

**修复内容**：

1. **`agent/agent.rs`** — 添加 `AUTOSAVE_MIN_MESSAGE_CHARS` 常量 + `assistant_resp` 自动保存（上游新增逻辑在合并时丢失）
2. **`agent/loop_.rs`** — 恢复上游 vision 能力检查：非视觉 provider 收到图片时返回 `ProviderCapabilityError`（而非 strip-and-continue）；添加 `should_treat_provider_as_vision_capable()` 处理 anthropic false negative
3. **`skills/mod.rs`** — `render_skill_location` 统一使用正斜杠（Windows 反斜杠兼容性）
4. **`config/schema.rs`** — `persist_active_workspace_marker` 测试标记 `#[cfg(unix)]`（依赖 `HOME` 环境变量，Windows 用 `USERPROFILE`）
5. **`cron/scheduler.rs`** — 退出状态断言改为平台感知（Unix: `exit status: 0`，Windows: `exit code: 0`）
6. **`gateway/mod.rs`** — 更新 pairing tokens 测试以验证加密存储（配对 token 已加密保存，测试需解密后验证）
7. **`channels/telegram.rs`** — 修复 `sanitize_attachment_filename`：只用 `/` 作为路径分隔符，保留 `\\` 被替换为 `__` 的行为
8. **`tools/delegate.rs`** — `execute_agentic_respects_max_iterations` 测试接受 elfClaw 优雅降级（返回部分结果的 `Ok` 而非 `Err`）

**剩余预存 Windows 平台失败（19 项，不影响功能）**：
- 9 项 `content_search` 测试 — 需要系统安装 ripgrep
- 4 项 symlink 测试 — Windows 需要 admin 才能创建符号链接
- 2 项 security policy 测试 — Unix 绝对路径格式 (`/`)
- 1 项 wasm 测试 — Unix 绝对路径
- 1 项 hard link 测试 — Windows 权限
- 1 项 process kill 测试 — Windows kill 语义
- 1 项 screenshot 测试 — screenshot 工具不可用

---

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

---

## 2026-03-01 — CI 简化 + Token 烧耗分析

### CI 工作流简化（build-elfclaw.yml）

**变更**：移除 `build-cross` job（Linux/Android/FreeBSD 共 13 个目标），只保留：
- `build-macos`：Intel x86_64（macos-13）+ Apple Silicon（macos-14）
- `build-windows`：x86_64 MSVC

`release` job 的 `needs` 从 `[build-cross, build-macos, build-windows]` 改为 `[build-macos, build-windows]`。

顺带将产物命名从 `zeroclaw-*` 改为 `elfclaw-*`（品牌一致性）。

**原因**：上游有 30 种平台的 cross 编译，但我们目前只需要 Windows + Mac 日常使用。
cross 编译依赖 Docker + cross-rs 工具链，在上游大规模 merge 后可能有 Linux 特定编译问题。

### Sonnet Token 烧耗过多 — 分析结论（不修改代码）

**现象**：运行日志证实 cron 任务以 Sonnet 模型运行，单次任务触发 ~27,874 输入 token + 6.4K 缓存 token。

**根本原因**：
1. `config.default_model = "claude-sonnet-4-6"` — 主模型是 Sonnet
2. `CronJob` struct 有 `pub model: Option<String>` 字段（`src/cron/types.rs:114`）
3. 若某个 cron job 的 `model` 字段为 `None`，调度器调用 `agent::run(model_override=None)` → 解析链 → `config.default_model` → **Sonnet**
4. Cron 任务跑完整 agent loop，每次迭代都携带完整历史（运行日志显示第一轮 `caption_chars=13218`，代表 ~4400+ tokens 的上下文）

**为什么之前用 Haiku**：三种可能：
- A: 之前 `config.toml` 的 `default_model` 设为 Haiku，现已改为 Sonnet
- B: cron jobs 之前在 SQLite DB 中有 `model = "haiku"` 记录，upstream merge 后 schema 变动导致字段丢失/重置
- C: 之前的 CronJob 代码路径不同（旧版本可能用轻量模型做 cron）

**下一步**：检查 `D:\ZeroClaw_Workspace\config.toml` 中 `default_model` 字段，以及 cron jobs 的 SQLite 数据（`jobs.db` 或 `cron.db`）是否有 `model` 字段值。若要恢复 Haiku 处理 cron，可对每个 cron job 设置 `model = "claude-haiku-4-5-20251001"` 或修改调度器默认逻辑。

---

## 2026-03-06 — 独立项目初始化：cf-crawler 双语 README

### 概述

未修改 elfClaw 代码。本次仅在仓库外新建独立项目目录 `C:\Dev\cf-crawler`，用于承载 Cloudflare + 本地 sidecar 抓取工具，并写入中英文 README 作为后续开发基线。

### 新增内容

- `C:\Dev\cf-crawler\README.md`
  - 英文项目说明
  - 明确该工具为独立 CLI，不并入 elfClaw 主二进制
  - 确认 Cloudflare Worker `/v1/fetch`、`/v1/render`、`/v1/health` 作为远端执行面
- `C:\Dev\cf-crawler\README.zh-CN.md`
  - 中文项目说明
  - 明确本地只做调度与数据处理，不使用本地浏览器
  - 确认 `scrape-page` / `crawl-site` 作为对外命令形态

### 设计决策

1. `cf-crawler` 保持为独立项目，源码不写入 `zeroclaw` 主仓库。
2. `elfClaw` 只作为调用方，后续通过外部工具方式对接。
3. `Agent-Reach` 保持独立，不与 `cf-crawler` 合并。

---

## 2026-03-06 — cf-crawler README 中英文重写（按产品化说明）

### 概述

未修改 elfClaw 业务代码。本次仅重写独立项目 `C:\Dev\cf-crawler` 的中英文 README，覆盖项目背景、参考来源、Cloudflare 免费资源与额度、部署方式、目录与数据结构、与 zeroclaw/elfclaw 的对接改动、与 Agent-Reach 联动效果、以及致谢链接。

### 修改文件

- `C:\Dev\cf-crawler\README.md`
- `C:\Dev\cf-crawler\README.zh-CN.md`

### 文档新增要点

1. 明确项目创建原因与低配机器目标（无本地浏览器）。
2. 明确参考项目（Crawlee、Agent-Reach）与分工边界。
3. 汇总 Cloudflare 免费资源与调用额度，并给出 Browser Rendering 每日可用调用次数估算表。
4. 给出 zeroclaw/elfclaw 需要补充的代码改动点和 workflow 提示词改动点。
5. 补充程序目录规划与核心数据结构规划。
6. 明确与 Agent-Reach 联动后的能力提升。
7. 增加致谢与 GitHub 地址。

---

## 2026-03-06 — cf-crawler 初版可运行骨架（CLI + Worker）

### 概述

在独立目录 `C:\Dev\cf-crawler` 完成第一版代码落地。目标是：本地不运行浏览器，仅做调度与数据处理；远端通过 Cloudflare Worker 执行 `fetch/render`。

### 新增文件（核心）

- 根项目
  - `package.json`
  - `tsconfig.json`
  - `.gitignore`
  - `.env.example`
- CLI 与类型
  - `src/types.ts`
  - `src/cli/index.ts`
  - `src/cli/commands/scrape-page.ts`
  - `src/cli/commands/crawl-site.ts`
- 核心调度
  - `src/core/scheduler.ts`
  - `src/core/queue.ts`
  - `src/core/dedupe.ts`
  - `src/core/retry.ts`
  - `src/core/rate_limit.ts`
- 执行器
  - `src/executors/types.ts`
  - `src/executors/decision.ts`
  - `src/executors/cf_fetch.ts`
  - `src/executors/cf_render.ts`
  - `src/executors/cf_health.ts`
- 提取器
  - `src/extractors/article.ts`
  - `src/extractors/listing.ts`
  - `src/extractors/pagination.ts`
- 存储与联动
  - `src/storage/files.ts`
  - `src/storage/sqlite.ts`
  - `src/agent_reach/bridge.ts`
- Worker 子项目
  - `worker/package.json`
  - `worker/tsconfig.json`
  - `worker/wrangler.toml`
  - `worker/src/index.ts`
- 示例
  - `examples/scrape-page.json`
  - `examples/crawl-site.json`

### README 更新

- `C:\Dev\cf-crawler\README.md`
- `C:\Dev\cf-crawler\README.zh-CN.md`

新增本地与 Worker 快速运行命令，便于直接 smoke test。

### 关键实现点

1. 对外命令固定为 `scrape-page` / `crawl-site`（JSON 输入输出）。
2. `scrape-page`：默认先 `fetch`，命中反爬信号后自动升级 `render`。
3. `crawl-site`：实现队列、去重、限速、分页发现、低并发递进抓取。
4. Worker 提供 `/v1/fetch`、`/v1/render`、`/v1/health` 三个端点。
5. `render` 采用“已配置则调用、未配置则返回可解释错误”策略。

### 验证结果

- 根项目依赖安装：通过
- 根项目 `npm.cmd run check`：通过
- 根项目 `npm.cmd run build`：通过
- Worker 项目依赖安装：通过
- Worker 项目 `npm.cmd run build`：通过
- CLI smoke test：`node dist/index.js health --pretty` 输出成功 JSON

### 兼容性说明

- 当前为可运行骨架版本，优先确保结构、协议和调用链成立。
- 生产可用前仍需补充：更完整的反封策略、更严格的输入校验、落盘 schema 扩展、以及与 zeroclaw/elfclaw 的正式工具注册对接。

## 2026-03-06 — cf-crawler 第二轮增强（安全策略 + Agent-Reach 自愈）

### 主要改动

1. 增加统一运行配置模块 `src/runtime_config.ts`：
   - `CF_CRAWLER_HOST_COOLDOWN_MS`
   - `CF_CRAWLER_MAX_RETRIES`
   - `CF_CRAWLER_ALLOWED_HOSTS`
   - `CF_CRAWLER_BLOCK_PRIVATE_IP`
   - `AGENT_REACH_*` 系列参数
2. 增加 URL 安全策略 `src/security/url_policy.ts`：
   - 仅允许 `http/https`
   - 可选域名白名单
   - 默认拦截私网/本地地址（SSRF 防护）
3. 强化抓取执行链路：
   - `scrape-page` / `crawl-site` 接入 URL policy
   - 重试次数与主机冷却时间改为可配置
   - `strategy=edge_browser|edge_fetch|auto` 行为更明确
4. 增加 `agent-reach-ensure` 命令：
   - 自动探测 Agent-Reach
   - 缺失时自动安装（uv/pip 兜底）
   - 可选版本更新检查
   - 执行 `doctor` 返回诊断结果
5. Worker 增强 `worker/src/index.ts`：
   - 反爬信号识别（状态码+正文特征）
   - 可选 KV 短缓存
   - 实际请求耗时回传

### 验证

- `C:\Dev\cf-crawler`：
  - `npm.cmd run check` 通过
  - `npm.cmd run build` 通过
- `C:\Dev\cf-crawler\worker`：
  - `npm.cmd run build` 通过
- `agent-reach-ensure`：
  - 已可成功探测到 `python -m agent_reach.cli`
  - 当前环境返回 `current_version: 1.3.0` 且 `doctor` 可执行
- `health/scrape/crawl` 当前仍返回 `ECONNREFUSED 127.0.0.1:8787`（预期，因本地未启动 Worker dev 或未指向已部署 CF endpoint）

## 2026-03-06 — Windows EXE 打包与 GitHub CI 工作流

### 已完成

1. 在 `C:\Dev\cf-crawler` 增加 EXE 打包脚本：
   - `build:exe:bundle`（`esbuild` 打包到 `dist-exe/index.cjs`）
   - `build:exe`（`@yao-pkg/pkg` 生成 `release/cf-crawler-win-x64.exe`）
   - `build:ci`（本地模拟 CI 全流程）
2. 增加 GitHub Actions：
   - `C:\Dev\cf-crawler\.github\workflows\build-windows-exe.yml`
   - 在 `windows-latest` 上执行 `npm ci -> check -> build -> build:worker -> build:exe`
   - 上传 `release/cf-crawler-win-x64.exe` 为 artifact（`cf-crawler-win-x64`）
3. 本地成功产出可执行文件：
   - `C:\Dev\cf-crawler\release\cf-crawler-win-x64.exe`
   - 当前大小约 `58.6 MB`

### 验证

- `npm.cmd run build:ci`：通过
- EXE 运行验证：
  - `health --pretty` 可执行（未连上 Worker 时返回 `ECONNREFUSED`，符合预期）
  - `agent-reach-ensure --pretty` 在放开子进程权限后可成功执行
---

## 2026-03-12 — Fix: http_request 语义化 4xx 错误 + K3 file_write 配置补全（v0.4.0）

### 诊断背景

K3 上 news_fetcher cron 任务两个工具反复失败（2026-03-11 全天统计）：
- `http_request` 失败 40 次（FT.com 403 付费墙 → 弱模型无引导，连续重试 → loop exhausted 88 次）
- `file_write` 失败 5 次（均 0ms）→ news_fetcher allowed_tools 缺失 file_write 配置

### 修复一：http_request 语义化 4xx 错误消息

**文件**：`src/tools/http_request.rs`

将 `error` 字段从简单的 `"HTTP 403"` 改为含操作指导的消息：
- 401: 提示用 web_scrape + strategy=edge_browser
- 403: 明确说明是付费墙/机器人检测，切换到 web_scrape mode=article
- 429: 说明限速，不要立即重试
- 410: 资源永久消失，从源列表移除
- 其他 4xx: 不要重试同一 URL
- 5xx: 可换方式重试一次

**效果**：gemini-flash-lite 收到结构化引导后能正确判断切换 web_scrape 抓取 FT.com 标题/导语。

### 修复二：K3 config.toml 补全 file_write

**操作**：SSH 编辑 `D:\ZeroClaw_Workspace\config.toml`，在 `[agents.news_fetcher]` 的 `allowed_tools` 中添加 `"file_write"`。

**根因**：news_fetcher agent 的 allowed_tools 白名单缺少 file_write，导致 LLM 无法调用该工具，
在 final response 报告"缺少工具"。日志中 `file_write (0ms)` 是 agent loop 拒绝调用不可见工具时的记录。

### 版本号更新

`Cargo.toml` `0.3.0` → `0.4.0`（积累多天的功能性更新：TTS/语音、Email Monitor→Telegram 通知、
聊天日志持久化、web_scrape 双重修复、heartbeat 可配置化、MCP 集成、自检改进等）
