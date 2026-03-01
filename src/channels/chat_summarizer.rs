//! Chat log auto-summarization worker.
//!
//! Scans JSON chat log files, detects changes via content hashing,
//! and generates summaries using a lightweight LLM model.
//! Summaries are stored in the SQLite `chat_summaries` table.

use anyhow::Result;

use crate::channels::chat_index::{file_content_hash, ChatIndex};
use crate::channels::chat_log;
use crate::config::Config;
use crate::providers::{self, traits::Provider, ProviderRuntimeOptions};

/// Report of a summarization run.
#[derive(Debug, Default)]
pub struct SummarizeReport {
    pub processed: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

/// Scan all chat log JSON files, generate summaries for changed files,
/// and write them to the SQLite index.
///
/// Uses the existing provider factory from `Config` — no hardcoded
/// provider names. Falls back to `default_model` when `summary_model`
/// is not configured.
pub async fn summarize_chat_logs(config: &Config) -> Result<SummarizeReport> {
    let mut report = SummarizeReport::default();

    let workspace = &config.workspace_dir;
    let entries = chat_log::list_log_files(workspace)?;
    if entries.is_empty() {
        return Ok(report);
    }

    let index = ChatIndex::open(workspace)?;

    // Determine which model to use
    let model = config
        .summary_model
        .as_deref()
        .or(config.default_model.as_deref())
        .unwrap_or("claude-haiku-4-5-20251001");

    // Build provider using the SAME factory as channels/agent — supports all
    // provider formats (anthropic-custom:URL, openai, ollama, gemini etc.)
    // and handles encrypted API keys automatically.
    let provider_name = config.default_provider.as_deref().unwrap_or("openrouter");

    let options = ProviderRuntimeOptions {
        zeroclaw_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        ..Default::default()
    };

    let provider = providers::create_resilient_provider_with_options(
        provider_name,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &options,
    )?;

    for entry in &entries {
        match process_single_file(&index, provider.as_ref(), model, entry).await {
            Ok(true) => report.processed += 1,
            Ok(false) => report.skipped += 1,
            Err(e) => {
                let msg = format!("{}/{}: {e}", entry.username, entry.date);
                tracing::warn!("Chat summarization error: {msg}");
                report.errors.push(msg);
            }
        }
    }

    // Run watchdog check after summarization
    if let Ok(Some(warning)) = index.watchdog_check() {
        tracing::warn!("{warning}");
    }

    Ok(report)
}

/// Process a single JSON log file.
/// Returns Ok(true) if processed, Ok(false) if skipped (no changes).
async fn process_single_file(
    index: &ChatIndex,
    provider: &dyn Provider,
    model: &str,
    entry: &chat_log::LogFileEntry,
) -> Result<bool> {
    // Read file content
    let content = std::fs::read_to_string(&entry.path)?;

    // Parse JSON first — we need chat_id for hash lookup
    let log: chat_log::DailyChatLog = serde_json::from_str(&content)?;
    if log.messages.is_empty() {
        return Ok(false);
    }

    // Compute hash and check against existing
    let current_hash = file_content_hash(&content);
    let existing_hash = index.get_source_hash("telegram", &log.chat_id, &entry.date)?;
    if existing_hash.as_deref() == Some(&current_hash) {
        return Ok(false); // No changes
    }

    let msg_count = log.messages.len() as i64;

    // Build conversation text for the LLM (limit to 50 messages)
    let messages_text: String = log
        .messages
        .iter()
        .rev()
        .take(50)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|m| format!("[{}] {}: {}", m.ts, m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "请用中文总结以下聊天记录，严格按照下面的格式输出两行，不要加任何多余内容：\n\
         摘要：<一句话总结对话的主要内容>\n\
         话题：<用逗号分隔的2-5个关键话题>\n\n\
         聊天记录（{} 与 {} 在 {} 的对话）：\n{}",
        log.chat_name, "ZeroClaw", entry.date, messages_text
    );

    // Call the LLM
    let response = provider.simple_chat(&prompt, model, 0.3).await?;

    // Parse the response
    let (summary, topics) = parse_summary_response(&response);

    // Write to SQLite index
    index.upsert_summary(
        "telegram",
        &log.chat_id,
        &log.chat_name,
        &entry.date,
        &summary,
        topics.as_deref(),
        None, // embedding placeholder
        msg_count,
        &current_hash,
    )?;

    tracing::info!(
        "Summarized {}/{}: {} msgs → {}",
        entry.username,
        entry.date,
        msg_count,
        &summary
    );

    Ok(true)
}

/// Parse "摘要：...\n话题：..." format from LLM response.
fn parse_summary_response(response: &str) -> (String, Option<String>) {
    let mut summary = response.trim().to_string();
    let mut topics: Option<String> = None;

    for line in response.lines() {
        let line = line.trim();
        if let Some(s) = line
            .strip_prefix("摘要：")
            .or_else(|| line.strip_prefix("摘要:"))
        {
            summary = s.trim().to_string();
        } else if let Some(t) = line
            .strip_prefix("话题：")
            .or_else(|| line.strip_prefix("话题:"))
        {
            topics = Some(t.trim().to_string());
        }
    }

    (summary, topics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_summary_standard_format() {
        let response = "摘要：讨论了天气和晚饭计划\n话题：天气,晚饭,计划";
        let (summary, topics) = parse_summary_response(response);
        assert_eq!(summary, "讨论了天气和晚饭计划");
        assert_eq!(topics.as_deref(), Some("天气,晚饭,计划"));
    }

    #[test]
    fn parse_summary_colon_variant() {
        let response = "摘要:简单的问候\n话题:问候";
        let (summary, topics) = parse_summary_response(response);
        assert_eq!(summary, "简单的问候");
        assert_eq!(topics.as_deref(), Some("问候"));
    }

    #[test]
    fn parse_summary_freeform() {
        let response = "这是一段关于编程的对话";
        let (summary, topics) = parse_summary_response(response);
        assert_eq!(summary, "这是一段关于编程的对话");
        assert!(topics.is_none());
    }

    #[test]
    fn parse_summary_with_extra_whitespace() {
        let response = "  摘要：  带空格的摘要  \n  话题：  话题1, 话题2  ";
        let (summary, topics) = parse_summary_response(response);
        assert_eq!(summary, "带空格的摘要");
        assert_eq!(topics.as_deref(), Some("话题1, 话题2"));
    }
}
