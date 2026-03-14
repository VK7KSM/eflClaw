# 上游 zeroclaw 与 elfClaw 对比分析报告

> 生成日期：2026-03-12
> 分析范围：上游 `zeroclaw-labs/zeroclaw`（本地路径 `C:\Dev\zeroclaw_original`）vs elfClaw fork（`C:\Dev\zeroclaw`）
> **本文档仅作分析参考，不记录已合并的功能。合并进度见末尾合并计划表。**

---

## 一、Max-Tokens Continuation（最大令牌续写机制）

**问题背景**

当 LLM 生成非常长的回复（例如写长篇代码、大量分析）时，模型可能因达到单次请求的最大输出 token 限制而被截断，返回 `stop_reason = "max_tokens"`。这时候 LLM 的回复是半截的——代码写到一半、句子说到中间就结束了。elfClaw 对这种情况完全没有任何处理，截断了就截断了，用户或 agent 必须手动重新提问。

**上游怎么实现的**

上游在 `src/agent/loop_.rs` 顶部定义了三个专用常量（大约在第 60-65 行）：

```rust
const MAX_TOKENS_CONTINUATION_RETRIES: usize = 3;
const MAX_TOKENS_CONTINUATION_CONTENT_CAP: usize = 120_000;
const MAX_TOKENS_CONTINUATION_EMPTY_THRESHOLD: usize = 50;
```

这三个常量分别控制：最多续写几次（3 次）、累积内容上限（120,000 字符）、如果续写结果少于 50 个字符就视为无效并停止。

实际的续写逻辑在大约 1440-1582 行。当 `run_agent_loop()` 主函数收到 provider 的响应并检测到停止原因是 `NormalizedStopReason::MaxTokens` 时，就进入一个专门的 `while` 循环。这个循环构建一条 continuation message，内容是：

```
Continue your previous response. Do not repeat any content that has already been output; begin immediately where you left off.
```

这条消息被加入对话历史，然后再次调用 provider，获得续写部分。续写得到的文本通过 `merge_continuation_text()` 函数与之前的内容合并。

`merge_continuation_text()` 函数（在第 565-620 行附近）非常精细——它不是简单地把两段文本拼接起来，因为 LLM 续写时经常会把上一段的最后几个词再重复一遍作为"衔接"。函数先检查两段文本是否有重叠部分：它从第一段文本的末尾开始往前找，寻找是否有一段子串同时出现在第二段的开头。如果找到了重叠，就去掉重复部分后拼接。如果没找到重叠，就直接拼接。这样最终用户看到的是一段连贯的完整输出。

整个续写过程会向 `runtime_trace` 发送进度事件，告知"已续写 N 次，当前累积 X 字符"，便于观测。

elfClaw 的 `loop_.rs` 中完全没有这套机制。如果模型输出被 max_tokens 截断，会话就结束了，截断内容就是最终输出。

---

## 二、NormalizedStopReason（规范化停止原因枚举）

**问题背景**

不同的 LLM 提供商对"为什么停止生成"这件事有各自不同的表达方式。OpenAI 用字符串 `"stop"` 和 `"tool_calls"`，Anthropic 用 `"end_turn"` 和 `"tool_use"`，Gemini 有 `STOP`、`MAX_TOKENS`、`SAFETY`……如果 agent 主循环要根据停止原因做决策（比如"是否需要续写"），就必须逐个 provider 硬编码判断逻辑，非常脆弱。

**上游怎么实现的**

上游在 `src/providers/traits.rs` 中定义了一个 `NormalizedStopReason` 枚举，统一抽象所有 provider 的停止原因：

```rust
pub enum NormalizedStopReason {
    EndTurn,
    ToolCall,
    MaxTokens,
    ContextWindowExceeded,
    SafetyBlocked,
    Cancelled,
    Unknown(String),
}
```

每个 provider 实现者（OpenAI adapter、Anthropic adapter、Gemini adapter 等）负责把自己特定的停止字符串映射到这个枚举。`agent/loop_.rs` 主循环只面对这个枚举做决策：`MaxTokens` → 触发续写，`ContextWindowExceeded` → 触发历史压缩，`SafetyBlocked` → 特殊错误处理，`EndTurn` 和 `ToolCall` → 正常流程。

elfClaw 目前的 providers 返回的是原始字符串或各自特定的类型，`loop_.rs` 里有散落的 `stop_reason.contains("max_tokens")` 等字符串比较。这在支持的 provider 少时问题不大，但随着 provider 增加会越来越难维护。

---

## 三、Cost Enforcement（预算执行系统）

**问题背景**

在生产环境中让 agent 自主运行，最大的风险之一是 API 费用失控。一个陷入循环的 agent、一个被注入恶意指令的 agent、或者一个处理了超大量数据的 agent，都可能在短时间内消耗数十甚至数百美元的 API 费用。elfClaw 没有任何费用管控机制。

**上游怎么实现的**

上游构建了一套完整的预算管控系统，跨越多个文件。

在 `src/agent/loop_.rs` 的 327-460 行区域，有这样几个关键结构：

`CostEnforcementContext` 结构体持有当前 session 的已用费用（`session_usd_spent: f64`）、单次请求费用上限（`max_request_cost_usd: Option<f64>`）、session 总费用上限（`max_session_cost_usd: Option<f64>`）、以及一个 `enforcement_mode: CostEnforcementMode`。

`CostEnforcementMode` 是一个枚举，有三个值：
- `Warn` — 超出预算时只记录日志警告，不阻止执行
- `RouteDown` — 超出预算时自动切换到更便宜的模型（使用 `model_fallbacks` 配置）
- `Block` — 超出预算时直接拒绝执行，返回错误

在发出每次 LLM 请求之前，`estimate_request_cost_usd()` 函数（第 440 行附近）先估算这次请求大概会花多少钱。估算方式是：统计 `messages` 中所有内容的字符数，用字符数除以 4 估算 token 数（粗略估计），再乘以对应模型的单价。模型单价通过 `lookup_model_pricing()` 函数查询，它内置了主流模型的 input/output token 价格表。

elfClaw 没有任何类似机制，完全没有费用追踪和预算管控。

---

## 四、Safety Heartbeat（安全心跳机制）

**问题背景**

LLM 在长对话中有一个公知的弱点：随着对话历史越来越长，早期 system prompt 中注入的安全规则和行为约束，会逐渐被 LLM "遗忘"——不是真正遗忘，而是因为 context window 中最近的内容权重更大，而久远的 system prompt 内容相对注意力下降。对于一个长时间运行的 agent session，如果安全策略只在最开始注入一次，那么经过几十轮对话后，LLM 可能开始忽略这些约束。

**上游怎么实现的**

上游在 `loop_.rs` 的第 318-379 行区域定义了 `SafetyHeartbeatConfig`：

```rust
pub struct SafetyHeartbeatConfig {
    pub interval_turns: usize,
    pub safety_reminder_text: String,
    pub enabled: bool,
}
```

主循环在每次 LLM 响应结束、准备下一轮时，会检查当前 turn 计数是否是 `interval_turns` 的倍数。如果是，就向对话历史中插入一条特殊的 `user` 角色消息，内容是 `safety_reminder_text`，然后立刻在后面加一条 `assistant` 角色的 ACK 消息。这一对消息对最终用户不可见，只存在于发送给 LLM 的 messages 列表中，起到定期"刷新"安全记忆的作用。

默认配置是每 15 个 turn 重注入一次。elfClaw 没有这个机制。

---

## 五、Non-CLI Approval（非 CLI 环境的工具执行审批）

**问题背景**

在 CLI 环境下，agent 要执行危险操作时，可以直接在终端问用户"是否确认？"。但在 Telegram bot、Discord bot 这类渠道里，用户是通过聊天界面交互的，没有终端提示符。上游对这种情况设计了一套专门的审批流程。

**上游怎么实现的**

上游在 `loop_.rs` 第 292-316 行定义了：

```rust
const NON_CLI_APPROVAL_WAIT_TIMEOUT_SECS: u64 = 300;

pub struct NonCliApprovalContext {
    pub pending_tool_name: String,
    pub pending_tool_args: serde_json::Value,
    pub approval_request_sent_at: Instant,
    pub approval_channel: String,
    pub reply_target: String,
}
```

当 agent 在非 CLI 渠道中需要执行需要审批的工具时，它不是直接执行，而是构建审批请求消息发给用户，然后进入 250ms 轮询循环等待 YES/NO 响应，300 秒超时后取消。elfClaw 在 Telegram bot 模式下没有工具审批机制，agent 会直接执行所有工具。

---

## 六、ProgressMode（进度显示三档配置）

上游定义了 `ProgressMode` 枚举，有三个值：
- `Verbose`：每次工具调用开始和返回时都输出完整信息（相当于 elfClaw 目前的唯一行为模式）
- `Compact`：只显示工具调用的一行摘要，不显示返回结果
- `Off`：完全静默，只有最终 LLM 回复出现时才产生输出

这个配置可以在 config 中按 channel 设置，比如 Telegram channel 默认 Compact，heartbeat/cron 任务默认 Off。elfClaw 目前只有 Verbose 模式，cron 任务下工具调用细节都会发到 Telegram，很吵。

上游位置：`config/schema.rs:4628`

---

## 七、Interactive Slash Commands with rustyline

上游 CLI 模式引入了 `rustyline`（跨平台 readline 库），并定义了 `SlashCommandCompleter` 实现 Tab 补全。已实现的斜杠命令包括 `/help`、`/clear`、`/model <name>`、`/cost`、`/tools`、`/save`。elfClaw 没有 `rustyline`，CLI 模式下没有 Tab 补全或斜杠命令支持。

---

## 八、Team Orchestration Engine（多 Agent 团队编排引擎）

上游 `src/agent/team_orchestration.rs`（2140 行）实现了完整的多 agent 团队协作系统。

`TeamTopology` 枚举定义四种协作拓扑：
- `Single`：单 agent，`execution_factor=1.00, base_pass_rate=0.78`
- `LeadSubagent`：主 agent 指挥子 agent，`factor=0.95, pass_rate=0.84`
- `StarTeam`：一个协调者 + 多个并行执行者，`factor=0.92, pass_rate=0.88`
- `MeshTeam`：所有 agent 互相通信，`factor=0.97, pass_rate=0.82`

`BudgetTier`（Low/Medium/High）控制：最多并行 worker 数（3/5/8）、每任务消息预算（10/20/32）。

`WorkloadProfile` 区分任务类型：Debugging（执行乘数 1.12，sync 乘数 1.25）、Research（均低于 1.0，更高效）、Implementation、Mixed。

elfClaw 只有基本的 `delegate` 工具，没有自动化的团队拓扑选择、负载感知调度。

---

## 九、AgentLoadTracker + AgentSelection（运行时负载追踪与选择）

`AgentLoadTracker`（`src/tools/agent_load_tracker.rs`，243 行）是线程安全的运行时状态记录器：

```rust
pub struct AgentLoadTracker {
    inner: Arc<RwLock<HashMap<String, AgentRuntimeLoad>>>,
}
struct AgentRuntimeLoad {
    in_flight: usize,
    assignment_events: VecDeque<Instant>,
    failure_events: VecDeque<Instant>,
}
```

调用 `start()` 返回 `AgentLoadLease` RAII 租约，任务完成时调用 `mark_success()` 或 `mark_failure()`。`Drop` impl 确保未显式结束的租约自动计入失败，`in_flight` 计数器永远不会泄漏。

`snapshot(window: Duration)` 返回所有 agent 的 in_flight 数量、最近分配次数、最近失败次数，用于负载感知调度。

elfClaw 的 `delegate` 工具是完全静态的，无运行时负载感知能力。

---

## 十、Memory Time Decay（记忆时间衰减算法）

上游 `src/memory/decay.rs`（148 行）实现了指数衰减算法：

```
new_score = old_score * 2^(-age_days / half_life_days)
```

默认半衰期 7 天。`MemoryCategory::Core` 的记忆完全豁免衰减（永久有效）。7 天后分数减半，14 天后降至 1/4，21 天后降至 1/8。分数足够低后，记忆在相关性检索中自然排名靠后，实际上等同于"遗忘"，但不会被物理删除。

elfClaw 的记忆系统没有时间衰减机制，所有记忆一旦写入权重永远不变。

---

## 十一、NativeRuntime 动态 Shell 检测链

elfClaw 的 `NativeRuntime` 是空结构体，`has_shell_access()` 直接返回 `true`，`build_shell_command()` 硬编码 PowerShell（Windows）或 sh（非 Windows）。

上游的 `NativeRuntime` 有实际的 `shell: Option<ShellProgram>` 字段，通过 `detect_native_shell()` 在启动时动态探测：

**Windows 优先级**：bash → sh → pwsh → powershell → cmd → cmd.exe → COMSPEC 环境变量

**Unix 优先级**：sh → bash

`has_shell_access()` 返回 `self.shell.is_some()`——找不到任何 shell 就老实返回 false。`ShellProgram::add_shell_args()` 根据 shell 类型自动选择正确的参数格式（`-c` / `-Command` / `/C`）。纯函数 `detect_native_shell_with()` 接受可测试的闭包，10 个单元测试覆盖各种 shell 环境。

---

## 十二、plugins/discovery.rs 去重算法 Bug

elfClaw 的去重代码（第 129 行）：
```rust
indices.sort_unstable();  // 升序 — BUG
for i in indices {
    deduped.push(all_plugins.swap_remove(i));  // swap_remove 破坏了升序假设
}
```

`Vec::swap_remove(i)` 会把最后一个元素移到位置 `i`，导致后续升序 index 指向错误的元素。

上游修复（降序处理，swap_remove 不影响更小的 index）：
```rust
indices.sort_unstable_by(|a, b| b.cmp(a));  // 降序
for i in indices {
    deduped.push(all_plugins.swap_remove(i));
}
deduped.reverse();  // 恢复顺序
```

---

## 十三、DEFAULT_MAX_TOOL_ITERATIONS 差异

上游：`DEFAULT_MAX_TOOL_ITERATIONS = 20`
elfClaw：`DEFAULT_MAX_TOOL_ITERATIONS = 10`（有意为之的安全决策）

elfClaw 在 Telegram 渠道和 cron 场景下选择 10，是为了减少 agent 陷入工具调用循环时的 API 浪费。两个值都合理，各有取舍，不存在谁对谁错。

---

## 合并计划表（按难度从易到难）

| 功能 | 影响等级 | 上游位置 | elfClaw 状态 | 合并难度 | 合并状态 |
|------|---------|---------|------------|---------|---------|
| 插件去重 Bug 修复 | 🔵 低 | `plugins/discovery.rs:121-136` | 有 bug | ⭐ 极易 | ✅ 已合并 |
| 记忆时间衰减 | 🔶 高 | `memory/decay.rs`（148 行） | 完全没有 | ⭐⭐ 易 | ✅ 已合并 |
| AgentLoadTracker 负载追踪 | 🔶 中 | `tools/agent_load_tracker.rs` | 完全没有 | ⭐⭐ 易 | ✅ 已合并 |
| Shell 动态检测链 | 🔶 中 | `runtime/native.rs:57-105` | 硬编码 PowerShell | ⭐⭐ 易 | ✅ 已合并 |
| check_runtime_capabilities | 🔶 中 | `doctor/mod.rs:659-758` | 缺失 | ⭐⭐⭐ 中 | 待定 |
| ProgressMode 分级进度 | 🔶 中 | `config/schema.rs:4628` | 没有 | ⭐⭐⭐ 中 | 待定 |
| CostConfig / 预算配置 | 🔶 中 | `config/schema.rs:1384-1492` | 完全没有 | ⭐⭐⭐ 中 | 待定 |
| SubAgent 排队等待 | 🔶 中 | `subagent_spawn.rs:110-144` | 直接失败 | ⭐⭐⭐ 中 | 待定 |
| Agent 负载感知选择 | 🔶 中 | `tools/agent_selection.rs` | 完全没有 | ⭐⭐⭐ 中 | 待定 |
| 编排配置热重载 | 🔶 中 | `tools/orchestration_settings.rs` | 完全没有 | ⭐⭐⭐ 中 | 待定 |
| Safety Heartbeat 安全注入 | 🔺 高 | `loop_.rs:318-379` | 完全没有 | ⭐⭐⭐⭐ 较难 | 待定 |
| Non-CLI 工具审批 | 🔺 高 | `loop_.rs:296-316` | 完全没有 | ⭐⭐⭐⭐ 较难 | 待定 |
| NormalizedStopReason 枚举 | 🔺 高 | `providers/traits.rs:71-91` | 完全没有 | ⭐⭐⭐⭐ 较难 | 待定 |
| Cost Enforcement 预算控制 | 🔺 高 | `loop_.rs:327-460` | 完全没有 | ⭐⭐⭐⭐⭐ 难 | 待定 |
| Max-Tokens Continuation | 🔺 高 | `loop_.rs:1435-1582` | 完全没有 | ⭐⭐⭐⭐⭐ 难 | 待定 |
| 多 Agent 编排引擎 | 🔺 高 | `team_orchestration.rs`（2140 行） | 完全没有 | ⭐⭐⭐⭐⭐⭐ 极难 | 待定 |

**依赖链总结（合并顺序参考）：**

```
插件去重 Bug（无依赖）
   ↓
记忆时间衰减（无依赖）
AgentLoadTracker（无依赖）
Shell 动态检测链（无依赖）
   ↓
check_runtime_capabilities（需要 Shell 链）
Agent 负载感知选择（需要 AgentLoadTracker）
CostConfig schema（需要先规划好 Cost Enforcement）
   ↓
NormalizedStopReason（需要所有 provider 同步）
   ↓
Cost Enforcement（需要 NormalizedStopReason + CostConfig）
Max-Tokens Continuation（需要 NormalizedStopReason）
SubAgent 排队修复（需要理解现有代码）
   ↓
多 Agent 编排引擎（需要 AgentLoadTracker + Agent选择 + SubAgent + 编排配置全部就绪）
```
