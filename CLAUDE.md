# CLAUDE.md — ZeroClaw Agent Engineering Protocol

This file defines the default working protocol for Claude agents in this repository.
Scope: entire repository.

## 0) 最高优先级规则（必须遵守）

1. **始终使用中文与用户对话。** 所有回复、解释、提问都必须使用中文。代码注释和变量名保持英文。
2. **每次开始工作前，先阅读 `dev_log.md`** 了解项目近期修改内容和设计决策。
3. **每完成一步修改后，立即将修改内容追加到 `dev_log.md`**，包括：改了什么文件、为什么改、改动要点。如果 `dev_log.md` 不存在，创建它。
4. **架构参考**：阅读 `Research.md` 了解 ZeroClaw 与 OpenClaw 的完整文件目录、二次开发现状、以及待改进的活跃时间逻辑和渠道路由抽象方案。

## 1) Project Snapshot (Read First)

ZeroClaw is a Rust-first autonomous agent runtime optimized for:

- high performance
- high efficiency
- high stability
- high extensibility
- high sustainability
- high security

Core architecture is trait-driven and modular. Most extension work should be done by implementing traits and registering in factory modules.

Key extension points:

- `src/providers/traits.rs` (`Provider`)
- `src/channels/traits.rs` (`Channel`)
- `src/tools/traits.rs` (`Tool`)
- `src/memory/traits.rs` (`Memory`)
- `src/observability/traits.rs` (`Observer`)
- `src/runtime/traits.rs` (`RuntimeAdapter`)
- `src/peripherals/traits.rs` (`Peripheral`) — hardware boards (STM32, RPi GPIO)

## 2) Deep Architecture Observations (Why This Protocol Exists)

These codebase realities should drive every design decision:

1. **Trait + factory architecture is the stability backbone**
    - Extension points are intentionally explicit and swappable.
    - Most features should be added via trait implementation + factory registration, not cross-cutting rewrites.
2. **Security-critical surfaces are first-class and internet-adjacent**
    - `src/gateway/`, `src/security/`, `src/tools/`, `src/runtime/` carry high blast radius.
    - Defaults already lean secure-by-default (pairing, bind safety, limits, secret handling); keep it that way.
3. **Performance and binary size are product goals, not nice-to-have**
    - `Cargo.toml` release profile and dependency choices optimize for size and determinism.
    - Convenience dependencies and broad abstractions can silently regress these goals.
4. **Config and runtime contracts are user-facing API**
    - `src/config/schema.rs` and CLI commands are effectively public interfaces.
    - Backward compatibility and explicit migration matter.
5. **The project now runs in high-concurrency collaboration mode**
    - CI + docs governance + label routing are part of the product delivery system.
    - PR throughput is a design constraint; not just a maintainer inconvenience.

## 3) Engineering Principles (Normative)

These principles are mandatory by default. They are not slogans; they are implementation constraints.

### 3.1 KISS (Keep It Simple, Stupid)

**Why here:** Runtime + security behavior must stay auditable under pressure.

Required:

- Prefer straightforward control flow over clever meta-programming.
- Prefer explicit match branches and typed structs over hidden dynamic behavior.
- Keep error paths obvious and localized.

### 3.2 YAGNI (You Aren't Gonna Need It)

**Why here:** Premature features increase attack surface and maintenance burden.

Required:

- Do not add new config keys, trait methods, feature flags, or workflow branches without a concrete accepted use case.
- Do not introduce speculative “future-proof” abstractions without at least one current caller.
- Keep unsupported paths explicit (error out) rather than adding partial fake support.

### 3.3 DRY + Rule of Three

**Why here:** Naive DRY can create brittle shared abstractions across providers/channels/tools.

Required:

- Duplicate small, local logic when it preserves clarity.
- Extract shared utilities only after repeated, stable patterns (rule-of-three).
- When extracting, preserve module boundaries and avoid hidden coupling.

### 3.4 SRP + ISP (Single Responsibility + Interface Segregation)

**Why here:** Trait-driven architecture already encodes subsystem boundaries.

Required:

- Keep each module focused on one concern.
- Extend behavior by implementing existing narrow traits whenever possible.
- Avoid fat interfaces and “god modules” that mix policy + transport + storage.

### 3.5 Fail Fast + Explicit Errors

**Why here:** Silent fallback in agent runtimes can create unsafe or costly behavior.

Required:

- Prefer explicit `bail!`/errors for unsupported or unsafe states.
- Never silently broaden permissions/capabilities.
- Document fallback behavior when fallback is intentional and safe.

### 3.6 Secure by Default + Least Privilege

**Why here:** Gateway/tools/runtime can execute actions with real-world side effects.

Required:

- Deny-by-default for access and exposure boundaries.
- Never log secrets, raw tokens, or sensitive payloads.
- Keep network/filesystem/shell scope as narrow as possible unless explicitly justified.

### 3.7 Determinism + Reproducibility

**Why here:** Reliable CI and low-latency triage depend on deterministic behavior.

Required:

- Prefer reproducible commands and locked dependency behavior in CI-sensitive paths.
- Keep tests deterministic (no flaky timing/network dependence without guardrails).
- Ensure local validation commands map to CI expectations.

### 3.8 Reversibility + Rollback-First Thinking

**Why here:** Fast recovery is mandatory under high PR volume.

Required:

- Keep changes easy to revert (small scope, clear blast radius).
- For risky changes, define rollback path before merge.
- Avoid mixed mega-patches that block safe rollback.

## 4) Repository Map (High-Level)

- `src/main.rs` — CLI entrypoint and command routing
- `src/lib.rs` — module exports and shared command enums
- `src/config/` — schema + config loading/merging
- `src/agent/` — orchestration loop
- `src/gateway/` — webhook/gateway server
- `src/security/` — policy, pairing, secret store
- `src/memory/` — markdown/sqlite memory backends + embeddings/vector merge
- `src/providers/` — model providers and resilient wrapper
- `src/channels/` — Telegram/Discord/Slack/etc channels
- `src/tools/` — tool execution surface (shell, file, memory, browser)
- `src/peripherals/` — hardware peripherals (STM32, RPi GPIO); see `docs/hardware-peripherals-design.md`
- `src/runtime/` — runtime adapters (currently native)
- `docs/` — task-oriented documentation system (hubs, unified TOC, references, operations, security proposals, multilingual guides)
- `.github/` — CI, templates, automation workflows

## 4.1 Documentation System Contract (Required)

Treat documentation as a first-class product surface, not a post-merge artifact.

Canonical entry points:

- root READMEs: `README.md`, `README.zh-CN.md`, `README.ja.md`, `README.ru.md`, `README.fr.md`, `README.vi.md`
- docs hubs: `docs/README.md`, `docs/README.zh-CN.md`, `docs/README.ja.md`, `docs/README.ru.md`, `docs/README.fr.md`, `docs/i18n/vi/README.md`
- unified TOC: `docs/SUMMARY.md`

Supported locales (current contract):

- `en`, `zh-CN`, `ja`, `ru`, `fr`, `vi`

Collection indexes (category navigation):

- `docs/getting-started/README.md`
- `docs/reference/README.md`
- `docs/operations/README.md`
- `docs/security/README.md`
- `docs/hardware/README.md`
- `docs/contributing/README.md`
- `docs/project/README.md`

Runtime-contract references (must track behavior changes):

- `docs/commands-reference.md`
- `docs/providers-reference.md`
- `docs/channels-reference.md`
- `docs/config-reference.md`
- `docs/operations-runbook.md`
- `docs/troubleshooting.md`
- `docs/one-click-bootstrap.md`

Required docs governance rules:

- Keep README/hub top navigation and quick routes intuitive and non-duplicative.
- Keep entry-point parity across all supported locales (`en`, `zh-CN`, `ja`, `ru`, `fr`, `vi`) when changing navigation architecture.
- If a change touches docs IA, runtime-contract references, or user-facing wording in shared docs, perform i18n follow-through for currently supported locales in the same PR:
  - Update locale navigation links (`README*`, `docs/README*`, `docs/SUMMARY.md`).
  - Update localized runtime-contract docs where equivalents exist (at minimum `commands-reference`, `config-reference`, `troubleshooting` for `fr` and `vi`).
  - For Vietnamese, treat `docs/i18n/vi/**` as canonical. Keep `docs/*.<locale>.md` compatibility shims aligned if present.
- Keep proposal/roadmap docs explicitly labeled; avoid mixing proposal text into runtime-contract docs.
- Keep project snapshots date-stamped and immutable once superseded by a newer date.

## 5) Risk Tiers by Path (Review Depth Contract)

Use these tiers when deciding validation depth and review rigor.

- **Low risk**: docs/chore/tests-only changes
- **Medium risk**: most `src/**` behavior changes without boundary/security impact
- **High risk**: `src/security/**`, `src/runtime/**`, `src/gateway/**`, `src/tools/**`, `.github/workflows/**`, access-control boundaries

When uncertain, classify as higher risk.

## 6) Agent Workflow (Required)

1. **Read before write**
    - Inspect existing module, factory wiring, and adjacent tests before editing.
2. **Define scope boundary**
    - One concern per PR; avoid mixed feature+refactor+infra patches.
3. **Implement minimal patch**
    - Apply KISS/YAGNI/DRY rule-of-three explicitly.
4. **Validate by risk tier**
    - Docs-only: lightweight checks.
    - Code/risky changes: full relevant checks and focused scenarios.
5. **Document impact**
    - Update docs/PR notes for behavior, risk, side effects, and rollback.
    - If CLI/config/provider/channel behavior changed, update corresponding runtime-contract references.
    - If docs entry points changed, keep all supported locale README/docs-hub navigation aligned (`en`, `zh-CN`, `ja`, `ru`, `fr`, `vi`).
6. **Respect queue hygiene**
    - If stacked PR: declare `Depends on #...`.
    - If replacing old PR: declare `Supersedes #...`.

### 6.1 Branch / Commit / PR Flow (Required)

All contributors (human or agent) must follow the same collaboration flow:

- Create and work from a non-`main` branch.
- Commit changes to that branch with clear, scoped commit messages.
- Open a PR to `main`; do not push directly to `main`.
- Wait for required checks and review outcomes before merging.
- Merge via PR controls (squash/rebase/merge as repository policy allows).
- Branch deletion after merge is optional; long-lived branches are allowed when intentionally maintained.

### 6.2 Worktree Workflow (Required for Multi-Track Agent Work)

Use Git worktrees to isolate concurrent agent/human tracks safely and predictably:

- Use one worktree per active branch/PR stream to avoid cross-task contamination.
- Keep each worktree on a single branch; do not mix unrelated edits in one worktree.
- Run validation commands inside the corresponding worktree before commit/PR.
- Name worktrees clearly by scope (for example: `wt/ci-hardening`, `wt/provider-fix`) and remove stale worktrees when no longer needed.
- PR checkpoint rules from section 6.1 still apply to worktree-based development.

### 6.3 Code Naming Contract (Required)

Apply these naming rules for all code changes unless a subsystem has a stronger existing pattern.

- Use Rust standard casing consistently: modules/files `snake_case`, types/traits/enums `PascalCase`, functions/variables `snake_case`, constants/statics `SCREAMING_SNAKE_CASE`.
- Name types and modules by domain role, not implementation detail (for example `DiscordChannel`, `SecurityPolicy`, `MemoryStore` over vague names like `Manager`/`Helper`).
- Keep trait implementer naming explicit and predictable: `<ProviderName>Provider`, `<ChannelName>Channel`, `<ToolName>Tool`, `<BackendName>Memory`.
- Keep factory registration keys stable, lowercase, and user-facing (for example `"openai"`, `"discord"`, `"shell"`), and avoid alias sprawl without migration need.
- Name tests by behavior/outcome (`<subject>_<expected_behavior>`) and keep fixture identifiers neutral/project-scoped.
- If identity-like naming is required in tests/examples, use ZeroClaw-native labels only (`ZeroClawAgent`, `zeroclaw_user`, `zeroclaw_node`).

### 6.4 Architecture Boundary Contract (Required)

Use these rules to keep the trait/factory architecture stable under growth.

- Extend capabilities by adding trait implementations + factory wiring first; avoid cross-module rewrites for isolated features.
- Keep dependency direction inward to contracts: concrete integrations depend on trait/config/util layers, not on other concrete integrations.
- Avoid creating cross-subsystem coupling (for example provider code importing channel internals, tool code mutating gateway policy directly).
- Keep module responsibilities single-purpose: orchestration in `agent/`, transport in `channels/`, model I/O in `providers/`, policy in `security/`, execution in `tools/`.
- Introduce new shared abstractions only after repeated use (rule-of-three), with at least one real caller in current scope.
- For config/schema changes, treat keys as public contract: document defaults, compatibility impact, and migration/rollback path.

## 7) Change Playbooks

### 7.1 Adding a Provider

- Implement `Provider` in `src/providers/`.
- Register in `src/providers/mod.rs` factory.
- Add focused tests for factory wiring and error paths.
- Avoid provider-specific behavior leaks into shared orchestration code.

### 7.2 Adding a Channel

- Implement `Channel` in `src/channels/`.
- Keep `send`, `listen`, `health_check`, typing semantics consistent.
- Cover auth/allowlist/health behavior with tests.

### 7.3 Adding a Tool

- Implement `Tool` in `src/tools/` with strict parameter schema.
- Validate and sanitize all inputs.
- Return structured `ToolResult`; avoid panics in runtime path.

### 7.4 Adding a Peripheral

- Implement `Peripheral` in `src/peripherals/`.
- Peripherals expose `tools()` — each tool delegates to the hardware (GPIO, sensors, etc.).
- Register board type in config schema if needed.
- See `docs/hardware-peripherals-design.md` for protocol and firmware notes.

### 7.5 Security / Runtime / Gateway Changes

- Include threat/risk notes and rollback strategy.
- Add/update tests or validation evidence for failure modes and boundaries.
- Keep observability useful but non-sensitive.
- For `.github/workflows/**` changes, include Actions allowlist impact in PR notes and update `docs/actions-source-policy.md` when sources change.

### 7.6 Docs System / README / IA Changes

- Treat docs navigation as product UX: preserve clear pathing from README -> docs hub -> SUMMARY -> category index.
- Keep top-level nav concise; avoid duplicative links across adjacent nav blocks.
- When runtime surfaces change, update related references (`commands/providers/channels/config/runbook/troubleshooting`).
- Keep multilingual entry-point parity for all supported locales (`en`, `zh-CN`, `ja`, `ru`, `fr`, `vi`) when nav or key wording changes.
- When shared docs wording changes, sync corresponding localized docs for supported locales in the same PR (or explicitly document deferral and follow-up PR).
- For docs snapshots, add new date-stamped files for new sprints rather than rewriting historical context.


## 8) Validation Matrix

Default local checks for code changes:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Preferred local pre-PR validation path (recommended, not required):

```bash
./dev/ci.sh all
```

Notes:

- Local Docker-based CI is strongly recommended when Docker is available.
- Contributors are not blocked from opening a PR if local Docker CI is unavailable; in that case run the most relevant native checks and document what was run.

Additional expectations by change type:

- **Docs/template-only**:
    - run markdown lint and link-integrity checks
    - if touching README/docs-hub/SUMMARY/collection indexes, verify EN/ZH/JA/RU navigation parity
    - if touching bootstrap docs/scripts, run `bash -n bootstrap.sh scripts/bootstrap.sh scripts/install.sh`
- **Workflow changes**: validate YAML syntax; run workflow lint/sanity checks when available.
- **Security/runtime/gateway/tools**: include at least one boundary/failure-mode validation.

If full checks are impractical, run the most relevant subset and document what was skipped and why.

## 9) Collaboration and PR Discipline

- Follow `.github/pull_request_template.md` fully (including side effects / blast radius).
- Keep PR descriptions concrete: problem, change, non-goals, risk, rollback.
- Use conventional commit titles.
- Prefer small PRs (`size: XS/S/M`) when possible.
- Agent-assisted PRs are welcome, **but contributors remain accountable for understanding what their code will do**.

### 9.1 Privacy/Sensitive Data and Neutral Wording (Required)

Treat privacy and neutrality as merge gates, not best-effort guidelines.

- Never commit personal or sensitive data in code, docs, tests, fixtures, snapshots, logs, examples, or commit messages.
- Prohibited data includes (non-exhaustive): real names, personal emails, phone numbers, addresses, access tokens, API keys, credentials, IDs, and private URLs.
- Use neutral project-scoped placeholders (for example: `user_a`, `test_user`, `project_bot`, `example.com`) instead of real identity data.
- Test names/messages/fixtures must be impersonal and system-focused; avoid first-person or identity-specific language.
- If identity-like context is unavoidable, use ZeroClaw-scoped roles/labels only (for example: `ZeroClawAgent`, `ZeroClawOperator`, `zeroclaw_user`) and avoid real-world personas.
- Recommended identity-safe naming palette (use when identity-like context is required):
    - actor labels: `ZeroClawAgent`, `ZeroClawOperator`, `ZeroClawMaintainer`, `zeroclaw_user`
    - service/runtime labels: `zeroclaw_bot`, `zeroclaw_service`, `zeroclaw_runtime`, `zeroclaw_node`
    - environment labels: `zeroclaw_project`, `zeroclaw_workspace`, `zeroclaw_channel`
- If reproducing external incidents, redact and anonymize all payloads before committing.
- Before push, review `git diff --cached` specifically for accidental sensitive strings and identity leakage.

### 9.2 Superseded-PR Attribution (Required)

When a PR supersedes another contributor's PR and carries forward substantive code or design decisions, preserve authorship explicitly.

- In the integrating commit message, add one `Co-authored-by: Name <email>` trailer per superseded contributor whose work is materially incorporated.
- Use a GitHub-recognized email (`<login@users.noreply.github.com>` or the contributor's verified commit email) so attribution is rendered correctly.
- Keep trailers on their own lines after a blank line at commit-message end; never encode them as escaped `\\n` text.
- In the PR body, list superseded PR links and briefly state what was incorporated from each.
- If no actual code/design was incorporated (only inspiration), do not use `Co-authored-by`; give credit in PR notes instead.

### 9.3 Superseded-PR PR Template (Recommended)

When superseding multiple PRs, use a consistent title/body structure to reduce reviewer ambiguity.

- Recommended title format: `feat(<scope>): unify and supersede #<pr_a>, #<pr_b> [and #<pr_n>]`
- If this is docs/chore/meta only, keep the same supersede suffix and use the appropriate conventional-commit type.
- In the PR body, include the following template (fill placeholders, remove non-applicable lines):

```md
## Supersedes
- #<pr_a> by @<author_a>
- #<pr_b> by @<author_b>
- #<pr_n> by @<author_n>

## Integrated Scope
- From #<pr_a>: <what was materially incorporated>
- From #<pr_b>: <what was materially incorporated>
- From #<pr_n>: <what was materially incorporated>

## Attribution
- Co-authored-by trailers added for materially incorporated contributors: Yes/No
- If No, explain why (for example: no direct code/design carry-over)

## Non-goals
- <explicitly list what was not carried over>

## Risk and Rollback
- Risk: <summary>
- Rollback: <revert commit/PR strategy>
```

### 9.4 Superseded-PR Commit Template (Recommended)

When a commit unifies or supersedes prior PR work, use a deterministic commit message layout so attribution is machine-parsed and reviewer-friendly.

- Keep one blank line between message sections, and exactly one blank line before trailer lines.
- Keep each trailer on its own line; do not wrap, indent, or encode as escaped `\n` text.
- Add one `Co-authored-by` trailer per materially incorporated contributor, using GitHub-recognized email.
- If no direct code/design is carried over, omit `Co-authored-by` and explain attribution in the PR body instead.

```text
feat(<scope>): unify and supersede #<pr_a>, #<pr_b> [and #<pr_n>]

<one-paragraph summary of integrated outcome>

Supersedes:
- #<pr_a> by @<author_a>
- #<pr_b> by @<author_b>
- #<pr_n> by @<author_n>

Integrated scope:
- <subsystem_or_feature_a>: from #<pr_x>
- <subsystem_or_feature_b>: from #<pr_y>

Co-authored-by: <Name A> <login_a@users.noreply.github.com>
Co-authored-by: <Name B> <login_b@users.noreply.github.com>
```

Reference docs:

- `CONTRIBUTING.md`
- `docs/README.md`
- `docs/SUMMARY.md`
- `docs/docs-inventory.md`
- `docs/commands-reference.md`
- `docs/providers-reference.md`
- `docs/channels-reference.md`
- `docs/config-reference.md`
- `docs/operations-runbook.md`
- `docs/troubleshooting.md`
- `docs/one-click-bootstrap.md`
- `docs/pr-workflow.md`
- `docs/reviewer-playbook.md`
- `docs/ci-map.md`
- `docs/actions-source-policy.md`

## 10) Anti-Patterns (Do Not)

- Do not add heavy dependencies for minor convenience.
- Do not silently weaken security policy or access constraints.
- Do not add speculative config/feature flags “just in case”.
- Do not mix massive formatting-only changes with functional changes.
- Do not modify unrelated modules “while here”.
- Do not bypass failing checks without explicit explanation.
- Do not hide behavior-changing side effects in refactor commits.
- Do not include personal identity or sensitive information in test data, examples, docs, or commits.

## 11) Handoff Template (Agent -> Agent / Maintainer)

When handing off work, include:

1. What changed
2. What did not change
3. Validation run and results
4. Remaining risks / unknowns
5. Next recommended action

## 12) Vibe Coding Guardrails

When working in fast iterative mode:

- Keep each iteration reversible (small commits, clear rollback).
- Validate assumptions with code search before implementing.
- Prefer deterministic behavior over clever shortcuts.
- Do not “ship and hope” on security-sensitive paths.
- If uncertain, leave a concrete TODO with verification context, not a hidden guess.

## 13) CodeGraph — Semantic Code Intelligence (MCP)

This project has a `.codegraph/` directory with a pre-built semantic index. Use CodeGraph MCP tools for **faster, smarter code exploration** instead of raw file scanning.

### 13.1 Index Overview

| Metric | Value |
|--------|-------|
| Files indexed | 312 |
| Nodes | 10,862 (functions: 7,415 / structs: 709 / enums: 106 / traits: 23 / methods: 47) |
| Edges | 17,243 |
| Languages | Rust 259, Python 17, TSX 15, TypeScript 12, JavaScript 9 |

### 13.2 Available Tools

| Tool | Purpose | When to Use |
|------|---------|-------------|
| `codegraph_context` | Build comprehensive context for a task | **Start here** — returns entry points, related symbols, and key code for a given task description |
| `codegraph_search` | Find symbols by name | Quick lookup of functions, classes, types, structs by name or partial name |
| `codegraph_files` | Get project file structure | **Use instead of filesystem scanning** — much faster, includes metadata |
| `codegraph_callers` | Find all callers of a symbol | Trace upstream references — who calls this function? |
| `codegraph_callees` | Find all callees of a symbol | Trace downstream dependencies — what does this function call? |
| `codegraph_impact` | Analyze change blast radius | **Use before making changes** — shows what code could be affected |
| `codegraph_node` | Get details for a specific symbol | Retrieve signature, location, and optionally full source code |
| `codegraph_status` | Show index statistics | Verify index health and coverage |

### 13.3 Usage Protocol

1. **Explore structure first**: Use `codegraph_files` instead of glob/filesystem for project navigation.
2. **Build context before coding**: Use `codegraph_context` with a task description to get relevant entry points and code.
3. **Check impact before editing**: Use `codegraph_impact` on symbols you plan to modify.
4. **Trace call graphs**: Use `codegraph_callers` / `codegraph_callees` to understand code flow.
5. **Quick symbol lookup**: Use `codegraph_search` instead of grep for finding functions, structs, traits.

### 13.4 Keeping the Index Fresh

If source files have changed significantly since last indexing, run:

```bash
npx -y @colbymchenry/codegraph sync
```

For a full re-index:

```bash
npx -y @colbymchenry/codegraph index --force
```

### 13.5 MCP Server Configuration & Multi-Project Usage

**The MCP server itself is project-agnostic** — do NOT hardcode `--path` in the server startup args, as that would break all other projects.

**Correct MCP server config** (no path binding):

```json
{
  "mcpServers": {
    "codegraph": {
      "command": "npx",
      "args": ["-y", "@colbymchenry/codegraph", "serve", "--mcp"]
    }
  }
}
```

**How to use with a specific project**: Always pass `projectPath` explicitly in every tool call.

```
codegraph_status(projectPath: "C:\\Dev\\zeroclaw")
codegraph_files(projectPath: "C:\\Dev\\zeroclaw")
codegraph_context(task: "...", projectPath: "C:\\Dev\\zeroclaw")
```

If `projectPath` is omitted, the server falls back to its working directory — which is usually wrong. **Always pass it.**

**Prerequisites**: The target project must have a `.codegraph/` directory. Run the following to initialize a new project:

```bash
npx -y @colbymchenry/codegraph init --index "C:\path\to\other-project"
```

## 14) Email Monitor → Telegram Notification (Session 2026-02-25)

### 14.1 Feature Goal

Email channel in `monitor` mode should:
1. Detect new emails via IMAP IDLE
2. Agent analyzes and classifies (important vs junk)
3. Agent uses `send_telegram` tool to notify user on Telegram
4. **User decides** what to do (ignore / ask agent to draft reply / ask agent to send reply)
5. Agent must **NOT** send any email unless explicitly instructed by user

### 14.2 Changes Made (This Session)

#### `src/channels/email_channel.rs`
- **Self-email filter** (line ~301): `fetch_unseen()` skips emails where sender == `from_address` to prevent infinite self-reply loops.
- **Digest prompt** (line ~540): Simplified to: analyze, classify, summarize in Chinese, notify user. Rule: do NOT use `send_email` tool.
- **ChannelMessage sender** (line ~585): Changed from `"email-monitor (username)"` to `reply_target` (user's chat_id). This ensures the digest message shares the user's existing Telegram conversation history via `conversation_history_key()`.

#### `src/tools/send_telegram.rs` [NEW]
- New `SendTelegramTool` implementing the `Tool` trait.
- Uses Telegram Bot API `sendMessage` with `bot_token` from `TelegramConfig`.
- Supports Markdown formatting with plain-text fallback.
- Parameters: `chat_id` (required), `message` (required).

#### `src/tools/mod.rs`
- Added `send_telegram` module declaration and `pub use`.
- Conditionally registers `SendTelegramTool` in `all_tools_with_runtime()` when telegram channel is configured.

#### `src/channels/mod.rs`
- **Tool exclusion for email digests** (line ~1732-1770): When `msg.id.starts_with("email-digest-")`, `send_email` is added to `excluded_tools` so the LLM cannot see or call it during digest processing.

### 14.3 Bug Status — ✅ FIXED (2026-02-27)

~~**`notify_channel` and `notify_to` are not configured in the actual config file.**~~

**已修复**：`资料/config.toml` 第 191-192 行已正确配置：

```toml
notify_channel = "telegram"
notify_to = "495916105"
```

`src/channels/email_channel.rs:565-596` 中的 `process_unseen_monitor()` 正确读取这两个字段并将 ChannelMessage 路由到 Telegram。`send_email` 工具在 email-digest 处理期间被正确排除（`src/channels/mod.rs:1785-1821`）。整个流程已验证正确。

### 14.4 Architecture Notes

- `conversation_history_key()` at line 272 uses format `{channel}_{sender}` — sender MUST match the user's identity for shared conversation context.
- `channel_delivery_instructions()` at line 400 adds Telegram formatting rules to the system prompt when `channel == "telegram"`.
- `build_channel_system_prompt()` at line 420 injects `channel=<name>, reply_target=<id>` into the system prompt.
- `run_tool_call_loop()` at line 2091 filters `excluded_tools` from `tool_specs` before sending to LLM — the LLM never sees excluded tools.
- `EmailChannel.send()` at line 689 sends via SMTP. `TelegramChannel.send()` at line 2419 sends via Bot API. Which one is called depends entirely on `target_channel = channels_by_name.get(msg.channel)`.

### 14.5 Correct Flow (After Config Fix)

```
Email arrives (IMAP IDLE)
    ↓
fetch_unseen() — filters out self-sent emails
    ↓
process_unseen_monitor() — builds digest
    ↓
ChannelMessage{channel: "telegram", reply_target: "495916105", sender: "495916105"}
    ↓
process_channel_message() picks it up
    ↓
target_channel = channels_by_name["telegram"] → TelegramChannel
    ↓
Agent processes digest (send_email tool excluded)
    ↓
Agent uses send_telegram tool OR text reply → both go to Telegram
    ↓
User sees notification on Telegram → decides next action
```

## 15) 架构债务记录（2026-02-27 分析）

> 这些问题目前不影响功能，但需要在后续整体重构时解决。详细改造方案见 `Research.md` 第五、六节。

### 15.1 孤儿文件：`src/heartbeat/engine.rs`

该文件包含完整的 `HeartbeatEngine` 实现（`run()`、`tick()`、`collect_tasks()`、`parse_tasks()`），
但生产代码中这些方法**完全未被调用**——仅在测试代码中使用。

实际的 heartbeat 逻辑已被独立重写在 `src/daemon/mod.rs:run_heartbeat_worker()` 中。
`engine.rs` 的唯一生产用途是 `ensure_heartbeat_file()`（daemon 启动时建文件，`daemon/mod.rs:22`）。

**结论**：`engine.rs` 是孤儿文件。重构时可将 `ensure_heartbeat_file()` 移到 daemon 内或 util 模块，然后删除整个 `src/heartbeat/` 目录。

### 15.2 活跃时间硬编码 — ✅ FIXED (2026-02-27)

~~位置：`src/daemon/mod.rs:188-194`~~
**已修复**：引入了分钟精度的可配置化检查。

- `HeartbeatConfig` 新增了 `active_hours_start` 和 `active_hours_end` 字段（`HH:MM` 格式）
- 支持配置文件 `[heartbeat]` 设置，并支持跨午夜时间段
- `daemon/mod.rs` 现读取配置进行过滤

### 15.3 渠道路由架构割裂

Heartbeat/Cron 投递路径（`deliver_announcement`）与普通消息路径（`channels_by_name.get()`）是两套独立代码：

| 对比项 | 普通消息 | Heartbeat/Cron |
|--------|---------|----------------|
| 渠道查找 | `channels_by_name.get()` HashMap | `match channel` 硬编码 |
| Channel 实例化 | 启动时统一创建 | 每次投递时重新 `::new()` |
| 支持渠道数 | 全部已配置渠道 | 仅 telegram/discord/slack/mattermost |
| 代码位置 | `channels/mod.rs` | `daemon/mod.rs:347` + `scheduler.rs:308` |

重构时应统一为单一投递路径，通过 `Channel` trait 发送，消除硬编码 match。

