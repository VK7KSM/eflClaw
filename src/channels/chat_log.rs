//! Chat log persistence — local JSON files per user per day.
//!
//! Each Telegram conversation is saved as a daily JSON file under
//! `{workspace_dir}/memory/telegram/{username}_{YYYY-MM-DD}.json`.
//!
//! This module handles:
//! - **Append**: write each turn (user + assistant) to the daily file
//! - **Load today**: restore current day's messages on startup
//! - **Load recent**: retrieve last N messages across days
//! - **List users**: scan directory for known chat partners

use chrono::{Local, NaiveDate};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A single chat turn stored in the JSON log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
    pub ts: String,
    #[serde(rename = "type")]
    pub msg_type: String,
    /// STT transcription text (for incoming voice messages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stt_text: Option<String>,
    /// TTS source text (for outgoing voice messages).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts_text: Option<String>,
    /// Path to image file (relative to workspace).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_path: Option<String>,
}

/// Top-level structure of a daily chat log JSON file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyChatLog {
    pub chat_id: String,
    pub chat_name: String,
    pub date: String,
    pub messages: Vec<ChatTurn>,
}

/// Compute the directory for chat logs.
fn chat_log_dir(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("memory").join("telegram")
}

/// Generate the file path for a specific user and date.
pub fn log_file_path(workspace_dir: &Path, username: &str, date: &NaiveDate) -> PathBuf {
    let filename = format!(
        "{}_{}.json",
        sanitize_filename(username),
        date.format("%Y-%m-%d")
    );
    chat_log_dir(workspace_dir).join(filename)
}

/// Current day's log file path.
fn today_log_path(workspace_dir: &Path, username: &str) -> PathBuf {
    let today = Local::now().date_naive();
    log_file_path(workspace_dir, username, &today)
}

/// Sanitize username for use in filenames (remove unsafe chars).
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Append a turn to today's log file for the given user.
///
/// Creates the file (and parent directories) if they don't exist.
/// If the file already exists, loads it, appends the turn, and rewrites.
pub fn append_turn(
    workspace_dir: &Path,
    chat_id: &str,
    username: &str,
    turn: ChatTurn,
) -> anyhow::Result<()> {
    let path = today_log_path(workspace_dir, username);

    // Ensure directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut log = if path.exists() {
        let content = std::fs::read_to_string(&path)?;
        serde_json::from_str::<DailyChatLog>(&content).unwrap_or_else(|_| DailyChatLog {
            chat_id: chat_id.to_string(),
            chat_name: username.to_string(),
            date: Local::now().date_naive().format("%Y-%m-%d").to_string(),
            messages: Vec::new(),
        })
    } else {
        DailyChatLog {
            chat_id: chat_id.to_string(),
            chat_name: username.to_string(),
            date: Local::now().date_naive().format("%Y-%m-%d").to_string(),
            messages: Vec::new(),
        }
    };

    log.messages.push(turn);

    let json = serde_json::to_string_pretty(&log)?;
    std::fs::write(&path, json)?;

    Ok(())
}

/// Create a text-type turn.
pub fn text_turn(role: &str, content: &str) -> ChatTurn {
    ChatTurn {
        role: role.to_string(),
        content: content.to_string(),
        ts: Local::now().to_rfc3339(),
        msg_type: "text".to_string(),
        stt_text: None,
        tts_text: None,
        image_path: None,
    }
}

/// Create a voice-type turn (incoming with STT).
pub fn voice_turn_user(stt_text: &str) -> ChatTurn {
    ChatTurn {
        role: "user".to_string(),
        content: stt_text.to_string(),
        ts: Local::now().to_rfc3339(),
        msg_type: "voice".to_string(),
        stt_text: Some(stt_text.to_string()),
        tts_text: None,
        image_path: None,
    }
}

/// Create a voice-type turn (outgoing with TTS).
pub fn voice_turn_assistant(tts_text: &str) -> ChatTurn {
    ChatTurn {
        role: "assistant".to_string(),
        content: tts_text.to_string(),
        ts: Local::now().to_rfc3339(),
        msg_type: "voice".to_string(),
        stt_text: None,
        tts_text: Some(tts_text.to_string()),
        image_path: None,
    }
}

/// Create an image-type turn.
pub fn image_turn(content: &str, image_path: &str) -> ChatTurn {
    ChatTurn {
        role: "user".to_string(),
        content: content.to_string(),
        ts: Local::now().to_rfc3339(),
        msg_type: "image".to_string(),
        stt_text: None,
        tts_text: None,
        image_path: Some(image_path.to_string()),
    }
}

/// Load messages from today's log file for a specific user.
pub fn load_today_messages(workspace_dir: &Path, username: &str) -> anyhow::Result<Vec<ChatTurn>> {
    let path = today_log_path(workspace_dir, username);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path)?;
    let log: DailyChatLog = serde_json::from_str(&content)?;
    Ok(log.messages)
}

/// Load the most recent N messages for a user, searching backwards through
/// daily files until enough messages are found or no more files exist.
pub fn load_recent_messages(
    workspace_dir: &Path,
    username: &str,
    limit: usize,
) -> anyhow::Result<Vec<ChatTurn>> {
    let dir = chat_log_dir(workspace_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let prefix = format!("{}_", sanitize_filename(username));
    let mut dated_files: Vec<(NaiveDate, PathBuf)> = Vec::new();

    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        if let Some(rest) = file_name.strip_prefix(&prefix) {
            if let Some(date_str) = rest.strip_suffix(".json") {
                if let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                    dated_files.push((date, entry.path()));
                }
            }
        }
    }

    // Sort descending by date (most recent first)
    dated_files.sort_by(|a, b| b.0.cmp(&a.0));

    let mut result = Vec::new();
    for (_date, path) in dated_files {
        if result.len() >= limit {
            break;
        }
        let content = std::fs::read_to_string(&path)?;
        if let Ok(log) = serde_json::from_str::<DailyChatLog>(&content) {
            // Prepend older messages (we'll reverse at the end)
            for msg in log.messages.into_iter().rev() {
                result.push(msg);
                if result.len() >= limit {
                    break;
                }
            }
        }
    }

    // Reverse so messages are in chronological order
    result.reverse();
    Ok(result)
}

/// List all known chat usernames by scanning the log directory.
pub fn list_chat_users(workspace_dir: &Path) -> anyhow::Result<Vec<String>> {
    let dir = chat_log_dir(workspace_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut users = std::collections::HashSet::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        if file_name.ends_with(".json") {
            // Format: username_YYYY-MM-DD.json — split from the right at last '_'
            // to handle usernames that contain underscores
            if let Some(pos) = file_name.rfind('_') {
                let username = &file_name[..pos];
                if !username.is_empty() {
                    users.insert(username.to_string());
                }
            }
        }
    }

    let mut sorted: Vec<String> = users.into_iter().collect();
    sorted.sort();
    Ok(sorted)
}

/// Load all users' today messages into a map (for startup restoration).
pub fn load_all_today_messages(
    workspace_dir: &Path,
) -> anyhow::Result<std::collections::HashMap<String, Vec<ChatTurn>>> {
    let users = list_chat_users(workspace_dir)?;
    let mut result = std::collections::HashMap::new();
    for username in users {
        let messages = load_today_messages(workspace_dir, &username)?;
        if !messages.is_empty() {
            result.insert(username, messages);
        }
    }
    Ok(result)
}

/// An entry from a scanned log file, with parsed metadata.
#[derive(Debug, Clone)]
pub struct LogFileEntry {
    pub username: String,
    pub date: String,
    pub path: PathBuf,
}

/// List all log files in the telegram chat log directory.
/// Returns entries with parsed (username, date, filepath).
pub fn list_log_files(workspace_dir: &Path) -> anyhow::Result<Vec<LogFileEntry>> {
    let dir = chat_log_dir(workspace_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().to_string();
        if !file_name.ends_with(".json") {
            continue;
        }
        // Format: username_YYYY-MM-DD.json — split from the right at last '_'
        if let Some(pos) = file_name.rfind('_') {
            let username = &file_name[..pos];
            if let Some(date_str) = file_name[pos + 1..].strip_suffix(".json") {
                if NaiveDate::parse_from_str(date_str, "%Y-%m-%d").is_ok() {
                    entries.push(LogFileEntry {
                        username: username.to_string(),
                        date: date_str.to_string(),
                        path: entry.path(),
                    });
                }
            }
        }
    }

    // Sort by date ascending
    entries.sort_by(|a, b| a.date.cmp(&b.date));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn append_and_load_today_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();

        append_turn(ws, "123", "TestUser", text_turn("user", "Hello")).unwrap();
        append_turn(ws, "123", "TestUser", text_turn("assistant", "Hi there!")).unwrap();

        let messages = load_today_messages(ws, "TestUser").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "Hello");
        assert_eq!(messages[0].msg_type, "text");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "Hi there!");
    }

    #[test]
    fn voice_turn_preserves_stt_tts() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();

        append_turn(ws, "123", "Alice", voice_turn_user("今天天气")).unwrap();
        append_turn(ws, "123", "Alice", voice_turn_assistant("悉尼晴天")).unwrap();

        let messages = load_today_messages(ws, "Alice").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].msg_type, "voice");
        assert_eq!(messages[0].stt_text.as_deref(), Some("今天天气"));
        assert_eq!(messages[1].msg_type, "voice");
        assert_eq!(messages[1].tts_text.as_deref(), Some("悉尼晴天"));
    }

    #[test]
    fn image_turn_preserves_path() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();

        append_turn(
            ws,
            "123",
            "Bob",
            image_turn("[IMAGE] photo", "telegram_files/photo.jpg"),
        )
        .unwrap();

        let messages = load_today_messages(ws, "Bob").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].msg_type, "image");
        assert_eq!(
            messages[0].image_path.as_deref(),
            Some("telegram_files/photo.jpg")
        );
    }

    #[test]
    fn list_chat_users_finds_all() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();

        append_turn(ws, "1", "Alice", text_turn("user", "hi")).unwrap();
        append_turn(ws, "2", "Bob", text_turn("user", "hey")).unwrap();

        let users = list_chat_users(ws).unwrap();
        assert_eq!(users, vec!["Alice", "Bob"]);
    }

    #[test]
    fn load_recent_messages_respects_limit() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();

        for i in 0..10 {
            append_turn(ws, "1", "User", text_turn("user", &format!("msg {i}"))).unwrap();
        }

        let recent = load_recent_messages(ws, "User", 5).unwrap();
        assert_eq!(recent.len(), 5);
        // Should be the last 5 messages
        assert_eq!(recent[0].content, "msg 5");
        assert_eq!(recent[4].content, "msg 9");
    }

    #[test]
    fn sanitize_filename_removes_unsafe_chars() {
        assert_eq!(sanitize_filename("user@name"), "user_name");
        assert_eq!(sanitize_filename("normal_user"), "normal_user");
        assert_eq!(sanitize_filename("user/path"), "user_path");
    }

    #[test]
    fn load_all_today_messages_returns_map() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();

        append_turn(ws, "1", "Alice", text_turn("user", "hi")).unwrap();
        append_turn(ws, "2", "Bob", text_turn("user", "hey")).unwrap();

        let all = load_all_today_messages(ws).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.contains_key("Alice"));
        assert!(all.contains_key("Bob"));
    }

    #[test]
    fn empty_dir_returns_empty_results() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();

        let messages = load_today_messages(ws, "Nobody").unwrap();
        assert!(messages.is_empty());

        let users = list_chat_users(ws).unwrap();
        assert!(users.is_empty());

        let recent = load_recent_messages(ws, "Nobody", 10).unwrap();
        assert!(recent.is_empty());
    }
}
