# elfClaw 稳定化方案

> 本文档记录 2026-09-23 会话中确定的完整方案。开发时先读这份文档，再读 `dev_log.md` 看最新进展。
> 语言规则见 `CLAUDE.md` §0 第 1 条：面向用户的一切输出（对话、commit message、本文档）都用中文。

## 1. 背景：为什么不迁移、只做稳定化

调研过 ZeroClaw 上游、PicoClaw、Nanobot 三个候选之后的结论：

- **ZeroClaw 上游**：半年内又提交了近 4000 次，目录已重构成 `crates/`，最大文件涨到 5 万多行，方向转向多 agent/SOP 企业功能，合不回去也没用。
- **PicoClaw**：7 月后基本停止维护，最近 16 周提交数归零，有两个未修的高危 CVE（命令注入），定时任务同样有"关机期间到期任务永久丢失"的缺陷。
- **Nanobot**：维护活跃但改动极快（单周增删几万行），几乎每个小版本都有不兼容变更，短期记忆丢失、Dream 死循环等问题还开着。

elfClaw 最大的优势——**运行几乎不占系统资源**——是三者里独一份的，值得保留。real 问题不在于底层框架本身，而在于：
**很多本该用代码保证的确定性行为，被做成了"让 LLM 自己想办法"，弱模型执行不好，就在 prompt 上加"严格规则"打补丁，越补越乱。**

## 2. K6 生产环境实测数据（2026-09-23 采集）

- 部署机：K6，Intel 7Y30 双核、8GB 内存、Win10 LTSC。**IP 已变为 `192.168.2.239`**（原 `192.168.2.29` 失效，本机 `~/.ssh/config` 已更新）。
- 两个实例：`C:\dev\elfClaw\ZeroClaw_Skynet`、`C:\dev\elfClaw\ZeroClaw_Workspace`，用户已确认**都保留**。
- 7 月 25 日之后两个实例都没运行过（开机自启是"登录后触发"，机器没人登录就不会跑）。
- Workspace 实例 `jobs.db`：22 个 cron 任务，其中"新闻源搜索"一个逻辑任务有 **7 个不同名字的重复项**，全部由心跳触发 LLM 建出来的。7 月的 57 次执行全部失败，原因是 Gemini 返回 403（Google 项目未启用 API，需要用户自行处理）。
- `brain.db` 记忆库：Workspace 实例 46 条（42 条是自动保存的原始对话，4 条纠错规则，**用户明确要求"记住"的事一条没有**）；Skynet 实例为空（用户手动清空过，因为记忆一多就会前言不搭后语、开始胡言乱语）。
- 日志 5.6 万条：`Tool loop exhausted`（工具调用循环超限）277 次，`cron_add` 调用 2366 次，`shell` 调用 641 次（近一半失败：bash/PowerShell 混用、白名单拦截），`http_request` 404 高达 269 次（AI 靠猜网址抓新闻）。AI 还自己改过 14 次 `HEARTBEAT.md`、10 次 skill 文件。

**根因判定**：记忆混乱不是"检索机制设计错"，而是**记忆条目没有时间戳**，模型分不清哪条是过期的；提醒失败不是"cron 逻辑简单"，而是**心跳每 30 分钟让弱模型比对一份被截断的任务列表**，越比越乱；shell 频繁出错不是"模型不听话"，而是**把 shell 命令生成这种精确活交给了 LLM**，这活它本来就干不好。

## 3. 设计原则（贯穿所有后续修改）

1. **能用代码实现的确定性行为，一律用代码实现，不让 LLM 参与。** 提醒到点直接发文字、cron 去重清理、新闻抓取流水线、skill 索引——这些都不该由 LLM 现编 shell 命令去做。
2. **LLM 只做两件事：理解/整理文字、和用户聊天。** 工具调用应该是简单的开关式动作（带类型参数），不是让模型拼命令行。
3. **shell 保留，但默认对聊天 AI 不可见。** 只有用户明确对某个具体任务授权时，才能执行 shell 命令；不再是"全局打开、靠审批拦"。
4. **AI 不能改自己的配置和 prompt 文件。** `AGENTS.md`/`SOUL.md`/`TOOLS.md`/`HEARTBEAT.md`/`IDENTITY.md`/`config.toml`/`skills/` 对写文件工具设为只读，代码层拒绝，不是靠 prompt 里写"不要改"。
5. **审批不是安全边界，能力收窄才是。** 把危险能力从模型手里拿掉之后，大部分工具可以免审批；只留 `send_email` 这类真正对外的动作需要确认。
6. **约束弱模型的 prompt，只在对应代码保证做好之后才删。** 不是先删 prompt 再补代码，是反过来。
7. **不要教 AI"应该做什么"，而要让它做不了不该做的事。** 这条是前 6 条的总纲。

## 4. 功能取舍

### 保留

Telegram（必留）、邮件监控（IMAP，收信分类通知，不自动回复）、浏览器（agent-browser）、cf-crawler、TTS（Edge TTS）/STT（Groq Whisper）、新闻推送流水线、Web 仪表盘、MCP（SSE + stdio）、其他渠道（Discord/Slack/Lark/IRC/Nostr）、两个实例（Skynet + Workspace）。

### 删除

- 小智（Xiaozhi）语音设备渠道
- `self_check` / `check_logs` 自检模块
- OpenAI 兼容网关（`/v1/chat/completions`、`/v1/models`，`src/gateway/openai_compat.rs`）
- 其他聊天软件 webhook 兼容层：`/webhook`、`/whatsapp`、`/linq`、`/wati`、`/nextcloud-talk`

### 网关路由取舍

| 路由 | 处理 |
|---|---|
| `/api/chat` | **保留**，用作不经 Telegram 的测试入口（走完整 agent 流程：工具、记忆） |
| `/v1/chat/completions`、`/v1/models` | 删除（OpenAI 兼容） |
| `/webhook`、`/whatsapp`、`/linq`、`/wati`、`/nextcloud-talk` | 删除 |
| `/api/*`（仪表盘）、`/ws/chat`、`/api/events` | 保留（Web 仪表盘要用） |
| `/pair`、`/health`、`/metrics` | 保留 |

### 死代码删除（确认过全代码库无生产调用方）

`src/goals/`（~930 行）、`src/tools/agents_ipc.rs` + `[agents_ipc]` 配置段（~1020 行）、`src/economic/`（~2530 行）、`src/heartbeat/engine.rs` 里未被调用的部分（只留 `ensure_heartbeat_file`）、`memory::apply_time_decay`（从未被调用）、`src/cron/consolidation.rs`（从未被调用）。

仓库根目录垃圾文件：`tmp_check.zip`（7.6MB，含 exe）、`test_script*.sh`/`update_config.sh`/`test_imap.py`（含已清理的密钥）、`heartbeat_problem.md`（空文件）、`PR_DESCRIPTION_UPDATE.md`。

## 5. 模型路由与多 Key 轮换

### 5.1 模型分工

| 用途 | 模型池（按优先级） |
|---|---|
| 日常对话 | `gemini-3.8-flash` → `gemini-3.7-flash` → `gemini-3.6-flash` |
| 兜底（对话池全部耗尽） | `gemini-3.5-flash` |
| 文字整理（新闻总结/聊天摘要/邮件分类，不聊天） | `gemini-3.5-flash` → `gemini-3.5-flash-lite` |
| 记忆检索 embedding | `gemini-embedding-2` |

`gemini-3.5-flash-lite` **不用于对话**（用户实测认为太弱，容易答非所问）。

对话池 + 兜底（3.8/3.7/3.6/3.5-flash）**全部 key 全部耗尽**时，直接告诉用户"今天额度用完了"，不再降级到 `flash-lite` 聊天。

### 5.2 多 Key 轮换策略（模型优先，key 次之）

用户会准备至少 5 个 Google 账号/项目各建一个 key（**注意：每个 key 必须来自不同 Google 项目，同项目内多个 key 共用同一份免费额度**）。轮换顺序：

```
gemini-3.8-flash: key A → key B → key C → key D → key E
  （全部耗尽后）↓
gemini-3.7-flash: key A → key B → key C → key D → key E
  （全部耗尽后）↓
gemini-3.6-flash: key A → key B → ...
  （全部耗尽后）↓
gemini-3.5-flash: key A → key B → ...
  （全部耗尽后）→ 告知用户额度用完
```

**429（配额/频率超限）按 key+模型 处理**：
- 判断为"当日额度耗尽"（错误信息含 quota/exceeded 类字样）→ 这个 key+模型 组合当天不再使用。
- 判断为"每分钟频率超限"→ 短暂冷却（约 1 分钟）后可再试。
- 换成**同一模型的下一个 key**，不是直接换模型。

**503（模型服务端过载）按模型处理，与 key 无关**：换 key 没用，直接跳到**下一个模型**；过一段时间再重试被跳过的模型。

**用量计数持久化到磁盘**，重启不清零，每日额度按 Google 重置时间（太平洋时间午夜，即悉尼时间约 17:00/18:00，具体以实测为准）清零。

### 5.3 Gemini 调用细节（已用真实 key 测试验证）

- **不设 `maxOutputTokens` 上限**，用各模型的最大值（这几个模型都是 65536）。思考深度只用 `thinkingConfig.thinkingLevel` 控制，不要用固定的小输出上限——思考 token 算在输出配额里，上限太小会导致思考没写完就被截断，返回空文本。
- **函数调用的 `id` 字段要原样回传**给下一轮请求（新模型的 functionCall 会带 `id`），虽然实测不带也能工作，但带上更严谨，尤其并行工具调用时能保证结果对应正确。
- `gemini-embedding-2` 与旧的 `gemini-embedding-001` 向量**不兼容**，不能混用；反正记忆库要重新设计，直接从新模型起步。
- Live 系列模型（`gemini-3.8-live` 等）只支持 WebSocket 实时语音，不能用于普通文字对话/工具调用，不要接入对话池。

### 5.4 实现状态（2026-09-23）

**已解决**（`src/providers/mod.rs` / `reliable.rs` / `gemini.rs`）：

- **多 key 轮换以前是完全不起作用的死代码**：`ReliableProvider` 里的 `rotate_key()` 会选出下一个 key，但只打一行警告日志"选中了但没法应用（`Provider` trait 没有 `set_api_key`）"，然后照样用原来的 key 重试——不管配置几个 key，实际永远只用第一个。已确认这是过去 429 频繁触发的直接原因之一。
- **改法**：`reliability.api_keys`（已有的配置字段，加密/脱敏都已经现成接好）不再传进 `ReliableProvider` 内部去"轮换"，而是在 `create_resilient_provider_with_options` 构造阶段，给每个额外 key 各建一个独立的 provider 实例，追加到 provider 链上（和 `fallback_providers` 用的是同一套机制，只是同一个 provider/模型换不同 key）。已有的"模型外层循环 → provider（现在也是 key）中层循环 → 重试内层循环"三层结构不用改，天然就产生"模型先轮完，再换下一个模型"的顺序——这正是 §5.2 要的效果，不需要额外写调度逻辑。
- **Gemini 每日额度 429 的判定**：新增 `is_gemini_daily_quota_exhausted`，专门识别 Gemini 报错里的 `GenerateRequestsPerDay...-FreeTier` 字样，和"每分钟"限额（`...PerMinute...`）区分开——只有"日额度耗尽"才会立刻跳过这个 key（不再在同一个耗尽的 key 上重试烧掉退避时间），每分钟限额仍走原来"退避重试，重试完自然换下一个 key"的路径。
- **`maxOutputTokens` 从写死的 `8192` 改成 `65536`**（所有配置的模型都支持）：思考 token 和回复 token 共用同一个输出配额，8192 在思考深度稍高时就会被思考本身耗尽，导致返回空文本，而空文本又被当成"临时故障"去重试——白白浪费额度。
- 测试：`src/providers/mod.rs` 新增 4 个测试直接验证 key 展开逻辑（数量、命名、跳过空字符串、无主 key 时仍能加额外 key）；`src/providers/reliable.rs` 新增 3 个测试验证 Gemini 每日/每分钟额度判定的区分度；删除了两个只测死代码行为的旧测试。

**还没做**（按影响排序，后续步骤补）：

1. **503（模型过载）目前还是会把一个模型的所有 key 都试一遍才换模型**，没有做"503 直接跳过剩余 key、换模型"的优化。不是正确性问题（迟早会换到下一个模型），只是慢——每个耗尽的 key 上还要等一次退避。
2. **"整条池子都耗尽"目前只会抛一个聚合错误**，还没有在 `channels/mod.rs` 接一层"识别到全部是配额/频率错误 → 回复用户'今天额度用完了'"的转换。现在用户看到的还是原始错误堆栈。
3. **额度耗尽状态没有持久化**：现在完全靠"live 请求时判定 429/503，当场跳过"，不需要单独的计数器就能正确工作（耗尽的 key 每次被试到都会很快拿到同样的 429 并跳过，不会重试），但每轮对话仍然会为每个已耗尽的 key/模型多花一次请求去确认"确实还没恢复"。加一个磁盘持久化的"跳过到什么时候"状态是优化项，不是必须项。
4. `资料/config.toml` 已经按上面的方案更新（`default_model`/`summary_model`/`worker_model` 换成新模型池，`[reliability.model_fallbacks]` 填好两条链），但 `api_keys = []` 还是空的——等用户建好额外的 Google 项目和 key 再填进去。

## 6. Cron / 提醒重新设计

1. **心跳不再让 LLM 同步任务。** 改为代码从 `HEARTBEAT.md` 解析出任务定义（固定 key 标识），直接与数据库对账，该增的增、该删的删，不经过一次 LLM 调用。✅ 已实现，见 `src/cron/heartbeat_decl.rs`。
2. **任务名加唯一约束**，同名任务的创建请求改为"更新"而不是返回 `already_exists` 空操作。✅ 已实现（`cron_add.rs` 删掉了挡住 store 层正确逻辑的那段工具层检查）。
3. **一次性任务（`at`）无论成功失败都清理**，不再"失败后停用但留在库里"。✅ 已实现（`is_one_shot` 判定不再看 `delete_after_run`）。
4. **新增"直接发文字"的提醒类型**：到期由代码直接推送，不经过 LLM，不会因为 429/503 而失败，也不会被误判为"已完成"从而消失。✅ 已实现：`JobType::Message`，`cron_add(job_type="message", message="...", delivery=...)`，`delivery` 必填（没地方投递的提醒没意义）。
5. `cron_list` 只输出精简字段，不把 `last_output`（最长 16KB）整段塞进去。✅ 已实现：`prompt`/`last_output` 截到 200 字符预览+总长度。
6. 时区默认悉尼，不再是裸 UTC。✅ 已实现：新增 `[cron].default_tz`（默认 `"Australia/Sydney"`），在 `add_shell_job`/`add_agent_job`/`update_job` 三处统一应用。
7. Agent 类型的定时任务失败重试时，不能把已经执行过的工具（发消息、写文件、建任务）重跑一遍。**未实现**——需要 agent loop 暴露"跑到哪一步了"的状态才能根治，属于更大的改动。本轮的 Gemini provider 修复（key 轮换 + 429/503 正确分类，见 §5.4）已经大幅减少了触发这个问题的中途失败次数，作为缓解措施先够用；根治留到后续。
8. **附带修复**：`cron_run`（手动立即执行）以前只记录运行结果，不投递、不清理一次性任务——手动跑一个提醒之后它还会在原定时间再触发一次。已改为复用 `persist_job_result`，和 scheduler 自动触发走同一条收尾逻辑。

## 7. 记忆重新设计

全部 7 条已实现（2026-09-23），详见 `dev_log.md` 对应条目。

1. **记事改成结构化记录**：内容 + 创建时间 + 到期时间（可空）+ 状态（未完成/已完成），不是自由格式的长 Markdown。✅ `src/memory/notes.rs`，独立 SQLite 文件 `notes.db`，不混进 embedding 的 `brain.db`。
2. **每轮对话只注入"未完成"的记事**，每条都带日期，注入条数有上限——不再是"整份文件塞进 prompt"或"语义检索 top5"这两种极端。✅ `open_notes_for_prompt()`，上限 30 条。
3. **提醒就是带到期时间的记事**，到点由代码直接发送（见第 6 节第 4 条），不依赖 LLM 判断。✅ `note_add(due_at=...)` + `JobType::Message`（Step 2）。
4. **关掉"每句聊天原文都自动存成记忆"**——这是记忆库被灌满对话噪音、把真正的记事挤出去的主因。聊天记录本来就有独立的日志系统，不需要再进记忆库。✅ `[memory].auto_save` 默认改为 `false`。
5. embedding 调用失败时**照样把原文存下来**（不算向量），不能因为一次 429 就丢整条写入。✅ `sqlite.rs::store()` 降级为 `embedding=NULL`，不再 `?` 直接丢弃整次写入。
6. 中文全文检索启用 trigram 分词（SQLite FTS5 默认的 unicode61 分词器对中文基本不起作用）。✅ `tokenize='trigram case_sensitive 0'` + 存量数据库自动迁移重建索引。
7. 系统提示词的"当前时间"**只保留一处实时注入**，消除"启动时烘焙一份、每条消息又追加一份"导致的两个时间源打架。✅ 只留 `build_channel_system_prompt`（每条消息都刷新）那一处。

**范围外**：`memory_store`/`memory_forget`（embedding 记忆的写入口）仍是 Restricted 级、需要审批——这两个不是"记事"用的工具，记事已经有了免审批的 `note_add`，embedding 记忆作为辅助系统保持原有审批级别不动。

## 8. Shell / 工具权限重新设计

1. **聊天 AI 默认看不到 shell 工具。** 浏览文件用 `file_read`/`glob_search`，不用 `ls`/`dir`/`cat`。
2. **cf-crawler、新闻抓取、skill 索引全部做成原生 typed 工具**，代码直接传参启动对应 exe，不经过 shell/bash/PowerShell 现拼命令行，彻底消除 bash 转义 vs PowerShell 语法不一致的问题。
3. **shell 只在用户明确对某个具体任务授权时才能执行**，授权范围限定在那次任务，不是全局打开一个"shell 权限开关"。
4. 移除 `self_check`/`check_logs`（已在第 4 节列为删除项，这里重复强调原因：这类"自检"是 AI 自己诊断自己出的错，容易越检查越乱）。

## 9. 验证方式：不需要每次都烧 Gemini 额度

三层测试策略：

1. **不调 LLM 的链路测试（主力，可反复跑）**：用假模型（`DummyProvider` 之类的测试替身）预置好要返回的工具调用，把"收消息 → 调工具 → 写库 → 发消息"整条链路跑一遍，直接断言输出和数据库状态。
2. **本机真模型测试（关键节点用）**：启动测试配置后，通过 `/api/chat` 接口发消息（走的是和 Telegram 一样的完整流程）直接读 JSON 返回；或用 `zeroclaw agent -m "..."` 单条消息模式。
3. **K6 实机核实（部署后）**：SSH 读取 `elfclaw-logs.jsonl`、`jobs.db`、`brain.db`，直接核实生产行为，不用等用户截图转述。

## 10. 分步计划

每一步：一个问题一个 commit，配一个复现测试（先证明旧代码测试失败，改完再证明通过）；`cargo fmt`/`cargo clippy -D warnings`/`cargo test` 全过；commit message 用中文；直接推 `main`（按 `CLAUDE.md` §0 单分支规则）。

- **Step 0（已完成，2026-09-23）**：
  - 未提交改动（skills index、MaxTokens 续传修复、skill 审计、telegram 相册/流式草稿、cron 审批）整理为 baseline commit，提交前清除其中一处泄露的 CF Worker secret。
  - 用 `git-filter-repo` 清理 git 全部历史中的 6 处真实密钥泄露（代理 key、Gmail 应用密码、crawler token、Telegram bot token、CF Worker secret、pairing token），强推 `origin/main`。
  - 删除仍携带旧历史的 3 个残留 dependabot 分支（会自动重新生成）；GitHub 密钥扫描告警标记为已解决。
  - `CLAUDE.md` §0 补充中文输出规则，明确覆盖 commit message。
- **Step 1**：删除第 4 节列出的死代码和垃圾文件（纯删除，无行为变化，`cargo test` 前后一致）。
- **Step 2**：Cron/提醒重写（第 6 节），含 K6 现有 22 个重复任务的清理脚本（先备份）。
- **Step 3**：记忆重写（第 7 节）。
- **Step 4**：模型路由 + 多 key 轮换 provider（第 5 节）+ Telegram 离线消息不丢弃 + 429/503 分类处理。
- **Step 5**：Shell/工具权限收紧（第 8 节）+ 网关精简（第 4 节路由表）。
- **Step 6**：清理约束弱模型的旧 prompt——只删已经被对应代码保证覆盖的那部分，不是一次性全删。

## 11. 已发现、暂缓到对应 Step 修复的安全问题

- skill 审计的高危模式检测用 `find_map`，只报告第一个命中的模式，白名单声明一个模式就可能连带放过同文件里的其他危险模式（如 `rm -rf`）。→ Step 5 一并修。
- `cron_add`/`cron_update` 当前被设为免审批，但内部 `validate_command_execution` 信任的是**模型自己传的 `approved` 参数**，等于没有人工审批。→ Step 8 节原则实施后，shell 命令生成本身就不该由模型现编，此问题随 Step 5 自然消除。
- `sqlite_query` 的受保护数据库路径检查是字符串后缀匹配，Windows 8.3 短文件名或路径变体可能绕过。→ Step 5。
- Telegram 相册在跨 `getUpdates` 长轮询批次时可能被拆成两条消息。→ Step 4（跟 Telegram 一起改）。

## 12. 密钥与账号管理

- **CF token**（`cfat_...`，账户 `baf365a52956bb35cf34ff922f4e8298`，有效期到 2026-11-25）：仅用于只读验证 `cf-crawler-worker` 状态和测试 `C:\Dev\cf-crawler` 爬虫程序，**不修改任何 CF 设置**。
- **GitHub token**（`ghp_...`）：仅用于操作 `VK7KSM/eflClaw` 和 `VK7KSM/cf-crawler` 两个仓库，不用于其他仓库。
- **Gemini key 池**：目前已验证 `daishuvpn@gmail.com` 一个 key（可正常调用 3.8/3.7/3.6/3.5-flash/3.5-flash-lite/embedding-2，首次请求偶发 503 属正常现象）；另有 `khunkasim@gmail.com` 一个 key 尚未测试。用户会补齐到至少 5 个。**多账号轮换放大免费额度违反 Google APIs 服务条款 §2(d)，用户已知晓此风险并选择承担，不再讨论。**
- **已清理的历史泄露密钥**（用户确认均已失效/过期，不需要轮换，只是清理历史避免 GitHub 骚扰）：LLM 代理 key（`sk-2c87...`）、Gmail 应用密码、`CF_CRAWLER_TOKEN`、Telegram bot token、CF Worker secret、gateway pairing token。

## 13. 代码语言约束

**全部用 Rust 实现，不引入 Python 等其他语言的运行时依赖。** 之前分析阶段用 Python 脚本做过一次性的数据分析（读 K6 拷回来的 SQLite/日志、测 Gemini key），那些是本地一次性工具，不进入 elfClaw 代码库；正式功能代码一律 Rust。
