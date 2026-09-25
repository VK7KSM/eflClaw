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
3. ~~**shell 保留，但默认对聊天 AI 不可见。** 只有用户明确对某个具体任务授权时，才能执行 shell 命令；不再是"全局打开、靠审批拦"。~~
   **2026-09-23 被更彻底的方案取代（Step 7）**：确认爬虫/邮件等功能都已不依赖 shell 后，用户指示"彻底移除shell"——
   `shell`/`process`/`schedule` 三个工具和 `SKILL.toml` `kind="shell"` 桥接层已从代码中整体删除，不存在"授权"问题了。
4. **AI 不能改自己的配置和 prompt 文件。** `AGENTS.md`/`SOUL.md`/`TOOLS.md`/`HEARTBEAT.md`/`IDENTITY.md`/`config.toml`/`skills/` 对写文件工具设为只读，代码层拒绝，不是靠 prompt 里写"不要改"。
   **2026-09-23 复查发现**：这条原则从未被拆成具体 Step，`file_write.rs`/`file_edit.rs` 代码里目前完全没有这层保护
   （已有的 `is_sensitive_file_path` 只管 `.env`/SSH key 等凭据文件，跟这条原则是两回事）。深入设计时发现一个真实的
   架构冲突：`HEARTBEAT.md` 在 Step 2 的心跳重设计里是心跳任务定义的**唯一权威来源**
   （`heartbeat_decl::reconcile()` 只认文件里声明的任务，没声明的会被当成"已删除"清理掉）——如果代码层完全禁止
   AI 写这个文件，用户就没法再通过聊天让 AI 帮忙管理心跳任务，等于让 Step 2 刚做完的能力失效。K6 数据里"AI 自己
   改过 14 次 HEARTBEAT.md"这个问题，很可能主要是**旧架构**（心跳每 30 分钟让 LLM 自己比对任务列表、自己决定要不
   要改文件）导致的，Step 2 已经把这个自动循环改成代码 reconcile、不再让 LLM 参与，根因可能已经消除了大半；
   `skills/` 这块也有类似的开放问题——目前没有独立于 `file_write`/`file_edit` 的、走审计流程的 agent 可调用技能
   安装工具，全面禁止 AI 写 `skills/` 目录会不会连带堵死"用户让 AI 帮忙写一个新技能"这条本来就存在的用法，也没有
   确认清楚。**问过用户后决定：暂不实现，留待后续**——这条原则的实现范围本身还需要更明确的设计（至少要先想清楚
   HEARTBEAT.md 和 `skills/` 这两块的写入边界该怎么划），不在本轮仓促拍板。
   **2026-09-24 用户拍板并实现骨架（Step 8）**：用户的方案是"HEARTBEAT.md 这类核心文件 AI 不能改，另给 AI 一个
   可以改的非核心辅助文件，辅助文件能改什么由 HEARTBEAT.md 规定死"。辅助文件定名 `HEARTBEAT_DATA.md`；核心文件
   "全部锁上"。已实现：新增 `src/security/protected_identity_files.rs`（名单：`IDENTITY.md`/`AGENTS.md`/
   `HEARTBEAT.md`/`SOUL.md`/`USER.md`/`TOOLS.md`/`BOOTSTRAP.md`/`config.toml`，按文件名匹配、大小写不敏感、
   不论目录层级），接入所有能写文件的工具：`file_write`、`file_edit`（原始路径和解析后路径两层）、`apply_patch`
   （扫描 diff 的目标文件）、`browser` 截图/PDF 输出、`screenshot`。`HEARTBEAT_DATA.md` 不在名单内，天然可写。
   **尚未做**（用户选择"先搭骨架"）：① 真实 `资料/HEARTBEAT.md` 迁移到 `<!-- heartbeat-task -->` 声明式格式、把
   RSS 源清单/死源记录挪进 `HEARTBEAT_DATA.md`；② HEARTBEAT.md 里声明"辅助文件契约"+ tick 时代码校验；
   ③ `skills/` 目录是否也要锁（与"让 AI 帮忙写技能"用法冲突，仍待用户决定）；④ 五个 `*_config` 工具
   （`model_routing_config`/`proxy_config`/`web_access_config`/`web_search_config`/`channel_ack_config`）是结构化
   地改 `config.toml` 的，不受文件名单约束——是否保留给聊天 AI 用待用户决定（见第 11 节）。
   **2026-09-24 用户纠正并定下边界（Step 8 补充）**：原则是"**只拦会导致运行不稳定的，不要把正常运行也拦掉
   再去解决拦出来的报错**"。
   - `SOUL.md`/`USER.md`/`IDENTITY.md`/`TOOLS.md` 本来就是设计给 AI 改的（性格、偏好、身份、本地笔记这类
     "高级配置"），改坏了也不会让 elfClaw 报错——**已放开**，偏好照旧写回这几个文件。
   - 仍锁：`AGENTS.md`、`HEARTBEAT.md`、`BOOTSTRAP.md`、`config.toml`（改坏会影响调度、系统提示词或启动）。
   - `skills/`、`workers/*.md` **不锁**（③ 已定）。
   - **agent 不许动底层运行配置，包括不许换模型**（④ 已定）：删除全部 8 个会改写 `config.toml` 的工具——
     上述 5 个 `*_config`，外加 `switch_provider`（切换默认模型/提供商）、`manage_auth_profile`（切换账号
     配置）、`openclaw_migration`（合并外部配置）。只读的 `check_provider_quota`/`estimate_quota_cost` 保留。
     子 agent 工具（`delegate`/`subagent_spawn`）只接受 agent 名字，模型由配置决定，agent 选不了，无需改动。
   **2026-09-24 完成 HEARTBEAT.md 迁移（Step 9）**：真实 `资料/HEARTBEAT.md` 改成 7 个 `heartbeat-task` 声明块
   （6 个新闻时段 + 新闻源搜索），新闻源全部挪进 `资料/HEARTBEAT_DATA.md`；HEARTBEAT.md 里写死了
   "HEARTBEAT_DATA.md 约定"（四节结构、各节谁改、能改什么）。约定**不做代码校验**：没有任何代码解析
   HEARTBEAT_DATA.md，写错了最多影响某个时段抓哪些源，不会让程序报错——按"只拦会导致不稳定的"原则不需要。
   **2026-09-24 Step 10 取代上述做法**：数据文件改为只由代码写入的 `HEARTBEAT_DATA.toml`，agent 通过 `news_schedule`/
   `news_report` 工具修改；HEARTBEAT.md 用 `news-rules` 规则块写死 agent 能改的边界，由代码逐条校验。原因：让模型整文件
   重写数据文件容易改坏，且 worker 回写封禁记录会覆盖主 agent 刚做的修改；另外用户要求主 agent 能自己增删新闻时段。
5. **审批不是安全边界，能力收窄才是。** 把危险能力从模型手里拿掉之后，大部分工具可以免审批；只留 `send_email` 这类真正对外的动作需要确认。
6. **约束弱模型的 prompt，只在对应代码保证做好之后才删。** 不是先删 prompt 再补代码，是反过来。
7. **不要教 AI"应该做什么"，而要让它做不了不该做的事。** 这条是前 6 条的总纲。

## 4. 功能取舍

### 保留

Telegram（必留）、邮件监控（IMAP，收信分类通知，不自动回复）、浏览器（agent-browser）、cf-crawler、TTS（Edge TTS）/STT（Groq Whisper）、新闻推送流水线、Web 仪表盘、MCP（SSE + stdio）、其他渠道（Discord/Slack/Lark/IRC/Nostr）、两个实例（Skynet + Workspace）。

### 删除

- 小智（Xiaozhi）语音设备渠道
- `self_check` / `check_logs` 自检模块（**已删除，2026-09-23，Step 5**）
- OpenAI 兼容网关（`/v1/chat/completions`、`/v1/models`，`src/gateway/openai_compat.rs`，**已删除，2026-09-23，Step 5**）
- 其他聊天软件 webhook 兼容层：`/webhook`、`/whatsapp`、`/linq`、`/wati`、`/nextcloud-talk`（暂缓，见第 10 节 Step 5）

### 网关路由取舍

| 路由 | 处理 |
|---|---|
| `/api/chat` | **保留**，用作不经 Telegram 的测试入口（走完整 agent 流程：工具、记忆） |
| `/v1/chat/completions`、`/v1/models` | **已删除（2026-09-23，Step 5）**：`src/gateway/openai_compat.rs` 整个文件删除（唯二用途——`/v1/models` 路由和被 `/v1/chat/completions` 覆盖前的旧 handler——都已确认无其他调用方）；`openclaw_compat.rs` 里的 `handle_v1_chat_completions_with_tools`（含专属 `Oai*` 请求/响应结构体）一并删除，只留 `/api/chat` |
| `/webhook`、`/whatsapp`、`/linq`、`/wati`、`/nextcloud-talk` | **暂缓**（见第 10 节 Step 5 说明：会级联到独立的 channel 实现文件，改动面明显更大，本次只处理了自包含的 OpenAI 兼容层） |
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

1. ~~503（模型过载）目前还是会把一个模型的所有 key 都试一遍才换模型~~ **已修复，2026-09-24**：实际情况比这里写的更糟——同一个 key+模型要重试 3 次（每次至少等 5 秒），然后才轮到下一个。K6 实测：4 个模型同时 503 时，要 71 秒才报错；3.8 和 3.7 在 503、3.6 正常时，要 26 秒才出结果。报错文字又被截断到 200 字符，只剩第一次尝试，看起来像"只试了一个模型"。
   改成按轮尝试（`ReliableProvider::call_with_failover`，四个 chat 方法共用，替换掉原来复制了四份的三层循环）：
   - 尝试顺序是"模型 → key"，每一轮每个条目试一次，失败就直接试下一个；某个模型报 503 时，这一轮里跳过它剩下的 key；整轮都失败才退避，进入下一轮（最多 `provider_retries + 1` 轮，总尝试次数不变）；非临时错误的条目在后面几轮直接跳过。
   - 能不能重试优先看真实 HTTP 状态码（`http_status`，只认 `API error (<code>` 这个前缀）；只有拿不到状态码时，才用原来的"报错文字里找数字/关键词"做兜底判断。
   - 每次失败都写一条 `LLM attempt failed: provider/model pass n/N status=… reason=…` 日志。
   - 整条链都失败时返回 `AllProvidersFailedError`，带按模型汇总的次数（如 `gemini-3.8-flash 503×3；gemini-3.6-flash 503×3`），渠道层据此给用户发中文说明。
2. ~~"整条池子都耗尽"目前只会抛一个聚合错误~~ **已修复，2026-09-23**：`src/providers/traits.rs` 新增结构化错误
   `AllProvidersRateLimitedError`（`thiserror` 派生，和已有的 `ProviderCapabilityError` 同一模式）。
   `src/providers/reliable.rs` 新增共享的 `finalize_all_failed(failures, all_rate_limited)`：`chat_with_system`/
   `chat_with_history`/`chat_with_tools`/`chat` 四个方法都在失败循环里额外用一个 `all_rate_limited` 布尔做 AND
   累积（每次失败调用已有的 `is_rate_limited()` 分类结果），链路耗尽时统一走这个函数——**全部**尝试都是 429
   （日额度耗尽或每分钟限流，不管是哪种）才返回结构化错误，混了任何一个非限流的真实失败（bug/鉴权/网络问题）都
   仍然走原来的原始聚合错误，不会被误判成"只是配额问题"而掩盖真正的故障。`channels/mod.rs` 新增
   `user_facing_llm_error_message()`，对这个结构化错误显示"今天的额度用完了，明天再试"，其他情况显示已脱敏的
   `safe_error`（顺带修了一个相邻 bug：原来这里已经算出 `safe_error` 却没用上，直接把未脱敏的原始 `{e}` 显示给
   用户）。新增测试：`reliable.rs` 2 个（全部限流 → 结构化错误；限流+真实错误混合 → 不构造结构化错误）、
   `channels/mod.rs` 2 个（消息选择逻辑的直接单测，不依赖完整 e2e 管道）。
3. **额度耗尽状态没有持久化**：现在完全靠"live 请求时判定 429/503，当场跳过"，不需要单独的计数器就能正确工作（耗尽的 key 每次被试到都会很快拿到同样的 429 并跳过，不会重试），但每轮对话仍然会为每个已耗尽的 key/模型多花一次请求去确认"确实还没恢复"。加一个磁盘持久化的"跳过到什么时候"状态是优化项，不是必须项。
4. `资料/config.toml` 已经按上面的方案更新（`default_model`/`summary_model`/`worker_model` 换成新模型池，`[reliability.model_fallbacks]` 填好两条链），但 `api_keys = []` 还是空的——等用户建好额外的 Google 项目和 key 再填进去。

## 6. Cron / 提醒重新设计

1. **心跳不再让 LLM 同步任务。** 改为代码从 `HEARTBEAT.md` 解析出任务定义（固定 key 标识），直接与数据库对账，该增的增、该删的删，不经过一次 LLM 调用。✅ 已实现，见 `src/cron/heartbeat_decl.rs`。
2. **任务名加唯一约束**，同名任务的创建请求改为"更新"而不是返回 `already_exists` 空操作。✅ 已实现（`cron_add.rs` 删掉了挡住 store 层正确逻辑的那段工具层检查）。
3. **一次性任务（`at`）无论成功失败都清理**，不再"失败后停用但留在库里"。✅ 已实现（`is_one_shot` 判定不再看 `delete_after_run`）。
4. **新增"直接发文字"的提醒类型**：到期由代码直接推送，不经过 LLM，不会因为 429/503 而失败，也不会被误判为"已完成"从而消失。✅ 已实现：`JobType::Message`，`cron_add(job_type="message", message="...", delivery=...)`，`delivery` 必填（没地方投递的提醒没意义）。
5. `cron_list` 只输出精简字段，不把 `last_output`（最长 16KB）整段塞进去。✅ 已实现：`prompt`/`last_output` 截到 200 字符预览+总长度。
6. 时区默认悉尼，不再是裸 UTC。✅ 已实现：新增 `[cron].default_tz`（默认 `"Australia/Sydney"`），在 `add_shell_job`/`add_agent_job`/`update_job` 三处统一应用（`add_shell_job` 已随 Step 7 删除）。
7. Agent 类型的定时任务失败重试时，不能把已经执行过的工具（发消息、写文件、建任务）重跑一遍。**未实现**——需要 agent loop 暴露"跑到哪一步了"的状态才能根治，属于更大的改动。本轮的 Gemini provider 修复（key 轮换 + 429/503 正确分类，见 §5.4）已经大幅减少了触发这个问题的中途失败次数，作为缓解措施先够用；根治留到后续。
8. **附带修复**：`cron_run`（手动立即执行）以前只记录运行结果，不投递、不清理一次性任务——手动跑一个提醒之后它还会在原定时间再触发一次。已改为复用 `persist_job_result`，和 scheduler 自动触发走同一条收尾逻辑。

## 7. 记忆重新设计

全部 7 条已实现（2026-09-23），详见 `dev_log.md` 对应条目。

1. **记事改成结构化记录**：内容 + 创建时间 + 到期时间（可空）+ 状态（未完成/已完成），不是自由格式的长 Markdown。✅ `src/memory/notes.rs`，独立 SQLite 文件 `notes.db`，不混进 embedding 的 `brain.db`。
2. **每轮对话只注入"未完成"的记事**，每条都带日期，注入条数有上限——不再是"整份文件塞进 prompt"或"语义检索 top5"这两种极端。✅ `open_notes_for_prompt()`，上限 30 条。
3. **提醒就是带到期时间的记事**，到点由代码直接发送（见第 6 节第 4 条），不依赖 LLM 判断。✅ `note_add(due_at=...)` + `JobType::Message`（Step 2）。
   **更正（2026-09-24）**：Step 3 实际只做了存储，`note_add` 从未建出提醒任务，到期不会发送任何东西。Step 10 补上：带 `due_at` 时自动建一次性 Message 任务 `note:<id>`，`note_done` 时自动取消，建不成就撤销记事并报错。
4. **关掉"每句聊天原文都自动存成记忆"**——这是记忆库被灌满对话噪音、把真正的记事挤出去的主因。聊天记录本来就有独立的日志系统，不需要再进记忆库。✅ `[memory].auto_save` 默认改为 `false`。
5. embedding 调用失败时**照样把原文存下来**（不算向量），不能因为一次 429 就丢整条写入。✅ `sqlite.rs::store()` 降级为 `embedding=NULL`，不再 `?` 直接丢弃整次写入。
6. 中文全文检索启用 trigram 分词（SQLite FTS5 默认的 unicode61 分词器对中文基本不起作用）。✅ `tokenize='trigram case_sensitive 0'` + 存量数据库自动迁移重建索引。
7. 系统提示词的"当前时间"**只保留一处实时注入**，消除"启动时烘焙一份、每条消息又追加一份"导致的两个时间源打架。✅ 只留 `build_channel_system_prompt`（每条消息都刷新）那一处。
   **更新（2026-09-24）**：当前时间连同其他每条消息都会变的内容，已经整体移出系统提示词，改为附在当前这条用户消息前面（见第 14 节）；唯一注入点现在是 `current_time_section()`。

**范围外**：`memory_store`/`memory_forget`（embedding 记忆的写入口）仍是 Restricted 级、需要审批——这两个不是"记事"用的工具，记事已经有了免审批的 `note_add`，embedding 记忆作为辅助系统保持原有审批级别不动。

## 8. Shell / 工具权限重新设计

1. **聊天 AI 默认看不到 shell 工具。** 浏览文件用 `file_read`/`glob_search`，不用 `ls`/`dir`/`cat`。
   **已完成，2026-09-23**：发现 `资料/config.toml` 的 `autonomy.non_cli_excluded_tools` 被显式写成空数组 `[]`，
   而代码里 `default_non_cli_excluded_tools()`（`src/config/schema.rs`）本来就会把 `shell` 排除在非 CLI 渠道
   （Telegram 等）之外——这个空数组把代码的安全默认值覆盖掉了，导致聊天 AI 一直能看到并调用 `shell` 工具，
   这正是"shell 出错→AI 自作主张改 prompt→越改越坏"这条投诉链路的**根本起点**。修复：把
   `non_cli_excluded_tools` 改成 `["shell"]`（只排除 shell，不动 browser/http_request/cron_*/memory_store
   等聊天要用的常规工具）。CLI 渠道不受影响（`effective_excluded_tools` 对 `msg.channel == "cli"` 恒为空）。
   ⚠️ `资料/config.toml` 是 `.gitignore` 忽略的本地部署参考镜像，这处改动**不会随 git push 同步到 K6**，
   需要手动同步到 K6 两个实例的真实 `config.toml` 并重启才会真正生效（详见 dev_log.md 对应条目）。
   **后续（Step 7）**：shell 整体删除后，`non_cli_excluded_tools` 又改回 `[]`（没有 shell 可隐藏了）。
2. **cf-crawler、新闻抓取、skill 索引全部做成原生 typed 工具**，代码直接传参启动对应 exe，不经过 shell/bash/PowerShell 现拼命令行，彻底消除 bash 转义 vs PowerShell 语法不一致的问题。
   **已完成（cf-crawler 部分），2026-09-23**：新增 `src/tools/cf_crawler.rs`，实现 `WebHealthTool`/
   `WebScrapeTool`/`WebCrawlTool`/`WebLoginTool` 四个原生 Rust 工具，用 `tokio::process::Command` 直接
   调用 `workspace/tools/cf-crawler-win-x64.exe`（带 `--json <payload>` 参数），完全不经过 shell/sh/
   PowerShell。这条修复直接命中 dev_log.md 里记录的一长串历史 bug：旧的 `资料/skills/cf-crawler/
   SKILL.toml`（`kind="shell"`，经 `src/skills/tool_handler.rs` 拼 `sh`/`powershell` 命令行）反复因为
   bash 把 `\t`/`\c` 当转义符吞掉、双重 workspace 路径拼接等问题失败，多个会话花了大量时间修补。
   用本机真实的 `cf-crawler-win-x64.exe`（`C:\Dev\cf-crawler\release\`）跑了两个手动验证测试
   （`cf_crawler::tests::manual_*`，默认 `#[ignore]`，不进 CI）：(a) 无凭据时验证了 stdout 解析能正确跳过
   cf-crawler 自己的 pino 错误日志行、取到最后一行真正的结果 JSON；(b) 传入含引号/反斜杠/tab/`&` 的
   url/goal 参数，验证 argv 直传不需要任何转义（`tokio::process::Command` 走 Windows CreateProcess，
   全程没有 shell 解释这一步，天然没有转义问题）。同步删除了 `SKILL.toml` 里 web_scrape/web_crawl/
   web_login/web_health 四个旧的 shell 版本工具定义（保留 `agent_reach_ensure`/`web_help`，它们没有
   复杂 JSON 参数，风险低，暂不迁移——**Step 7 删除 shell 桥接层后这两个也一并移除**，SKILL.toml 现为 v0.5.0，
   只剩提示文字），移除 `[agents.news_fetcher].allowed_tools` 里的 `"shell"`
   （之前作为 web_scrape 不稳定时的兜底，现在根因已消除）。**"新闻抓取"里 shell 依赖已随 cf-crawler
   迁移一并解决**（news_fetcher 唯一用 shell 的地方就是调 cf-crawler）；**"skill 索引"**——审计后发现
   `src/skills/index.rs`/`audit.rs` 本身不调用 shell，这条指的就是 SKILL.toml 的 `kind="shell"` 模板
   机制本身，cf-crawler 是当前唯一使用该机制处理复杂 JSON 参数的技能，已随上述改动解决。
3. **shell 只在用户明确对某个具体任务授权时才能执行**，授权范围限定在那次任务，不是全局打开一个"shell 权限开关"。
   **问题已被更彻底的方案取代，2026-09-23（Step 7）**：本条原本设想的是"按任务临时授权"这种细粒度开关机制，
   但用户在被问到"agent 跑爬虫/发邮件到底怎么调用外部程序、是不是必须用 shell"之后，回答是二者都不用
   shell（爬虫走 `src/tools/cf_crawler.rs` 的 `tokio::process::Command` 直调 exe，邮件走 `lettre` 原生 SMTP 库），
   于是直接指示"那就彻底移除shell"——不再需要任何授权机制，因为压根没有可供授权的 shell 工具了。
   `shell`/`process`/`schedule` 三个工具（`process`/`schedule` 与 `shell` 同属"LLM 自由拼命令字符串"这一类风险，
   一并删除）连同 `src/skills/tool_handler.rs`（`SKILL.toml` `kind="shell"` → 可调用工具的桥接层，唯一用途就是
   执行 shell 定义的技能工具）整体从代码库删除，`SecurityPolicy`/`AutonomyConfig` 里所有 shell 命令白名单/风险分级
   相关字段和方法（`allowed_commands`、`command_risk_level`、`validate_command_execution` 等）一并删除。详细改动
   清单见 dev_log.md 对应条目。
4. 移除 `self_check`/`check_logs`（已在第 4 节列为删除项，这里重复强调原因：这类"自检"是 AI 自己诊断自己出的错，容易越检查越乱）。**已完成，2026-09-23（Step 5 第二部分）。**

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
- **Step 1（已完成，2026-09-23）**：删除第 4 节列出的死代码和垃圾文件（纯删除，无行为变化，`cargo test` 前后一致）。
- **Step 2（已完成，2026-09-23）**：Cron/提醒重写（第 6 节，7 条全部完成）。
- **Step 3（已完成，2026-09-23）**：记忆重写（第 7 节，7 条全部完成）。
- **Step 4（已完成，2026-09-23）**：
  - 多 key 轮换 provider（第 5 节，实际先于 Step 2 完成——用户明确要求优先）：修好了完全不生效的 key 轮换死代码、Gemini 每日额度 429 正确分类、输出上限 8192→65536。
  - Telegram 离线消息不再丢弃：启动探测（"startup probe"）以前会把探测响应里的消息直接吞掉（只取 update_id 推进 offset，内容从不处理）——`getUpdates` 不会因为返回过一次就消费掉更新，所以只要探测不动 offset，紧接着的正式轮询会重新收到同一批消息并正常处理。改动是纯删除（删掉推进 offset 那段），配了一个 wiremock 集成测试，在旧代码上跑确认会超时失败，新代码上通过。
  - 429/503 分类处理：已随多 key 轮换一起完成（见 §5.4）。
  - **未处理**：Telegram 相册跨 `getUpdates` 轮询批次被拆分的问题（见第 11 节，改动面更大且非用户反馈的实际痛点，往后放）。
- **Step 5（进行中，2026-09-23）**：
  - **已完成（第一部分）**：网关精简——删除 OpenAI 兼容层（`/v1/chat/completions`、`/v1/models`）。`src/gateway/openai_compat.rs` 整个文件（720 行）确认无其他调用方后整体删除；`openclaw_compat.rs` 里专为该兼容层写的 `handle_v1_chat_completions_with_tools` handler、8 个 `Oai*` 请求/响应结构体、对应的 7 个单测一并删除，只保留 `/api/chat`（唯一需要保留的、被 `run_gateway_chat_with_tools` 走完整 agent 循环的入口）。纯删除，`cargo test --lib` 前后同为 11 个预置失败（Windows 符号链接权限相关，与本次改动无关），无新增失败；`cargo clippy` 在两个改动文件里零新增问题。
  - **已完成（第二部分）**：`self_check`/`check_logs` 自检模块删除。调查确认这是一套完整的 `/selfcheck` 用户命令功能（非死代码）：`SelfCheckGate`（开关状态机，仅 `/selfcheck` 命令能打开）→ `self_check(action="analyze")` 收集日志/源码 → 用 worker model 跑一次隔离的 `agent::loop_::run()` 分析 → 报告存到 `homework/`；`check_logs` 是配套的日志查询工具，两者都被硬编码为"未经 `/selfcheck` 打开就对聊天 AI 隐藏"。全部删除：`src/tools/self_check.rs`（888 行）、`src/tools/check_logs.rs`（129 行）整体删除；`channels/mod.rs` 里的 `/selfcheck` 命令解析、gate 开关调用、两段系统提示词说明、`effective_excluded_tools` 里的 gate 分支全部移除并简化；`channels/telegram.rs` 的 Telegram 命令菜单去掉 `selfcheck` 条目；`cron/scheduler.rs` 两处后台任务 prompt 里"禁止调用 self_check/check_logs"的规则连带删除（工具已不存在，规则本身变得多余）；`tools/mod.rs` 移除模块声明/`pub use`/风险分级/构造调用；`elfclaw_log/mod.rs`、`tools/source_sync.rs`、`agent/loop_.rs` 更新了引用这两个工具的过时注释。`query_recent()` 保留（`gateway/api.rs` 的仪表盘日志接口仍在用）；`source_sync` 工具本身保留（是独立注册的常驻工具，不是 self_check 专属）。验证：`cargo test --lib` 4181 passed，同样 11 个预置失败无新增（测试数从 4190 降到 4181，对应删掉的自检模块专属单测）；`cargo clippy` 在全部改动文件里零新增问题；`cargo fmt --all -- --check` 改动前后均为 154 处预置漂移，未引入新的格式问题；`资料/config.toml` 确认无字段引用这两个工具。
  - **已完成（第三部分）**：Shell/工具权限收紧（第 8 节第 1、2 条，完整记录见第 8 节本身）。
    要点：(a) 修复 `资料/config.toml` 里意外清空的 `non_cli_excluded_tools`，让聊天 AI 默认看不到 `shell`；
    (b) 新增 `src/tools/cf_crawler.rs` 四个原生工具替换 cf-crawler 的 shell 模板版本，用本机真实 exe
    做了手动验证（含特殊字符 argv 直传测试），同步精简 `SKILL.toml` 和 `news_fetcher.allowed_tools`。
    第 8 节第 3 条（按任务临时授权 shell 的具体机制）尚未实现，问过用户后明确选择暂不设计、
    优先级排到 Step 6 之后（见第 8 节第 3 条本身）。——**已被 Step 7"彻底移除 shell"取代，不再需要。**
  - **暂缓，原因是改动面比预期大，需要单独一步做**：
    1. `/webhook`、`/whatsapp`、`/linq`、`/wati`、`/nextcloud-talk` 路由删除——调查发现这些会级联到独立的 channel 实现文件（如 `src/channels/whatsapp.rs`/`whatsapp_web.rs`）和 `AppState` 里的多个专属字段，不是单文件自包含改动。
  - ~~后续会话按这个顺序继续：第 8 节第 3 条的 shell 按任务授权机制设计……~~（已被 Step 7 取代）；webhook 系路由删除仍暂缓。
- **Step 6（已完成，2026-09-23）**：清理约束弱模型的旧 prompt——只删已经被对应代码保证覆盖的那部分，不是一次性全删。
  审计范围：本次会话（Step 1-5）新增/移除的功能在 `资料/workers/*.md`、`资料/skills/**/SKILL.{toml,md}` 里留下的过时提示文字
  （全局 `grep` 确认没有遗留对 self_check/check_logs/economic/goals/agents_ipc/openai_compat/`/v1/*` 的引用——这些在 Step
  1/5 删除时已经顺手清理干净）。找到唯一一处实质性遗留：`资料/workers/news_fetcher.md` 的"## Shell 运行规则"整节
  （CWD 约定、允许/禁止命令表、复合命令拆分规则、三次失败看门狗，约 29 行）——这套规则是在 news_fetcher 还拥有 `shell`
  工具权限时写的详细使用手册；Step 5 第三部分已经把 `"shell"` 从 `[agents.news_fetcher].allowed_tools` 里移除，
  该小节描述的能力对这个子 agent 已经不存在，整节删除（还删了 "如果 shell 工具报错" 一行、把 CRITICAL 节开头
  "禁止用 shell 工具调用 cf-crawler.exe" 改写成纯正面指令，不再需要先禁止一个已经拿不到的工具）。194 行 → 165 行。
  未做全量审计：`资料/` 下还有 894 个 markdown 文件（多是第三方技能包的参考文档，如 scientific-tools 系列），
  本次只清理了这次会话自己造成的过时内容，没有对整个技能库做地毯式审计——那是规模完全不同的另一项工作，
  不在"清理这次改动留下的旧约束"这个 Step 6 的原始意图范围内。同样是 `.gitignore` 忽略的本地文件，需要手动
  同步到 K6 才生效（见 dev_log.md）。
- **Step 7（已完成，2026-09-23）**：`shell`/`process`/`schedule` 工具彻底移除（第 8 节第 3 条，问题被
  用户"彻底移除shell"的指示直接取代，不再需要按任务授权设计）。同时修了一个借此机会才发现的独立预置
  bug：`agent/loop_/parsing.rs` 的 `map_tool_name_alias()` 把真实存在的 `browser_open`/`browser`/
  `web_search` 三个工具错误地别名到 `"shell"`（历史遗留，与本次改动无关；**更正（Step 8 复查）**：
  `web_search` 并不是真实工具名，搜索工具注册名是 `web_search_tool`，现已改为把 `web_search` 别名到它），导致 LLM 以 GLM 简写格式调用
  `browser_open/url>...` 时会被静默改写成裸 `curl` shell 命令执行，完全绕过 `browser_open` 工具自己的
  域名白名单/URL 校验（`validate_url`）——这是本次改动之前就存在的、真实可复现的安全问题（用测试直接
  证明：修复前 `map_tool_name_alias("browser_open")` 返回 `"shell"`），移除 shell 后这个错误映射的后果
  从"静默绕过安全校验"变成"报错找不到 shell 工具"，但 `browser_open`/`browser`/`web_search` 三个真实工具
  本身也被连带弄坏了（调用它们会失败）——两个问题一并修：三个工具名不再被别名重写，落回各自本名；
  新增回归测试锁定这个行为。详细改动清单、`cron` 表 `job_type` 列 `DEFAULT 'shell'` 迁移安全修复
  （K6 生产库里若有旧 shell 类型任务行，原本会在下次读取时因枚举不再接受 `"shell"` 而解析失败，新增
  幂等迁移把旧值改写成 `'agent'`）、以及 5 个纯文本 skill 文件（`elfradio-runner`/`skill-creator`/
  `scientific-tools`/`self-improving`/`skill-evolution-manager`）里假设 shell 可用但现已失效的提示文字
  （这些是 prose-only skill，只影响系统提示词文本，不是可调用工具，未删除，留给后续会话按需处理），
  见 dev_log.md 对应条目。
- **Step 8（已完成，2026-09-24）**：核心文件写保护骨架（第 3 节第 4 条，详见该条）+ 对 Step 0-7 的整体复查。
  复查发现并修复的问题：
  1. **AI 能直接改真实 `config.toml`**：部署配置 `workspace_only = false`，而解析后路径检查只看父目录；
     `forbidden_paths` 里的 `"config.toml"` 是相对路径，永远匹配不上 `C:\dev\...\config.toml` 这个绝对路径。
     把 `config.toml` 加进保护名单，回归测试先在去掉保护时确认失败（写入成功），再确认修复后被拦截。
  2. **截图可以覆盖核心文件**：`screenshot` 工具把文件名直接拼到 workspace 根目录，`browser` 截图/PDF 输出
     同理——`filename: "HEARTBEAT.md"` 会用 PNG 覆盖它。两处唯一出口都加了保护检查。
  3. **`web_search_tool` 风险分级一直没生效**：两张分级表写的都是不存在的 `"web_search"`，真实工具
     `web_search_tool` 于是落到默认 Standard 级，每次搜索都要审批（跟 `AGENTS.md` 要求"不确定就先搜索"直接冲突）。
     改名 + 新增测试把分级表和工具真实注册名绑定。同时别名表 `web_search` → `web_search_tool`，默认参数改为
     `query`（原来错配成 `url`）。
  4. **Shell 残留的提示词**（Step 6 审计漏掉了根目录的核心文件）：每条消息都注入的
     `Shell: PowerShell. Use python…` 平台提示、系统提示词"允许用 shell 收集信息"、循环中断恢复提示里的
     "避免 shell 命令"、默认 AIEOS 身份里声明的 `shell` 能力、新工作区脚手架 `TOOLS.md` 里的 shell 条目——全部删除。
     部署版 `资料/AGENTS.md` 删掉整节"Shell 运行规则"和"Worker Shell 规则管理"（后者**明文要求主 agent 发现
     worker shell 失败时用 `file_write` 改写 worker 工作手册**，正是"AI 自作主张改 prompt"链路），`资料/TOOLS.md`
     删掉 shell 条目。
  5. **提示词里让 AI 改核心文件的指令**（现在会被代码拦截、白白失败）：部署版 `AGENTS.md`/`SOUL.md`/`TOOLS.md`
     和脚手架模板里的"学到教训就更新 AGENTS.md/TOOLS.md""把学到的写回 USER.md/SOUL.md/IDENTITY.md""删除
     BOOTSTRAP.md"等全部改为：记到 `MEMORY.md` / 用 `note_add` 记事 / 告诉爸爸由他改。
     **更正**：这条做过头了——用户指出 `SOUL.md`/`USER.md`/`IDENTITY.md`/`TOOLS.md` 本来就该让 AI 改，锁住它们是
     "制造问题再解决问题"。已撤回：这四个文件移出保护名单，相关指令恢复原样（只有 `SOUL.md` 里"日程提醒"一条
     保留为 `note_add`，那是 Step 3 的代码驱动提醒，与文件保护无关）。
  6. **技能提示词还在宣传调不了的工具**：`SKILL.toml` 的 `[[tools]]` 以前会被渲染进系统提示词，但唯一能执行
     它们的桥接层（仅支持 `kind="shell"`）已随 Step 7 删除——新装的技能会让模型去调"不存在的工具"。不再渲染。
  7. **解析器里残留的 curl 命令拼接**：`build_curl_command` 和两处 `"shell"` 分支（把 URL 改写成
     `curl -s '<url>'`）只服务于已删除的 shell 工具，删除。
  8. **验证了升级安全**：K6 上还没同步的旧 `config.toml`（仍含 `allowed_commands` 等已删字段、工具列表里还有
     `"shell"`）照样能加载和通过校验，只打"Unknown config key ignored"警告——用临时测试确认后删除。
  9. **文档更正**：Step 7 的 dev_log 条目误写"本次未改动 `资料/`、无新增同步需求"，实际改了 `资料/config.toml`
     和 `资料/skills/cf-crawler/SKILL.toml`；K6 手动同步完整清单见 dev_log.md 2026-09-24 条目。
  **本轮所有步骤此前只跑了 `cargo test --lib`，集成测试（`tests/` 下 24 个）一直没跑过**。这次补跑：230 通过，
  3 个失败（`agent_loop_robustness` 的循环检测测试，期望旧版的报错行为，而 2026-03 起 elfClaw 改成返回友好提示
  文字——早于本轮、与本轮无关，未修）。

- **Step 9（已完成，2026-09-24）**：HEARTBEAT.md 迁移到声明式格式（第 3 节第 4 条）。
  - **为什么必须做**：Step 2 起心跳不再把 HEARTBEAT.md 发给模型读，只由代码解析声明块；而真实的 HEARTBEAT.md
    是纯文字、一个声明块都没有——不迁移的话，新版装上后**一条新闻都不会推送**。
  - **代码**：`heartbeat_decl.rs` 的声明格式新增可选字段 `delegate_to`，任务由调度器直接交给对应子 agent 执行
    （跳过主 agent 中转，调度器注释里记录这能省约 5.5 万 token 和两次模型调用）。两道校验：agent 名不在
    `[agents]` 里就报错跳过（否则调度器会让任务带着**全部工具**跑）；从声明里删掉 `delegate_to` 时重建任务
    （否则更新补丁会保留旧值）。新增 4 个测试，后两个先去掉对应逻辑确认会失败。
  - **部署文件**：`HEARTBEAT.md`（7 个任务：06:30 早报综合、09:30 科技AI、12:30 军事无人机、15:30 中国亚太、
    18:30 无线电Maker、21:30 金融澳洲，每天 10:00/22:00 新闻源搜索；全部 `delegate_to = "news_fetcher"`，
    推送到 Telegram 495916105）；新建 `HEARTBEAT_DATA.md`（时段源清单、封禁/观察中的源、已踢掉的死源、
    候选新源——原 `news_sources.md` 的候选源并入这里，原 `homework/news/ban_list.md` 的封禁记录改写到这里）；
    `workers/news_fetcher.md` 改为从 HEARTBEAT_DATA.md 读源、最终回复即推送内容、不再调 `send_telegram`；
    `AGENTS.md` 委派规则同步；`config.toml` 里 `news_fetcher` 的工具去掉 `send_telegram`（留着会重复推送）、
    加上 `file_write`（**现存 bug**：手册要求写新闻文件去重和记录封禁，但一直没有这个权限）。
  - **行为变化**：以前主 agent 读完 worker 报告会再发一句"评价"，现在没有这一步了，推送的就是新闻本身；
    封禁源信息放在新闻消息最后一行的「⚠️ 源状态」里。
  - **验证**：临时测试直接读取真实 `资料/config.toml` 和 `资料/HEARTBEAT.md` 跑对账——配置通过校验，7 个任务
    全部建出、零错误、重复对账不新增，每个任务的悉尼本地时间、子 agent、推送对象都正确，每个时段在
    HEARTBEAT_DATA.md 都有对应源清单（测试已删除）。

- **Step 10（第一部分已完成，2026-09-24）**：cron 与提醒加固（用户要求"主 agent 能稳定地增删提醒和新闻推送任务"）。
  修复：`note_add` 到期不提醒；`cron_add` 名字必填 + 写锁内查重（实测复现了并行工具调用导致的重复创建）+ 完全相同的任务拒绝
  + 同时间其他任务给出提醒；`cron_remove` 支持按名字删除全部同名任务；`heartbeat:`/`news:`/`note:` 受管任务不能被
  `cron_remove`/`cron_update` 改动（以前删了会被对账重建，工具却回复成功）；`cron_update` 不能改名到已有名字；HEARTBEAT.md
  有解析错误时对账不删除任务；名字跨任务类型复用会把 agent 任务改坏的问题。详见 dev_log.md。
  **第二部分（已完成，2026-09-24）**：新闻时段改为 `news_schedule`（主 agent 增删改时段和源）/`news_report`（worker 上报
  抓取结果、登记候选源）结构化工具 + 只由代码写入的 `HEARTBEAT_DATA.toml`；HEARTBEAT.md 用 `news-rules` 规则块写死推送对象、
  子 agent、静默时段、数量上限、封禁阈值。计数封禁、剔除封禁源、同步定时任务全部由代码完成；数据文件读写加锁（实测无锁时并发
  改动会丢失）。另修了心跳错误每小时重复发 Telegram 的问题。取代 Step 9 让模型直接编辑数据文件的做法。详见 dev_log.md。

## 11. 已发现、暂缓到对应 Step 修复的安全问题

- skill 审计的高危模式检测用 `find_map`，只报告第一个命中的模式，白名单声明一个模式就可能连带放过同文件里的其他危险模式（如 `rm -rf`）。
  **已修复，2026-09-23**：`detect_high_risk_snippet` 改名为 `detect_high_risk_snippets`，返回 `Vec<&str>`（全部命中）而不是
  `Option<&str>`（只有第一个）；4 个调用点相应改成遍历全部命中；允许白名单场景下，只会豁免白名单显式声明的那个具体
  pattern，不会连带放过同文件里其他未声明的 pattern。新增回归测试
  `audit_allowlisting_one_real_pattern_does_not_hide_a_second_real_pattern`——先在旧代码上跑确认失败（真的会漏报
  `rm -rf /`），再在新代码上确认通过。
- `cron_add`/`cron_update` 当前被设为免审批，但内部 `validate_command_execution` 信任的是**模型自己传的 `approved` 参数**，等于没有人工审批。
  **已彻底解决，2026-09-23（Step 7，随 shell 整体移除一并消除，不是单独修的）**：之前判断"暂不修"是因为修法
  需要给 `cron_add` 引入调用方渠道信息，改动面大；但 Step 7 把 `JobType::Shell` 变体、`cron_add`/`cron_update`
  的 `approved` 参数、以及它们各自调用的 `validate_command_execution` 全部删除了——**这个漏洞依附的代码路径
  本身已经不存在**，不再需要单独设计权限模型。现在 `cron_add`/`cron_update` 只能创建 `agent`（走完整 agent
  循环+其自身工具权限）或 `message`（纯文本提醒，不执行任何代码）类型的任务，两者都不存在"模型自报
  已审批"这种绕过方式。
- `sqlite_query` 的受保护数据库路径检查是字符串后缀匹配，Windows 8.3 短文件名或路径变体可能绕过。
  **已修复，2026-09-23**：新增 `system_db_match()` 辅助函数，比较 `Path::file_name()` 而不是字符串后缀——顺带修了一个
  过度拦截的副作用 bug（原来的 `"...".ends_with("brain.db")` 会把 `my_brain.db` 这种无关文件也误判为受保护数据库）。
  检查点从"只在原始未解析字符串上做一次"改成两层：原始字符串上的快速路径（提前拒绝常见情况）+ **在 `canonicalize()`
  之后的解析路径上再做一次权威检查**（这层才是真正堵住绕过的关键——8.3 短文件名、符号链接等各种别名，
  `canonicalize()` 都会解析成同一个真实文件，但原始字符串检查看不穿）。新增 7 个测试，含端到端 `execute()` 测试证明
  `my_brain.db` 不再被误拦、`brain.db`（含子目录形式）仍被正确拦截。**注**：Windows 8.3 短文件名场景本身无法在可移植的
  单测里可靠复现（依赖 NTFS 卷是否启用 8.3 别名生成，这点因环境而异，本沙箱环境对符号链接/硬链接相关测试也缺相应权限
  ——是已知的 11 个预置测试失败之一），修复的正确性基于 `tokio::fs::canonicalize()`/Windows API 文档保证的标准行为
  （解析短文件名和符号链接到规范长文件名），不是靠这类场景的直接测试验证。
- **五个 `*_config` 工具让聊天 AI 能改自己的 `config.toml`（2026-09-24 复查发现）**——**已解决**：用户定下
  "agent 不许动底层配置、不许换模型"，这 5 个连同 `switch_provider`/`manage_auth_profile`/`openclaw_migration`
  共 8 个改写 `config.toml` 的工具已全部删除（见第 3 节第 4 条）。原始记录：
  `model_routing_config`/`proxy_config`/`web_access_config`/`web_search_config`/`channel_ack_config` 都会直接写回
  `config.toml`。代码里它们被标为 Restricted（注释写"对非 CLI 渠道隐藏"），但**默认分级只在 `tool_overrides`
  里点名时才生效**，部署配置又是 `non_cli_excluded_tools = []`，所以 Telegram 上的聊天 AI 实际上能看到并调用它们；
  部署配置还把 `web_access_config` 放进了 `auto_approve`——意味着 AI 可以免审批地放宽自己的网址白名单。
  这和第 3 节第 4 条"AI 不能改自己的配置"直接冲突，但删掉它们也会失去"聊天里让 AI 帮忙换模型"之类的用法，
  属于产品取舍，没有擅自处理。
- Telegram 相册在跨 `getUpdates` 长轮询批次时可能被拆成两条消息。**Step 4 未处理**——相册分组逻辑需要跨多次 poll 缓冲，改动面比离线消息那个大，且不是用户反馈过的实际痛点，往后放。

## 12. 密钥与账号管理

- **CF token**（`cfat_...`，账户 `baf365a52956bb35cf34ff922f4e8298`，有效期到 2026-11-25）：仅用于只读验证 `cf-crawler-worker` 状态和测试 `C:\Dev\cf-crawler` 爬虫程序，**不修改任何 CF 设置**。
- **GitHub token**（`ghp_...`）：仅用于操作 `VK7KSM/eflClaw` 和 `VK7KSM/cf-crawler` 两个仓库，不用于其他仓库。
- **Gemini key 池**：目前已验证 `daishuvpn@gmail.com` 一个 key（可正常调用 3.8/3.7/3.6/3.5-flash/3.5-flash-lite/embedding-2，首次请求偶发 503 属正常现象）；另有 `khunkasim@gmail.com` 一个 key 尚未测试。用户会补齐到至少 5 个。**多账号轮换放大免费额度违反 Google APIs 服务条款 §2(d)，用户已知晓此风险并选择承担，不再讨论。**
- **已清理的历史泄露密钥**（用户确认均已失效/过期，不需要轮换，只是清理历史避免 GitHub 骚扰）：LLM 代理 key（`sk-2c87...`）、Gmail 应用密码、`CF_CRAWLER_TOKEN`、Telegram bot token、CF Worker secret、gateway pairing token。

## 14. 请求体积与响应速度（2026-09-24）

**起因**：K6 上在群里说一句 hello，输入是 50,182 token，耗时 20.3 秒。用 Gemini 官方计数接口实测，这 5 万 token 的构成是：技能说明全文 17,239（`prompt_injection_mode = "full"`，其中 agent-browser 一个就占 7,149）、内置工具定义 11,116、工作区 md 文件约 8,400、GitHub MCP 的 41 个工具 7,356、prompts.chat MCP 的 10 个工具 1,869，其余是固定说明。慢主要是因为 `reasoning_level = 3`，对 Gemini 3 Flash 就是最高的 "high" 思考档。

**要点：模型接口没有状态，每次请求都要带上全部内容。** 不存在"第一次发过、以后就不发"这种做法。能做的只有两件事：让每次发的内容变少，以及让 Gemini 自带的隐式前缀缓存（默认开启，开头至少 4096 token 完全相同才能命中）真正命中。

**Cloudflare AI Gateway 不能解决这个问题**：它的缓存是整个请求一模一样才命中（按完整请求体算哈希），而我们每条消息内容都不同，不会减少发给 Gemini 的 token，还多绕了一层。

**改动**：

1. 配置：`reasoning_level` 3 → 2（medium）；技能改为 `prompt_injection_mode = "compact"`（只放名字和简介，用到时模型再读 SKILL.md）；两个 MCP 服务器全部移除（`[mcp] enabled = false`），GitHub MCP 的明文令牌也一起从配置里删掉了。
2. **系统提示词只放不变的内容**：当前时间、未完成的记事、定时任务列表、学到的纠错规则、聊天摘要，这些每条消息都可能变，改由 `attach_turn_context()` 附在当前这条用户消息前面（以"[系统附加的实时信息，不是用户说的话]"开头），不写进系统提示词，也不存进聊天历史。这样系统提示词、工具定义和之前的聊天记录在两次请求之间一个字都不变，满足前缀缓存命中的条件。原来的 `build_runtime_status_section` 拆成两部分：固定的 `build_runtime_static_section`（自主等级、子 agent 列表按字母排序、能力边界，放进缓存的系统提示词）和每条消息都刷新的 `build_cron_jobs_section`。GitHub MCP 的说明文字也一并删掉了。
3. **工作区 md 文件改了立即生效**：以前系统提示词只在启动时生成一次，AI 改了 SOUL/USER/MEMORY.md 要重启才生效。现在由 `ChannelSystemPrompt` 在每条消息时比对 7 个文件（AGENTS/SOUL/TOOLS/IDENTITY/USER/BOOTSTRAP/MEMORY.md）的修改时间和大小，有变化才重建，没变化就原样复用。
4. **聊天历史**：每人最多保留条数从 50 降到 20；某个人超过 1 小时没发消息，下一条消息到来时先清空他的历史（`expire_idle_sender_history`）。更早的上下文仍然可以通过每条消息附带的聊天摘要、首条消息的记忆检索和 `search_chat_log` 找回。
5. **可以验证**：Gemini 每次调用都会多记一条 `LLM tokens: gemini/<model> prompt=… cached=… thoughts=…` 日志（`elfclaw_log::log_llm_token_breakdown`）。`cached` 表示命中缓存的 token 数，`thoughts` 表示思考用掉的 token 数（这部分不算在输出 token 里）。

**记忆相关说明**：MEMORY.md 属于工作区 md 文件，每次请求都整份附带，单个文件最多 2 万字符，超出部分会被截断，所以应该只放精选的长期信息。记忆库 brain.db 不会整份发送：只在一段对话的第一条消息时，按相关度附上最多 4 条（最多 4000 字符），其他时候由模型需要时自己调 `memory_recall` 去查。


## 15. 对话模型顺序与额度冷却（2026-09-24）

- **顺序（用户决定）**：`gemini-3.6-flash`（最快，日常对话够用）→ `3.7-flash` → `3.8-flash` → `3.5-flash` 兜底。6 个 key 来自 6 个不同的 Google 项目，各有一份独立的免费额度。
- **轮换**：同一个模型先把 6 个 key 轮完，再换下一个模型。某个模型返回 503（过载）时，这一轮直接换下一个模型，因为过载和 key 无关；返回 429 时换同一模型的下一个 key。
- **冷却**：当日额度用完的 key+模型，跳过到太平洋时间午夜（Google 在这个时间重置免费额度）；每分钟限流跳过 60 秒。冷却记录只在内存里；所有组合都在冷却中时照样尝试，不会不试就拒绝。
- **多 key 解决不了 503**：503 是 Google 那边这个模型过载，换 key 没用，只能换模型或等待。


## 16. 新闻推送由程序主导（2026-09-25）

原则同第 3 节：确定性的事交给代码，模型只写文字。

- 新闻时段是 `JobType::News` 任务，到点执行 `cron::news_pipeline::run_slot`，不再经过 agent 的工具循环。
- **程序负责**：
  - 抓取所有来源（RSS/Atom、Telegram 公开频道网页、Google 新闻 RSS、Polymarket 赔率异动、Hacker News，没有 feed 的网页交给 cf-crawler）；
  - 关键词过滤、只取 36 小时内的内容、跨来源去重、14 天历史去重（`state/news_history.db`）；
  - 行情（Yahoo 加中行牌价）；
  - 排版；
  - 来源失败计数和封禁。
- **模型只调用一次**：从候选里挑选，写中文标题和一句话摘要，返回 JSON。链接由程序按编号从原始条目取，模型改不了。模型不可用时照样推送原文标题。
- 中共党政媒体由程序按域名标"官方口径"，模型只在它透露政策动向时才选，并说明是官方说法。
- 时段、来源在 `HEARTBEAT_DATA.toml`。时段可设 `quotes`（附带行情）、`max_items`；来源可设 `name`（显示名）、`filter`（关键词过滤，给量很大的快讯频道用）。

### 16.1 会展推送（`kind = "expo"`，每天 09:00）

- 时段设 `kind = "expo"` 后，`run_slot` 改为调用 `cron::expo_pipeline::run`。
- 覆盖范围：悉尼、墨尔本、布里斯班、黄金海岸、阿德莱德，60 天内开幕的展会。
- 每个展会通知三次：首次发现、开展前约 30 天、开展前 7 天内。已经错过的节点不再补发，例如发现时离开展只剩 5 天，就只发这一次。
- 程序负责：
  - 抓取来源。Eventbrite 这类页面里的 schema.org Event 结构化数据由程序直接读，日期、场馆、链接都以它为准；
  - 其他页面（会展中心日程、EventsEye、展会官网）转成纯文字，`browser = true` 的来源走 cf-crawler 浏览器渲染；
  - 核对城市、类别白名单和日期窗口（持续超过 21 天的是长期展览，不收）；
  - 与数据库合并（同城、开幕日相差不超过 1 天、名称相近算同一个展会）；
  - 决定当天要发哪些通知，并排版。
  - 状态存在 `state/expo.db`，展会结束 30 天后删除。
- 模型每天最多调用两次：
  - 从材料里挑出展会、归类并读出日期。链接取自结构化数据或页面自带的链接，模型给的链接不采用；
  - 对还没查过票价的展会（每天最多 12 个），程序先抓它自己的页面，模型再根据页面文字写票价和免费入场办法。
- 类别白名单：汽车、建筑家居、商品博览、电子科技、工业制造、图书、游戏、动漫同人、收藏潮玩、安防、博彩、航空航天、防务军工、成人。模型给出的其他类别（包括"其他"）由代码丢弃。


## 13. 代码语言约束

**全部用 Rust 实现，不引入 Python 等其他语言的运行时依赖。** 之前分析阶段用 Python 脚本做过一次性的数据分析（读 K6 拷回来的 SQLite/日志、测 Gemini key），那些是本地一次性工具，不进入 elfClaw 代码库；正式功能代码一律 Rust。
