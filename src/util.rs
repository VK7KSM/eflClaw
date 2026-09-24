//! Utility functions for `ZeroClaw`.
//!
//! This module contains reusable helper functions used across the codebase.

/// Truncate a string to at most `max_chars` characters, appending "..." if truncated.
///
/// This function safely handles multi-byte UTF-8 characters (emoji, CJK, accented characters)
/// by using character boundaries instead of byte indices.
///
/// # Arguments
/// * `s` - The string to truncate
/// * `max_chars` - Maximum number of characters to keep (excluding "...")
///
/// # Returns
/// * Original string if length <= `max_chars`
/// * Truncated string with "..." appended if length > `max_chars`
///
/// # Examples
/// ```ignore
/// use zeroclaw::util::truncate_with_ellipsis;
///
/// // ASCII string - no truncation needed
/// assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
///
/// // ASCII string - truncation needed
/// assert_eq!(truncate_with_ellipsis("hello world", 5), "hello...");
///
/// // Multi-byte UTF-8 (emoji) - safe truncation
/// assert_eq!(truncate_with_ellipsis("Hello 🦀 World", 8), "Hello 🦀...");
/// assert_eq!(truncate_with_ellipsis("😀😀😀😀", 2), "😀😀...");
///
/// // Empty string
/// assert_eq!(truncate_with_ellipsis("", 10), "");
/// ```
pub fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => {
            let truncated = &s[..idx];
            // Trim trailing whitespace for cleaner output
            format!("{}...", truncated.trim_end())
        }
        None => s.to_string(),
    }
}

/// Return the greatest valid UTF-8 char boundary at or below `index`.
///
/// This mirrors `str::floor_char_boundary` behavior while remaining compatible
/// with stable toolchains where that API is not available.
pub fn floor_utf8_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }

    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Allowed serial device path prefixes shared across hardware transports.
pub const ALLOWED_SERIAL_PATH_PREFIXES: &[&str] = &[
    "/dev/ttyACM",
    "/dev/ttyUSB",
    "/dev/tty.usbmodem",
    "/dev/cu.usbmodem",
    "/dev/tty.usbserial",
    "/dev/cu.usbserial",
    "COM",
];

/// Validate serial device path against per-platform rules.
pub fn is_serial_path_allowed(path: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::sync::OnceLock;
        if !std::path::Path::new(path).is_absolute() {
            return false;
        }
        static PAT: OnceLock<regex::Regex> = OnceLock::new();
        let re = PAT.get_or_init(|| {
            regex::Regex::new(r"^/dev/tty(ACM|USB|S|AMA|MFD)\d+$").expect("valid regex")
        });
        return re.is_match(path);
    }

    #[cfg(target_os = "macos")]
    {
        use std::sync::OnceLock;
        if !std::path::Path::new(path).is_absolute() {
            return false;
        }
        static PAT: OnceLock<regex::Regex> = OnceLock::new();
        let re = PAT.get_or_init(|| {
            regex::Regex::new(r"^/dev/(tty|cu)\.(usbmodem|usbserial)[^\x00/]*$")
                .expect("valid regex")
        });
        return re.is_match(path);
    }

    #[cfg(target_os = "windows")]
    {
        use std::sync::OnceLock;
        static PAT: OnceLock<regex::Regex> = OnceLock::new();
        let re = PAT.get_or_init(|| regex::Regex::new(r"^COM\d{1,3}$").expect("valid regex"));
        return re.is_match(path);
    }

    #[allow(unreachable_code)]
    false
}

/// Utility enum for handling optional values.
pub enum MaybeSet<T> {
    Set(T),
    Unset,
    Null,
}

/// Object keys whose values are replaced by `redact_sensitive_json`
/// (matched case-insensitively as substrings, e.g. `password_field`,
/// `api_key`, `credentials`).
const SENSITIVE_KEY_PARTS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "credential",
];

/// elfClaw 2026-09-24: copy of `value` with every object value whose key looks
/// sensitive replaced by `"[REDACTED]"`, at any depth and whatever its length
/// or type. For tool arguments that are written to logs or shown in approval
/// prompts (e.g. `web_login` credentials) — unlike the regex-based
/// `scrub_credentials`, short values and nested objects are covered too.
pub fn redact_sensitive_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, v)| {
                    let lower = key.to_ascii_lowercase();
                    let redacted = if SENSITIVE_KEY_PARTS.iter().any(|part| lower.contains(part)) {
                        serde_json::Value::String("[REDACTED]".into())
                    } else {
                        redact_sensitive_json(v)
                    };
                    (key.clone(), redacted)
                })
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_sensitive_json).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_sensitive_json_hides_nested_and_short_values() {
        let args = serde_json::json!({
            "session_id": "zeroclaw_session",
            "login_url": "https://example.com/login",
            "credentials": {"username": "zeroclaw_user", "password": "pw1"},
            "steps": [{"api_key": "k"}, {"note": "keep"}],
            "Token": 42
        });
        let redacted = redact_sensitive_json(&args);
        assert_eq!(redacted["credentials"], "[REDACTED]");
        assert_eq!(redacted["steps"][0]["api_key"], "[REDACTED]");
        assert_eq!(redacted["steps"][1]["note"], "keep");
        assert_eq!(redacted["Token"], "[REDACTED]");
        assert_eq!(redacted["session_id"], "zeroclaw_session");
        assert_eq!(redacted["login_url"], "https://example.com/login");
        let text = redacted.to_string();
        assert!(!text.contains("pw1") && !text.contains("zeroclaw_user"));
    }

    #[test]
    fn test_truncate_ascii_no_truncation() {
        // ASCII string shorter than limit - no change
        assert_eq!(truncate_with_ellipsis("hello", 10), "hello");
        assert_eq!(truncate_with_ellipsis("hello world", 50), "hello world");
    }

    #[test]
    fn test_truncate_ascii_with_truncation() {
        // ASCII string longer than limit - truncates
        assert_eq!(truncate_with_ellipsis("hello world", 5), "hello...");
        assert_eq!(
            truncate_with_ellipsis("This is a long message", 10),
            "This is a..."
        );
    }

    #[test]
    fn test_truncate_empty_string() {
        assert_eq!(truncate_with_ellipsis("", 10), "");
    }

    #[test]
    fn test_truncate_at_exact_boundary() {
        // String exactly at boundary - no truncation
        assert_eq!(truncate_with_ellipsis("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_emoji_single() {
        // Single emoji (4 bytes) - should not panic
        let s = "🦀";
        assert_eq!(truncate_with_ellipsis(s, 10), s);
        assert_eq!(truncate_with_ellipsis(s, 1), s);
    }

    #[test]
    fn test_truncate_emoji_multiple() {
        // Multiple emoji - safe truncation at character boundary
        let s = "😀😀😀😀"; // 4 emoji, each 4 bytes = 16 bytes total
        assert_eq!(truncate_with_ellipsis(s, 2), "😀😀...");
        assert_eq!(truncate_with_ellipsis(s, 3), "😀😀😀...");
    }

    #[test]
    fn test_truncate_mixed_ascii_emoji() {
        // Mixed ASCII and emoji
        assert_eq!(truncate_with_ellipsis("Hello 🦀 World", 8), "Hello 🦀...");
        assert_eq!(truncate_with_ellipsis("Hi 😊", 10), "Hi 😊");
    }

    #[test]
    fn test_truncate_cjk_characters() {
        // CJK characters (Chinese - each is 3 bytes)
        let s = "这是一个测试消息用来触发崩溃的中文"; // 21 characters
        let result = truncate_with_ellipsis(s, 16);
        assert!(result.ends_with("..."));
        assert!(result.is_char_boundary(result.len() - 1));
    }

    #[test]
    fn test_truncate_accented_characters() {
        // Accented characters (2 bytes each in UTF-8)
        let s = "café résumé naïve";
        assert_eq!(truncate_with_ellipsis(s, 10), "café résum...");
    }

    #[test]
    fn test_truncate_unicode_edge_case() {
        // Mix of 1-byte, 2-byte, 3-byte, and 4-byte characters
        let s = "aé你好🦀"; // 1 + 1 + 2 + 2 + 4 bytes = 10 bytes, 5 chars
        assert_eq!(truncate_with_ellipsis(s, 3), "aé你...");
    }

    #[test]
    fn test_truncate_long_string() {
        // Long ASCII string
        let s = "a".repeat(200);
        let result = truncate_with_ellipsis(&s, 50);
        assert_eq!(result.len(), 53); // 50 + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_zero_max_chars() {
        // Edge case: max_chars = 0
        assert_eq!(truncate_with_ellipsis("hello", 0), "...");
    }

    #[test]
    fn test_floor_utf8_char_boundary_ascii() {
        assert_eq!(floor_utf8_char_boundary("hello", 0), 0);
        assert_eq!(floor_utf8_char_boundary("hello", 3), 3);
        assert_eq!(floor_utf8_char_boundary("hello", 99), 5);
    }

    #[test]
    fn test_floor_utf8_char_boundary_multibyte() {
        let s = "aé你🦀";
        assert_eq!(floor_utf8_char_boundary(s, 1), 1);
        // Index 2 is inside "é" (2-byte char), floor should move back to 1.
        assert_eq!(floor_utf8_char_boundary(s, 2), 1);
        // Index 5 is inside "你" (3-byte char), floor should move back to 3.
        assert_eq!(floor_utf8_char_boundary(s, 5), 3);
    }
}
